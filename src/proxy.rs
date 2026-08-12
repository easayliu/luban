//! 转发代理：Claude Code → luban → 官方 Anthropic API。
//!
//! 透传请求体，仅替换鉴权：校验来访 API Key 后，注入选中凭证的 OAuth access_token
//! 与 `anthropic-beta: oauth-2025-04-20`，响应流式原样回传。

use axum::{
    body::{Body, Bytes},
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use futures_util::StreamExt;
use rand::RngExt;

use crate::config;
use crate::store;
use crate::web::AppState;

/// 转发 `/v1/*` 到官方 API。
pub async fn handle(
    State(state): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let started = std::time::Instant::now();
    let path_and_query =
        uri.path_and_query().map(|pq| pq.as_str()).unwrap_or(uri.path()).to_string();
    // 来访 UA：转发日志与各条拒绝日志都带上（都是 info/warn，不必开 debug）。整组识别头那条
    // debug 留着不动——排查形态时才需要那六项，日常只要认出「谁在发」，一项就够。
    let client_ua = ua_of(&headers);

    // 1) 校验来访 API Key（未配置则放行）。生效 key：环境覆盖优先，否则用库中配置。
    if let Some(expected) = effective_client_key(&state)
        && !client_authorized(&headers, &expected)
    {
        tracing::warn!(%method, path = %path_and_query, ua = %client_ua, "rejected: invalid inbound API key");
        return (StatusCode::UNAUTHORIZED, "invalid API key").into_response();
    }

    // 2) 请求体只解析这一次，下面五项判定全从这份结果上读。
    //
    //    此前 extract_device_id / body_has_user_id / request_model / request_speed 各自
    //    `from_slice` 一遍整个 body，`Simulation::detect` 再来一遍，加上 `rewrite_body`
    //    自己那次，一条请求要把同一份 JSON 完整解析 6 次以上（429 换号重试时后两项还按轮次
    //    翻倍）。body 上限刚放到 64MB，长对话几 MB 是常态，这是白烧的 CPU。
    //
    //    `rewrite_body` 仍自己解析：它要一份**可变且每轮独立**的副本（每次重试都从客户端
    //    原始体重新改写），共用这份只读的反而要多克隆一次。
    //
    //    解析失败（不是 JSON）时为 `None`，各项判定按「读不出来」退化，与逐个解析时一致。
    let body_json: Option<serde_json::Value> = serde_json::from_slice(&body).ok();

    // 提取 device_id（在 metadata.user_id 里；兼容 CC 内嵌 JSON 与扁平串两种格式）。
    let device_id = extract_device_id(body_json.as_ref());
    // 该字段在不在（与「能否解析出设备标识」是两回事）：决定要不要给它补一份官方身份。
    // body 逐轮不变，算一次即可。见 [`Upstream::bare_session`]。
    let has_user_id = body_has_user_id(body_json.as_ref());

    // 2.1) 这条路径是否消耗订阅额度——决定要不要卡设备身份、要不要改写出站体。
    //      判定吃 `uri.path()` 而非上面那个带查询串的 `path_and_query`：豁免要精确匹配。
    let billable = is_billable_messages(uri.path());

    // 2.2) 无有效设备身份（无 metadata / 无法识别的 user_id 格式）→ 计费路径默认直接拒绝：
    //      这类请求既无法做身份伪装、也无从计入设备上限（会绕过 device_limit）。
    //      网页可关掉该校验（放行裸客户端），此时它们退化为不绑定、不占名额的负载均衡挑选。
    if device_id.is_none() {
        if billable && state.store.require_device_id() {
            tracing::warn!(%method, path = %path_and_query, ua = %client_ua, "rejected: request has no usable device identity (metadata.user_id missing or unrecognized)");
            return (StatusCode::FORBIDDEN, "missing a usable device identity (metadata.user_id)")
                .into_response();
        }
        tracing::debug!(%method, path = %path_and_query, billable, "allowing a request with no device identity");
    }

    // 3) 按 device_id 粘性选出凭证的 access_token（必要时刷新）。
    // 请求的模型名：冷却按「账号 + 模型」分格，fable 那类模型级 429 不该拖累整个账号。
    let req_model = request_model(body_json.as_ref());
    // 首发与换号重试用同一份选号入参，只有「已试过哪些号」不同——写成函数而不是就地各构一份，
    // 免得两处的 device_id/model 哪天漂开。
    fn select<'a>(
        device_id: Option<&'a str>,
        billable: bool,
        model: Option<&'a str>,
        exclude: &'a [i64],
    ) -> store::Select<'a> {
        store::Select { device_id, rate_limited: billable, exclude, model, ..Default::default() }
    }
    let (token, cred) = match store::valid_access_token_for_device(
        &state.store,
        &state.http,
        select(device_id.as_deref(), billable, req_model.as_deref(), &[]),
    )
    .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::warn!(%method, path = %path_and_query, ua = %client_ua, error = %e, "refusing to forward");
            // 两类「等多久是算得出来的」限流 → 429 且带 `retry-after`，给出来客户端才知道该
            // 等多久，而不是立刻重试再撞一次：裸请求速率上限取窗口长度；所有号都在上游 429
            // 冷却中（硬门禁）取最早解冻的那个的剩余时间。
            let computable_retry =
                e.downcast_ref::<store::BareRateLimited>().map(|rl| rl.retry_after_secs).or_else(
                    || e.downcast_ref::<store::AllRateLimited>().map(|rl| rl.retry_after_secs),
                );
            if let Some(secs) = computable_retry {
                let retry = secs.to_string();
                return (
                    StatusCode::TOO_MANY_REQUESTS,
                    [(header::RETRY_AFTER, retry)],
                    e.to_string(),
                )
                    .into_response();
            }
            // 设备数达硬上限 → 429（等多久取决于别人什么时候释放，给不出 retry-after）；
            // 其余（无凭证/刷新失败等）→ 503。
            let status = if e.downcast_ref::<store::DeviceLimitReached>().is_some() {
                StatusCode::TOO_MANY_REQUESTS
            } else {
                StatusCode::SERVICE_UNAVAILABLE
            };
            return (status, e.to_string()).into_response();
        }
    };

    // 4) 目标 URL：上游 base + 原路径与查询串。
    let url = format!("{}{}", config::UPSTREAM_BASE_URL, path_and_query);

    // 5) 组装转发头：复制安全头，注入鉴权与 beta。形态类改动逐项受网页开关控制，
    //    一条 SQL 读齐（默认全开 = 加入开关前的既有行为）。
    let flags = state.store.forward_flags();
    // 设备指纹叠加客户端原始 device_id 与平台 arch/os，使不同设备得到不同伪装 device_id。
    // 头与体两侧都要用它（模拟模式的 session_id 也由它派生），故在装头之前先算好。
    let device_fp = device_fingerprint(device_id.as_deref(), &headers);
    // 6) 转发前改写 body：system 形态对齐（拆/并成官方的 4 块 + 基座标 scope=global）
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
    let upgrade_stream = billable
        && flags.nonstream_as_sse
        && body_json.as_ref().is_some_and(|v| !stream_requested(v));
    // 工具名混淆映射：从**客户端原始体**扫一次就够（后续改写不动工具名），请求侧与回程
    // 两侧共用同一份。见 [`ToolNameMap`]。
    let tool_names = (billable && flags.tool_name_mimic)
        .then(|| build_tool_name_map(body_json.as_ref()).map(std::sync::Arc::new))
        .flatten();
    if let Some(map) = &tool_names {
        tracing::debug!(count = map.forward.len(), "obfuscating tool names");
    }

    // 7) 发起上游请求并流式回传。头名的拼写与顺序由 orig_header_case 决定（关掉即退回
    //    「全小写 + Host/User-Agent/Content-Length 钉在队尾」，也就是换 wreq 之前的形态）。
    //    `body` 自此保持**客户端原始请求体**不变——改写后的那份直接交给 `send`，
    //    因为签名重试那条路要拿原始体重新走一遍改写，留着原件比留改写件更省事也更不易错。
    //    非计费路径（count_tokens 等）由 [`Upstream::shape`] 原样透传：那儿既没有 `metadata`
    //    可伪装，改写 `system` 形态反而会让计出来的 token 数偏离客户端实际要发的那份，还平白
    //    多担一份上游挑刺的风险。
    //
    //    **上游 429 换号重试**（`rate_limit_retry`）：某个号被限流时，客户端自己重试也只会
    //    继续撞同一个号——设备是粘性绑定的，而绑定只看凭证有没有被停用，不看它是不是刚被限流。
    //    于是这里在收到 429 时给该号打上冷却（时长取自上游的 `retry-after`/`*-reset`，见
    //    [`RateLimitInfo::cooldown`]），换一个**没试过的**号重发，并把设备**改绑**过去。
    //
    //    整套（选号 → 装头 → 改体）必须逐轮重来：`Authorization` 换了、`metadata` 里的伪装
    //    身份随号变、模拟路径的 session_id 也由账号派生——只换 token 会发出一条自相矛盾的请求。
    //    故首发也走这个循环，不存在「首发与重试形态不一致」的可能。
    let mut tried: Vec<i64> = Vec::new();
    let (mut token, mut cred) = (token, cred);
    let mut retried = 0usize;
    let max_retry = if flags.rate_limit_retry { state.store.rate_limit_retry_max() } else { 0 };
    let (upstream, resp, sent) = loop {
        let sim = Simulation::detect(body_json.as_ref(), flags, &cred, &device_fp);
        // CC 形态的来访不走模拟，但它若不带 metadata.user_id，那份身份仍然是缺的。
        let bare_session = bare_session_id(
            &headers,
            flags,
            sim.as_ref(),
            billable,
            has_user_id,
            &cred,
            &device_fp,
        );
        let out =
            build_forward_headers(&headers, &token, flags, sim.as_ref(), bare_session.as_deref());
        // 模拟路径的出站 URL 补 `?beta=true`（见 [`ensure_beta_query`]）。非计费路径不补：
        // `count_tokens` 官方带不带这个参数，抓包里没有样本，没有依据的形态就别猜着改。
        let target = if sim.is_some() && billable { ensure_beta_query(&url) } else { url.clone() };
        let upstream = Upstream {
            state: &state,
            method: method.clone(),
            url: target,
            headers: out,
            flags,
            billable,
            sim,
            bare_session,
            force_stream: upgrade_stream,
            tool_names: tool_names.clone(),
        };
        // 改写后的出站体单独留一份：上游把请求判成第三方应用时要把它原样摘要打出来
        // （见 [`log_third_party_rejection`]）。`Bytes` 是引用计数，clone 不拷贝字节。
        let sent = upstream.shape(&body, &cred, &device_fp);
        let resp = upstream.send(sent.clone()).await;

        // 只认「上游明确回 429」这一种：连不上/超时那类换个号一样连不上，重试只是浪费时间。
        let limited = match &resp {
            Ok(up) if up.status() == StatusCode::TOO_MANY_REQUESTS => {
                Some(RateLimitInfo::from_headers(up.headers()))
            }
            _ => None,
        };
        let Some(info) = limited else { break (upstream, resp, sent) };
        // 基础窗口真耗尽 → 停调度整个账号；只有超额池（7d_oi）满或纯容量限制 → 只冷却这个模型。
        let scope = rate_limit_scope(&info, req_model.as_deref());
        let cooldown = info.cooldown(scope.account_level());
        tracing::warn!(
            cred_id = cred.id, cred = %cred.label,
            model = %req_model.as_deref().unwrap_or("-"),
            scope = scope.label(),
            cooldown_secs = cooldown.as_secs(),
            ratelimit = %info.raw,
            "upstream 429"
        );

        // 冷却与重试同受一个开关：关掉即完全退回「原样透传 429」的既有行为。
        if max_retry == 0 {
            break (upstream, resp, sent);
        }
        park_rate_limited(&state.store, &cred, &scope, cooldown);
        tried.push(cred.id);
        if retried >= max_retry {
            tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                retried,
                "upstream 429, credential-swap retry cap reached, passing the response through"
            );
            break (upstream, resp, sent);
        }

        // 换一个没试过的号。选号顺带**改绑**这台设备（绑定的号不在候选里时会重选并改绑），
        // 于是这台设备之后的请求直接落在新号上，不必每条都先撞一次 429。
        match store::valid_access_token_for_device(
            &state.store,
            &state.http,
            select(device_id.as_deref(), billable, req_model.as_deref(), &tried),
        )
        .await
        {
            Ok((next_token, next_cred)) => {
                tracing::warn!(
                    cred_id = cred.id,
                    cred = %cred.label,
                    to_cred_id = next_cred.id,
                    to_cred = %next_cred.label,
                    cooldown_secs = cooldown.as_secs(),
                    attempt = retried + 1,
                    "upstream 429: credential put on cooldown, retrying with another one"
                );
                (token, cred) = (next_token, next_cred);
                retried += 1;
            }
            // 没有别的号可用（都试过/都停用了）：保留最初那条 429 原样透传，别把它变成 503。
            Err(e) => {
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    error = %e,
                    "upstream 429 but no credential to swap to, passing through as is"
                );
                break (upstream, resp, sent);
            }
        }
    };
    // 请求日志里记哪个设备：客户端自己带了就记它的，裸客户端记出站那份**伪装** device_id。
    // 不记的话这段流量在日志里只留下 `device=-`，既看不出是谁、也无从聚合。见 [`sim_device_id`]。
    // 取最终那一轮的凭证与模拟参数——换过号的话，实际发出去的就是那份。
    let logged_device = device_id.clone().or_else(|| {
        sim_device_id(
            upstream.sim.as_ref(),
            upstream.bare_session.as_deref(),
            flags,
            &cred,
            &device_fp,
        )
    });

    match resp {
        Ok(up) => {
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
            // 解析上游限流头（订阅账号 5h/7d 额度体现在此），随请求日志入库。
            let ratelimit = RateLimitInfo::from_headers(up.headers());

            // 包裹响应流：首块到达记 TTFT，边转发边嗅探用量；
            // 流结束(或断开)时在 Drop 里记 total、输出一条日志并落库。
            let mut rl = ReqLog {
                started,
                ttft_ms: None,
                method: method.to_string(),
                path: path_and_query,
                ua: client_ua,
                // 取最终那一轮的出站头——换过号的话，实际发出去的就是那份（同 logged_device）。
                ua_out: ua_of(&upstream.headers),
                cred_id: cred.id,
                cred_label: cred.label.clone(),
                device_id: logged_device,
                status: status.as_u16(),
                sse_aggregated: false,
                sniffer: UsageSniffer::new(is_stream, compressed),
                req_speed,
                req_model: req_model.clone(),
                ratelimit,
                stream_broke: None,
                store: state.store.clone(),
            };

            // 400/401/403：先缓冲响应体做账号级错误判定，命中则自动停用该凭证并清空其
            // 设备绑定，让下一次请求立即改选其它凭证；命中与否响应体都原样透传。
            if matches!(
                status,
                StatusCode::BAD_REQUEST | StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN
            ) {
                let builder = resp_builder(&up);
                return match up.bytes().await {
                    Ok(bytes) => {
                        rl.ttft_ms = Some(rl.started.elapsed().as_millis());
                        rl.sniffer.feed(&bytes);
                        // 无条件把上游的错误文本打出来。此前只有被判成账号级错误时才有日志，
                        // 普通 400（`invalid_request_error`，多半是请求形态被上游拒了）只会留下
                        // ReqLog 里那条 `status=400` 而不带任何原因——body 虽原样透传给了客户端，
                        // 但服务端侧查不出所以然。压缩体跳过：打出来只会是乱码字节。
                        if !compressed {
                            let (etype, message) = parse_upstream_error(&bytes);
                            tracing::warn!(
                                cred_id = cred.id, cred = %cred.label,
                                status = status.as_u16(),
                                error_type = %etype.as_deref().unwrap_or("-"),
                                message = %message.chars().take(500).collect::<String>(),
                                "upstream returned 4xx"
                            );
                        }
                        // 上游把这条请求判成了第三方应用（额度改扣超额池）。这类 400 光看
                        // 错误文本查不出所以然——问题出在**我们发出去的那份请求**长什么样，
                        // 故把出站头与出站体的结构摘要一并打出来，作为形态对齐的依据。
                        if !compressed && is_third_party_rejection(&bytes) {
                            log_third_party_rejection(&sent, &upstream.headers, &cred, status);
                        }
                        // 压缩体读不出内容，宁可漏判也不误判（乱码可能碰巧命中特征词）。
                        if let Some(reason) =
                            (!compressed).then(|| detect_account_ban(status, &bytes)).flatten()
                        {
                            tracing::warn!(
                                cred_id = cred.id, cred = %cred.label,
                                status = status.as_u16(),
                                reason = %reason,
                                "account-level error detected, auto-disabling the credential"
                            );
                            if let Err(e) = state.store.mark_banned(cred.id, &reason) {
                                tracing::warn!(error = %e, "failed to auto-disable the credential");
                            }
                        }
                        // 「thinking 块签名无效」：这条会话的历史是**别的账号**签发的（设备
                        // 绑定过期或凭证被停用后换了号），当前账号验不了，于是整段历史一并
                        // 作废——客户端只会看到一条它无法自行修复的 400。这里把历史 thinking
                        // 降级成 text 再用同一个账号重发一次；重试若仍失败就当无事发生，
                        // 原样透传最初那条响应，所以开着它最坏也只是多一次往返。
                        if status == StatusCode::BAD_REQUEST
                            && !compressed
                            && is_thinking_signature_error(&bytes)
                        {
                            if !flags.thinking_signature_retry {
                                tracing::warn!(
                                    cred_id = cred.id, cred = %cred.label,
                                    "upstream rejected a thinking-block signature (the history was most likely signed by another credential); demote-and-retry is off, passing through as is"
                                );
                            } else if let Some(up) =
                                retry_demoted_thinking(&upstream, &cred, &device_fp, &body, &mut rl)
                                    .await
                            {
                                return relay_upstream(up, rl, upgrade_stream, tool_names.clone())
                                    .await;
                            }
                        }
                        // 上游的错误文本可能回显假名（如「tool analyze_ski00 not found」），
                        // 整段已在内存里，顺手还原一次，成本可忽略。
                        let bytes = match &tool_names {
                            Some(map) => Bytes::from(map.restore(&bytes)),
                            None => bytes,
                        };
                        builder.body(Body::from(bytes)).unwrap_or_else(|e| {
                            (StatusCode::BAD_GATEWAY, e.to_string()).into_response()
                        })
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "failed to read the upstream error body");
                        builder.body(Body::empty()).unwrap_or_else(|e| {
                            (StatusCode::BAD_GATEWAY, e.to_string()).into_response()
                        })
                    }
                };
            }

            relay_upstream(up, rl, upgrade_stream, tool_names).await
        }
        Err(e) => {
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
            (StatusCode::BAD_GATEWAY, format!("upstream request failed [{kind}]: {detail}"))
                .into_response()
        }
    }
}

/// 一次转发要发往上游的全部固定入参（方法/URL/已装好的转发头/开关），只有请求体每次不同。
///
/// 存在的理由是**重试**：签名降级重试必须和首发除了 body 之外逐字节一致，否则「重试成功了」
/// 有可能只是因为顺手换了别的东西，排查时会被带偏。把这些一次装好、两次共用，就不存在
/// 「重建时漏了一项」的可能。
struct Upstream<'a> {
    state: &'a AppState,
    method: Method,
    url: String,
    /// [`build_forward_headers`] 的产物，逐次 clone 后发出。
    headers: HeaderMap,
    flags: store::ForwardFlags,
    /// 见 [`is_billable_messages`]。为假时出站体一律原样透传，见 [`Self::shape`]。
    billable: bool,
    /// 非 CC 客户端的模拟参数；`None` 即来访本来就是 CC 形态。见 [`Simulation`]。
    sim: Option<Simulation>,
    /// **CC 形态但不带 `metadata.user_id`** 的来访要补的那份身份用的 session_id
    /// （`sim` 为 `Some` 时恒为 `None`——那条路的会话 id 在 [`Simulation::session_id`] 里）。
    ///
    /// 这类来访多是第三方 CC 兼容客户端：系统提示词学了官方的，metadata 却不发。而官方
    /// **每条**请求都带那个字段，缺了就是一处白给的判据，所以照样补（[`ensure_cc_metadata`]）。
    /// 取值优先用来访自己带的 `X-Claude-Code-Session-Id`：官方头体两处逐字相同，另派生一个
    /// 只会让它们对不上，那比两处都缺更显眼；没带才派生，并由 [`build_forward_headers`]
    /// 把同一个值补进头里。
    bare_session: Option<String>,
    /// 来访是非流式、要改写成 `stream:true` 发出（回程再聚合成整段 JSON）。
    /// 见 [`store::ForwardFlags::nonstream_as_sse`]。
    force_stream: bool,
    /// 工具名混淆映射；`None` 即没有要混淆的工具（真 CC／全在白名单里／`tools` 为空），
    /// 此时请求与回程两侧都零开销。见 [`ToolNameMap`]。
    tool_names: Option<std::sync::Arc<ToolNameMap>>,
}

impl Upstream<'_> {
    /// 出站体改写的唯一入口：非计费路径（count_tokens 等）原样透传。
    ///
    /// 首发与「thinking 签名降级重试」两条路都必须走这里，否则同一条 count_tokens 会出现
    /// 首发透传、重试却被 shape 过的分裂形态。
    ///
    /// **模拟模式下 count_tokens 会低估**：出站头已经是官方那套，体却没补 system 前缀，
    /// 于是客户端数出来的 token 比它真发时少一个基座（opus 族约 300、sonnet 族约 2700）。
    /// 宁可低估也不在这条路径上改体：`count_tokens` 的请求体没有 `metadata`，改了既伪装不成
    /// 也只是多担一份上游挑刺的风险，而这条路径既不产生 usage 也不消耗额度。
    fn shape(&self, body: &Bytes, cred: &crate::credentials::Credential, device_fp: &str) -> Bytes {
        if self.billable {
            rewrite_body(
                body,
                cred,
                device_fp,
                self.flags,
                self.sim.as_ref(),
                self.bare_session.as_deref(),
                self.force_stream,
                self.tool_names.as_deref(),
            )
        } else {
            body.clone()
        }
    }

    /// 发一次。头名的拼写与顺序由 `orig_header_case` 决定（关掉即退回「全小写 +
    /// Host/User-Agent/Content-Length 钉在队尾」，也就是换 wreq 之前的形态）。
    async fn send(&self, body: Bytes) -> Result<wreq::Response, wreq::Error> {
        let req = self
            .state
            .http
            .request(self.method.clone(), &self.url)
            .headers(self.headers.clone())
            .body(body);
        let req =
            if self.flags.orig_header_case { req.orig_headers(orig_header_case()) } else { req };
        req.send().await
    }
}

/// 从上游响应里取出决定「响应体怎么读」的两项：是否 SSE 流、以及我们解不开的
/// `content-encoding`（正常恒为 `None`，见 [`handle`] 里的说明）。
fn resp_shape(up: &wreq::Response) -> (bool, Option<String>) {
    let is_stream = up
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|v| v.contains("text/event-stream"))
        .unwrap_or(false);
    let encoding = up
        .headers()
        .get(header::CONTENT_ENCODING)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty() && !v.eq_ignore_ascii_case("identity"))
        .map(str::to_string);
    (is_stream, encoding)
}

/// 拼出回给客户端的响应骨架：上游状态码 + 放行的上游响应头（见 [`is_resp_forwardable`]）。
fn resp_builder(up: &wreq::Response) -> axum::http::response::Builder {
    resp_builder_as(up, None)
}

/// 同 [`resp_builder`]，但把 `content-type` 换成 `ct`。
///
/// **必须在这一层换而不是事后 `.header()` 追加**：`Builder::header` 是**追加**语义，
/// 那样会得到两个 `content-type`（`text/event-stream` 在前），客户端按哪个都可能。
/// 聚合路径回的是整段 JSON，上游那份 SSE 的 `content-type` 必须原地替掉。
fn resp_builder_as(up: &wreq::Response, ct: Option<&str>) -> axum::http::response::Builder {
    let mut builder = Response::builder().status(up.status());
    for (k, v) in up.headers().iter() {
        if !is_resp_forwardable(k) {
            continue;
        }
        if ct.is_some() && k == header::CONTENT_TYPE {
            continue;
        }
        builder = builder.header(k, v);
    }
    match ct {
        Some(ct) => builder.header(header::CONTENT_TYPE, ct),
        None => builder,
    }
}

/// 回程总入口：按来访形态决定原样流式回传，还是把上游的 SSE 聚合成整段 JSON。
///
/// `upgrade_stream` 为真即「来访是非流式、我们替它改成了流式」（见
/// [`store::ForwardFlags::nonstream_as_sse`]）。此时**只有上游真回了 SSE 才聚合**——
/// 上游若因为别的原因回了整段 JSON（形态没被接受、或哪天默认变了），原样透传才是对的，
/// 拿聚合器去解一份非 SSE 的 body 只会得到一个空 Message。
async fn relay_upstream(
    up: wreq::Response,
    rl: ReqLog,
    upgrade_stream: bool,
    tool_names: Option<std::sync::Arc<ToolNameMap>>,
) -> Response {
    let (is_stream, _) = resp_shape(&up);
    if upgrade_stream && is_stream {
        aggregate_sse(up, rl, tool_names.as_deref()).await
    } else {
        stream_upstream(up, rl, tool_names)
    }
}

/// 把上游响应包成流式回传：首块到达记 TTFT，边转发边嗅探用量；
/// 流结束（或客户端断开）时 `rl` 在 Drop 里记 total、输出一条日志并落库。
fn stream_upstream(
    up: wreq::Response,
    mut rl: ReqLog,
    tool_names: Option<std::sync::Arc<ToolNameMap>>,
) -> Response {
    let builder = resp_builder(&up);
    let stream = up.bytes_stream().map(move |chunk| {
        if rl.ttft_ms.is_none() {
            rl.ttft_ms = Some(rl.started.elapsed().as_millis());
        }
        match &chunk {
            Ok(bytes) => rl.sniffer.feed(bytes),
            // 上游把流掐了。错误照旧原样交给 axum（客户端拿到的行为不变），但要留个痕
            // 给收尾时的日志——否则这条请求在服务端侧只剩一行 `forwarded status=200`。
            Err(e) => {
                rl.stream_broke = Some(format!("[{}] {e}", upstream_error_kind(e)));
            }
        }
        chunk
    });
    // 用量嗅探喂的是**还原前**的字节（`usage` 里没有工具名，两者等价），还原只包在最外层。
    let body = match tool_names {
        Some(map) => Body::from_stream(restore_tool_names_stream(stream, map)),
        None => Body::from_stream(stream),
    };
    builder.body(body).unwrap_or_else(|e| (StatusCode::BAD_GATEWAY, e.to_string()).into_response())
}

/// 收齐上游 SSE、聚合成一条整段 JSON 的 Message 回给客户端（来访本来发的就是非流式）。
///
/// 用量嗅探照旧按 SSE 逐行走——**比非流式那条路更准**：整段 JSON 模式有 1MB 的累积上限，
/// 超了就整条丢用量，逐行模式没有这个限制。TTFT 记的是上游首字节，客户端感知不到（它只会
/// 在末尾一次性收到整段），故日志里这两列会不一致，`sse_aggregated=true` 用来标出这类记录。
async fn aggregate_sse(
    up: wreq::Response,
    mut rl: ReqLog,
    tool_names: Option<&ToolNameMap>,
) -> Response {
    let builder = resp_builder_as(&up, Some("application/json"));
    rl.sse_aggregated = true;
    let mut agg = SseAggregator::default();
    let mut stream = up.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let bytes = match chunk {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(error = %e, "upstream SSE broke while aggregating; failing the request");
                rl.status = StatusCode::BAD_GATEWAY.as_u16();
                return (StatusCode::BAD_GATEWAY, format!("upstream stream failed: {e}"))
                    .into_response();
            }
        };
        if rl.ttft_ms.is_none() {
            rl.ttft_ms = Some(rl.started.elapsed().as_millis());
        }
        rl.sniffer.feed(&bytes);
        agg.feed(&bytes);
    }
    match agg.finish() {
        // 正常收尾：整段 Message，`content-type` 已换成 application/json。
        Aggregated::Message(msg) => match serde_json::to_vec(&msg) {
            // 聚合完再还原：整段都在内存里，不必操心分块边界。
            Ok(body) => builder
                .body(Body::from(match tool_names {
                    Some(map) => map.restore(&body),
                    None => body,
                }))
                .unwrap_or_else(|e| (StatusCode::BAD_GATEWAY, e.to_string()).into_response()),
            Err(e) => {
                rl.status = StatusCode::BAD_GATEWAY.as_u16();
                (
                    StatusCode::BAD_GATEWAY,
                    format!("failed to serialize the aggregated message: {e}"),
                )
                    .into_response()
            }
        },
        // 流中 `event: error`：错误 JSON 原样当响应体，**状态码按 `error.type` 映射**
        // （见 [`error_status`]）。这条错误是裹在 200 里来的，照搬 200 等于把一次失败记成
        // 成功——客户端要靠状态码分支，日志与统计也要靠它，所以翻译成非流式那边该有的那个。
        Aggregated::UpstreamError(payload) => {
            let status = error_status(&payload);
            tracing::warn!(
                status = status.as_u16(),
                error = %payload.get("error").map(|e| e.to_string()).unwrap_or_else(|| payload.to_string()),
                "upstream sent an error event mid-stream; mapping it to a status code"
            );
            rl.status = status.as_u16();
            // 同一份 error 事件也进了 sniffer（两者都在 feed 同一条流）。这条路已经就地
            // 告警并把状态码换给了客户端，留着它只会让 `ReqLog::drop` 再报一次同样的事。
            rl.sniffer.stream_error = None;
            match serde_json::to_vec(&payload) {
                Ok(body) => builder
                    .status(status)
                    .body(Body::from(body))
                    .unwrap_or_else(|e| (StatusCode::BAD_GATEWAY, e.to_string()).into_response()),
                Err(e) => {
                    rl.status = StatusCode::BAD_GATEWAY.as_u16();
                    (StatusCode::BAD_GATEWAY, e.to_string()).into_response()
                }
            }
        }
        // 没收到 `message_stop` 就断了 → 502。**不能把攒了一半的内容当完整响应回去**：
        // 客户端拿到的会是一条看着正常、实则被截断的 Message，比一个明确的错误糟得多。
        Aggregated::Incomplete(why) => {
            tracing::warn!(reason = why, "upstream SSE ended without message_stop; returning 502");
            rl.status = StatusCode::BAD_GATEWAY.as_u16();
            (StatusCode::BAD_GATEWAY, format!("incomplete upstream stream: {why}")).into_response()
        }
    }
}

/// 上游流中 `event: error` 的 `error.type` → HTTP 状态码。
///
/// 取值表照抄非流式那条路上同一个错误会用的状态码（见 Anthropic 的 errors 文档），
/// 这样开不开「非流式请求流式化」，客户端看到的状态码都一样。
///
/// **认不出来的类型一律 500**，不是 200：它确实是个错误，回 200 会让客户端与统计都把它
/// 当成功。500 是最不误导的兜底——客户端会当服务端故障重试，而不是把错误体当成模型输出。
fn error_status(payload: &serde_json::Value) -> StatusCode {
    let kind =
        payload.get("error").and_then(|e| e.get("type")).and_then(|t| t.as_str()).unwrap_or("");
    match kind {
        "invalid_request_error" => StatusCode::BAD_REQUEST,
        "authentication_error" => StatusCode::UNAUTHORIZED,
        // 计费问题上游同样回 403（额度/欠费与权限不足共用一个状态码）。
        "permission_error" | "billing_error" => StatusCode::FORBIDDEN,
        "not_found_error" => StatusCode::NOT_FOUND,
        "request_too_large" => StatusCode::PAYLOAD_TOO_LARGE,
        "timeout_error" => StatusCode::REQUEST_TIMEOUT,
        "rate_limit_error" => StatusCode::TOO_MANY_REQUESTS,
        // 529 不在 `StatusCode` 的常量表里，只能按数字构造（`http` 允许 100~999）。
        "overloaded_error" => {
            StatusCode::from_u16(529).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
        }
        "api_error" => StatusCode::INTERNAL_SERVER_ERROR,
        other => {
            tracing::warn!(
                error_type = other,
                "unrecognized upstream error type in a mid-stream error event; falling back to 500"
            );
            StatusCode::INTERNAL_SERVER_ERROR
        }
    }
}

/// [`SseAggregator::finish`] 的三种结局。
enum Aggregated {
    /// 收到了 `message_stop`，攒出一条完整 Message。
    Message(serde_json::Value),
    /// 流中来了 `event: error`，带上那份错误 JSON 原样回给客户端。
    UpstreamError(serde_json::Value),
    /// 流断在半路（没有 `message_start` 或没有 `message_stop`）。
    Incomplete(&'static str),
}

/// 把 `/v1/messages` 的 SSE 事件流攒回一条整段 Message，规则与官方各语言 SDK 一致：
///
/// | 事件 | 动作 |
/// |---|---|
/// | `message_start` | 取 `.message` 当骨架（`content` 清空重攒） |
/// | `content_block_start` | `content[index] = .content_block`（原样收下） |
/// | `content_block_delta` | 按 `delta.type` 追加到该块的对应字段 |
/// | `content_block_stop` | `input_json_delta` 攒的串在这里解析成 `.input` |
/// | `message_delta` | `.delta` 合进顶层、`.usage` 合进 `usage` |
/// | `message_stop` | 收尾 |
/// | `ping` / 未知事件 | 忽略 |
///
/// **未知的块类型自动透传**（`content_block_start` 整个收下，不挑字段），所以上游新增块类型
/// 时这里不用改。**未知的 `delta.type` 会丢内容**，故打一条 warn 而不是静默——那是唯一需要
/// 跟着上游演进的地方。
#[derive(Default)]
struct SseAggregator {
    /// 未处理完的行尾（`feed` 按行切，最后一段不完整的留着等下一块）。
    buf: Vec<u8>,
    /// `message_start` 给的骨架；没收到它就说明流从一开始就不对。
    msg: Option<serde_json::Value>,
    /// 各 `tool_use` 块正在累积的 `partial_json`，键是块下标。
    partial_json: std::collections::HashMap<usize, String>,
    /// 已经 warn 过的未知 `delta.type`，同一条流里只报一次。
    warned: Vec<String>,
    /// 收到过 `message_stop`。
    done: bool,
    /// 流中的 `event: error` 负载（整个 data 对象）。
    error: Option<serde_json::Value>,
}

impl SseAggregator {
    /// 喂入一块响应字节，按整行处理，不完整的行尾留在 `buf` 里。
    fn feed(&mut self, chunk: &[u8]) {
        self.buf.extend_from_slice(chunk);
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            self.parse_line(&line[..line.len() - 1]);
        }
        // 防御：异常超长行避免无界增长（与 [`UsageSniffer::feed`] 同口径）。
        if self.buf.len() > 1_000_000 {
            self.buf.clear();
        }
    }

    /// 解析一行。只认 `data:` 行——事件类型在 payload 自己的 `type` 字段里，
    /// `event:` 行没有额外信息，忽略即可。
    fn parse_line(&mut self, line: &[u8]) {
        let Ok(s) = std::str::from_utf8(line) else { return };
        let Some(json) = s.trim().strip_prefix("data:") else { return };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(json.trim()) else { return };
        self.apply(&v);
    }

    /// 处理一个已解析的事件。
    fn apply(&mut self, v: &serde_json::Value) {
        match v.get("type").and_then(|t| t.as_str()).unwrap_or_default() {
            "message_start" => {
                let Some(mut msg) = v.get("message").cloned() else { return };
                // 骨架里的 `content` 一律清空：官方那份是 `[]`，内容全靠后面的块事件攒。
                if let Some(obj) = msg.as_object_mut() {
                    obj.insert("content".into(), serde_json::Value::Array(Vec::new()));
                }
                self.msg = Some(msg);
            }
            "content_block_start" => {
                let (Some(idx), Some(block)) = (event_index(v), v.get("content_block").cloned())
                else {
                    return;
                };
                if let Some(slot) = self.block_mut(idx) {
                    *slot = block;
                }
            }
            "content_block_delta" => {
                let (Some(idx), Some(delta)) = (event_index(v), v.get("delta").cloned()) else {
                    return;
                };
                self.apply_delta(idx, &delta);
            }
            "content_block_stop" => {
                let Some(idx) = event_index(v) else { return };
                // 攒完的 `partial_json` 在这里落成 `input`。空串意味着这个块没有增量，
                // 保留 `content_block_start` 给的那份（官方那份是 `{}`）。
                let Some(raw) = self.partial_json.remove(&idx).filter(|s| !s.is_empty()) else {
                    return;
                };
                match serde_json::from_str::<serde_json::Value>(&raw) {
                    Ok(input) => {
                        if let Some(block) = self.block_mut(idx)
                            && let Some(obj) = block.as_object_mut()
                        {
                            obj.insert("input".into(), input);
                        }
                    }
                    Err(e) => tracing::warn!(
                        index = idx,
                        error = %e,
                        "failed to parse the accumulated tool_use input_json; keeping the block's original input"
                    ),
                }
            }
            "message_delta" => {
                let Some(msg) = self.msg.as_mut().and_then(|m| m.as_object_mut()) else { return };
                // `delta` 里是顶层字段（stop_reason/stop_sequence/…）：逐个合进去，
                // 不认识的字段照样合——那是上游新增的顶层信息，丢了才是错。
                if let Some(delta) = v.get("delta").and_then(|d| d.as_object()) {
                    for (k, val) in delta {
                        msg.insert(k.clone(), val.clone());
                    }
                }
                // `usage` 是**增量覆盖**：这里给的是最终 output_tokens 等，逐键盖上去，
                // message_start 那份里没被提到的键（cache_read 等）保留。
                if let Some(usage) = v.get("usage").and_then(|u| u.as_object()) {
                    let slot = msg
                        .entry("usage")
                        .or_insert_with(|| serde_json::Value::Object(Default::default()));
                    if let Some(obj) = slot.as_object_mut() {
                        for (k, val) in usage {
                            obj.insert(k.clone(), val.clone());
                        }
                    }
                }
            }
            "message_stop" => self.done = true,
            // 上游明确报错：整份 data 收下（形状与非流式的错误响应体一致：`{type, error}`）。
            "error" => self.error = Some(v.clone()),
            // `ping` 与将来新增的事件：没有内容要攒，忽略。
            _ => {}
        }
    }

    /// 按 `delta.type` 把增量追加到对应字段。
    fn apply_delta(&mut self, idx: usize, delta: &serde_json::Value) {
        let kind = delta.get("type").and_then(|t| t.as_str()).unwrap_or_default();
        // 文本类三种：同样是「取一个字符串字段追加到块的同名目标字段」。
        let text = |field: &str| delta.get(field).and_then(|t| t.as_str()).unwrap_or_default();
        match kind {
            "text_delta" => self.append_str(idx, "text", text("text")),
            "thinking_delta" => self.append_str(idx, "thinking", text("thinking")),
            "signature_delta" => self.append_str(idx, "signature", text("signature")),
            // tool_use 的入参是分片的 JSON 串，攒到 content_block_stop 再整体解析。
            "input_json_delta" => {
                self.partial_json.entry(idx).or_default().push_str(text("partial_json"));
            }
            "citations_delta" => {
                let Some(citation) = delta.get("citation").cloned() else { return };
                if let Some(block) = self.block_mut(idx)
                    && let Some(obj) = block.as_object_mut()
                {
                    match obj.get_mut("citations").and_then(|c| c.as_array_mut()) {
                        Some(list) => list.push(citation),
                        None => {
                            obj.insert(
                                "citations".into(),
                                serde_json::Value::Array(vec![citation]),
                            );
                        }
                    }
                }
            }
            // 认不出来的增量类型 = 这块内容会丢。绝不静默：它是本聚合器唯一需要跟着上游
            // 演进的地方，日志里没有信号的话，症状会是「响应少了一段」而查不出所以然。
            other => {
                if !self.warned.iter().any(|w| w == other) {
                    self.warned.push(other.to_string());
                    tracing::warn!(
                        delta_type = other,
                        "unknown SSE delta type while aggregating; its content is dropped from the aggregated response"
                    );
                }
            }
        }
    }

    /// 把 `s` 追加到第 `idx` 块的 `field` 字段（字段不存在就新建）。
    fn append_str(&mut self, idx: usize, field: &str, s: &str) {
        if s.is_empty() {
            return;
        }
        let Some(block) = self.block_mut(idx) else { return };
        let Some(obj) = block.as_object_mut() else { return };
        match obj.get_mut(field).and_then(|t| t.as_str()).map(|t| format!("{t}{s}")) {
            Some(joined) => {
                obj.insert(field.into(), serde_json::Value::String(joined));
            }
            None => {
                obj.insert(field.into(), serde_json::Value::String(s.to_string()));
            }
        }
    }

    /// 取第 `idx` 块的可变引用，必要时用 `null` 把数组补长——块事件理论上顺序到达，
    /// 但下标是上游给的，按它填才不会因为一次乱序把内容写错位置。
    fn block_mut(&mut self, idx: usize) -> Option<&mut serde_json::Value> {
        let content = self.msg.as_mut()?.as_object_mut()?.get_mut("content")?.as_array_mut()?;
        while content.len() <= idx {
            content.push(serde_json::Value::Null);
        }
        content.get_mut(idx)
    }

    /// 收尾判定，见 [`Aggregated`]。
    fn finish(self) -> Aggregated {
        if let Some(err) = self.error {
            return Aggregated::UpstreamError(err);
        }
        match self.msg {
            None => Aggregated::Incomplete("no message_start event"),
            Some(_) if !self.done => Aggregated::Incomplete("no message_stop event"),
            Some(msg) => Aggregated::Message(msg),
        }
    }
}

/// 取事件里的 `index`（块事件用它定位是第几块）。
fn event_index(v: &serde_json::Value) -> Option<usize> {
    v.get("index")?.as_u64().map(|i| i as usize)
}

/// 上游以「thinking 块签名无效」拒绝后的兜底：把历史 thinking 降级成 text，用**同一个凭证**
/// 重发一次。
///
/// 成功则返回重试那次的上游响应，并把 `rl` 改按它记账（交给调用方 [`stream_upstream`]）；
/// 任何一步不成都返回 `None`、`rl` 不动，由调用方继续透传最初那条 400——这条兜底路径在设计上
/// 不会让结果变差，最坏就是白花一次往返。
///
/// **代价是每轮一次**：客户端自己的会话记录里那些原始 thinking 块并不会因为这次重试而改写，
/// 于是这条会话的后续每一轮都会先撞一次 400 再降级重发，直到会话结束。会话能继续跑，但上游
/// 请求数翻倍。真正的解法是别让会话中途换号（见 `store::CredentialStore::select_for_device`
/// 的软绑定：名额到点就还，但设备回来仍优先回原号），这里只是兜底。
async fn retry_demoted_thinking(
    upstream: &Upstream<'_>,
    cred: &crate::credentials::Credential,
    device_fp: &str,
    client_body: &Bytes,
    rl: &mut ReqLog,
) -> Option<wreq::Response> {
    let Some(demoted) = demote_thinking_blocks(client_body) else {
        tracing::warn!(
            cred_id = cred.id,
            cred = %cred.label,
            "upstream rejected a thinking-block signature, but the body has no thinking block to demote, passing through as is"
        );
        return None;
    };
    tracing::warn!(
        cred_id = cred.id,
        cred = %cred.label,
        "upstream rejected a thinking-block signature (the history was most likely signed by another credential): demoted historical thinking to text, retrying once with the same credential"
    );

    let up = match upstream.send(upstream.shape(&demoted, cred, device_fp)).await {
        Ok(up) => up,
        Err(e) => {
            tracing::warn!(error = %error_chain(&e), "the retry after demoting thinking could not be sent, passing the original 400 through");
            return None;
        }
    };
    let status = up.status();
    if !status.is_success() {
        // 最常见的是末轮为 `tool_result` 的工具续跑：上游另外要求「最后一条 assistant
        // 消息必须以 thinking 块开头」，降级完照样被拒，只是换了条错误信息。
        tracing::warn!(
            cred_id = cred.id,
            cred = %cred.label,
            status = status.as_u16(),
            "the retry after demoting thinking was rejected too, passing the original 400 through"
        );
        return None;
    }

    // 重试成功：这条请求日志改按重试那次记账——状态码、用量、限流都以它为准，TTFT 重新
    // 计时。`started` 不动，故 total_ms 含两次往返，那正是客户端实际等到的时间。
    let (is_stream, encoding) = resp_shape(&up);
    rl.status = status.as_u16();
    rl.ttft_ms = None;
    rl.sniffer = UsageSniffer::new(is_stream, encoding.is_some());
    rl.ratelimit = RateLimitInfo::from_headers(up.headers());
    Some(up)
}

/// 上游那条 400 是不是「thinking 块签名验不过」，形如
/// `messages.1.content.0: Invalid \`signature\` in \`thinking\` block`。
///
/// 只按 message 文本判、不卡 `error.type`：这条错误上游归在 `invalid_request_error` 名下，
/// 跟一大堆真正的请求形态错误同类，靠类型分不出来；而 `signature` 与 `thinking` 同时出现在
/// 一句错误里只有这一种情况。
fn is_thinking_signature_error(body: &[u8]) -> bool {
    let (_, message) = parse_upstream_error(body);
    let hay = message.to_lowercase();
    hay.contains("signature") && hay.contains("thinking")
}

/// 把 assistant 轮里的 `thinking` 块降级成 `text` 块：推理原文原样搬进 text（外面裹一层
/// `<previous_thinking>`，让模型分得清那不是它当时说给用户的话），带不过去的签名丢掉。
/// `redacted_thinking` 只有一段密文 `data`、没有可搬的内容，直接删。
///
/// **为什么是降级而不是整块删**：删掉模型就丢了自己上一轮的推理链，续跑时容易从头再想一遍
/// 甚至改主意——用户看到的是「它突然忘了刚才在干嘛」。搬成 text 则历史完整，只是从「想过的」
/// 变成「说过的」。
///
/// 返回 `None` 表示没有可降级的块，那这条 400 另有原因，不值得再花一次往返。
///
/// **救不了工具续跑轮**：请求末尾是 `tool_result` 时，上游另外要求「最后一条 assistant 消息
/// 必须以 thinking 块开头」，降级完照样被拒。这种情况下重试白跑一次，随后原样透传最初那条
/// 400——不会更差，但也确实救不回来。
fn demote_thinking_blocks(body: &Bytes) -> Option<Bytes> {
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let msgs = v.get_mut("messages")?.as_array_mut()?;
    let mut changed = false;
    for msg in msgs.iter_mut() {
        let Some(obj) = msg.as_object_mut() else { continue };
        if obj.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        // `content` 是字符串形态的 assistant 轮压根没有 thinking 块，跳过即可。
        let Some(content) = obj.get_mut("content").and_then(|c| c.as_array_mut()) else { continue };
        let mut next = Vec::with_capacity(content.len());
        let mut touched = false;
        for blk in content.iter() {
            match blk.get("type").and_then(|t| t.as_str()) {
                Some("thinking") => {
                    touched = true;
                    let text = blk.get("thinking").and_then(|t| t.as_str()).unwrap_or_default();
                    if !text.trim().is_empty() {
                        next.push(previous_thinking_block(text));
                    }
                }
                Some("redacted_thinking") => touched = true,
                _ => next.push(blk.clone()),
            }
        }
        // 降级后空掉的 assistant 轮（整轮只有 thinking）是上游必拒的形态——`content` 不能是
        // 空数组。这种轮次原样留着：反正整条请求本来就要重试，少改一处也比发一个铁定被拒的
        // body 强。
        if !touched || next.is_empty() {
            continue;
        }
        *content = next;
        changed = true;
    }
    if !changed {
        return None;
    }
    serde_json::to_vec(&v).ok().map(Bytes::from)
}

/// 由一段历史推理原文构造替代它的 text 块，key 序与官方内容块一致：`type` → `text`。
fn previous_thinking_block(thinking: &str) -> serde_json::Value {
    let mut blk = serde_json::Map::new();
    blk.insert("type".into(), "text".into());
    blk.insert(
        "text".into(),
        format!("<previous_thinking>\n{thinking}\n</previous_thinking>").into(),
    );
    serde_json::Value::Object(blk)
}

/// 展开 error 的 source 链，拼成「顶层 -> 次层 -> …」，暴露底层真实原因。
fn error_chain(e: &dyn std::error::Error) -> String {
    let mut s = e.to_string();
    let mut src = e.source();
    while let Some(inner) = src {
        let msg = inner.to_string();
        // 避免与上层完全重复的冗余拼接。
        if !s.ends_with(&msg) {
            s.push_str(" -> ");
            s.push_str(&msg);
        }
        src = inner.source();
    }
    s
}

/// 粗分上游 HTTP 客户端的错误类别，便于一眼定位（超时 / 连接 / DNS-TLS 等）。
fn upstream_error_kind(e: &wreq::Error) -> &'static str {
    if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect"
    } else if e.is_request() {
        "request"
    } else if e.is_body() {
        "body"
    } else if e.is_decode() {
        "decode"
    } else {
        "other"
    }
}

/// 随响应流一起存活；流结束/断开时在 Drop 里输出一条转发日志（含 TTFT、总耗时与用量）并落库。
struct ReqLog {
    started: std::time::Instant,
    ttft_ms: Option<u128>,
    method: String,
    path: String,
    /// **来访**客户端自报的 `User-Agent`（已截断，见 [`ua_of`]）。
    ///
    /// 存在的理由：`path` 记的是来访原样的路径查询串（`?beta=true` 是官方 CC 自己带的，
    /// luban 只在出站 URL 上补，见 [`ensure_beta_query`]），于是「带 metadata.user_id 却
    /// 没有 `?beta=true`」这类第三方 CC 兼容客户端在日志里和官方客户端长得一样。UA 是
    /// 分辨它们最省事的一项。
    ua: String,
    /// **实际发给上游**的那份 `User-Agent`（见 [`build_forward_headers`]）。
    ///
    /// 与 `ua` 分开记而不是只留一份：模拟路径整套头换成官方的（[`official_headers`]），
    /// 出站恒为 [`config::CC_USER_AGENT`]；非模拟路径原样转发来访那份。于是两列一比就知道
    /// 这条走没走模拟——只存一份的话，要么看不见真实客户端是谁，要么看不见上游收到的是什么。
    ua_out: String,
    cred_id: i64,
    cred_label: String,
    /// 完整 device_id；日志里只展示前 8 位（脱敏）。
    device_id: Option<String>,
    status: u16,
    /// 这条来访本来是非流式、被改成流式发给上游再聚合回整段 JSON（见
    /// [`store::ForwardFlags::nonstream_as_sse`]）。
    ///
    /// 日志与 `usage_logs` 两处都记：它解释了同一条记录里 `ttft_ms` 与 `total_ms` 为什么会
    /// 差很多——TTFT 记的是上游首字节，而客户端是在末尾一次性收到整段的。没有这个标记的话，
    /// 这类记录在明细里看着就像一次「首字节极快、总耗时极长」的异常请求。
    sse_aggregated: bool,
    /// 增量嗅探到的响应用量。
    sniffer: UsageSniffer,
    /// 请求体里声明的速度档；仅在响应未回报 `usage.speed` 时兜底。
    req_speed: Option<String>,
    /// 请求体里声明的模型名；仅在响应没带 `usage`（4xx/5xx，尤其是 429）时兜底。
    /// 否则那些记录只留下 `model=-`，排查「哪个模型被拒得多」时等于没有信息。
    req_model: Option<String>,
    /// 上游返回的订阅账号限流快照。
    ratelimit: RateLimitInfo,
    /// 转发途中上游把流掐了（传输层错误，非 `event: error`）时的错误描述。
    ///
    /// 与 [`UsageSniffer::stream_error`] 分开记：那个是上游**说**自己出错了，这个是连接
    /// 本身断了，两者排查方向不同（前者看上游侧原因，后者看网络/超时）。此前
    /// [`stream_upstream`] 里这个分支是 `if let Ok` 的隐式丢弃——错误原样交给 axum，
    /// 客户端拿到一条截断的流，服务端侧一行日志都没有。
    stream_broke: Option<String>,
    store: std::sync::Arc<store::CredentialStore>,
}

impl Drop for ReqLog {
    fn drop(&mut self) {
        self.sniffer.finish();
        // 透传流路径的两类「200 里的失败」在此收口。响应头早发出去了，客户端拿到的
        // 状态码改不动（也不该改，行为保持原样），但**记账用的** status 必须反映真实结果：
        // 照搬 200 会让失败从成功率里凭空消失，正是 `aggregate_sse` 那条路早就避开的坑。
        if let Some(payload) = self.sniffer.stream_error.take() {
            let mapped = error_status(&payload);
            tracing::warn!(
                cred_id = self.cred_id, cred = %self.cred_label,
                sent_status = self.status,
                status = mapped.as_u16(),
                error = %payload.get("error").map(|e| e.to_string()).unwrap_or_else(|| payload.to_string()),
                "upstream sent an error event mid-stream; the client already got the 200 header plus that payload, logging it as the mapped status"
            );
            self.status = mapped.as_u16();
        }
        // 传输中断只告警、不改 status：这里分不清是上游掐的还是客户端自己走了（用户按了
        // Ctrl-C 也会让流提前结束），记成 5xx 会把正常的中途取消算成服务端故障。
        if let Some(why) = self.stream_broke.take() {
            tracing::warn!(
                cred_id = self.cred_id, cred = %self.cred_label,
                status = self.status,
                error = %why,
                "the upstream stream broke mid-transfer; the client got a truncated response"
            );
        } else if self.sniffer.is_stream
            && !self.sniffer.saw_message_stop
            && !self.sse_aggregated
            && self.status == StatusCode::OK.as_u16()
        {
            // 三种断流里最安静的一种：没报错、没断连，`message_stop` 就是没来。同样不改
            // status——上游 EOF 与客户端提前离开在这一层是同一个现象。定位断点靠
            // `last_event` 与 `events`：断在 `message_start` 是刚开口就没了，断在
            // `content_block_delta` 是生成到一半，断在 `message_delta` 则是只差收尾那一步。
            tracing::warn!(
                cred_id = self.cred_id, cred = %self.cred_label,
                status = self.status,
                last_event = %self.sniffer.last_event.as_deref().unwrap_or("-"),
                events = self.sniffer.events,
                output_tokens = self.sniffer.output_tokens.unwrap_or(0),
                "the stream ended without message_stop; either the upstream stopped sending or the client left early, and the reply the client got is truncated"
            );
        }
        let has_usage = self.sniffer.has_usage();
        // 速度档以上游回报为准（fast 被限流时会回落），响应没带才退回请求声明。
        let speed = self.sniffer.speed.clone().or_else(|| self.req_speed.clone());
        // 模型同理以响应为准（上游可能回落到别的模型），没有才用请求侧声明的那个。
        let model = self.sniffer.model.clone().or_else(|| self.req_model.clone());
        let cost_usd = crate::pricing::estimate_usd(crate::pricing::Usage {
            model: model.as_deref(),
            speed: speed.as_deref(),
            input_tokens: self.sniffer.input_tokens,
            output_tokens: self.sniffer.output_tokens,
            cache_creation_total: self.sniffer.cache_creation_tokens,
            cache_5m_tokens: self.sniffer.cache_creation_5m,
            cache_1h_tokens: self.sniffer.cache_creation_1h,
            cache_read_tokens: self.sniffer.cache_read_tokens,
        });
        let total_ms = self.started.elapsed().as_millis();
        // 伪装设备（见 [`sim_device_id`]）同样只展示前 8 位，但保留 `sim:` 前缀——
        // 截断时把前缀一起截掉，日志里就和真实 device_id 混在一起分不出来了。
        let device_short: String = self
            .device_id
            .as_ref()
            .map(|d| match d.strip_prefix("sim:") {
                Some(hex) => format!("sim:{}", hex.chars().take(8).collect::<String>()),
                None => d.chars().take(8).collect(),
            })
            .unwrap_or_else(|| "-".into());
        let ttft = self.ttft_ms.map(|v| v as i64);
        let total = i64::try_from(total_ms).ok();

        tracing::info!(
            method = %self.method,
            path = %self.path,
            ua = %self.ua,
            cred_id = self.cred_id, cred = %self.cred_label,
            device = %device_short,
            status = self.status,
            model = %model.as_deref().unwrap_or("-"),
            speed = %speed.as_deref().unwrap_or("-"),
            has_usage,
            input_tokens = self.sniffer.input_tokens.unwrap_or(0),
            output_tokens = self.sniffer.output_tokens.unwrap_or(0),
            cache_creation_tokens = self.sniffer.cache_creation_tokens.unwrap_or(0),
            cache_read_tokens = self.sniffer.cache_read_tokens.unwrap_or(0),
            ttft_ms = self.ttft_ms.map(|v| v as u64).unwrap_or(0),
            total_ms,
            sse_aggregated = self.sse_aggregated,
            cost_usd = cost_usd.map(|c| format!("{c:.5}")).unwrap_or_else(|| "-".into()),
            "forwarded"
        );

        let rec = store::UsageRecord {
            cred_id: Some(self.cred_id),
            cred_label: self.cred_label.clone(),
            device_id: self.device_id.clone(),
            model,
            path: self.path.clone(),
            // 日志里没带 UA 用 `-` 占位（对齐列宽），入库要还原成 NULL——`-` 会被当成
            // 一个真实存在的 UA，按 UA 分组时凭空多出一类。
            ua: (self.ua != "-").then(|| self.ua.clone()),
            ua_out: (self.ua_out != "-").then(|| self.ua_out.clone()),
            status: self.status,
            sse_aggregated: self.sse_aggregated,
            has_usage,
            input_tokens: self.sniffer.input_tokens,
            output_tokens: self.sniffer.output_tokens,
            cache_creation_tokens: self.sniffer.cache_creation_tokens,
            cache_5m_tokens: self.sniffer.cache_creation_5m,
            cache_1h_tokens: self.sniffer.cache_creation_1h,
            cache_read_tokens: self.sniffer.cache_read_tokens,
            ttft_ms: ttft,
            total_ms: total,
            unified_status: self.ratelimit.unified_status.clone(),
            rl_5h_status: self.ratelimit.five_h_status.clone(),
            rl_5h_reset: self.ratelimit.five_h_reset,
            rl_5h_utilization: self.ratelimit.five_h_utilization,
            rl_7d_status: self.ratelimit.seven_d_status.clone(),
            rl_7d_reset: self.ratelimit.seven_d_reset,
            rl_7d_utilization: self.ratelimit.seven_d_utilization,
            rl_representative: self.ratelimit.representative.clone(),
            rl_overage_in_use: self.ratelimit.overage_in_use,
            windows: self.ratelimit.windows(),
            ratelimit_raw: (!self.ratelimit.raw.is_empty()).then(|| self.ratelimit.raw.clone()),
            cost_usd,
        };
        spawn_usage_log(self.store.clone(), rec);
    }
}

/// 把一条用量日志交给阻塞线程池落库。
///
/// **为什么不能就地写**：调用方是 [`ReqLog::drop`]，而它是在响应流跑完（或客户端断开）时
/// 由 tokio 的工作线程执行的。`insert_usage_log` 是同步 SQLite 写，还要抢那把全局 `conn`
/// 锁——就地写等于在异步工作线程上做阻塞 IO，并发流一多就会把 worker 堵住，连带拖慢所有
/// 在途转发。日志裁剪那条路早就走 `spawn_blocking` 了（见 [`crate::web::run`]），这里同理。
///
/// 运行时退出时会等阻塞任务跑完（`#[tokio::main]` 结束时 drop runtime 即如此），故正常
/// 关停不会丢日志。拿不到运行时句柄的场合（单元测试里直接 drop 一个 `ReqLog`）退回就地写，
/// 那种场景本来就没有 worker 可堵。
fn spawn_usage_log(store: std::sync::Arc<store::CredentialStore>, rec: store::UsageRecord) {
    let write = move || {
        if let Err(e) = store.insert_usage_log(&rec) {
            tracing::warn!(error = %e, "failed to write the usage log");
        }
    };
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => {
            handle.spawn_blocking(write);
        }
        Err(_) => write(),
    }
}

/// 从上游响应中增量嗅探 token 用量。
///
/// - SSE 流：逐行解析 `data:` 事件——`message_start` 带 input/cache 与 model，
///   `message_delta` 带最终 output_tokens。后见到的非空值覆盖旧值。
/// - 非流式 JSON：累积整段响应体，在 [`Self::finish`] 时解析顶层 `usage`。
#[derive(Default)]
struct UsageSniffer {
    is_stream: bool,
    /// 响应体带我们解不开的 `content-encoding`，只能一律不解析（`feed` 直接丢弃）。
    /// 正常路径下恒为 false——wreq 已解码，见 [`handle`] 里 `compressed` 的说明。
    opaque: bool,
    /// SSE 模式下未处理完的行尾；非流式模式下累积的整段响应体。
    buf: Vec<u8>,
    model: Option<String>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_creation_tokens: Option<i64>,
    /// 缓存写细分：5 分钟 / 1 小时档（上游 `usage.cache_creation` 下）。
    cache_creation_5m: Option<i64>,
    cache_creation_1h: Option<i64>,
    cache_read_tokens: Option<i64>,
    /// 上游回报的实际速度档（`usage.speed`，如 `"fast"`）。fast 有独立限流，
    /// 被限流时会回落到标准档，故以响应为准、请求体只作兜底。
    speed: Option<String>,
    /// 流中途上游改口报错的那份 `event: error` 负载（整个 data 对象）。
    ///
    /// 这类错误是裹在 **200** 里来的：响应头早已发出，靠状态码看不出任何异常。此前
    /// [`Self::merge`] 只挑 usage/model，它从眼前流过去不留痕迹，于是一次失败在日志与
    /// `usage_logs` 里都是一条 `status=200`——客户端那头报错（如上游发的 `client_gone`），
    /// 服务端这头查无此事，且成功率统计里凭空少了一次失败。收尾时由 [`ReqLog::drop`]
    /// 取走告警。[`aggregate_sse`] 那条路另有 [`SseAggregator`] 就地处理，不走这里。
    stream_error: Option<serde_json::Value>,
    /// 见过 `message_stop` —— 流式响应正常收尾的唯一标志。
    ///
    /// 上游的流可能既不报错、也不断连，就是**发到一半 EOF**：`bytes_stream` 平静地返回
    /// `None`，[`Self::stream_error`] 和 [`ReqLog::stream_broke`] 双双为空，这一层看什么
    /// 都正常，而客户端拿到的是半截回复（Claude Code 报 `Connection closed mid-response`）。
    /// [`aggregate_sse`] 靠 [`Aggregated::Incomplete`] 拦住了这一类，透传路径此前没有对应
    /// 的检查——三种断流方式里最安静的那种，恰恰完全无声。
    saw_message_stop: bool,
    /// 最后见到的 SSE 事件类型，以及已解析的事件总数（含 `ping`）。
    ///
    /// 断流告警只报「没收到 `message_stop`」时，`output_tokens` 是唯一线索，而它**定位不了
    /// 断点**：官方流式文档的三个示例里，`message_start` 的 `message.usage` 就带
    /// `output_tokens`，取值 1/2/3，而同一条流 `message_delta` 的最终值是 15/89/510。
    /// 也就是说一个小数字既可能是「刚开口就断」，也可能是「生成完了只差收尾」，
    /// 而两者排查方向相反。事件类型才是判据，逐行解析本来就在做，顺手记下。
    ///
    /// `ping` 同样计入：文档说流中可能夹带任意多个 `ping`，`last_event=ping` 表示连接还活着
    /// 但上游没在产出内容，与 `last_event=message_start` 是两种不同的死法。
    last_event: Option<String>,
    events: u32,
}

impl UsageSniffer {
    fn new(is_stream: bool, opaque: bool) -> Self {
        Self { is_stream, opaque, ..Default::default() }
    }

    /// 喂入一块响应字节。
    fn feed(&mut self, chunk: &[u8]) {
        if self.opaque {
            return;
        }
        if self.is_stream {
            self.buf.extend_from_slice(chunk);
            // 逐个完整行处理，保留最后不完整的一段在 buf 里。
            while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.buf.drain(..=pos).collect();
                self.parse_line(&line[..line.len() - 1]);
            }
            // 防御：异常超长行避免无界增长。
            if self.buf.len() > 1_000_000 {
                self.buf.clear();
            }
        } else if self.buf.len() < 1_000_000 {
            // 非流式：累积整段响应体（JSON 消息响应通常很小）。
            self.buf.extend_from_slice(chunk);
        }
    }

    /// 解析一行 SSE 数据行（`data: {...}`）或裸 JSON 行。
    fn parse_line(&mut self, line: &[u8]) {
        let s = match std::str::from_utf8(line) {
            Ok(s) => s.trim(),
            Err(_) => return,
        };
        let json_str = s.strip_prefix("data:").map(str::trim).unwrap_or(s);
        if !json_str.starts_with('{') {
            return;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(json_str) {
            // 只在流式模式下认：非流式那条路的错误体由 `detect_account_ban` 一侧处理，
            // 这里再记一份会让同一个 4xx 告警两次。
            if self.is_stream
                && let Some(t) = v.get("type").and_then(|t| t.as_str())
            {
                self.last_event = Some(t.to_string());
                self.events += 1;
                match t {
                    "error" => self.stream_error = Some(v.clone()),
                    "message_stop" => self.saw_message_stop = true,
                    _ => {}
                }
            }
            self.merge(&v);
        }
    }

    /// 合并一段 JSON 里的用量字段（顶层或 `message.` 下）。
    fn merge(&mut self, v: &serde_json::Value) {
        if let Some(m) = v
            .get("model")
            .and_then(|m| m.as_str())
            .or_else(|| v.get("message").and_then(|m| m.get("model")).and_then(|m| m.as_str()))
        {
            self.model = Some(m.to_string());
        }
        let usage = v.get("usage").or_else(|| v.get("message").and_then(|m| m.get("usage")));
        if let Some(u) = usage {
            if let Some(x) = u.get("input_tokens").and_then(|x| x.as_i64()) {
                self.input_tokens = Some(x);
            }
            if let Some(x) = u.get("output_tokens").and_then(|x| x.as_i64()) {
                self.output_tokens = Some(x);
            }
            if let Some(x) = u.get("cache_creation_input_tokens").and_then(|x| x.as_i64()) {
                self.cache_creation_tokens = Some(x);
            }
            if let Some(x) = u.get("cache_read_input_tokens").and_then(|x| x.as_i64()) {
                self.cache_read_tokens = Some(x);
            }
            if let Some(s) = u.get("speed").and_then(|s| s.as_str()) {
                self.speed = Some(s.to_string());
            }
            // 缓存写细分（5m / 1h）：`usage.cache_creation.ephemeral_*_input_tokens`。
            if let Some(cc) = u.get("cache_creation") {
                if let Some(x) = cc.get("ephemeral_5m_input_tokens").and_then(|x| x.as_i64()) {
                    self.cache_creation_5m = Some(x);
                }
                if let Some(x) = cc.get("ephemeral_1h_input_tokens").and_then(|x| x.as_i64()) {
                    self.cache_creation_1h = Some(x);
                }
            }
        }
    }

    /// 收尾：非流式模式在此解析累积的整段 JSON。
    fn finish(&mut self) {
        if !self.is_stream
            && !self.buf.is_empty()
            && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&self.buf)
        {
            self.merge(&v);
        }
    }

    /// 是否解析到任一用量字段。
    fn has_usage(&self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cache_creation_tokens.is_some()
            || self.cache_read_tokens.is_some()
    }
}

/// 该路径是否会消耗订阅额度——设备身份校验、出站体改写、裸请求限流计数都只对它生效。
///
/// 排除 `count_tokens`：官方该端点的请求体压根没有 `metadata` 字段（只接
/// model/messages/system/tools/tool_choice/thinking），CC 自然也不会塞，于是
/// [`extract_device_id`] 在这条路径上恒为 `None`——开着设备校验时它 100% 被拒，
/// 客户端的 `/context` 显示与压缩前的 token 预估直接失效。而拦它并没有收益：
/// 不产生 usage、不消耗额度、不返回内容，既无身份可伪装，也本就不该占设备名额。
/// 放行后走 `select_for_device(None)`，即不写绑定、不占名额、按优先级档 + 档内负载
/// 均衡挑一个号——正是想要的语义（计 token 与选中哪个账号无关）。同理它也**不计入**裸请求
/// 速率上限：拿一条不产生 usage、不消耗额度的请求去占名额，只会把真正的请求挤掉。
///
/// **豁免必须精确匹配，且吃的是不含查询串的 `uri.path()`**：这个判定的两端不对称——
/// 判成计费只是多一道校验，判成不计费却是放掉设备校验，所以拿不准时必须倒向计费。
/// 若这里用前缀匹配，`/v1/messages/count_tokens/../` 这类路径就会被判成豁免，而出站 URL
/// 交给 wreq 时点段会按 RFC 3986 归一化掉，上游看到的其实是 `/v1/messages/`——等于给了
/// 一条绕开 `device_limit` 的路。精确匹配后这类路径一律落回计费侧，先过校验再说。
fn is_billable_messages(path: &str) -> bool {
    path.starts_with("/v1/messages") && path != "/v1/messages/count_tokens"
}

/// 从请求体提取「客户端设备标识」，用于粘性选择与设备指纹派生。
/// 兼容两种 `metadata.user_id` 格式：
/// - CC 内嵌 JSON（`{"device_id":...}`）：取 `device_id`。
/// - 扁平串 `user_<hash>_account_<acct>_session_<sess>`（如 Windows 客户端）：取 `<hash>`。
///
/// 解析失败或标识为空时返回 `None`（退化为纯优先级选择、不做粘性绑定）。
fn extract_device_id(body: Option<&serde_json::Value>) -> Option<String> {
    let user_id = body?.get("metadata")?.get("user_id")?.as_str()?;
    // CC 内嵌 JSON 优先。
    if let Ok(inner) = serde_json::from_str::<serde_json::Value>(user_id)
        && let Some(dev) = inner.get("device_id").and_then(|d| d.as_str())
        && !dev.is_empty()
    {
        return Some(dev.to_string());
    }
    // 退化：扁平串格式，取 device 段。
    let flat = parse_flat_user_id(user_id)?;
    (!flat.device.is_empty()).then_some(flat.device)
}

/// 来访体里有没有 `metadata.user_id`。
///
/// 与 [`extract_device_id`] 的区别：那个要求能**解析出设备标识**，格式认不出就是 `None`；
/// 这里只问「这个字段在不在」——决定的是要不要给它补一份官方身份（见 [`ensure_cc_metadata`]），
/// 而字段已经在的话，改写它是 [`spoof_identity`] 的活，两条路只能有一条动它。
fn body_has_user_id(body: Option<&serde_json::Value>) -> bool {
    body.and_then(|v| Some(v.get("metadata")?.get("user_id")?.is_string())).unwrap_or(false)
}

/// 扁平 `metadata.user_id` 中我们需要的两段：`user_<device>_account_<..>_session_<session>`。
/// account 段被凭证真实值覆盖，故不保留。
struct FlatUserId {
    device: String,
    session: String,
}

/// 解析扁平 user_id；不匹配该形态时返回 `None`。
/// 按标记切分，允许 account 段为空（`account__session`）。
fn parse_flat_user_id(s: &str) -> Option<FlatUserId> {
    let rest = s.strip_prefix("user_")?;
    let (device, rest) = rest.split_once("_account_")?;
    let (_account, session) = rest.split_once("_session_")?;
    Some(FlatUserId { device: device.to_string(), session: session.to_string() })
}

/// 账号被停用时的**状态词**：单独出现不作数，必须与 [`BAN_SUBJECTS`] 中的主语同时出现。
///
/// 这些词曾是裸子串匹配，代价是上游回显请求字段名时会误伤——`"thinking.type.disabled" is
/// not supported for this model` 是一条再普通不过的参数错误，却因字段名里含 `disabled`
/// 被判成封号；客户端只要重试，池子里的号会被逐个扣光。状态词离开主语没有信息量，
/// 「谁 disabled 了」才是判据，故改为共现。
const BAN_STATES: &[&str] =
    &["disabled", "suspended", "banned", "terminated", "deactivated", "violat"];

/// [`BAN_STATES`] 的合法主语：状态词说的是这几样东西时才算账号级错误。
const BAN_SUBJECTS: &[&str] = &["account", "organization", "workspace", "api key", "credential"];

/// 与主语无关、单独出现即判定的特征词：OAuth 刷新失败的报文里没有 account 主语。
const BAN_KEYWORDS: &[&str] = &["invalid_grant", "oauth"];

/// 反向豁免：命中其一则**一定不是**账号级问题，无论状态码与特征词如何都不停用。
/// 用于挡住「特征词碰巧出现在非账号报错里」的误杀，见 [`detect_account_ban`]。
/// 首项不写死 endpoint/model，是因为两者都出现过同款文案。
const NOT_ACCOUNT_PHRASES: &[&str] =
    &["not supported for this", "does not support", "unsupported model"];

/// 从上游错误响应体解析 `(error.type, error.message)`；解析失败时 message 退化为整段原文。
fn parse_upstream_error(body: &[u8]) -> (Option<String>, String) {
    let text = String::from_utf8_lossy(body);
    let v = serde_json::from_slice::<serde_json::Value>(body).ok();
    let field = |name: &str| {
        v.as_ref().and_then(|v| v.get("error")?.get(name)?.as_str().map(str::to_string))
    };
    (field("type"), field("message").unwrap_or_else(|| text.to_string()))
}

/// 依据状态码与响应体判定是否应自动停用该凭证，命中则返回写入 `ban_reason` 的原因
/// （`[状态码] 类型: 消息`，截断至 200 字符）。
///
/// 三档都要求响应体确实是 Anthropic 的错误 JSON（能取到 `error.type`）或命中特征词，
/// 避免把「非账号问题的 4xx」当成封号，把健康账号打成停用：
/// - 401：`authentication_error` 才停用。裸 401（CDN/网关拦截，无 `error.type`）不停用。
/// - 403：**仅**命中特征词时停用。普通 `permission_error`（如 Pro 账号请求
///   Opus、beta 未开通、区域限制）是能力/权限问题而非封号，原样透传即可。
/// - 400：同 403，仅命中特征词时停用；普通 `invalid_request_error` 是客户端请求错误。
///
/// 「命中特征词」= [`BAN_KEYWORDS`] 之一，或 [`BAN_SUBJECTS`] 与 [`BAN_STATES`] 各中一项。
fn detect_account_ban(status: StatusCode, body: &[u8]) -> Option<String> {
    let (etype, message) = parse_upstream_error(body);
    let reason = || {
        let head = match &etype {
            Some(t) => format!("[{}] {t}: {message}", status.as_u16()),
            None => format!("[{}] {message}", status.as_u16()),
        };
        head.chars().take(200).collect::<String>()
    };
    let hay = format!("{} {}", etype.as_deref().unwrap_or(""), message).to_lowercase();
    // 先排除「端点/能力不支持」这类与账号状态无关的报错——它们可能带上 oauth 等特征词
    // （如 401 `OAuth authentication is currently not supported for this endpoint`），
    // 但账号本身是好的，停用了反而白扣一个号。
    if NOT_ACCOUNT_PHRASES.iter().any(|p| hay.contains(p)) {
        return None;
    }
    let hits_keyword = || {
        BAN_KEYWORDS.iter().any(|k| hay.contains(k))
            || (BAN_SUBJECTS.iter().any(|s| hay.contains(s))
                && BAN_STATES.iter().any(|s| hay.contains(s)))
    };
    match status {
        StatusCode::UNAUTHORIZED => {
            (etype.as_deref() == Some("authentication_error") || hits_keyword()).then(reason)
        }
        StatusCode::FORBIDDEN | StatusCode::BAD_REQUEST => hits_keyword().then(reason),
        _ => None,
    }
}

/// 「被判成第三方应用」的特征文案。上游原文形如：
/// `Third-party apps now draw from your extra usage, not your plan limits.
/// Add more at claude.ai/settings/usage and keep going`。
///
/// 两条都不带主语，故只能裸子串匹配；但它们只用来决定**要不要多打一条日志**，
/// 误伤的代价仅是一条多余的 info，与 [`BAN_KEYWORDS`] 那种会停用凭证的判据不同。
const THIRD_PARTY_PHRASES: &[&str] = &["third-party app", "extra usage"];

/// 上游是否把这条请求判成了第三方应用（额度改扣超额池而非订阅额度）。
///
/// **注意它不会被 [`detect_account_ban`] 误判成封号**：这段文案里既没有 `oauth`/
/// `invalid_grant`，也凑不出「主语 + 状态词」的共现，故不会停用凭证——账号是好的，
/// 被拒的是请求形态。
fn is_third_party_rejection(body: &[u8]) -> bool {
    let (etype, message) = parse_upstream_error(body);
    let hay = format!("{} {}", etype.as_deref().unwrap_or(""), message).to_lowercase();
    THIRD_PARTY_PHRASES.iter().any(|p| hay.contains(p))
}

/// 摘要里每段文本最多保留的字符数。
const DUMP_TEXT_HEAD: usize = 200;

/// 截断到 `n` 个字符，并在尾部标出被吃掉多少，避免「看着是全文其实是截断」。
/// 按 `char` 截而不是按字节切：请求体里有中文，按字节切会 panic。
fn head(s: &str, n: usize) -> String {
    let total = s.chars().count();
    if total <= n {
        return s.to_string();
    }
    format!("{}…(+{})", s.chars().take(n).collect::<String>(), total - n)
}

/// 不能进日志的头。取值本身有鉴权效力，打出来等于把凭证写进日志文件。
/// 保留头名与位置（值换成 `<redacted>`），因为**头序**本身也是要看的东西。
fn is_secret_header(name: &str) -> bool {
    matches!(name, "authorization" | "x-api-key" | "cookie" | "proxy-authorization")
}

/// 上游把请求判成第三方应用时，把**我们实际发出去的那份请求**的形态打成一条 info。
///
/// 这类 400 的错误文本本身没有信息量（它只说「你是第三方」，不说凭什么），要查只能看
/// 出站报文长什么样：头的拼写与顺序、`system` 块数与断点、`tools` 里的名字与类型、
/// `metadata` 身份、顶层 key 顺序——判据在这些里面，不在错误文本里。
///
/// **打摘要而不是原文**，两个理由：
///   - 隐私：`messages` 是用户的对话内容，服务端日志不该留。故只记 role + 每块的类型，
///     `text` 只记长度；`tool_use` 记名字（工具名正是要查的那个维度）。
///   - 体积：`system` 在模拟路径下是 10KB 量级的官方基座，原文打出来会把日志刷没。
///     故每块只记长度 + 前 [`DUMP_TEXT_HEAD`] 字符 + `cache_control` 原样。
///
/// 其余顶层字段（`model`/`stream`/`metadata`/`tool_choice`/`context_management`…）**原样**
/// 打出——它们既不含用户内容，又都是形态判据。顶层 key 的**顺序**也照抄（本 crate 开了
/// serde_json 的 `preserve_order`），因为顺序本身就是一处判据。
fn log_third_party_rejection(
    sent: &Bytes,
    headers: &HeaderMap,
    cred: &crate::credentials::Credential,
    status: StatusCode,
) {
    let hdr = headers
        .iter()
        .map(|(k, v)| {
            let name = k.as_str();
            let value = if is_secret_header(name) {
                "<redacted>".to_string()
            } else {
                v.to_str()
                    .map(|s| head(s, DUMP_TEXT_HEAD))
                    .unwrap_or_else(|_| "<non-ascii>".to_string())
            };
            format!("{name}: {value}")
        })
        .collect::<Vec<_>>()
        .join(" | ");
    let body = match serde_json::from_slice::<serde_json::Value>(sent) {
        Ok(v) => request_digest(&v).to_string(),
        Err(_) => format!(
            "<unparsable {} bytes> {}",
            sent.len(),
            head(&String::from_utf8_lossy(sent), 512)
        ),
    };
    tracing::info!(
        cred_id = cred.id, cred = %cred.label,
        status = status.as_u16(),
        bytes = sent.len(),
        headers = %hdr,
        body = %body,
        "upstream rejected the request as a third-party app; dumping the outbound request shape"
    );
}

/// 出站请求体的结构摘要，见 [`log_third_party_rejection`] 的取舍说明。
/// 非对象（理论上到不了这儿）原样返回。
fn request_digest(v: &serde_json::Value) -> serde_json::Value {
    let Some(obj) = v.as_object() else { return v.clone() };
    let mut out = serde_json::Map::new();
    for (k, val) in obj {
        let digest = match k.as_str() {
            "messages" => messages_digest(val),
            "system" => system_digest(val),
            "tools" => tools_digest(val),
            _ => val.clone(),
        };
        out.insert(k.clone(), digest);
    }
    serde_json::Value::Object(out)
}

/// `messages` 的摘要：只留「第几条、谁说的、里面是些什么块」，不留任何正文。
/// `tool_use` 例外——它的 `name` 正是第三方判定最可能盯的维度，必须打出来。
fn messages_digest(v: &serde_json::Value) -> serde_json::Value {
    let Some(arr) = v.as_array() else { return v.clone() };
    let turns = arr
        .iter()
        .map(|m| {
            let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("?");
            let blocks = match m.get("content") {
                Some(serde_json::Value::Array(bs)) => {
                    bs.iter().map(block_label).collect::<Vec<_>>().join(",")
                }
                Some(serde_json::Value::String(s)) => format!("text(len={})", s.len()),
                _ => "?".to_string(),
            };
            format!("{role}:{blocks}")
        })
        .collect::<Vec<_>>();
    serde_json::json!({ "count": arr.len(), "turns": turns })
}

/// 单个内容块在摘要里的写法：`text` 只记长度，`tool_use` 记名字，其余只记类型。
fn block_label(b: &serde_json::Value) -> String {
    let t = b.get("type").and_then(|t| t.as_str()).unwrap_or("?");
    match t {
        "tool_use" => {
            format!("tool_use({})", b.get("name").and_then(|n| n.as_str()).unwrap_or("?"))
        }
        "text" => format!(
            "text(len={})",
            b.get("text").and_then(|t| t.as_str()).map(str::len).unwrap_or(0)
        ),
        other => other.to_string(),
    }
}

/// `system` 的摘要：每块记长度、前 [`DUMP_TEXT_HEAD`] 字符与 `cache_control` 原样。
/// 块数与断点位置是形态对齐的核心判据（见 [`align_system_shape`]），必须能一眼数出来。
fn system_digest(v: &serde_json::Value) -> serde_json::Value {
    let blocks: Vec<serde_json::Value> = match v {
        serde_json::Value::String(s) => {
            vec![serde_json::json!({ "len": s.len(), "head": head(s, DUMP_TEXT_HEAD) })]
        }
        serde_json::Value::Array(bs) => bs
            .iter()
            .map(|b| {
                let text = b.get("text").and_then(|t| t.as_str()).unwrap_or("");
                let mut o = serde_json::Map::new();
                o.insert("len".to_string(), serde_json::json!(text.len()));
                o.insert("head".to_string(), serde_json::json!(head(text, DUMP_TEXT_HEAD)));
                if let Some(cc) = b.get("cache_control") {
                    o.insert("cache_control".to_string(), cc.clone());
                }
                serde_json::Value::Object(o)
            })
            .collect(),
        other => return other.clone(),
    };
    serde_json::Value::Array(blocks)
}

/// `tools` 的摘要：`name`/`type` **原样**（第三方判定最可能盯的就是这两项），
/// `description` 截断，`input_schema` 只留顶层参数名。见过的键之外若还有别的，
/// 把键名列出来——摘要不该悄悄吃掉一个没见过的字段。
fn tools_digest(v: &serde_json::Value) -> serde_json::Value {
    const KNOWN: &[&str] = &["name", "type", "description", "input_schema"];
    let Some(arr) = v.as_array() else { return v.clone() };
    let out = arr
        .iter()
        .map(|t| {
            let mut o = serde_json::Map::new();
            for key in ["name", "type"] {
                if let Some(x) = t.get(key) {
                    o.insert(key.to_string(), x.clone());
                }
            }
            if let Some(d) = t.get("description").and_then(|d| d.as_str()) {
                o.insert("desc".to_string(), serde_json::json!(head(d, 80)));
            }
            if let Some(props) =
                t.get("input_schema").and_then(|s| s.get("properties")).and_then(|p| p.as_object())
            {
                o.insert(
                    "schema_props".to_string(),
                    serde_json::json!(props.keys().collect::<Vec<_>>()),
                );
            }
            let extra: Vec<&String> = t
                .as_object()
                .map(|m| m.keys().filter(|k| !KNOWN.contains(&k.as_str())).collect())
                .unwrap_or_default();
            if !extra.is_empty() {
                o.insert("extra_keys".to_string(), serde_json::json!(extra));
            }
            serde_json::Value::Object(o)
        })
        .collect::<Vec<_>>();
    serde_json::Value::Array(out)
}

/// 生效的接入 key：启动时 `--api-key`/env 覆盖优先，否则用库中网页配置的值。
fn effective_client_key(state: &AppState) -> Option<String> {
    if let Some(k) = &state.client_key {
        return Some(k.to_string());
    }
    state.store.get_setting(store::CLIENT_API_KEY).ok().flatten().filter(|s| !s.trim().is_empty())
}

/// 校验来访身份：`x-api-key: <key>` 或 `Authorization: Bearer <key>`。
fn client_authorized(headers: &HeaderMap, expected: &str) -> bool {
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok())
        && v == expected
    {
        return true;
    }
    if let Some(v) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok())
        && v.strip_prefix("Bearer ").map(str::trim) == Some(expected)
    {
        return true;
    }
    false
}

/// 合并来访的 `anthropic-beta`：**客户端自有的那串一字不动**，只把 API-key 模式的客户端不会
/// 自带的那四项补进去，各自插到官方位置上。这里是「注入哪几项」的唯一真源。
///
/// 只追加不落位会得到官方客户端不会产生的排列（缺失项全堆在末尾），集合对了顺序错，一次精确
/// 字符串匹配即可判定中间有代理。但**落位不能靠一张全局顺序表**——haiku 的客户端把
/// `claude-code-20250219` 排在队尾，opus/sonnet 排在队首，任何单一总序都同时满足不了
/// （见 [`config::cc_beta_order_is_not_a_table`]）。四对 raw 抓包里唯一稳定的是「客户端自有串
/// 的相对顺序在订阅模式下逐字不变」，故这里保留原串，按经验规则插入：
///
/// - [`config::OAUTH_BETA_HEADER`]：OAuth 鉴权必需。客户端串以
///   [`config::CC_BETA_CLAUDE_CODE`] 开头就插它后面，否则插最前（haiku 即后者）。
/// - [`config::CC_BETA_ADVANCED_TOOL_USE`]：有 [`config::CC_BETA_EFFORT`] 就插它前面，
///   没有就跟在客户端自有串之后（haiku）。
/// - [`config::CC_BETA_PROMPT_CACHING_SCOPE`]：四份抓包里客户端都自带，真缺时补在末尾。
/// - [`config::CC_BETA_EXTENDED_CACHE_TTL`]：官方恒为最后一项，故**最后**追加——这也是本
///   函数里插入顺序有讲究的唯一一处。
///
/// 三对抓包（opus-5 / sonnet-5 / haiku-4.5）用这套规则都能**逐字节**还原官方串；fable-5 那对
/// 两侧会话配置不同（`context-1m` / `server-side-fallback`），能验的是落位，也一致。
/// 回归测试见 [`tests::merged_beta_matches_official_order`]。
fn merge_beta(incoming: Option<&str>) -> String {
    let mut parts: Vec<String> = incoming
        .map(|s| s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();
    let has = |parts: &[String], beta: &str| parts.iter().any(|p| p == beta);

    if !has(&parts, config::OAUTH_BETA_HEADER) {
        let at = usize::from(parts.first().is_some_and(|p| p == config::CC_BETA_CLAUDE_CODE));
        parts.insert(at, config::OAUTH_BETA_HEADER.to_string());
    }
    if !has(&parts, config::CC_BETA_ADVANCED_TOOL_USE) {
        let at = parts.iter().position(|p| p == config::CC_BETA_EFFORT).unwrap_or(parts.len());
        parts.insert(at, config::CC_BETA_ADVANCED_TOOL_USE.to_string());
    }
    if !has(&parts, config::CC_BETA_PROMPT_CACHING_SCOPE) {
        parts.push(config::CC_BETA_PROMPT_CACHING_SCOPE.to_string());
    }
    if !has(&parts, config::CC_BETA_EXTENDED_CACHE_TTL) {
        parts.push(config::CC_BETA_EXTENDED_CACHE_TTL.to_string());
    }
    parts.join(",")
}

/// 组装发往上游的请求头：原样转发可转发头，再对需要 luban 决定取值的头**原位覆盖**。
///
/// **头序**：`HeaderMap` 按插入序迭代，hyper 也按这个顺序写到线上，所以来访客户端的头序
/// 默认是保住的。但「先在 [`is_forwardable`] 里剥离、之后再 `insert`」会把那些头从原位摘走、
/// 追加到队尾（`anthropic-beta`/`anthropic-version`/`authorization` 都是），得到官方客户端
/// 不会产生的排列——和 [`merge_beta`] 要解决的问题同类，只是从「值内顺序」变成「头之间顺序」。
/// 故这里让它们照常转发，再用 `insert` 覆盖：`insert` 命中已有 key 时原位替换值，位置不动。
///
/// 只在客户端没带时才补的头（`accept-encoding`、`x-client-request-id`）没有原位可循，
/// 追加在末尾；官方客户端这两个头都带，走的是原位覆盖那条路。
///
/// **开关**（[`store::ForwardFlags`]，默认全开）：`merge_beta` 关掉即原样转发客户端那串
/// `anthropic-beta`（含不再塞 `oauth-2025-04-20`）；`fill_client_headers` 关掉即不补任何
/// 客户端没带的头。唯一无条件执行的是注入 `Authorization`——实测那是上游唯一必需的改动。
///
/// 注意 `fill_client_headers` 关掉后，若客户端自己也没带 `accept-encoding`，兜底会落到
/// [`crate::web::upstream_client`] 的 `default_headers`（同为官方取值），不会退化成
/// tower-http 那个非官方的 `zstd,gzip,deflate,br`。
///
/// 无法对齐的部分（头名大小写、hyper 自己追加的 `user-agent`/`host`/`content-length`）
/// 见 [`crate::config::known_fingerprint_gaps`]。
fn build_forward_headers(
    headers: &HeaderMap,
    token: &str,
    flags: store::ForwardFlags,
    sim: Option<&Simulation>,
    bare_session: Option<&str>,
) -> HeaderMap {
    let mut out = match sim {
        // 模拟模式：来访那套头一个不留，整体换成官方的（见 [`official_headers`]）。
        Some(sim) => official_headers(sim),
        None => {
            let mut out = HeaderMap::new();
            // `append` 而非 `insert`：同名多值头要全部保留，`insert` 会只剩最后一个。
            for (k, v) in headers.iter() {
                if is_forwardable(k, v) {
                    out.append(k.clone(), v.clone());
                }
            }
            // CC 形态的来访要补 metadata 时（见 [`Upstream::bare_session`]），头上的会话 id
            // 必须与体里那个同值——官方两处逐字相同。`bare_session` 已经优先取的就是来访
            // 自己那个头的值，故这里只在它压根没带时才补，不覆盖客户端原值。
            if let Some(sid) = bare_session
                && !out.contains_key("x-claude-code-session-id")
                && let Ok(v) = HeaderValue::from_str(sid)
            {
                out.insert("x-claude-code-session-id", v);
            }
            out
        }
    };
    // anthropic-version 缺省补齐。
    if flags.fill_client_headers && !out.contains_key("anthropic-version") {
        out.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    }
    // anthropic-beta 合并，确保带上 oauth，并按官方客户端顺序重排。
    // 模拟模式下先把来访那串与官方自有串取并集，再交给同一套落位规则。
    let incoming = headers.get("anthropic-beta").and_then(|v| v.to_str().ok());
    let incoming = match sim {
        Some(sim) => Some(simulated_beta(sim.beta, incoming)),
        None => incoming.map(str::to_string),
    };
    if flags.merge_beta {
        match HeaderValue::from_str(&merge_beta(incoming.as_deref())) {
            Ok(v) => {
                out.insert("anthropic-beta", v);
            }
            // merge_beta 只产出 ASCII，理论上不可达；真发生时保留来访原值，别把这个头发空。
            Err(e) => {
                tracing::warn!(error = %e, "building anthropic-beta failed, keeping the inbound value")
            }
        }
    }
    if flags.fill_client_headers {
        // accept-encoding：客户端没带时补上官方客户端的取值（缺失本身就是特征）。
        if !out.contains_key(header::ACCEPT_ENCODING) {
            out.insert(
                header::ACCEPT_ENCODING,
                HeaderValue::from_static(config::CC_ACCEPT_ENCODING),
            );
        }
        // x-client-request-id：官方客户端每请求一个 uuid v4；API-key 模式的 CC 不发，补齐。
        if !out.contains_key("x-client-request-id")
            && let Ok(v) = HeaderValue::from_str(&uuid_v4())
        {
            out.insert("x-client-request-id", v);
        }
    }
    // 注入 OAuth 鉴权，原位覆盖来访的任何鉴权头。
    match HeaderValue::from_str(&format!("Bearer {token}")) {
        Ok(v) => {
            out.insert(header::AUTHORIZATION, v);
        }
        // 这个头现在是**照常转发再覆盖**的，覆盖失败就必须摘掉：
        // 留在原地等于把来访者的接入 key 漏给上游。
        Err(e) => {
            tracing::error!(error = %e, "building Authorization failed, dropping the header so the inbound key cannot leak");
            out.remove(header::AUTHORIZATION);
        }
    }
    out
}

/// 模拟模式下整套重建的转发头：[`config::CC_SIM_HEADERS`] 那张固定表 + 两个随请求变的值
/// （会话 id、请求 id）。`Authorization` 与 `anthropic-beta` 由 [`build_forward_headers`]
/// 随后覆盖上去。
///
/// **来访客户端自己的头一个都不带过去**：一个 UA 是 `python-httpx/0.27`、没有 `x-app`、
/// 却发着 CC 系统提示词和 OAuth token 的请求，本身就是个比缺任何单项都强的判据；留着任何
/// 一个非官方头都等于白伪装。唯一的例外是 `anthropic-beta`——客户端可能真的需要某个 beta，
/// 那串在 [`simulated_beta`] 里与官方自有串取并集，不丢。
///
/// 插入序即 [`config::CC_SIM_HEADERS`] 的表序（官方线序），末尾两个动态头除外——线上的
/// 拼写与顺序另由 `orig_header_case` 按 [`config::CC_HEADER_ORDER`] 归位，那张表里
/// `X-Claude-Code-Session-Id` 与 `x-client-request-id` 都在各自的官方位置上。
fn official_headers(sim: &Simulation) -> HeaderMap {
    let mut out = HeaderMap::new();
    for (name, value) in config::CC_SIM_HEADERS {
        match (HeaderName::from_bytes(name.as_bytes()), HeaderValue::from_str(value)) {
            (Ok(n), Ok(v)) => {
                out.insert(n, v);
            }
            // 常量表，理论上不可达；真写错了也只是少一个头，不该因此拒掉整条请求。
            _ => tracing::error!(
                header = name,
                "building a simulated header failed (bad constant table), skipping it"
            ),
        }
    }
    // 与 `metadata.user_id` 里的 session_id 同值——官方两处逐字相同。
    if let Ok(v) = HeaderValue::from_str(&sim.session_id) {
        out.insert("x-claude-code-session-id", v);
    }
    if let Ok(v) = HeaderValue::from_str(&uuid_v4()) {
        out.insert("x-client-request-id", v);
    }
    out
}

/// 模拟模式的 `anthropic-beta` 自有串：官方那串（`seed`，按模型族取自 [`cc_beta_seed`]）
/// 打底，来访客户端自己带的项去重后**追加在后面**，再交给 [`merge_beta`] 落位。
///
/// 追加而非插空：客户端带的多半是官方不发的项（`output-128k` 之类），本来就没有「官方位置」
/// 可言，硬塞进官方串中间反而造出一个官方不产生的排列。丢掉它们更不行——那是客户端明确要的
/// 能力，丢了它的请求就直接变了语义。
fn simulated_beta(seed: &str, incoming: Option<&str>) -> String {
    let mut parts: Vec<&str> = seed.split(',').map(str::trim).collect();
    for p in incoming.unwrap_or("").split(',').map(str::trim).filter(|p| !p.is_empty()) {
        if !parts.contains(&p) {
            parts.push(p);
        }
    }
    parts.join(",")
}

/// 按官方拼写与顺序构造 `OrigHeaderMap`（`wreq` 据它决定线上头名的大小写**与顺序**）。
///
/// 整张 [`config::CC_HEADER_ORDER`] 无条件塞进去，不按本次请求裁剪——预检实测：表里有、
/// 本次没带的头**不会**凭空发出。反之表外的头照发，但一律小写且排在所有表内头之后，
/// 所以自定义头不会因缺表项而丢失，只是拿不到官方位置。
///
/// 这也是 `Host`/`User-Agent`/`Content-Length` 唯一的归位途径：它们由 HTTP 客户端自己追加，
/// 不在我们的 `HeaderMap` 里，但只要列进这张表就会落到官方位置，而不是被钉在队尾。
pub(crate) fn orig_header_case() -> wreq::header::OrigHeaderMap {
    let mut orig = wreq::header::OrigHeaderMap::new();
    for name in config::CC_HEADER_ORDER {
        // 返回值是 `HeaderMap::append` 的语义（false = 新键），不是成功与否，别拿来判错。
        orig.insert(*name);
    }
    orig
}

/// 生成一个随机 uuid v4（小写带连字符），用于补齐 `x-client-request-id`。
fn uuid_v4() -> String {
    uuid_from_bytes(rand::rng().random())
}

/// 把 16 字节按 uuid v4 的形态格式化（打上 version/variant 位，小写带连字符）。
/// 随机来源见 [`uuid_v4`]，派生来源见 [`Simulation::session_id`]。
fn uuid_from_bytes(mut b: [u8; 16]) -> String {
    b[6] = (b[6] & 0x0f) | 0x40; // version 4
    b[8] = (b[8] & 0x3f) | 0x80; // variant 10
    let h = |r: &[u8]| r.iter().map(|x| format!("{x:02x}")).collect::<String>();
    format!("{}-{}-{}-{}-{}", h(&b[0..4]), h(&b[4..6]), h(&b[6..8]), h(&b[8..10]), h(&b[10..16]))
}

/// 一条**非 Claude Code 请求**要装成官方客户端时的全部派生量。
///
/// `Some` 即本条请求走模拟路径：转发头整套换成官方那套（[`official_headers`]）、`system`
/// 补上官方前缀（[`simulate_system`]）、`metadata` 补上身份（[`ensure_cc_metadata`]）。
/// `None` 即来访本来就是 CC 形态（或开关关着），照既有路径走，一个字节都不多改。
///
/// **存在的理由**：订阅(OAuth)凭证在上游是「只授权给 Claude Code 用」的，`system` 里缺那句
/// [`config::CC_SYSTEM_IDENTITY`] 就用不了额度。于是任何非 CC 客户端（各种 SDK、第三方
/// 前端、curl）经 luban 都是死路一条。补齐它等于把这些客户端接进订阅额度，而既然要补，
/// 就得**整条链路一起补**：只补那句身份声明、头却还是 `python-httpx`，反倒是个真实客户端
/// 绝不会产生的组合。
///
/// **代价**：每条请求多一个基座前缀（opus 族 1214 字节、sonnet 族 10682 字节，约 300 /
/// 2700 token）。它带 `ttl:1h` + `scope:global` 断点，全网同一份，稳定后基本走缓存读价；
/// 但**部署后每个模型族的第一条**要按写入价付一次，而且会**改变模型行为**——客户端拿到的
/// 是一个被告知「你是 Claude Code」的模型，输出风格与工具偏好都会随之偏移。不想要就把
/// [`store::SIMULATE_CC`] 关掉，代价是这类请求退回「上游直接拒」。
struct Simulation {
    /// 按模型族选出的官方基座提示词；模型认不出来时 `None`——基座是逐字节从抓包取的，
    /// 猜错一族（把 sonnet 的 10682 字节发给 opus）比不发更糟。见 [`cc_system_base`]。
    base: Option<&'static str>,
    /// 按模型族选出的 `anthropic-beta` 自有串（haiku 与另外三族不同，见 [`cc_beta_seed`]）。
    beta: &'static str,
    /// `X-Claude-Code-Session-Id` 与 `metadata.user_id` 里 `session_id` 的**同一个**取值：
    /// 官方两处逐字相同，只对上一处等于自己造一个新判据。
    ///
    /// 由「账号 + 设备指纹」派生而非每请求随机：真实客户端一个会话内多次请求共用一个
    /// session_id，每请求一个新的等于宣告「每条请求都是新开的会话」。代价是同一设备的
    /// session_id 永不变（真实客户端会随会话轮换），这条记在这儿。
    session_id: String,
}

impl Simulation {
    /// 判定 + 派生一次做完。返回 `None` 的三种情形：开关关着、请求体不是我们能改的 JSON、
    /// 来访已经是 CC 形态（[`is_cc_shaped`]）。
    ///
    /// **依赖 `merge_beta`**：模拟出来的 `anthropic-beta` 要靠它落位并补上 `oauth`，关掉它
    /// 就是「system 装成了 CC、头上却没有 oauth beta」的自相矛盾（且上游直接拒）。同
    /// [`rewrite_body`] 里 `system_shape` 依赖 `merge_beta` 是一个道理。
    fn detect(
        body: Option<&serde_json::Value>,
        flags: store::ForwardFlags,
        cred: &crate::credentials::Credential,
        device_fp: &str,
    ) -> Option<Self> {
        if !flags.simulate_cc || !flags.merge_beta {
            return None;
        }
        let v = body?;
        if is_cc_shaped(v) {
            return None;
        }
        let model = v.get("model").and_then(|m| m.as_str()).unwrap_or_default();
        let base = cc_system_base(model);
        let beta = cc_beta_seed(model);
        tracing::debug!(
            model,
            base_bytes = base.map(str::len).unwrap_or(0),
            "non-CC request, simulating the official shape"
        );
        Some(Self { base, beta, session_id: session_id_for(cred, device_fp) })
    }
}

/// 来访是否已经是 Claude Code 形态——判据是 `system` 里有没有那句
/// [`config::CC_SYSTEM_IDENTITY`]，因为那正是上游认的东西。
///
/// 用 `contains` 而不是 `starts_with`：谁把那句话塞在自己提示词中间，那也是在自称 CC，
/// 再给他前面插一份官方前缀只会得到两句身份声明。`system` 是字符串形态的一并认。
fn is_cc_shaped(v: &serde_json::Value) -> bool {
    match v.get("system") {
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .any(|t| t.contains(config::CC_SYSTEM_IDENTITY)),
        Some(serde_json::Value::String(s)) => s.contains(config::CC_SYSTEM_IDENTITY),
        _ => false,
    }
}

/// 按模型族选官方基座：opus-5 / fable-5 共用一份，sonnet-5 / haiku-4.5 共用另一份
/// （`cap/raw` 四份直连抓包，两两 sha256 相同）。认不出的模型返回 `None`，只注入身份句。
///
/// 匹配的是**族名**而非具体版本：CC 的基座随模型族走，`claude-opus-4-1` 这种老版本
/// 拿到的也正是 CC 今天会发给它的那份。
fn cc_system_base(model: &str) -> Option<&'static str> {
    let m = model.to_ascii_lowercase();
    if m.contains("opus") || m.contains("fable") {
        Some(config::CC_SYSTEM_BASE_OPUS)
    } else if m.contains("sonnet") || m.contains("haiku") {
        Some(config::CC_SYSTEM_BASE_SONNET)
    } else {
        None
    }
}

/// 按模型族选 `anthropic-beta` 的客户端自有串：**haiku 一份，其余（含认不出的模型）一份**。
///
/// 分家的理由不是「haiku 少两项」，而是它把 `claude-code-20250219` 排在队尾——拿另外三族
/// 那串去发 haiku，得到的是真实客户端不产生的排列。详见 [`config::CC_BETA_SIMULATED_HAIKU`]。
///
/// **和 [`cc_system_base`] 的分族方式不一样是对的**：基座上 haiku 与 sonnet 相同，beta 上
/// haiku 自成一族。两处各按各的证据分，别为了「看起来整齐」并成一个函数。
fn cc_beta_seed(model: &str) -> &'static str {
    if model.to_ascii_lowercase().contains("haiku") {
        config::CC_BETA_SIMULATED_HAIKU
    } else {
        config::CC_BETA_SIMULATED
    }
}

/// 来访自己带的 `X-Claude-Code-Session-Id`（非空才算）。补 metadata 时优先用它，
/// 见 [`Upstream::bare_session`]。
fn incoming_session_id(headers: &HeaderMap) -> Option<String> {
    let v = headers.get("x-claude-code-session-id")?.to_str().ok()?.trim();
    (!v.is_empty()).then(|| v.to_string())
}

/// CC 形态来访要补 `metadata.user_id` 时用的 session_id；不需要补时为 `None`。
/// 语义与各项前提见 [`Upstream::bare_session`]。
///
/// 六个前提缺一不可：
/// - `sim.is_none()`：模拟那条路自己带 session_id，不走这里；
/// - `flags.fill_metadata`：本功能自己的开关（网页可关）；
/// - `flags.spoof_identity`：身份伪装总开关——补出来的那份身份正是它管的东西，
///   它关着还补，等于绕过总开关；
/// - `billable`：非计费路径（count_tokens）出站体一律原样透传，补了也发不出去；
/// - `!has_user_id`：字段已经在就交给 [`spoof_identity`] 原格式改写，两条路只能有一条动它；
/// - `spoof_device_id` 有值：这是 [`ensure_cc_metadata`] 造身份的前提（无 `account_uuid`
///   就造不出自洽身份）。不满足时连头也不补——否则会补出一个「头上有会话 id、体里没
///   metadata」的新破绽，比两处都缺更显眼。
fn bare_session_id(
    headers: &HeaderMap,
    flags: store::ForwardFlags,
    sim: Option<&Simulation>,
    billable: bool,
    has_user_id: bool,
    cred: &crate::credentials::Credential,
    device_fp: &str,
) -> Option<String> {
    if sim.is_some()
        || !flags.fill_metadata
        || !flags.spoof_identity
        || !billable
        || has_user_id
        || cred.spoof_device_id(device_fp).is_none()
    {
        return None;
    }
    Some(incoming_session_id(headers).unwrap_or_else(|| session_id_for(cred, device_fp)))
}

/// 模拟用的 session_id：`sha256("luban-session" ‖ account_uuid ‖ 设备指纹)` 取前 16 字节，
/// 按 uuid v4 形态格式化。同一设备同一账号恒定，换账号或换设备即不同。
///
/// 前缀是为了和 [`crate::credentials::Credential::spoof_device_id`] 分开取值——同样的输入
/// 派生出两个字段，不加区分前缀就会得到「device_id 与 session_id 的高位相同」这种真实
/// 客户端不产生的相关性。
fn session_id_for(cred: &crate::credentials::Credential, device_fp: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"luban-session\0");
    h.update(cred.account_uuid.as_deref().unwrap_or("").as_bytes());
    h.update([0u8]);
    h.update(device_fp.as_bytes());
    let digest = h.finalize();
    let mut b = [0u8; 16];
    b.copy_from_slice(&digest[..16]);
    uuid_from_bytes(b)
}

/// 一次请求最多 4 个缓存断点（`cache_control`），超了上游整条拒。
/// 官方自己用掉 3 个（基座、其余、末条消息），故模拟时得数着加，见 [`simulate_system`]。
const MAX_CACHE_BREAKPOINTS: usize = 4;

/// 官方 `system` **恒为 4 块**：`cap/raw` 里四份订阅直连抓包（00006/00009/00031/00035）
/// 无一例外都是 `[billing, 身份句, 基座, 其余]`，API-key 模式那三份是 3 块合并态
/// （见 [`align_system_shape`]）——两种切法都不超过 4。
///
/// 块数超了就不再是 CC 形态，上游按第三方应用计费，客户端会看到
/// `Third-party apps now draw from your extra usage, not your plan limits.`
/// ——请求照样有回复，只是从订阅额度转到了超额池。故对齐它是**计费正确性**问题，
/// 不只是形态好看：见 [`cap_system_blocks`] 与 [`merge_system_blocks`]。
const MAX_SYSTEM_BLOCKS: usize = 4;

/// 把非 CC 请求的 `system` 换成官方形态的四块：
///
/// ```text
/// [0] x-anthropic-billing-header: …            无断点（cch 由 ensure_billing_cch 补）
/// [1] You are Claude Code, …（57B）            无断点
/// [2] 官方基座（按模型族）                      {ephemeral, scope:global}
/// [3] 客户端自己的 system（并成一块）           {ephemeral}
/// ```
///
/// 客户端的 `system` 是字符串就裹成一个文本块，是数组就并成一块（见
/// [`merge_system_blocks`]），没有就只有前三块。
///
/// **客户端那堆块必须并成一块**：官方末块就是「基座之后的全部内容」拼成的一大段，
/// 客户端自己拆成 N 块发过来，照搬就会得到 3+N 块——超过 [`MAX_SYSTEM_BLOCKS`]
/// 即被上游判为第三方应用、改扣超额池。
///
/// **断点是数着加的**：客户端可能自己就用满了 4 个（比如给每条工具定义都标了缓存），这时
/// 再加就会让整条请求被上游拒——那是把「形态更像」换成「根本发不出去」。预算不够时基座与
/// 末块照发，只是不带断点（少一次缓存复用，不影响正确性）。预算在**合并之后**才算：
/// 合并会消掉客户端 `system` 里那几个断点，先算就是按一个已经不存在的数字克扣基座。
fn simulate_system(v: &mut serde_json::Value, sim: &Simulation, cache: CacheShape) -> bool {
    let client: Vec<serde_json::Value> = match v.get("system") {
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => {
            vec![text_block_bare(s)]
        }
        Some(serde_json::Value::Array(a)) => merge_system_blocks(a.clone()),
        _ => Vec::new(),
    };
    // system 之外的断点（tools、messages）+ 合并后的客户端断点，才是本条请求已占的数目。
    let outside = count_cache_control(v) - v.get("system").map(count_cache_control).unwrap_or(0);
    let used = outside + client.iter().map(count_cache_control).sum::<usize>();
    let mut budget = MAX_CACHE_BREAKPOINTS.saturating_sub(used);

    let mut blocks =
        vec![text_block_bare(&billing_header_text()), text_block_bare(config::CC_SYSTEM_IDENTITY)];
    if let Some(base) = sim.base {
        if budget > 0 {
            budget -= 1;
            blocks.push(text_block(base, cache_control(cache)));
        } else {
            blocks.push(text_block_bare(base));
        }
    }
    // 末块补断点：官方在 system 末尾必有一个，但客户端自己标过就不重复标。
    let tail_open = client.last().is_some_and(|b| b.get("cache_control").is_none());
    blocks.extend(client);
    if tail_open
        && budget > 0
        && let Some(last) = blocks.last_mut().and_then(|b| b.as_object_mut())
    {
        last.insert("cache_control".into(), cache_control(cache.tail()));
    }

    insert_top_level(v, "system", serde_json::Value::Array(blocks), &["messages", "model"]);
    true
}

/// 把一串 `system` 文本块并成**一块**，正文用 `\n\n` 相连。
///
/// **为什么是拼而不是丢**：官方末块本身就是「基座之后的全部内容」拼成的一大段
/// （`cap/raw/00006` 里 12KB 一块），客户端把同样的内容拆成几块发过来，拼回去正是还原
/// 官方的切法——一个字都不少，只是不再各自成块。
///
/// **断点取最后一个**：合并后是连续的一段，末尾那个断点覆盖它前面的全部前缀，缓存语义与
/// 合并前的最后一个断点等价；中间那几个断点没有了，少几次缓存复用，不影响正确性。
///
/// 出现不是文本块的成员（`text` 不是字符串）就**原样交回**——`system` 里只能放文本块，
/// 别的东西是我们不认识的形态，宁可照发也不猜着改。正文全空的一并丢掉：发一个空文本块
/// 既没意义，上游也不收。
fn merge_system_blocks(blocks: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    if blocks.len() <= 1 {
        return blocks;
    }
    let texts: Vec<&str> = blocks.iter().filter_map(|b| b.get("text")?.as_str()).collect();
    if texts.len() != blocks.len() {
        return blocks;
    }
    let text = texts.into_iter().filter(|t| !t.trim().is_empty()).collect::<Vec<_>>().join("\n\n");
    if text.is_empty() {
        return Vec::new();
    }
    let cc = blocks.iter().rev().find_map(|b| b.get("cache_control").cloned());
    vec![match cc {
        Some(cc) => text_block(&text, cc),
        None => text_block_bare(&text),
    }]
}

/// 把 `system` 压回 [`MAX_SYSTEM_BLOCKS`] 块：第 4 块起的全部内容并进第 4 块
/// （见 [`merge_system_blocks`]）。
///
/// 兜住[`simulate_system`] 管不到的那两类来访：一是**自称 CC 却发了 5 块以上**的第三方
/// 客户端（[`is_cc_shaped`] 认它是 CC，于是既不模拟、也不走 [`align_system_shape`] 的三块
/// 分支），二是任何在我们之前就把 `system` 拆碎了的中间层。它们不改就是照着第三方额度扣。
///
/// 已经不超过 4 块（含官方的 4 块与 API-key 的 3 块）时不动结构、返回 `false`。
fn cap_system_blocks(v: &mut serde_json::Value) -> bool {
    let Some(sys) = v.get_mut("system").and_then(|s| s.as_array_mut()) else {
        return false;
    };
    if sys.len() <= MAX_SYSTEM_BLOCKS {
        return false;
    }
    let tail = sys.split_off(MAX_SYSTEM_BLOCKS - 1);
    let before = tail.len();
    let merged = merge_system_blocks(tail);
    let changed = merged.len() < before;
    sys.extend(merged);
    changed
}

/// `system[0]` 那条 billing header 的正文。`cch` 不在这里补——那是
/// [`ensure_billing_cch`] 的活，模拟与非模拟两条路共用它。
fn billing_header_text() -> String {
    format!("x-anthropic-billing-header: cc_version={}; cc_entrypoint=cli;", config::CC_VERSION)
}

/// 写入一个顶层字段，并把**新增**的那个放到官方 key 序里该在的位置：`after` 里最靠后的
/// 那个已有键之后（一个都没有就追加在末尾）。字段本来就在时原位替换，位置不动。
///
/// 官方线序是 `model → messages → system → tools → metadata → max_tokens → … → stream`，
/// 直接 append 会让补出来的 `system`/`metadata` 落到 `stream` 后面。key 顺序是这条链路上
/// 唯一还留得住的形态信息（body 全程 `preserve_order`，见 [`rewrite_body`]），既然要装，
/// 就装到底。注意来访客户端自己那部分 key 序照旧不动——那是它的形态，不是我们要改的。
fn insert_top_level(
    v: &mut serde_json::Value,
    key: &str,
    value: serde_json::Value,
    after: &[&str],
) {
    let Some(obj) = v.as_object_mut() else { return };
    if obj.contains_key(key) {
        obj.insert(key.into(), value);
        return;
    }
    let at = after.iter().filter_map(|k| obj.keys().position(|have| have == k)).max();
    match at {
        Some(at) => obj.shift_insert(at + 1, key.into(), value),
        None => obj.insert(key.into(), value),
    };
}

/// 递归数出 body 里现有的 `cache_control` 个数（上游按整条请求算，不只是 `system`）。
fn count_cache_control(v: &serde_json::Value) -> usize {
    match v {
        serde_json::Value::Object(map) => {
            let here = usize::from(map.contains_key("cache_control"));
            here + map.values().map(count_cache_control).sum::<usize>()
        }
        serde_json::Value::Array(items) => items.iter().map(count_cache_control).sum(),
        _ => 0,
    }
}

/// 模拟模式下给没有 `metadata.user_id` 的请求造一个官方形态的身份（键序与 CC 一致：
/// `device_id` → `account_uuid` → `session_id`，紧凑 JSON 塞在字符串里）。
///
/// 客户端自己带了 `user_id` 就不动——那条交给 [`spoof_identity`] 按原格式定点改写，
/// 两条路只能有一条动它。凭证没有 `account_uuid`（旧库未回填）时返回 `false` 不造：
/// 一个 `account_uuid` 为空、`device_id` 却是 64 位 hex 的组合，真实客户端不产生。
fn ensure_cc_metadata(
    v: &mut serde_json::Value,
    cred: &crate::credentials::Credential,
    device_fp: &str,
    session_id: &str,
) -> bool {
    if v.get("metadata").and_then(|m| m.get("user_id")).is_some() {
        return false;
    }
    let account_uuid = match cred.account_uuid.as_deref() {
        Some(u) if !u.trim().is_empty() => u.to_string(),
        _ => return false,
    };
    let Some(device_id) = cred.spoof_device_id(device_fp) else { return false };

    let mut inner = serde_json::Map::new();
    inner.insert("device_id".into(), device_id.into());
    inner.insert("account_uuid".into(), account_uuid.into());
    inner.insert("session_id".into(), session_id.into());
    // 紧凑序列化（无空白），与 CC 发的那串形态一致。
    let user_id = serde_json::Value::Object(inner).to_string();

    // metadata 已经是个对象（只是没有 user_id）就往里塞，否则整个造一个——后者也覆盖掉
    // 「metadata 存在但不是对象」这种畸形值。位置按官方 key 序落在 tools/system 之后。
    if let Some(meta) = v.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        meta.insert("user_id".into(), user_id.into());
        return true;
    }
    let mut meta = serde_json::Map::new();
    meta.insert("user_id".into(), user_id.into());
    insert_top_level(
        v,
        "metadata",
        serde_json::Value::Object(meta),
        &["tools", "system", "messages"],
    );
    true
}

/// 读取请求体声明的模型名（顶层 `model`）。用于按模型分格的限流冷却，见
/// [`rate_limit_scope`]。解析失败或没有该字段时返回 `None`（退化为账号级冷却）。
fn request_model(body: Option<&serde_json::Value>) -> Option<String> {
    Some(body?.get("model")?.as_str()?.to_string())
}

/// 读取请求体声明的速度档（顶层 `speed` 字段，如 `"fast"`；配套 header
/// `anthropic-beta: fast-mode-*`）。解析失败或没有该字段时返回 `None`。
fn request_speed(body: Option<&serde_json::Value>) -> Option<String> {
    Some(body?.get("speed")?.as_str()?.to_string())
}

/// 来访有没有要流式响应（顶层 `stream:true`）。
///
/// **口径与上游一致**：只有布尔 `true` 算流式。字段缺失、`false`、以及 `"true"` 这种字符串
/// 都不是——上游那边它们同样得到一份整段 JSON，判断口径跟着响应形态走才不会错配。
fn stream_requested(body: &serde_json::Value) -> bool {
    body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false)
}

/// 把顶层 `stream` 置为 `true`；已经是 `true` 就返回 `false`（无改动）。
///
/// 位置由 `preserve_order` 保证：字段已在则原位改值，不在则追加到末尾——而官方线序里
/// `stream` 本来就是最后一个（见 [`insert_top_level`] 的说明），两条路都落在官方位置上。
fn set_stream_true(v: &mut serde_json::Value) -> bool {
    let Some(obj) = v.as_object_mut() else { return false };
    if obj.get("stream").and_then(|s| s.as_bool()) == Some(true) {
        return false;
    }
    obj.insert("stream".into(), serde_json::Value::Bool(true));
    true
}

/// 给出站 URL 补上官方客户端恒带的 `?beta=true`（已经有 `beta=` 就原样返回）。
///
/// **依据**：`cap/raw` 八份抓包（四份直连、四份经 luban 的 API-key 模式）的请求行**无一例外**
/// 是 `POST /v1/messages?beta=true`。而 Anthropic 公开的 API 里没有这个参数——文档与各语言 SDK
/// 一律发裸 `/v1/messages`，beta 能力全靠 `anthropic-beta` 头开。两边合起来说明它是 **CC 客户端
/// 自己的标记**，不是 beta 功能的开关：补它是形态对齐，漏它不影响功能（模拟路径现在就能用）。
///
/// 只在[`Simulation`]那条路上补——那条路已经把头和体整套装成了 CC，URL 上再漏掉这个参数，
/// 就是「头上声明了一整串官方 beta、URL 却没开 beta 模式」这种真实客户端不产生的组合。
///
/// 客户端自己写了 `beta=`（含 `beta=false`）时不动：那是它自己的选择，替它改属于越权。
fn ensure_beta_query(url: &str) -> String {
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or("");
    if query.split('&').any(|kv| kv.split_once('=').map(|(k, _)| k) == Some("beta")) {
        return url.to_string();
    }
    let sep = if query.is_empty() { '?' } else { '&' };
    format!("{url}{sep}beta=true")
}

/// 转发前改写请求体，各项分别受 [`store::ForwardFlags`] 里的开关控制（默认全开；全关即
/// 请求体逐字节原样转发）：
///
/// 0. **模拟**（`simulate_cc`，仅当 `sim` 为 `Some`，即来访不是 CC 形态）：补上官方
///    `system` 前缀与 `metadata` 身份，见 [`Simulation`]。它先跑——后面几项都是在
///    「已经是 CC 形态」的前提下做微调。
/// 1. **system 形态**（`system_shape`）：把 API-key 模式的 3 块改写成订阅模式的 4 块，
///    见 [`align_system_shape`]。含拆块与基座标 `scope:"global"`（后者另受
///    `cache_scope_global` 管）。模拟路径已经直接产出 4 块，故两者互斥，不叠加。同一开关还管**块数封顶**
///    （[`cap_system_blocks`]）：超过 4 块的 `system` 会被上游判成第三方应用、改扣超额池。
/// 2. **身份伪装**（`spoof_identity`）：把 `metadata.user_id` 里的 `account_uuid`/`device_id`
///    换成该凭证自洽的身份（真实 account_uuid + 由其稳定派生的 device_id），避免
///    「真账号 + 陌生设备」的矛盾。它也管着模拟路径的 `metadata` 注入——凭空造一份身份，
///    本来就是同一件事。
/// 3. **cch**（`billing_cch`）：给 `x-anthropic-billing-header` 补订阅模式独有的 `cch`。
/// 4. **流式化**（`force_stream`，由 `nonstream_as_sse` 拨）：把 `stream` 置成 `true`。
///    官方 CC 恒为 `true`，回程由 [`aggregate_sse`] 聚合回整段 JSON，客户端无感。
///
/// **key 顺序**：改写要把 body 重新序列化，serde_json 默认的 `Map = BTreeMap` 会把**整个
/// body**（含 tools/messages/content/cache_control 里每一个对象）的 key 按字母序重排，得到
/// 官方客户端不会产生的排列——集合对了顺序错，一次精确比对即可判定中间有代理。故本 crate
/// 开了 serde_json 的 `preserve_order`（见 Cargo.toml），解析出的顺序原样写回，
/// 新增字段追加在末尾。回归测试见 [`tests::preserves_key_order`]。
///
/// 解析失败或结构异常时原样返回——绝不因改写失败而阻断转发。
// 参数多是有意的：这些全是「一次改写要知道的上下文」，打包成结构体只会多一层间接，
// 而调用点只有 `Upstream::shape` 一处。
#[allow(clippy::too_many_arguments)]
fn rewrite_body(
    body: &Bytes,
    cred: &crate::credentials::Credential,
    device_fp: &str,
    flags: store::ForwardFlags,
    sim: Option<&Simulation>,
    bare_session: Option<&str>,
    force_stream: bool,
    tool_names: Option<&ToolNameMap>,
) -> Bytes {
    // `system_shape` 不连着 `merge_beta`：它只负责拆块，而裸的 `{"type":"ephemeral"}` 是 GA
    // 能力，不需要任何 beta 声明。断点上那两项可选字段才各自要一个 beta。
    let shape = flags.system_shape;
    // `scope:"global"` 要 `prompt-caching-scope-2026-01-05`、`ttl:"1h"` 要
    // `extended-cache-ttl-2025-04-11`，两个都由 `merge_beta` 补。故各自的开关之外还得叠上
    // 它——否则就是「body 里写了字段、头上没声明」的自相矛盾。
    let cache = CacheShape {
        global: flags.cache_scope_global && flags.merge_beta,
        ttl_1h: flags.cache_ttl_1h && flags.merge_beta,
    };
    // 全关且不模拟：连解析都不必做，原样返回。
    if sim.is_none()
        && !shape
        && !flags.spoof_identity
        && !flags.billing_cch
        && !flags.strip_extra_fields
        && !force_stream
        && tool_names.is_none()
    {
        return body.clone();
    }
    // 补 metadata 用的 session_id：模拟模式取 Simulation 那份，CC 形态来访取 `bare_session`
    // （见 [`Upstream::bare_session`]）。两者都与出站头上的 `X-Claude-Code-Session-Id` 同值。
    let meta_session = sim.map(|s| s.session_id.as_str()).or(bare_session);
    let mut v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return body.clone(),
    };
    let simulated = sim.is_some_and(|sim| simulate_system(&mut v, sim, cache));
    // `context_management` 只补在模拟路径上：声明它的 `context-management-2025-06-27` 出自模拟
    // seed，而 [`Simulation::detect`] 本身就要求 `merge_beta` 开着，故「体里有 `edits`、头上没
    // 声明」这个反向矛盾在这条路上构造不出来——不必像 `scope_global` 那样再叠一次 `merge_beta`。
    let ctx_mgmt = sim.is_some() && ensure_context_management(&mut v);
    // 官方那第三个断点在最后一条消息上，模拟路径此前从不碰 `messages`，故要补。
    // 跟在 `simulate_system` 之后：断点预算得把它已经用掉的那些算进去。
    let msg_shape = sim.is_some() && align_message_shape(&mut v, cache);
    // 模拟已经产出官方的 4 块形态，再走一遍三块拆分器只会切错地方。
    let shaped = shape && !simulated && align_system_shape(&mut v, cache);
    // 封顶跟在两条整形之后：那两条产出的都是 4 块，故只对它们都没管住的来访生效。
    let capped = shape && cap_system_blocks(&mut v);
    let cch_added = flags.billing_cch && ensure_billing_cch(&mut v);
    // 收尾：把客户端自己那些断点的 `ttl` 也补齐，否则就是「system 有、消息没有」这种官方
    // 不产生的半对齐（见 [`fill_cache_ttl`]）。放在所有整形之后，才能覆盖到全部断点。
    //
    // **只在整形真的成了才补**：`ttl:"1h"` 属于订阅那套四块形态，API-key 的三块形态官方
    // 一个 ttl 都不带（`cap/raw/00012`）。整形没做成（比如锚点漂了、`system_shape` 关着）
    // 时 body 还是三块，这时补 ttl 就是把半对齐换了个方向，比不补更糟。
    let ttl_filled = cache.ttl_1h && (simulated || shaped) && fill_cache_ttl(&mut v);
    tracing::debug!(
        metadata = %v.get("metadata").map(|m| m.to_string()).unwrap_or_else(|| "<none>".into()),
        "inbound metadata"
    );
    let sim_meta = flags.spoof_identity
        && meta_session.is_some_and(|sid| ensure_cc_metadata(&mut v, cred, device_fp, sid));
    let spoofed =
        flags.spoof_identity && spoof_identity(&mut v, cred, device_fp, flags.spoof_device_id);
    // 流式化：`stream` 在官方线序里就在队尾，来访带了它就原位改值、没带就追加，两条路
    // 落点都与官方一致（`preserve_order` 下 `insert` 对已有键不动位置）。
    let streamed = force_stream && set_stream_true(&mut v);
    // 剥掉官方不发的顶层字段。放在最后：前面几步只增不减，剥这一步与它们无交集，
    // 摆在队尾就不必操心谁先谁后。
    let stripped = flags.strip_extra_fields && strip_extra_fields(&mut v);
    // 来访已有的顶层字段仍可能带着第三方客户端的键序。模拟路径既然已在整体
    // 替换客户端形态，就在所有增删之后对齐整个顶层对象，不只安排 luban 新增的键。
    let top_level_ordered = sim.is_some() && align_cc_top_level_order(&mut v);
    // 工具名混淆放在最末：它只改 `name` 字段，与前面每一步都无交集。
    let tools_mimicked = tool_names.is_some_and(|m| apply_tool_names(&mut v, m));
    tracing::debug!(
        simulated,
        sim_meta,
        shaped,
        capped,
        spoofed,
        cch_added,
        ctx_mgmt,
        msg_shape,
        ttl_filled,
        streamed,
        stripped,
        top_level_ordered,
        tools_mimicked,
        device_fp = %device_fp,
        spoof_device = %cred.spoof_device_id(device_fp).as_deref().unwrap_or("-"),
        "rewrote body"
    );
    if !shaped
        && !capped
        && !spoofed
        && !cch_added
        && !simulated
        && !sim_meta
        && !ctx_mgmt
        && !msg_shape
        && !ttl_filled
        && !streamed
        && !stripped
        && !top_level_ordered
        && !tools_mimicked
    {
        return body.clone();
    }
    match serde_json::to_vec(&v) {
        Ok(bytes) => Bytes::from(bytes),
        Err(_) => body.clone(),
    }
}

/// 裸客户端（无 `metadata.user_id`）在请求日志里用的设备标识：出站那份**伪装** device_id，
/// 加 `sim:` 前缀。没伪装过就返回 `None`（日志照旧是 `-`）。
///
/// **只在真伪装过时才记**：要求 [`ensure_cc_metadata`] 确实把这个 id 写进了出站体，也就是
/// `spoof_identity` 开着、且走了会补身份的那两条路之一——模拟路径（`sim` 为 `Some`）或
/// CC 形态补身份（`bare_session` 为 `Some`，见 [`Upstream::bare_session`]）。否则记出来的是
/// 一个上游根本没见过的 id，比留个 `-` 更误导。
///
/// **前缀不是装饰**：这个值只随「账号 + 平台指纹」变（裸客户端没有自己的 device_id，指纹退化
/// 成 `"|<arch>|<os>"`，同账号同平台的所有裸客户端共用一个），看着就像「一台设备打了全部
/// 请求」。前缀让它在日志与 `usage_logs` 里一眼可辨，不至于被当成真实设备读。它也**不写设备绑定**，故不占 `device_limit` 名额、不会出现在设备列表里
/// （[`store::CredentialStore::list_devices`] 从 `device_bindings` 出发）。
fn sim_device_id(
    sim: Option<&Simulation>,
    bare_session: Option<&str>,
    flags: store::ForwardFlags,
    cred: &crate::credentials::Credential,
    device_fp: &str,
) -> Option<String> {
    if (sim.is_none() && bare_session.is_none()) || !flags.spoof_identity {
        return None;
    }
    cred.spoof_device_id(device_fp).map(|d| format!("sim:{d}"))
}

/// 构造设备指纹：客户端原始 `device_id` + 平台 `arch`/`os`，用于派生每设备唯一的伪装
/// device_id。刻意只取**稳定的硬件/系统身份**，排除会随客户端升级变动的字段
/// （runtime 版本、UA 版本号），以免每次升级都刷新 device_id。
fn device_fingerprint(client_device_id: Option<&str>, headers: &HeaderMap) -> String {
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("");
    format!("{}|{}|{}", client_device_id.unwrap_or(""), h("x-stainless-arch"), h("x-stainless-os"),)
}

/// 从一组头里取 `User-Agent` 供日志与落库用：没有该头或不是可打印 ASCII 时为 `-`。
///
/// 来访头与出站头两侧都用它——[`ReqLog`] 两份 UA 各存各的，取值规则必须是同一套，
/// 否则「入站 == 出站」这个判断会因为两边截断/回退方式不同而失真。
///
/// 截断到 120 字符：官方 CC 那串（`claude-cli/2.1.220 (external, cli)`）只有 35 字符，
/// 浏览器与各路 SDK 拼出来的能有几百，整条打出来会把日志行撑得没法看。截断只影响日志与
/// 落库，转发出去的那份头一个字节都不动。
///
/// 取值恒为可见 ASCII：`to_str()` 对非 ASCII 头值直接失败，那类一律落 `-`。按 `char` 截而不是
/// `&s[..120]` 只是不给未来留坑——真按字节切，哪天换个不做此保证的取值方式就会切出 panic。
fn ua_of(headers: &HeaderMap) -> String {
    const MAX: usize = 120;
    match headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()) {
        Some(ua) if !ua.trim().is_empty() => ua.chars().take(MAX).collect(),
        _ => "-".into(),
    }
}

/// 把 `metadata.user_id` 里的 `account_uuid`/`device_id` 换成凭证自洽身份，**保持原格式**：
/// - CC 内嵌 JSON：**字符串级定点替换**这两个字段的值，字段顺序与其余内容原样不动。
///   真实 CC 发的是紧凑 JSON `{"device_id":..,"account_uuid":..,"session_id":..}`。外层 body
///   已靠 serde_json 的 `preserve_order` 保住顺序，但这层仍绕开 serde：内层是**字符串里的
///   JSON**，重新序列化会连空白、转义写法一起归一化，只有定点替换才逐字节不变。
/// - 扁平串 `user_<hash>_account_<acct>_session_<sess>`（如 Windows）：换掉 device 段与
///   account 段，保留 session 段，仍以扁平串回写——不把 Windows 请求伪装成 CC 的 JSON 形态。
///
/// `spoof_device` 关掉时**只换 account 段**，来访自带的 `device_id` 原样保留——依据与代价
/// 见 [`store::ForwardFlags::spoof_device_id`]（一句话：抓包证明两种官方模式的 `device_id`
/// 相同，换掉它是反关联策略而非形态要求）。account 段照换：那才是两种模式真正的差别。
///
/// 凭证无 `account_uuid`（如旧库未回填）或 user_id 结构无法识别时不改动，返回 `false`。
fn spoof_identity(
    v: &mut serde_json::Value,
    cred: &crate::credentials::Credential,
    device_fp: &str,
    spoof_device: bool,
) -> bool {
    let account_uuid = match cred.account_uuid.as_deref() {
        Some(u) if !u.trim().is_empty() => u,
        _ => return false,
    };
    // 关掉时不必派生，也就不该因为派生不出来而放弃改写 account 段。
    let device_id = match spoof_device {
        true => match cred.spoof_device_id(device_fp) {
            Some(d) => Some(d),
            None => return false,
        },
        false => None,
    };
    let user_id = match v.get_mut("metadata").and_then(|m| m.get_mut("user_id")) {
        Some(u) => u,
        None => return false,
    };
    let inner_str = match user_id.as_str() {
        Some(s) => s.to_string(),
        None => return false,
    };

    // 格式一：CC 内嵌 JSON——先确认是 JSON 对象，再对原始字符串做定点值替换，
    // 保持字段顺序与其余内容（session_id 等）逐字节不变。
    if serde_json::from_str::<serde_json::Value>(&inner_str)
        .ok()
        .as_ref()
        .and_then(|v| v.as_object())
        .is_some()
    {
        let mut s = inner_str;
        let mut changed = false;
        if let Some(next) = replace_json_str_field(&s, "account_uuid", account_uuid) {
            s = next;
            changed = true;
        }
        if let Some(d) = device_id.as_deref()
            && let Some(next) = replace_json_str_field(&s, "device_id", d)
        {
            s = next;
            changed = true;
        }
        if changed {
            *user_id = serde_json::Value::String(s);
        }
        return changed;
    }

    // 格式二：扁平串——保持格式，只换 device 与 account，保留 session。
    // `spoof_device` 关掉时 device 段也一并保留，只换 account 段。
    if let Some(flat) = parse_flat_user_id(&inner_str) {
        let device = device_id.as_deref().unwrap_or(&flat.device);
        let rebuilt = format!("user_{}_account_{}_session_{}", device, account_uuid, flat.session);
        *user_id = serde_json::Value::String(rebuilt);
        return true;
    }

    false
}

/// 在紧凑 JSON 字符串里，把 `"key":"<旧值>"` 的值原地替换成 `new_val`，字段位置与其余
/// 内容逐字节不变。仅处理**字符串型且值内无转义引号**的字段——`device_id`(hex)、
/// `account_uuid`(UUID，可能为空串)均满足，`new_val` 同为 hex/UUID，无需 JSON 转义。
/// 找不到该字段（或其不是 `"key":"` 形态）时返回 `None`，**不新增字段**，以免改变结构。
fn replace_json_str_field(s: &str, key: &str, new_val: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let val_start = s.find(&needle)? + needle.len();
    // 值到下一个引号为止（值内无转义引号，故直接找 '"'）。
    let val_end = val_start + s[val_start..].find('"')?;
    let mut out = String::with_capacity(s.len() - (val_end - val_start) + new_val.len());
    out.push_str(&s[..val_start]);
    out.push_str(new_val);
    out.push_str(&s[val_end..]);
    Some(out)
}

/// 给 `system[0]` 的 `x-anthropic-billing-header` 补上 `cch=<值>`，对齐订阅客户端。
///
/// 官方客户端只在订阅(OAuth)模式下发这个字段，API-key 模式（接入 luban 的形态）不发，
/// 于是「OAuth token + 无 cch」是个确定性判据。抓包实测补齐后与真实客户端形态一致：
/// `…cc_version=2.1.218.0b9; cc_entrypoint=cli; cch=00000;`
///
/// 只在该块确实是 billing header、且尚无 `cch=` 时改写；其余情况返回 `false` 不动结构。
///
/// 注意：`system[0]` 位于第一个缓存断点之前，属于被缓存的前缀——改写它会让**部署后的第一次
/// 请求**缓存未命中一次。因此 `cch` 的取值必须对同一前缀保持稳定，见 [`cch_value`]。
fn ensure_billing_cch(v: &mut serde_json::Value) -> bool {
    let blk = match v.get_mut("system").and_then(|s| s.as_array_mut()).and_then(|a| a.first_mut()) {
        Some(b) => b,
        None => return false,
    };
    let text = match blk.get("text").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return false,
    };
    if !text.starts_with("x-anthropic-billing-header:") || text.contains("cch=") {
        return false;
    }
    let mut s = text.trim_end().to_string();
    if !s.ends_with(';') {
        s.push(';');
    }
    s.push_str(&format!(" cch={};", cch_value()));
    match blk.get_mut("text") {
        Some(t) => {
            *t = serde_json::Value::String(s);
            true
        }
        None => false,
    }
}

/// 补上官方客户端恒发的 `context_management`，落在官方位置（`thinking` 之后、
/// `output_config`/`stream` 之前）。已经有这个字段就原样不动，返回 `false`。
///
/// **依据**：`cap/raw` 八份抓包（四份直连、四份经 luban）的顶层 `context_management`
/// **逐字节相同**——`{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}`，
/// 四个模型族无一例外，连 haiku 那两份也一样。这与 `thinking`/`output_config` 那种逐族不同
/// 的字段不是一类，不存在「补哪一份」的选择问题。
///
/// **为什么该补**：这个字段要 `context-management-2025-06-27` 认，而两份 seed
/// （[`config::CC_BETA_SIMULATED`] 与 [`config::CC_BETA_SIMULATED_HAIKU`]）**都带着它**。
/// 不补就是「头上声明了 context-management、体里零个 `edits`」——与
/// [`ensure_beta_query`] 要消灭的那个组合同一个形状，只是落在体上。
///
/// `keep:"all"` 意为「一条都不清」，故补它不改变本次请求的语义，也不动计价：与
/// `known_fingerprint_gaps` 第 7 条的 `fallbacks`（补上等于替用户决定换模型）正相反，
/// 那条不补的理由在这里不成立。
///
/// **但它不是独立字段——依赖 `thinking`**。上游对「有 `clear_thinking` 却没开 thinking」
/// 的请求直接回 400：
///
/// ```text
/// `clear_thinking_20251015` strategy requires `thinking` to be enabled or adaptive
/// ```
///
/// 抓包看不出这层依赖：八份**全都**开着 thinking（opus/sonnet/fable 是 `{"type":"adaptive"}`，
/// haiku 是 `{"budget_tokens":31999,"type":"enabled"}`），于是 8/8 共现让它看着像个独立字段。
/// 这是一次「共现不等于无依赖」的教训——v0.2.51 上线后普通请求即因此 400。
///
/// **不替客户端补 `thinking`** 来满足这个依赖，三条理由都写在抓包里：
/// 1. haiku 那份是 `budget_tokens:31999` 配 `max_tokens:32000`，budget 必须小于 max_tokens。
///    客户端发 `max_tokens:1024` 时这个值根本塞不进去，要么改它的 max_tokens（改掉它明确
///    要的上限与费用天花板），要么自己算一个 budget——两条都是替它做决定。
/// 2. 开了 thinking，响应里就多出 thinking 块，客户端未必认得，直接把它弄坏。
/// 3. thinking token 按输出计费，等于未经同意加钱。
///
/// 故只在客户端**自己已经开着** thinking 时才补，其余情形一个字节都不动。
fn ensure_context_management(v: &mut serde_json::Value) -> bool {
    let Some(obj) = v.as_object_mut() else { return false };
    // 客户端自己带了就不动——那是它自己的编辑策略，替它改属于越权（同 [`ensure_beta_query`]
    // 对客户端自带 `beta=` 的口径）。
    if obj.contains_key("context_management") {
        return false;
    }
    // 没开 thinking 就不补：`clear_thinking` 依赖它，硬补上游直接 400（见函数文档）。
    // 上游认的是 `enabled`/`adaptive` 两种，`disabled` 与字段缺失都不算。
    let thinking_on = obj
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|t| t.as_str())
        .is_some_and(|t| matches!(t, "enabled" | "adaptive"));
    if !thinking_on {
        return false;
    }
    let value = serde_json::json!({
        "edits": [{ "type": "clear_thinking_20251015", "keep": "all" }]
    });
    // 官方顺序是 `… max_tokens, thinking, context_management, output_config, stream`。
    // 走到这里必有 `thinking`，故锚点首选它，落位与官方一致。
    insert_top_level(
        v,
        "context_management",
        value,
        &["thinking", "max_tokens", "metadata", "tools", "system", "messages", "model"],
    );
    true
}

/// `cch` 的取值。当前是常量 [`config::BILLING_CCH`]。
///
/// 真实算法无法从抓包反推，所以这里只能给占位值。**代价**：它跨账号恒定，所有经由 luban
/// 的请求都带同一个真实客户端从不产生的 `cch`，上游一按此聚类就把所有账号串成一串。
///
/// 想改成每账号不同，把本函数换成从「已在缓存前缀内的内容」派生即可，例如
/// `sha256(account_uuid ‖ system[1..] 文本)` 取前 5 位小写 hex：前缀不变则取值不变，
/// 不会打爆 prompt cache，同时每个账号各自不同。（若上游会校验 cch 与内容的对应关系，
/// 两种做法都是错值——那种情况下正确的选择是根本不补，见 config 里的说明。）
fn cch_value() -> &'static str {
    config::BILLING_CCH
}

/// 把 API-key 模式的 3 块 `system` 改写成订阅模式的 4 块，并把全部缓存断点对齐到官方形态。
///
/// 两种形态的差别只有「切法」，文本本身逐字节相同（`cap/raw` 那对同机同版本抓包验证过）：
///
/// ```text
/// 官方直连(00006)                     API-key 模式(00002)
/// [0] billing header      无断点      [0] billing header      无断点
/// [1] 身份句 57B          无断点      [1] 身份句 57B          {ephemeral}   ← 多余断点
/// [2] 基座 1210B  {type,ttl:1h,scope:global}
/// [3] 其余        {type,ttl:1h}       [2] 基座‖"\n\n"‖其余    {ephemeral}
///                                     （luban 拆出来的两块不写 ttl，见 cache_control）
/// ```
///
/// 故改写是三件事，**同受一个开关控制**：
/// 1. 在 [`config::CC_SYSTEM_BASE_ANCHOR`] 前的 `\n\n` 处把合并块切成基座 + 其余；
/// 2. 基座标 `{type:ephemeral, scope:global}`，其余标 `{type:ephemeral}`；
/// 3. 去掉身份句上那个断点——它的缓存前缀只有 127 字节（约 35 token），远低于最小可缓存长度，
///    本就是空转，官方也不发。
///
/// **不含 `ttl`**：官方那三个断点全是 `1h`，但缓存时长是客户端掏钱买的，替它翻倍不合适，
/// 理由见 [`cache_control`]。客户端自己写了 `ttl` 的照原样转发。
///
/// **`scope:global` 只标基座**，且另受 [`store::ForwardFlags::cache_scope_global`] 管。
/// 之前是「标 text 最长的那块」，在三块形态下必然选中合并块，而合并块含 `# Environment` 的
/// cwd/git、技能清单这些本机内容——跨账号不可能撞上，标了换不来复用。拆开之后基座是纯静态的，
/// 全网同一份，这个标记才真正有意义。
///
/// 保守起见只处理「确实是 API-key 三块形态」：`system` 长度不为 3、锚点匹配不到、或锚点前不是
/// `\n\n`，一律不动结构返回 `false`。客户端本来就是 4 块（订阅形态）时同样不动。
fn align_system_shape(v: &mut serde_json::Value, cache: CacheShape) -> bool {
    let sys = match v.get_mut("system").and_then(|s| s.as_array_mut()) {
        Some(s) if s.len() == 3 => s,
        _ => return false,
    };
    // 合并块必须本来就是个带断点的文本块，否则不是我们认识的形态。
    if sys[2].get("cache_control").is_none() {
        return false;
    }
    let text = match sys[2].get("text").and_then(|t| t.as_str()) {
        Some(t) => t.to_string(),
        None => return false,
    };
    // 逐个模型族的锚点找，取**最早**命中的那个：基座是前缀，切得越靠前越不会把基座切碎。
    // 锚点前必须紧跟 `\n\n`——那两个字节是两块的分隔符，切开后两边都不保留它。
    // 用字节比较：`find` 给的是字节偏移，`p - 2` 未必落在字符边界上，直接切片会 panic。
    let at = config::CC_SYSTEM_BASE_ANCHORS
        .iter()
        .filter_map(|anchor| text.find(anchor))
        .filter(|&p| p >= 2 && &text.as_bytes()[p - 2..p] == b"\n\n")
        .min();
    let Some(at) = at else { return false };

    if let Some(obj) = sys[1].as_object_mut() {
        obj.remove("cache_control");
    }
    sys[2] = text_block(&text[..at - 2], cache_control(cache));
    sys.push(text_block(&text[at..], cache_control(cache.tail())));
    true
}

/// 把 `messages` 对齐到官方形态：内容一律块数组，并给**最后一条消息的最后一块**补上官方那
/// 第三个缓存断点。返回是否改动过。
///
/// **依据**：`cap/raw` 八份抓包每条都是**恰好 3 个断点**，前两个在 `system`（基座、其余），
/// 第三个恒在最后一条消息的最后一个内容块上——`role` 是什么无关：六份非 haiku 落在末尾那条
/// `role:"system"` 消息上，两份 haiku 没有那条消息，就落在 `user` 消息的末块。规则是位置，
/// 不是角色。而模拟路径此前从不碰 `messages`，第三方 SDK 自己一般也不标，于是出去的请求
/// 只有 1~2 个断点。
///
/// **内容字符串化归一**：官方 8/8 的 `content` 都是块数组，而第三方 SDK 常发裸字符串。
/// 断点是块的属性，字符串上挂不住，所以要转。**转就全转**：只转最后一条会得到「一部分消息
/// 是字符串、一部分是数组」这种两边都不像的形态。两种写法在 API 上语义完全相同，转换只改
/// 表示、不改内容，与 [`simulate_system`] 把字符串 `system` 收成块是同一个路子。
///
/// **只在模拟路径调用**：CC 形态的来访自己就标好了第三个断点（`cap/raw/00012` 那条经 luban
/// 的真实请求即如此），替它再标一次只会多占预算。
///
/// **预算**：断点总数封顶 [`MAX_CACHE_BREAKPOINTS`]，超了上游整条拒。这里数的是**改写后
/// 整个 body** 的现存断点，故 [`simulate_system`] 已经用掉的那些都算在内；满了就不补——
/// 少一次缓存命中，总好过整条请求被拒。
///
/// **只往非空的 `text` 块上标**，两条理由各自独立：
/// - 抓包 8/8 那第三个断点都在 `text` 块上，别的块型没有样本，没依据的形态不猜着改；
/// - 末块未必是 `text`。会话以 assistant 轮结尾时（prefill）末块可能是 `thinking`——那种块
///   连签名都要上游验（见 [`is_thinking_signature_error`] 那条重试路），往上面挂 `cache_control`
///   是拿一条能发出去的请求去赌一个没有样本的组合。`tool_result`/`image` 同理。
///
/// 空 `text` 块一并跳过：发一个空文本块本身就会被上游拒，见 [`merge_system_blocks`]。
fn align_message_shape(v: &mut serde_json::Value, shape: CacheShape) -> bool {
    let mut changed = false;
    let Some(msgs) = v.get_mut("messages").and_then(|m| m.as_array_mut()) else { return false };
    for m in msgs.iter_mut() {
        let Some(content) = m.get_mut("content") else { continue };
        // 空串不转：`{"type":"text","text":""}` 是个上游会拒的块，而原样的 `""` 至少还是
        // 客户端自己发出来的形态——改写不该把一条请求的失败方式换个花样。
        match content.as_str() {
            Some(s) if !s.is_empty() => {
                *content = serde_json::Value::Array(vec![text_block_bare(s)]);
                changed = true;
            }
            _ => {}
        }
    }
    // 断点要在归一之后再数：刚转出来的块本身不带断点，但它得先存在才挂得上。
    if count_cache_control(v) >= MAX_CACHE_BREAKPOINTS {
        return changed;
    }
    let last = v
        .get_mut("messages")
        .and_then(|m| m.as_array_mut())
        .and_then(|a| a.last_mut())
        .and_then(|m| m.get_mut("content"))
        .and_then(|c| c.as_array_mut())
        .and_then(|blocks| blocks.last_mut())
        .and_then(|b| b.as_object_mut());
    let Some(block) = last else { return changed };
    // 客户端自己标过就不动——那是它自己的缓存策略。
    if block.contains_key("cache_control") {
        return changed;
    }
    // 只往非空 `text` 块上标：别的块型没有抓包样本，`thinking` 那种还要上游验签名（见函数文档）。
    let plain_text = block.get("type").and_then(|t| t.as_str()) == Some("text")
        && block.get("text").and_then(|t| t.as_str()).is_some_and(|t| !t.is_empty());
    if !plain_text {
        return changed;
    }
    // 用 `tail()`：官方只在基座标 `scope`，消息这个断点是 `{type, ttl}`。
    block.insert("cache_control".into(), cache_control(shape.tail()));
    true
}

/// 剥掉官方客户端**从不发送**的顶层字段，返回是否改动过。只在
/// [`store::ForwardFlags::strip_extra_fields`] 开着时调用。
///
/// 判据逐条取自 `cap/raw/00006`（opus-5）与 `00009`（sonnet-5）两份直连抓包——两份的顶层键
/// 恒为 `model, messages, system, tools, metadata, max_tokens, thinking, context_management,
/// output_config, stream`，多一个就是白送的判据。
///
/// 目前两项：
///
/// 1. **`tool_choice`**：官方两份抓包里这个键**压根不存在**。但只删**等价于默认值**的那一种
///    （恰好只有 `{"type":"auto"}` 一个键）——`{"type":"tool", "name":…}`/`{"type":"any"}`
///    是客户端在强制选工具，`disable_parallel_tool_use` 也是它要的行为，删了就是改语义。
///    删掉的那种对模型零影响：`auto` 本来就是缺省。
///
/// 2. **`thinking.display`**：官方发的是裸的 `{"type":"adaptive"}`。
///
///    **这一项有代价，不是零影响**：`display:"summarized"` 是客户端主动要思考摘要，剥掉之后
///    上游按缺省的 `omitted` 走，回程的 `thinking` 块文本为空，客户端那边的「思考过程」就空了。
///    功能不坏（块还在、签名照旧），只是看不到内容。拿「一条 400 直接打不通」换「思考摘要看不
///    到」是划算的，但划算不等于无损，故写在这里，并由开关兜底——不接受这个代价就关掉它。
///
/// **对真实 CC 是空操作**：官方本来就不发这两样，走一遍什么也删不掉，故无需再叠客户端判定。
fn strip_extra_fields(v: &mut serde_json::Value) -> bool {
    let Some(obj) = v.as_object_mut() else { return false };
    let mut changed = false;
    if obj.get("tool_choice").is_some_and(is_default_tool_choice) {
        obj.remove("tool_choice");
        changed = true;
    }
    if let Some(thinking) = obj.get_mut("thinking").and_then(|t| t.as_object_mut())
        && thinking.remove("display").is_some()
    {
        changed = true;
    }
    changed
}

/// `tool_choice` 是否等价于「不写这个字段」，即恰好只有 `{"type":"auto"}` 一个键。
/// 多带任何一个键（如 `disable_parallel_tool_use`）都是客户端在要一种非缺省行为，不能删。
fn is_default_tool_choice(v: &serde_json::Value) -> bool {
    v.as_object()
        .is_some_and(|o| o.len() == 1 && o.get("type").and_then(|t| t.as_str()) == Some("auto"))
}

/// 官方 Claude Code 对话请求的顶层键序。
///
/// 八份 `cap/raw/*.req.raw` 都保持这个相对顺序。haiku 没有 `output_config`，fable 多一个
/// `fallbacks`，但共有键一个都没挪位。模拟后若仍保留 `model, system, messages, ... stream,
/// tools` 这种来访顺序，即使字段集已对齐，也仍是一个稳定的第三方指纹。
const CC_BODY_KEY_ORDER: &[&str] = &[
    "model",
    "messages",
    "system",
    "tools",
    "metadata",
    "max_tokens",
    "thinking",
    "context_management",
    "fallbacks",
    "output_config",
];

/// 把请求对象改成 [`CC_BODY_KEY_ORDER`] 的顺序，并保证 `stream` 在最后。
///
/// 不认识的字段可能有语义，不能丢；保留它们彼此的原始顺序，放在已知字段与 `stream`
/// 之间。本函数只在 [`Simulation`] 路径调用，真 CC 请求继续保留客户端的字节与顺序。
fn align_cc_top_level_order(v: &mut serde_json::Value) -> bool {
    let Some(obj) = v.as_object_mut() else { return false };
    let before: Vec<String> = obj.keys().cloned().collect();
    let mut old = std::mem::take(obj);
    let mut ordered = serde_json::Map::new();

    for key in CC_BODY_KEY_ORDER {
        if let Some(value) = old.shift_remove(*key) {
            ordered.insert((*key).to_string(), value);
        }
    }
    let stream = old.shift_remove("stream");
    ordered.extend(old);
    if let Some(value) = stream {
        ordered.insert("stream".to_string(), value);
    }

    let changed = before.iter().map(String::as_str).ne(ordered.keys().map(String::as_str));
    *obj = ordered;
    changed
}

/// MCP 形态假名的工具段前缀池。来访原名加 `mcp__hermes__` 后，同一条探测由 400 变为
/// 200，证明 MCP 命名空间是上游豁免的形态。`manage_bfl00` 之类普通假名仍可被判成
/// 第三方，故生成的假名统一放在 `mcp__luban__*` 下。
const FAKE_TOOL_PREFIXES: &[&str] = &[
    "analyze_",
    "compute_",
    "fetch_",
    "generate_",
    "lookup_",
    "modify_",
    "process_",
    "query_",
    "render_",
    "resolve_",
    "sync_",
    "update_",
    "validate_",
    "convert_",
    "extract_",
    "manage_",
    "monitor_",
    "parse_",
    "review_",
    "search_",
    "transform_",
    "handle_",
];

/// 已知触发上游第三方判定的工具名（实测）。命中其中任意一个即回 400
/// `Third-party apps now draw from your extra usage…`，加 `mcp__` 前缀后豁免。
/// 抓包（`cap/raw/*.req.raw`）确认官方 CC 从不发这些名字，故只需对这张表做最小混淆。
const BLOCKED_TOOL_NAMES: &[&str] = &["skill_manage", "skill_view", "skills_list"];

/// 一次请求内的工具名混淆映射。
///
/// **为什么要混淆**：`tools[*].name` 是上游判定「这是不是第三方应用」的一个已验证判据，
/// 命中 [`BLOCKED_TOOL_NAMES`] 就把额度改扣超额池并回 400。加 `mcp__` 前缀后实测豁免。
struct ToolNameMap {
    /// 真名 → 假名，请求侧用。
    forward: std::collections::HashMap<String, String>,
    /// (假名, 真名)，按假名长度**倒序**——短假名可能是长假名的子串，先替长的才不会被吃掉。
    reverse: Vec<(String, String)>,
    /// 最长假名的字节数。回程滑动窗口靠它决定留多少字节，见 [`Self::feed`]。
    max_fake: usize,
}

/// 某个 tool 是否该混淆：仅命中 [`BLOCKED_TOOL_NAMES`] 的工具名需要改写，其余原样透传。
/// server tool（`web_search_20250305` 等）即使同名也不改——改了上游直接拒。
fn should_mimic_tool(t: &serde_json::Value) -> bool {
    let kind = t.get("type").and_then(|k| k.as_str()).unwrap_or_default();
    if !matches!(kind, "" | "custom" | "function") {
        return false;
    }
    let Some(name) = t.get("name").and_then(|n| n.as_str()) else { return false };
    BLOCKED_TOOL_NAMES.contains(&name)
}

/// 从请求体扫出要混淆的工具名，生成映射。没有可混淆的（`tools` 不是数组／全在白名单里）
/// 返回 `None`，此后请求与回程两侧都零开销。
///
/// **假名对同一组工具名恒定**：seed 取 `sha256(名字集合)`，同一会话内每轮请求得到同一套假名，
/// 上游的 prompt cache 才命中得了。客户端中途增删工具会让整套假名全变——历史里的
/// `tool_use.name` 由 [`apply_tool_names`] 用**新**映射一起重写，故仍然自洽，代价只是缓存失效。
fn build_tool_name_map(body: Option<&serde_json::Value>) -> Option<ToolNameMap> {
    let tools = body?.get("tools")?.as_array()?;
    let declared: std::collections::HashSet<&str> =
        tools.iter().filter_map(|t| t.get("name").and_then(|n| n.as_str())).collect();
    let real: Vec<&str> = tools
        .iter()
        .filter(|t| should_mimic_tool(t))
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    if real.is_empty() {
        return None;
    }

    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    for (i, name) in real.iter().enumerate() {
        if i > 0 {
            sha2::Digest::update(&mut hasher, b"\0");
        }
        sha2::Digest::update(&mut hasher, name.as_bytes());
    }
    let digest = sha2::Digest::finalize(hasher);
    let seed = u64::from_be_bytes(digest[..8].try_into().expect("sha256 至少 8 字节"));

    let mut forward = std::collections::HashMap::with_capacity(real.len());
    let mut reverse = Vec::with_capacity(real.len());
    let mut max_fake = 0usize;
    for (i, name) in real.iter().enumerate() {
        if forward.contains_key(*name) {
            continue; // 同名工具重复声明：一个映射就够。
        }
        let prefix = FAKE_TOOL_PREFIXES
            [(seed.wrapping_add(i as u64) % FAKE_TOOL_PREFIXES.len() as u64) as usize];
        // 取真名开头三个 ASCII 字母数字，纯粹为了假名在日志里还认得出是谁。
        let head: String = name.chars().filter(|c| c.is_ascii_alphanumeric()).take(3).collect();
        let stem = format!("mcp__luban__{prefix}{head}{i:02}");
        let mut fake = stem.clone();
        // 假名撞上任何已声明工具都会让上游分不清该调谁。序号已保证假名之间唯一，
        // 这里再兜住来访本来就声明了同名 MCP 工具的极端情形。
        let mut collision = 0usize;
        while declared.contains(fake.as_str()) {
            collision += 1;
            fake = format!("{stem}_{collision}");
        }
        max_fake = max_fake.max(fake.len());
        reverse.push((fake.clone(), (*name).to_string()));
        forward.insert((*name).to_string(), fake);
    }
    if forward.is_empty() {
        return None;
    }
    reverse.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
    Some(ToolNameMap { forward, reverse, max_fake })
}

/// 把映射应用到请求体，返回是否改动过。三处必须**同时**改：
///
/// - `$.tools[*].name`
/// - `$.tool_choice.name`（仅 `type == "tool"`，即客户端强制指定了某个工具）
/// - `$.messages[*].content[*].name`（仅 `type == "tool_use"`，即历史里的工具调用）
///
/// 漏掉第三处的话，上游会因为 `tool_use` 引用了一个 `tools` 里没声明的名字而拒掉整条请求。
fn apply_tool_names(v: &mut serde_json::Value, map: &ToolNameMap) -> bool {
    let mut changed = false;
    let mut rename = |obj: &mut serde_json::Value| {
        let Some(name) = obj.get("name").and_then(|n| n.as_str()) else { return };
        let Some(fake) = map.forward.get(name) else { return };
        obj["name"] = serde_json::Value::String(fake.clone());
        changed = true;
    };

    if let Some(tools) = v.get_mut("tools").and_then(|t| t.as_array_mut()) {
        for t in tools.iter_mut() {
            if should_mimic_tool(t) {
                rename(t);
            }
        }
    }
    if let Some(tc) = v.get_mut("tool_choice")
        && tc.get("type").and_then(|t| t.as_str()) == Some("tool")
    {
        rename(tc);
    }
    if let Some(messages) = v.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            let Some(blocks) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
                continue;
            };
            for b in blocks.iter_mut() {
                if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    rename(b);
                }
            }
        }
    }
    changed
}

impl ToolNameMap {
    /// 回程还原：假名 → 真名。按假名长度倒序逐个替换。
    ///
    /// **按字节而不是按 `str` 做**：回程是流式的，一个 chunk 可以在任意字节处切断，
    /// `String::from_utf8` 会在半个多字节字符上失败。假名全是 ASCII，字节级替换在 UTF-8 上
    /// 安全（ASCII 不会出现在多字节序列内部）。
    fn restore(&self, buf: &[u8]) -> Vec<u8> {
        let mut out = buf.to_vec();
        for (fake, real) in &self.reverse {
            out = replace_bytes(&out, fake.as_bytes(), real.as_bytes());
        }
        out
    }

    /// 流式还原的一步：吃进一块，吐出**可以安全发走**的部分。
    ///
    /// 假名可能被 TCP 分块从中间切开（`analyze_ski00` 拆成 `analyze_sk` + `i00`），那一次就
    /// 还原不了，客户端会拿到假名，下一轮请求带着假名回来，请求侧映射表里查不到，上游收到
    /// 未声明的工具名再回一个 400。
    ///
    /// **顺序是「先整体还原、再留尾」，不能反过来**：先按长度切、只还原切出去的那半，
    /// 跨在切点上的假名照样被劈开——留多少字节都挡不住，因为切点可以落在假名内部的任意位置。
    /// 先对 `pending ‖ chunk` 整体做一次替换，完整的假名就都换掉了；剩下最多
    /// `max_fake - 1` 个字节可能是某个假名的前半截，留到下一轮与后续字节拼起来再替。
    /// 重复还原是幂等的（真名里不含假名），故留下来那段下一轮再过一遍也不会出错。
    fn feed(&self, pending: &mut Vec<u8>, chunk: &[u8]) -> Bytes {
        pending.extend_from_slice(chunk);
        let restored = self.restore(pending);
        let hold = self.max_fake.saturating_sub(1).min(restored.len());
        let cut = restored.len() - hold;
        *pending = restored[cut..].to_vec();
        Bytes::copy_from_slice(&restored[..cut])
    }

    /// 流结束时把留存的尾巴吐出来。**不能省**：SSE 以 `\n\n` 收尾，尾巴扣着不发的话
    /// 客户端的解析器会一直等那个终止符。
    fn flush(&self, pending: &mut Vec<u8>) -> Bytes {
        if pending.is_empty() {
            return Bytes::new();
        }
        Bytes::from(self.restore(&std::mem::take(pending)))
    }
}

/// 字节级子串替换。`from` 为空时原样返回（否则会死循环）。
fn replace_bytes(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() || haystack.len() < from.len() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i <= haystack.len() - from.len() {
        if &haystack[i..i + from.len()] == from {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out.extend_from_slice(&haystack[i..]);
    out
}

/// 把上游响应流包一层工具名还原。滑动窗口的状态跟着流走，流结束时 flush 尾巴。
///
/// 用 `unfold` 而不是 `map`：`map` 收不到「上游流结束」这个事件，没法把留存的尾字节吐出去。
fn restore_tool_names_stream<S>(
    inner: S,
    map: std::sync::Arc<ToolNameMap>,
) -> impl futures_util::Stream<Item = Result<Bytes, wreq::Error>>
where
    S: futures_util::Stream<Item = Result<Bytes, wreq::Error>> + Unpin,
{
    futures_util::stream::unfold(
        (inner, map, Vec::<u8>::new(), false),
        |(mut inner, map, mut pending, done)| async move {
            if done {
                return None;
            }
            match inner.next().await {
                Some(Ok(bytes)) => {
                    let out = map.feed(&mut pending, &bytes);
                    Some((Ok(out), (inner, map, pending, false)))
                }
                // 上游把流掐了：错误原样交给下游（行为与不还原时一致），本次不再吐尾巴——
                // 半截的假名还原出来也是半截，交给客户端反而更糟。
                Some(Err(e)) => Some((Err(e), (inner, map, pending, true))),
                None => {
                    let tail = map.flush(&mut pending);
                    (!tail.is_empty()).then(|| (Ok(tail), (inner, map, pending, true)))
                }
            }
        },
    )
}

/// 把 body 里**所有**缓存断点的 `ttl` 补齐成 `1h`，返回是否改动过。只在
/// [`store::ForwardFlags::cache_ttl_1h`] 开着时调用。
///
/// **为什么要走一遍全身**：[`align_system_shape`] 只重建 `system` 那两块，客户端自己标在
/// `messages`/`tools` 上的断点不在它手里。于是 0.2.50 之后出现过一种官方不产生的组合——
/// `cap/raw/00012`（真 CC 经 luban）复现：system 两个断点有 `ttl:"1h"`、消息那个没有。
/// 而官方三个断点**要么都有**（订阅模式 00009）、**要么都没有**（API-key 模式 00012），
/// 没有中间态。这与 [`ensure_beta_query`] 当初要消灭的是同一个形状：只对齐了一半，
/// 拼出个两边都不像的组合。
///
/// **客户端自己写了 `ttl` 的不动**：那是它掏钱买的时长，与 [`cache_control`] 同一口径。
///
/// 键序按官方 `type` → `ttl` → `scope` **重建**而非追加：客户端若已写了 `scope`，
/// 直接追加会得到 `{type,scope,ttl}` 这个官方不产生的排列。
fn fill_cache_ttl(v: &mut serde_json::Value) -> bool {
    let mut changed = false;
    match v {
        serde_json::Value::Object(map) => {
            if let Some(cc) = map.get_mut("cache_control").and_then(|c| c.as_object_mut())
                && !cc.contains_key("ttl")
            {
                let mut rebuilt = serde_json::Map::new();
                if let Some(t) = cc.get("type") {
                    rebuilt.insert("type".into(), t.clone());
                }
                rebuilt.insert("ttl".into(), "1h".into());
                for (k, val) in cc.iter() {
                    if k != "type" {
                        rebuilt.insert(k.clone(), val.clone());
                    }
                }
                *cc = rebuilt;
                changed = true;
            }
            for (k, val) in map.iter_mut() {
                if k != "cache_control" {
                    changed |= fill_cache_ttl(val);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for it in items.iter_mut() {
                changed |= fill_cache_ttl(it);
            }
        }
        _ => {}
    }
    changed
}

/// 构造一个 `system` 文本块，key 序与官方一致：`type` → `text` → `cache_control`。
fn text_block(text: &str, cache_control: serde_json::Value) -> serde_json::Value {
    let mut blk = serde_json::Map::new();
    blk.insert("type".into(), "text".into());
    blk.insert("text".into(), text.into());
    blk.insert("cache_control".into(), cache_control);
    serde_json::Value::Object(blk)
}

/// 不带缓存断点的 `system` 文本块（官方的 `system[0]`/`system[1]` 都是这个形态）。
fn text_block_bare(text: &str) -> serde_json::Value {
    let mut blk = serde_json::Map::new();
    blk.insert("type".into(), "text".into());
    blk.insert("text".into(), text.into());
    serde_json::Value::Object(blk)
}

/// 缓存断点的两项可选形态，各由一个开关拨。合成一个结构体而不是并排传两个 `bool`：
/// 相邻同型参数换了位置编译器不会吭声，而这两项落错地方产出的都是官方不发的组合。
#[derive(Clone, Copy)]
struct CacheShape {
    /// 标 `scope:"global"`。**只有基座那块**该带，见 [`store::ForwardFlags::cache_scope_global`]。
    global: bool,
    /// 写 `ttl:"1h"`。官方**每个断点都带**，见 [`store::ForwardFlags::cache_ttl_1h`]。
    ttl_1h: bool,
}

impl CacheShape {
    /// 非基座断点的形态：去掉 `scope`、保留 `ttl`——官方只在基座标 `scope`
    /// （`cap/raw/00006` 三个断点里仅一个有），而三个断点**都**有 `ttl`。
    fn tail(self) -> Self {
        Self { global: false, ..self }
    }
}

/// 构造 `cache_control`，key 序与官方一致：`type` → `ttl` → `scope`
/// （逐字节取自 `cap/raw/00006`：`{"type":"ephemeral","ttl":"1h","scope":"global"}`）。
///
/// `ttl:"1h"` **默认写**，对齐官方——四份订阅直连抓包的三个断点 3/3 全是 `1h`，不写就是
/// 每条请求上一处稳定差异。代价要知情：1h 的缓存**写入**单价是默认 5m 的 2 倍,故
/// [`store::ForwardFlags::cache_ttl_1h`] 可以关掉，关掉即沿用客户端自己传的时长。
/// 长会话里 1h 通常反而更省（5m 内没接上话就得按写入价重写一遍），但那取决于使用节奏，
/// 所以给了开关。客户端自己写了 `ttl` 的照发，两条路都不覆盖它。
///
/// `global` 同理由 [`store::ForwardFlags::cache_scope_global`] 拨。两项各要一个 beta 认
/// （`prompt-caching-scope` / `extended-cache-ttl`），故都还连着 `merge_beta`，
/// 见 [`rewrite_body`]。
fn cache_control(shape: CacheShape) -> serde_json::Value {
    let mut cc = serde_json::Map::new();
    cc.insert("type".into(), "ephemeral".into());
    if shape.ttl_1h {
        cc.insert("ttl".into(), "1h".into());
    }
    if shape.global {
        cc.insert("scope".into(), "global".into());
    }
    serde_json::Value::Object(cc)
}

/// 一次 429 该冷却到什么范围，见 [`rate_limit_scope`]。
#[derive(Debug, Clone, PartialEq)]
enum LimitScope {
    /// 基础额度窗口真的耗尽：该账号所有模型一起让位。
    Account,
    /// 只有这一个模型没路可走（容量限制，或它专用的超额池满了）：其余模型照常。
    Model(String),
}

impl LimitScope {
    fn account_level(&self) -> bool {
        matches!(self, Self::Account)
    }

    /// 传给 [`store::CredentialStore::mark_rate_limited`] 的模型维度。
    fn model(&self) -> Option<&str> {
        match self {
            Self::Account => None,
            Self::Model(m) => Some(m),
        }
    }

    fn label(&self) -> &str {
        match self {
            Self::Account => "account",
            Self::Model(_) => "model",
        }
    }
}

/// 判定一次 429 是「这个账号没额度了」还是「只有这一个模型没路可走」。
///
/// **规则是实测倒逼出来的**，两次真实的 fable-5 429 头长这样（形态一致）：
///
/// ```text
/// unified-status: rejected                            ← 不是 "rate_limited"
/// representative-claim: seven_day_overage_included     ← 指向 7d_oi
/// 5h:    allowed,         utilization=0.20             ← 基础窗口很空
/// 7d:    allowed,         utilization=0.70
/// 7d_oi: rejected,        utilization=1.02             ← 满掉的只有「7 天含超额」
/// overage-status: rejected（org_level_disabled）
/// retry-after: 304802
/// ```
///
/// 演化了三版，每版的教训都写在规则里：
///
/// 1. **状态词不止一个**：`rejected` 与 `rate_limited` 都算被拒（`allowed`/`allowed_warning`
///    才是放行）。第一版只认 `rate_limited`，漏判。
/// 2. **窗口名不能写死**：被拒的是 `7d_oi`（7 天含超额），第一版根本没解析它。故扫**所有**
///    `unified-<窗口>-status/utilization`，不必维护 `representative-claim` 到窗口名的映射
///    （`seven_day_overage_included` → `7d_oi` 这种对应关系纯属猜谜）。
/// 3. **超额族窗口不算账号额度**：第二版把「任一窗口被拒/打满」一律判账号级，于是上面那条
///    把整个账号冷却了 24 小时——可它满掉的只是**超额/回补池**（`7d_oi` 比基础 7d 的
///    利用率还高，说明两边记的不是同一笔账；fable 走的正是这个池子，见
///    [`config::CC_BETA_SIMULATED`] 里关于 `fallback-credit` 的注）。实测在 7d_oi 仍
///    rejected 期间，同一账号的 sonnet/opus 连通性测试照常 200——账号好好的，只有 fable
///    没路。故 `_oi`/`overage` 窗口被拒只判**模型级**，账号级只看基础窗口。
///
/// `unified-status` 是**本次请求**的判决：fable 被超额池拒掉时它同样是 `rejected`，说明
/// 不了账号整体，故只在没有任何逐窗口明细时才拿它兜底（保守判账号级）。模型级的冷却时长
/// 优先吃 `retry-after`（两次实测都给了，直指池子重置时刻），不会重蹈「30 秒放出去反复撞」
/// 的循环——那是早年时长不认 `retry-after` 的锅，不是作用域的。请求体里读不出模型名时
/// 退回账号级——没有模型可挂，宁可保守。
fn rate_limit_scope(info: &RateLimitInfo, model: Option<&str>) -> LimitScope {
    let Some(model) = model else { return LimitScope::Account };
    let rejected = |s: &str| s.contains("rate_limited") || s.contains("rejected");
    let base_gone = info.window_status.iter().any(|(w, s)| !is_overage_window(w) && rejected(s))
        || info.window_utilization.iter().any(|(w, u)| !is_overage_window(w) && *u >= 1.0);
    let no_detail = info.window_status.is_empty() && info.window_utilization.is_empty();
    let unified_gone = no_detail && info.unified_status.as_deref().is_some_and(rejected);
    if base_gone || unified_gone {
        LimitScope::Account
    } else {
        LimitScope::Model(model.to_string())
    }
}

/// 一次确认的上游 429 该怎么把这个号挪出调度池，两档分开处理：
///
/// - **账号级**（基础窗口真耗尽）：走 [`store::CredentialStore::pause_for_rate_limit`]，
///   把**调度开关关掉并落库**，同时记下到点自动恢复的时刻。落库是关键——额度耗尽动辄几小时
///   到几天，只记内存的话一次进程重启就忘了，重启后又拿这个号去撞一发 429；而且后台看不到
///   这个号为什么不干活。恢复有三条路：到点惰性自动恢复、连通性测试通过自动恢复、
///   控制台手动打开。
/// - **模型级**（容量限制/超额池满，账号本身好着）：仍走进程内的
///   [`store::CredentialStore::mark_rate_limited`]。这一档默认才 30 秒，落库既不值得、也会
///   在卡片上把一个健康账号显示成「已停用」——它的 sonnet/opus 明明还在正常服务。
fn park_rate_limited(
    store: &store::CredentialStore,
    cred: &crate::credentials::Credential,
    scope: &LimitScope,
    cooldown: std::time::Duration,
) {
    let Some(model) = scope.model() else {
        let resume_at = crate::credentials::now_secs() + cooldown.as_secs();
        let reason = format!(
            "upstream rate limit: account quota exhausted, scheduling resumes automatically in about {}",
            human_secs(cooldown)
        );
        match store.pause_for_rate_limit(cred.id, &reason, resume_at) {
            Ok(_) => tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                resume_at,
                "account-level rate limit: taken out of the pool, resumes automatically when it expires (or enable it manually / run a connectivity test from the console)"
            ),
            // 落库失败不该把这条请求也搭进去：至少退回进程内冷却，本进程内仍不会再选它。
            Err(e) => {
                tracing::error!(
                    cred_id = cred.id, cred = %cred.label,
                    error = %e,
                    "persisting the rate-limit pause failed, falling back to an in-process cooldown"
                );
                store.mark_rate_limited(cred.id, None, cooldown);
            }
        }
        return;
    };
    store.mark_rate_limited(cred.id, Some(model), cooldown);
}

/// 把秒数写成人话（`3h 12m` / `45m` / `30s`），写进 `ban_reason` 给人看。
///
/// 只保留两级、且不做四舍五入：这行字是给人快速判断「还要等多久」的，`2d 3h` 足够，
/// 精确到秒反而更难读。真要精确时刻的话，`resume_at` 是原样落库的，前端自己格式化即可。
fn human_secs(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    let (days, hours, mins) = (secs / 86400, secs % 86400 / 3600, secs % 3600 / 60);
    match (days, hours, mins) {
        (0, 0, 0) => format!("{secs}s"),
        (0, 0, m) => format!("{m}m"),
        (0, h, 0) => format!("{h}h"),
        (0, h, m) => format!("{h}h {m}m"),
        (d, 0, _) => format!("{d}d"),
        (d, h, _) => format!("{d}d {h}h"),
    }
}

/// 该窗口是否属于**超额/回补池**而非账号基础额度（实测形态：`7d_oi`、`overage`）。
///
/// 它满了只说明「这条超额通道走不通」，不代表账号额度耗尽——同一时刻别的模型照常 200，
/// 故既不判账号级（[`rate_limit_scope`]），也不拿它的 reset 当账号冷却
/// （[`RateLimitInfo::exhausted_base_reset`]）。两处必须同一口径，故抽成一个函数。
fn is_overage_window(w: &str) -> bool {
    w.ends_with("_oi") || w.contains("overage")
}

/// 上游订阅账号限流快照，从 `anthropic-ratelimit-unified-*` 响应头解析。
///
/// 5h/7d 两个窗口各有 status/reset(unix 秒)/utilization(0~1)；`representative` 指明
/// 当前起约束作用的窗口（如 `five_hour`）。`raw` 保留全部匹配头，字段变化时兜底回看。
#[derive(Default, Clone)]
struct RateLimitInfo {
    unified_status: Option<String>,
    five_h_status: Option<String>,
    five_h_reset: Option<i64>,
    five_h_utilization: Option<f64>,
    seven_d_status: Option<String>,
    seven_d_reset: Option<i64>,
    seven_d_utilization: Option<f64>,
    representative: Option<String>,
    /// `retry-after`（秒）。429 时上游一般会给，是冷却时长最直接的来源，见
    /// [`RateLimitInfo::cooldown`]。
    retry_after: Option<i64>,
    /// 不带窗口名的 `anthropic-ratelimit-unified-reset`（unix 秒）：上游给的「整体什么时候
    /// 恢复」，比按 `representative-claim` 反查窗口更直接。
    unified_reset: Option<i64>,
    /// `anthropic-ratelimit-unified-overage-in-use`：本次请求是否动用了 **usage credits**
    /// （Anthropic 官方术语，旧称 extra usage：套餐包含的用量用完后不拦你，切成按标准
    /// API 价的按量计费继续跑）。别把它叫「超额计费」——那不是官方说法。
    /// 这是「额度满了但不 429」的关键标记——基础窗口 rejected、请求却 200 成功，
    /// 烧的是按量计费的钱；把它落进快照，前端才能把这种号和真正健康的号区分开。
    overage_in_use: Option<bool>,
    /// **所有** `anthropic-ratelimit-unified-<窗口>-status` 的取值（窗口名原样保留）。
    ///
    /// 刻意不写死窗口名：实测除了 `5h`/`7d`，还有 `7d_oi`（7 天含超额），而**真正被拒的
    /// 正是它**——只解析 5h/7d 会看到「两个窗口都没满」，从而把一次账号级限流误判成模型
    /// 容量限制。窗口种类是上游说了算的，只能全收，见 [`rate_limit_scope`]。
    window_status: Vec<(String, String)>,
    /// 所有 `…-<窗口>-utilization` 的取值。同上，全收。
    window_utilization: Vec<(String, f64)>,
    /// 所有 `…-<窗口>-reset` 的取值（unix 秒，窗口名原样保留）。同上，全收——冷却要睡到
    /// **被拒的那个窗口**自己的重置时刻，而它未必是 5h/7d 中的一个。
    window_reset: Vec<(String, i64)>,
    /// 全部匹配到的限流/anthropic- 头，`k=v` 以 `, ` 连接。
    raw: String,
}

impl RateLimitInfo {
    fn from_headers(headers: &HeaderMap) -> Self {
        let mut info = RateLimitInfo::default();
        let mut pairs: Vec<String> = Vec::new();
        for (k, v) in headers.iter() {
            let name = k.as_str().to_ascii_lowercase();
            if !(name.contains("ratelimit")
                || name == "retry-after"
                || name.starts_with("anthropic-"))
            {
                continue;
            }
            let val = v.to_str().unwrap_or("<non-utf8>");
            pairs.push(format!("{name}={val}"));
            match name.as_str() {
                "anthropic-ratelimit-unified-status" => info.unified_status = Some(val.to_string()),
                "anthropic-ratelimit-unified-5h-status" => {
                    info.five_h_status = Some(val.to_string())
                }
                "anthropic-ratelimit-unified-5h-reset" => info.five_h_reset = val.parse().ok(),
                "anthropic-ratelimit-unified-5h-utilization" => {
                    info.five_h_utilization = val.parse().ok()
                }
                "anthropic-ratelimit-unified-7d-status" => {
                    info.seven_d_status = Some(val.to_string())
                }
                "anthropic-ratelimit-unified-7d-reset" => info.seven_d_reset = val.parse().ok(),
                "anthropic-ratelimit-unified-7d-utilization" => {
                    info.seven_d_utilization = val.parse().ok()
                }
                "anthropic-ratelimit-unified-representative-claim" => {
                    info.representative = Some(val.to_string())
                }
                "retry-after" => info.retry_after = val.trim().parse().ok(),
                "anthropic-ratelimit-unified-reset" => info.unified_reset = val.parse().ok(),
                "anthropic-ratelimit-unified-overage-in-use" => {
                    info.overage_in_use = Some(val.trim() == "true")
                }
                _ => {}
            }
            // 通用收集：`anthropic-ratelimit-unified-<窗口>-status|utilization`，窗口名不限。
            if let Some(rest) = name.strip_prefix("anthropic-ratelimit-unified-") {
                if let Some(win) = rest.strip_suffix("-status") {
                    info.window_status.push((win.to_string(), val.to_string()));
                } else if let Some(win) = rest.strip_suffix("-utilization")
                    && let Ok(u) = val.parse::<f64>()
                {
                    info.window_utilization.push((win.to_string(), u));
                } else if let Some(win) = rest.strip_suffix("-reset")
                    && let Ok(ts) = val.parse::<i64>()
                {
                    // 不带窗口名的 `…-unified-reset` 不会命中：那时 rest 是 `reset`，
                    // 剥不掉 `-reset` 前缀那一横，不会造出一个名字为空的假窗口。
                    info.window_reset.push((win.to_string(), ts));
                }
            }
        }
        info.raw = pairs.join(", ");
        info
    }

    /// 该凭证被上游 429 之后应冷却多久。
    ///
    /// **冷却时长一律由上游给的重置时刻算出，没有任何写死的窗口长度**——「5h 窗口」指的是
    /// 它的统计口径，不是「睡 5 小时」：账号是在自己那个窗口的 `*-reset` 时刻回血的，那才是
    /// 该醒的点。只有上游一个时间都没给时才落到默认值。
    ///
    /// **账号级**（额度真耗尽）取值优先级：
    /// 1. **被拒/打满的那个基础窗口**自己的 `*-reset`（见 [`Self::exhausted_base_reset`]）
    ///    ——判账号级正是因为它满了，它什么时候重置，账号就什么时候能用；
    /// 2. 不带窗口名的 `anthropic-ratelimit-unified-reset`——上游给的「整体什么时候恢复」，
    ///    逐窗口明细缺失时的兜底；
    /// 3. `retry-after`（秒）——连一个 reset 时刻都没给时才用它；
    /// 4. 各窗口 `*-reset` 里**最早**的那个，连哪个满了都不知道时，宁可早醒也不要多睡；
    /// 5. 都没有 → [`DEFAULT_RATE_LIMIT_COOLDOWN_SECS`]。
    ///
    /// **为什么 `retry-after` 在账号级被降到第三位**（它曾经排第一）：它是个**相对秒数**，
    /// 要重新锚回我们自己的时钟才能变成时刻，而 `*-reset` 本身就是绝对时刻。两者口径不同，
    /// 于是漂移有三处叠加——上游把剩余时间向下取整成整秒、本地时钟与上游未必一致、界面显示
    /// 又是截断到分钟（不进位）。线上实测的症状：同一张卡片一边写「12:20 重置」（读的是
    /// `5h-reset`），一边写「12:19 自动恢复」（`now + retry-after`），真实差距不到一秒，
    /// 显示出来却整整差一分钟，且那一分钟里发出去的请求必然再撞 429。
    /// 改成直接吃窗口的 `*-reset` 之后，恢复时刻与卡片上的重置时刻**是同一个数**，
    /// 不存在对不上的可能。`retry-after` 仍是没有 reset 时的兜底。
    ///
    /// **模型级不看任何 reset**：窗口都没跑满，reset 说的是「这个窗口什么时候重置」，跟
    /// 「这个模型什么时候有容量」是两码事，拿它当冷却会让一个好账号的某个模型白白闲置几小时。
    /// 那一档只认 `retry-after`，没有就用 [`DEFAULT_MODEL_COOLDOWN_SECS`]。
    ///
    /// 结果夹在 `[1s, `[`MAX_RATE_LIMIT_COOLDOWN_SECS`]`]`：**睡满上游说的那个 reset**，
    /// 到点自动回到调度池里参与正常选号，不做定时探活、也不提前放出去撞。上限经历过
    /// 6h → 24h → 7d：夹得比真实窗口短，等于每到上限就把这个号放出去白撞一次 429
    /// （上游实测给过 63 小时的 `retry-after`，7d 窗口耗尽时还会更长）。
    /// 冷却现在是硬门禁（见 [`store::CredentialStore::select_for_device`]），
    /// 睡过头的代价由「连通性测试成功自动解除」和控制台的手动解除兜底。
    fn cooldown(&self, account_level: bool) -> std::time::Duration {
        let now = crate::credentials::now_secs() as i64;
        let earliest_window_reset = self
            .window_reset_candidates()
            .into_iter()
            .filter(|reset| *reset > now)
            .min()
            .map(|reset| reset - now);
        let future_secs = |reset: i64| Some(reset - now).filter(|d| *d > 0);
        let secs = if account_level {
            // 绝对时刻优先，相对秒数兜底——两者口径不同，混用会让恢复时刻和卡片上的重置
            // 时刻差出一分钟（见上面的方法文档）。
            self.exhausted_base_reset()
                .and_then(future_secs)
                .or_else(|| self.unified_reset.and_then(future_secs))
                .or(self.retry_after)
                .or(earliest_window_reset)
                .unwrap_or(DEFAULT_RATE_LIMIT_COOLDOWN_SECS)
        } else {
            self.retry_after.unwrap_or(DEFAULT_MODEL_COOLDOWN_SECS)
        }
        .clamp(1, MAX_RATE_LIMIT_COOLDOWN_SECS);
        std::time::Duration::from_secs(secs as u64)
    }

    /// 把逐项收集的三张表（status / utilization / reset）按窗口名合并成一份结构化快照，
    /// 供落库展示（见 [`store::QuotaWindow`]）。
    ///
    /// 顺序按**首次出现**的窗口名排，即上游响应头里的顺序——前端照着渲染就是上游的原序，
    /// 不必自己定一套排法。三张表是分开收的（解析时一个头只落一处），故这里以 status 打头、
    /// 再把只出现在另外两张表里的窗口补上，避免漏掉「只报了 utilization 没报 status」的窗口。
    fn windows(&self) -> Vec<store::QuotaWindow> {
        let mut names: Vec<&str> = Vec::new();
        for name in self
            .window_status
            .iter()
            .map(|(w, _)| w.as_str())
            .chain(self.window_utilization.iter().map(|(w, _)| w.as_str()))
            .chain(self.window_reset.iter().map(|(w, _)| w.as_str()))
        {
            if !names.contains(&name) {
                names.push(name);
            }
        }
        names
            .into_iter()
            .map(|name| store::QuotaWindow {
                name: name.to_string(),
                status: self.window_status.iter().find(|(w, _)| w == name).map(|(_, s)| s.clone()),
                utilization: self
                    .window_utilization
                    .iter()
                    .find(|(w, _)| w == name)
                    .map(|(_, u)| *u),
                reset: self.window_reset.iter().find(|(w, _)| w == name).map(|(_, t)| *t),
            })
            .collect()
    }

    /// 各窗口的 `*-reset`（unix 秒），全窗口通收后再并上 5h/7d 专用字段（重复无所谓，
    /// 调用方只取 min/max）。
    fn window_reset_candidates(&self) -> Vec<i64> {
        self.window_reset
            .iter()
            .map(|(_, ts)| *ts)
            .chain([self.five_h_reset, self.seven_d_reset].into_iter().flatten())
            .collect()
    }

    /// 已被拒/已打满的**基础窗口**（排除超额族）中最晚的那个 `*-reset`——账号级冷却该睡到的
    /// 时刻，也是 [`rate_limit_scope`] 判账号级的依据本身。
    ///
    /// 取**最晚**而不是最早：5h 和 7d 同时耗尽时，5h 到点了 7d 照样拦着，早醒只是白撞一发
    /// 429 再重新睡回去。而只有一个窗口满时 max 退化成它自己，正是要的答案。
    ///
    /// 只看基础窗口，与 [`rate_limit_scope`] 用同一个 [`is_overage_window`] 口径：超额池
    /// （`7d_oi`/`overage`）满不是账号额度耗尽，它压根走不到账号级这一档。
    fn exhausted_base_reset(&self) -> Option<i64> {
        let rejected = |w: &str| {
            self.window_status.iter().any(|(name, s)| {
                name == w && (s.contains("rate_limited") || s.contains("rejected"))
            }) || self.window_utilization.iter().any(|(name, u)| name == w && *u >= 1.0)
        };
        self.window_reset
            .iter()
            .filter(|(w, _)| !is_overage_window(w) && rejected(w))
            .map(|(_, ts)| *ts)
            .max()
    }
}

/// 模型级冷却在没有 `retry-after` 时的时长。
///
/// 取 30 秒：容量限制是「这一阵挤」，不是「这个号没额度了」，躲一小会儿就该让它回来试；
/// 押太久等于把一个健康账号的这个模型白白闲置。
const DEFAULT_MODEL_COOLDOWN_SECS: i64 = 30;

/// 冷却时长的上限：7 天窗口 + 1 小时余量。
///
/// 账号的基础窗口最长就是 7d，睡满它即可；留 1 小时余量是因为 `retry-after` 是相对本次
/// 请求算的，而 reset 时刻本身还可能被上游微调。上限存在的意义只剩「挡住明显异常的头」
/// （比如 reset 落在几年后），不再是「每隔 N 小时放出去试一次」——那种试探每次都要白撞
/// 一发 429，而额度没到点是不会自己长回来的。
const MAX_RATE_LIMIT_COOLDOWN_SECS: i64 = 7 * 24 * 3600 + 3600;

/// 上游 429 但没给任何可用的等待时间时，凭证的默认冷却时长。
///
/// 取一分钟：这种情况多半是突发/并发限流（额度耗尽那种上游会明确给 reset），躲过这一阵即可；
/// 冷却太长会让一个其实还能用的号长时间闲置。
const DEFAULT_RATE_LIMIT_COOLDOWN_SECS: i64 = 60;

/// 请求头是否可转发：跳过接入 key、Host、逐跳头。
///
/// `accept-encoding` 刻意**保留转发**：官方客户端必带 `gzip, deflate, br, zstd`，剥掉它等于
/// 发出一个「自称 claude-cli 却不声明压缩支持」的请求。上游会照单压缩（连 140 字节的错误体
/// 都压，SSE 也不例外），故上游客户端开了对应的解压 feature，wreq 收到时已解码；
/// 回给客户端的是未压缩内容（见 [`is_resp_forwardable`]）。
///
/// `authorization`/`anthropic-beta`/`anthropic-version` 也**保留转发**——它们的值随后会被
/// [`build_forward_headers`] 原位覆盖。在这里剥离会让它们被追加到头列表末尾，破坏来访头序。
///
/// `connection` 只在值为 `keep-alive` 时转发：官方客户端显式发这个头（抓包 040 可见），
/// 而 hyper 认为 HTTP/1.1 隐含 keep-alive、默认不发，剥掉就是个稳定差异。其余取值
/// （`close` 会打掉连接池、`upgrade` 更不能转）照旧当逐跳头丢弃。
fn is_forwardable(name: &HeaderName, value: &HeaderValue) -> bool {
    // HeaderName 构造时即归一化为小写，无需再转。
    let n = name.as_str();
    if n == "connection" {
        return value.as_bytes().eq_ignore_ascii_case(b"keep-alive");
    }
    !matches!(
        n,
        "host"
            | "x-api-key"
            | "content-length"
            | "expect"
            | "keep-alive"
            | "proxy-authorization"
            | "proxy-connection"
            | "te"
            | "trailer"
            | "transfer-encoding"
            | "upgrade"
    )
}

/// 响应头是否可回传：跳过由框架管理的分帧类头。
///
/// `content-encoding` 保留转发，但正常情况下它**根本不会出现**——wreq 解码后会把它连同
/// `content-length` 一起摘掉，我们回给客户端的是未压缩内容。只有上游用了我们没开的编码时
/// 它才会残留，那时压缩体确实是原样透传的，这个头必须跟着走，否则客户端会把压缩字节当明文解析。
///
/// 客户端向 luban 声明了 `accept-encoding` 却收到未压缩内容，这在 HTTP 里完全合法
/// （accept-encoding 是偏好不是要求）。代价是 luban→客户端这一腿不再压缩。
fn is_resp_forwardable(name: &HeaderName) -> bool {
    let n = name.as_str().to_ascii_lowercase();
    !matches!(n.as_str(), "content-length" | "transfer-encoding" | "connection")
}

// ---------- 连通性测试 ----------

/// 一次连通性测试最多等多久。上游客户端本身没设超时（流式响应可以跑很久），但测试是人在
/// 网页上等着的：它只发一条 `max_tokens=1` 的请求，正常几百毫秒就该回来，超过这个数就是
/// 上游不通或被中间设备吞了，报出来比让页面一直转圈有用。
const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// 一次连通性测试的结果（[`crate::web`] 原样 JSON 回给前端）。
#[derive(serde::Serialize)]
pub struct ProbeReport {
    /// 上游是否 2xx。
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
    fn from_info(info: &RateLimitInfo) -> Option<Self> {
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
/// [`store::CredentialStore::mark_banned`]、刷新时发现 `refresh_token` 被作废亦然（见
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
        let refresh_http = state.http.clone();
        let refresh_cred = cred.clone();
        let refresh = tokio::spawn(async move {
            store::access_token_of(&refresh_store, &refresh_http, &refresh_cred).await
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

    // 复用「裸客户端」那份设备指纹（`device_fingerprint(None, 空头)` 恒为 `"||"`），不另造一个：
    // 指纹只用于派生伪装 device_id 与 session_id，每加一份就等于给这个账号在上游多一台设备，
    // 而测试并不需要一个自己的身份。
    let device_fp = device_fingerprint(None, &HeaderMap::new());
    let flags = store::ForwardFlags::default();
    // 直接构造 `Simulation` 而不走 `Simulation::detect`：这条请求本来就是 luban 自己发的裸
    // 请求（body 里没有那句身份声明），detect 只会在开关关掉时返回 None，那样发出去必被上游拒。
    let sim = Simulation {
        base: cc_system_base(model),
        beta: cc_beta_seed(model),
        session_id: session_id_for(cred, &device_fp),
    };
    let headers = build_forward_headers(&HeaderMap::new(), &token, flags, Some(&sim), None);
    // 出站 UA 要随日志落库（入站那份没有——测试不来自任何客户端）。在 headers 被 move 进
    // Upstream 之前取，取值规则与转发路径同一套。
    let out_ua = ua_of(&headers);
    let plog = ProbeLog {
        store: &state.store,
        cred,
        req_model: model,
        started: &started,
        out_ua: (out_ua != "-").then_some(out_ua),
    };
    let upstream = Upstream {
        state,
        method: Method::POST,
        // 这条请求整条都是照官方形态造的，URL 上那个 `?beta=true` 一并带上。
        url: ensure_beta_query(&format!("{}/v1/messages", config::UPSTREAM_BASE_URL)),
        headers,
        flags,
        billable: true,
        sim: Some(sim),
        // 走的是模拟那条路（sim 恒为 Some），会话 id 在 Simulation 里。
        bare_session: None,
        // 连通性测试保持非流式：它下面那套读法（`up.bytes()` 一把梭 + [`probe_report`] 按
        // 整段 Message 解析出 model/error_type）是照非流式响应写的，改成 SSE 就全得跟着改，
        // 而这条请求本来就不是客户端流量（`max_tokens:1` 的 ping），形态对齐的收益也不在这。
        force_stream: false,
        // 探测体不带 `tools`（见 [`probe_body`]），没有可混淆的名字。
        tool_names: None,
    };

    let body = probe_body(model);
    let sent = upstream.send(upstream.shape(&body, cred, &device_fp));
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
            let (_, content_encoding) = resp_shape(&up);
            let compressed = content_encoding.is_some();
            // 429 照真实流量打冷却。开关读库里真实配置（形态那份 flags 是恒定全开的基准，
            // 与「要不要管 429」无关）；与转发一样，重试次数配成 0 也视同关闭。
            if status == StatusCode::TOO_MANY_REQUESTS
                && state.store.forward_flags().rate_limit_retry
                && state.store.rate_limit_retry_max() > 0
            {
                let scope = rate_limit_scope(&info, Some(model));
                let cooldown = info.cooldown(scope.account_level());
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    model,
                    scope = scope.label(),
                    cooldown_secs = cooldown.as_secs(),
                    ratelimit = %info.raw,
                    "connectivity test hit an upstream 429, taking the credential out of the pool"
                );
                park_rate_limited(&state.store, cred, &scope, cooldown);
            } else if status.is_success() {
                // 对称的另一面：测试成功同样照真实判决恢复——上游此刻放行了「这个账号 +
                // 这个模型」，不必干等到点（上游的 retry-after 偏保守时，好号会被白白晾着）。
                //
                // 两档各恢复各的：账号级那档是**落库的调度开关**，测试通过即重新启用
                // （只对限流暂停的号生效，人工关掉的不该被一次测试打开）；模型级那档是进程内
                // 冷却，清账号格 + 被测模型那一格，其它模型不动——sonnet 通了证明不了 fable 通。
                match state.store.resume_if_rate_limited(cred.id) {
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
                state.store.clear_rate_limited(cred.id, Some(model));
            }
            match tokio::time::timeout_at(deadline, up.bytes()).await {
                // 已拿到真实状态码与限流头，只是 body 没有结束；保留这些信息并照样落一条日志。
                Err(_) => {
                    plog.record(status, &Bytes::new(), &info);
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
                    plog.record(status, &Bytes::new(), &info);
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
                    plog.record(status, &bytes, &info);
                    // 命中封号特征照真实流量停用：判定器与转发共用同一个（含 401 裸响应、
                    // 「端点不支持」豁免那些规则），测试报出「已封禁」的同时卡片也变红，
                    // 而不是弹窗里一个结论、列表里另一个。
                    if let Some(reason) =
                        (!compressed).then(|| detect_account_ban(status, &bytes)).flatten()
                    {
                        tracing::warn!(
                            cred_id = cred.id, cred = %cred.label,
                            status = status.as_u16(),
                            reason = %reason,
                            "connectivity test detected an account-level error, auto-disabling the credential"
                        );
                        if let Err(e) = state.store.mark_banned(cred.id, &reason) {
                            tracing::warn!(error = %e, "failed to auto-disable the credential");
                        }
                    }
                    probe_report(status, &bytes, started.elapsed().as_millis(), quota)
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
        "connectivity test"
    );
    report
}

/// 测试用的最小请求体：一条 `ping`、`max_tokens=1`。
///
/// 其余部分（官方 `system` 四块、`metadata` 身份）由 [`rewrite_body`] 在模拟路径上补齐，
/// 与真实转发用的是同一份代码——这里手抄一份官方形态，只会得到「测试通过但转发失败」。
///
/// key 序按官方的 `model → messages → … → max_tokens` 写；补出来的 `system`/`metadata`
/// 会被 [`insert_top_level`] 放到它们的官方位置上。
///
/// 不发 `stream: true`（官方客户端恒为流式）：一条 1 token 的响应用非流式读最省事，而这
/// 属于任何 API 客户端都会产生的常规形态，不是「真实客户端不产生」的那类破绽。
fn probe_body(model: &str) -> Bytes {
    let v = serde_json::json!({
        "model": model,
        "messages": [{ "role": "user", "content": "ping" }],
        "max_tokens": 1,
    });
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
struct ProbeLog<'a> {
    store: &'a std::sync::Arc<store::CredentialStore>,
    cred: &'a crate::credentials::Credential,
    req_model: &'a str,
    started: &'a std::time::Instant,
    /// 实际发出去那份 UA，由调用方从出站头取（没有该头时为 `None`）。
    out_ua: Option<String>,
}

impl ProbeLog<'_> {
    fn record(&self, status: StatusCode, bytes: &Bytes, ratelimit: &RateLimitInfo) {
        log_probe_usage(self, status, bytes, ratelimit)
    }
}

fn log_probe_usage(
    ctx: &ProbeLog<'_>,
    status: StatusCode,
    bytes: &Bytes,
    ratelimit: &RateLimitInfo,
) {
    let ProbeLog { store, cred, req_model, started, out_ua } = ctx;
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
    };
    // 与转发路径同理：这里在 async 上下文里，同步写库会占住工作线程，见 [`spawn_usage_log`]。
    spawn_usage_log((*store).clone(), rec);
}

/// 把上游响应翻译成一份结果：2xx 取回报的模型名，其余取 `error.type`/`error.message`。
/// 限流头由调用方先行解析（读 body 会把响应消费掉），成败两条路都带上。
fn probe_report(
    status: StatusCode,
    bytes: &[u8],
    latency_ms: u128,
    quota: Option<ProbeQuota>,
) -> ProbeReport {
    if status.is_success() {
        let model = serde_json::from_slice::<serde_json::Value>(bytes)
            .ok()
            .and_then(|v| Some(v.get("model")?.as_str()?.to_string()));
        return ProbeReport {
            ok: true,
            status: status.as_u16(),
            latency_ms,
            model,
            error_type: None,
            error: None,
            quota,
        };
    }
    let (error_type, message) = parse_upstream_error(bytes);
    ProbeReport {
        ok: false,
        status: status.as_u16(),
        latency_ms,
        model: None,
        error_type,
        // 上游偶尔糊一大坨（HTML 拦截页之类），截断到能看清病因即可。
        error: Some(message.chars().take(500).collect()),
        quota,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Bytes, HeaderValue, StatusCode, UsageSniffer, apply_tool_names, build_forward_headers,
        build_tool_name_map, config, detect_account_ban, ensure_billing_cch, head, header,
        is_billable_messages, is_secret_header, is_third_party_rejection, merge_beta,
        replace_json_str_field, request_digest, request_speed, store, strip_extra_fields, uuid_v4,
    };

    /// 设备身份校验与出站体改写的作用域：只认 `/v1/messages`，且 `count_tokens` 除外
    /// ——那条路径的请求体没有 `metadata` 可带，卡它等于把客户端的 token 预估打死。
    ///
    /// 入参是 `uri.path()`（不含查询串），故 `?beta=true` 不影响判定。
    #[test]
    fn count_tokens_is_not_billable() {
        assert!(is_billable_messages("/v1/messages"));
        assert!(!is_billable_messages("/v1/messages/count_tokens"));
        assert!(!is_billable_messages("/v1/models"));
    }

    /// 豁免精确匹配：任何「顶着 count_tokens 前缀但归一化后不是它」的路径都必须落回计费侧。
    /// 出站 URL 交给 wreq 时点段会按 RFC 3986 消解，`…/count_tokens/../` 到上游就成了
    /// `/v1/messages/`——前缀匹配会在这里漏掉设备校验，等于放开 `device_limit`。
    #[test]
    fn count_tokens_exemption_does_not_leak_via_prefix() {
        assert!(is_billable_messages("/v1/messages/count_tokens/.."));
        assert!(is_billable_messages("/v1/messages/count_tokens/../"));
        assert!(is_billable_messages("/v1/messages/count_tokens/"));
        assert!(is_billable_messages("/v1/messages/count_tokensX"));
    }

    /// 把原始 body 解析一次，模拟 [`super::handle`] 里那一步——生产路径全程只解析一次，
    /// 测试也走同一个形态，免得两边对「非法 JSON 怎么办」的理解漂开。
    fn parsed(b: &Bytes) -> Option<serde_json::Value> {
        serde_json::from_slice(b).ok()
    }

    /// 形态开关全开（= 默认，也是加入开关机制之前的既有行为）。
    fn all_on() -> store::ForwardFlags {
        store::ForwardFlags::default()
    }

    /// `rewrite_body` 的测试简写：固定不做流式化。绝大多数用例验的是 system/metadata 那几项
    /// 改写，流式化另有专门用例（[`forces_stream_true_and_keeps_key_order`]）。
    fn rewrite_body(
        body: &Bytes,
        cred: &crate::credentials::Credential,
        device_fp: &str,
        flags: store::ForwardFlags,
        sim: Option<&super::Simulation>,
        bare_session: Option<&str>,
    ) -> Bytes {
        super::rewrite_body(body, cred, device_fp, flags, sim, bare_session, false, None)
    }

    /// 三个模型族的 `anthropic-beta`，逐字取自 `cap/raw` 的原始报文头
    /// （claude-cli/2.1.220，每对都是同机、经 luban 与直连相隔几十秒）：
    /// `(模型, 客户端自己发的, 官方订阅客户端发的)`。
    ///
    /// haiku 那对是关键反例：它的客户端把 `claude-code-20250219` 排在**队尾**、`oauth` 在
    /// 队首，与 opus/sonnet 正好相反，任何单一顺序表都同时对不上两边。
    const BETA_PAIRS: &[(&str, &str, &str)] = &[
        (
            "opus-5 (00002/00006)",
            "claude-code-20250219,context-1m-2025-08-07,interleaved-thinking-2025-05-14,\
             redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
             mid-conversation-system-2026-04-07,effort-2025-11-24,fallback-credit-2026-06-01",
            "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,\
             interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
             advanced-tool-use-2025-11-20,effort-2025-11-24,fallback-credit-2026-06-01,\
             extended-cache-ttl-2025-04-11",
        ),
        (
            "sonnet-5 (00012/00009)",
            "claude-code-20250219,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
             effort-2025-11-24",
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
             redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
             mid-conversation-system-2026-04-07,advanced-tool-use-2025-11-20,\
             effort-2025-11-24,extended-cache-ttl-2025-04-11",
        ),
        (
            "haiku-4.5 (00026/00031)",
            "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,claude-code-20250219",
            "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,claude-code-20250219,\
             advanced-tool-use-2025-11-20,extended-cache-ttl-2025-04-11",
        ),
    ];

    /// 补齐 + 落位后应与官方客户端的 beta 串**逐字节一致**，三个模型族都要过。
    #[test]
    fn merged_beta_matches_official_order() {
        for (model, client, official) in BETA_PAIRS {
            let v = HeaderValue::from_str(client).unwrap();
            assert_eq!(
                &merge_beta(Some(v.to_str().unwrap())),
                official,
                "{model} 的 beta 串没对齐"
            );
        }
    }

    /// 客户端自有的那串**一字不动**：这是三对抓包里唯一稳定的不变量，重排它就等于自造判据。
    #[test]
    fn merged_beta_preserves_client_order() {
        for (model, client, _) in BETA_PAIRS {
            let v = HeaderValue::from_str(client).unwrap();
            let out = merge_beta(Some(v.to_str().unwrap()));
            let kept: Vec<&str> =
                out.split(',').filter(|b| client.split(',').any(|c| c.trim() == *b)).collect();
            let sent: Vec<&str> = client.split(',').map(str::trim).collect();
            assert_eq!(kept, sent, "{model} 的客户端自有串被重排了: {out}");
        }
    }

    /// 未知 beta 不被丢弃，也不被挪位——它在客户端串里什么位置就还在什么位置。
    #[test]
    fn merged_beta_keeps_unknown_betas_in_place() {
        let (_, client, official) = BETA_PAIRS[1];
        let v = HeaderValue::from_str(&format!("{client},some-future-beta-2027-01-01")).unwrap();
        let out = merge_beta(Some(v.to_str().unwrap()));
        // 客户端把它放在自有串末尾，官方串里它就该在 effort 之后、extended-cache-ttl 之前。
        assert_eq!(
            out,
            official.replace(
                ",extended-cache-ttl-2025-04-11",
                ",some-future-beta-2027-01-01,extended-cache-ttl-2025-04-11"
            )
        );
    }

    /// 无来访 beta 是退化情形（真实客户端必带），仍要给出确定输出：四个注入项按落位规则排。
    #[test]
    fn merged_beta_from_empty_is_deterministic() {
        assert_eq!(
            merge_beta(None),
            "oauth-2025-04-20,advanced-tool-use-2025-11-20,prompt-caching-scope-2026-01-05,\
             extended-cache-ttl-2025-04-11"
        );
    }

    /// 抓包 040 里的真实 account_uuid。
    const ACCOUNT_UUID: &str = "27aa7c53-0d20-42d2-806a-60c710529405";

    /// 来访客户端的头（API-key 模式的 CC，取自抓包 041）。构造成 `HeaderMap` 时保持插入序，
    /// 与 axum 从线上解析出来的顺序一致。
    fn incoming_headers() -> super::HeaderMap {
        let mut h = super::HeaderMap::new();
        for (k, v) in [
            ("accept", "application/json"),
            ("accept-encoding", "gzip, deflate, br, zstd"),
            ("authorization", "Bearer luban-CLIENT-KEY"),
            ("connection", "keep-alive"),
            ("content-type", "application/json"),
            ("proxy-connection", "Keep-Alive"),
            ("x-claude-code-session-id", "6eb83bfc-fdf8-4c43-ba4a-6ff95c60a0de"),
            ("x-stainless-arch", "arm64"),
            ("x-stainless-os", "MacOS"),
            ("anthropic-beta", "claude-code-20250219,effort-2025-11-24"),
            ("anthropic-dangerous-direct-browser-access", "true"),
            ("anthropic-version", "2023-06-01"),
            ("x-app", "cli"),
        ] {
            h.insert(super::HeaderName::from_static(k), HeaderValue::from_static(v));
        }
        h
    }

    /// 按顺序取出 `HeaderMap` 里的头名。
    fn names(h: &super::HeaderMap) -> Vec<String> {
        h.iter().map(|(k, _)| k.as_str().to_string()).collect()
    }

    /// 需要 luban 改写取值的头必须**留在来访客户端给它们的位置上**。
    ///
    /// 剥离后再 `insert` 会把它们追加到队尾，得到官方客户端不会产生的头序——和 `merge_beta`
    /// 处理的是同一类问题（那个管值内顺序，这个管头之间的顺序）。
    #[test]
    fn forward_headers_keep_client_order() {
        let out =
            build_forward_headers(&incoming_headers(), "sk-ant-oat01-REAL", all_on(), None, None);

        assert_eq!(
            names(&out),
            vec![
                "accept",
                "accept-encoding",
                "authorization", // 原位，值被换成 OAuth token
                "connection",    // keep-alive 保留转发
                "content-type",
                // proxy-connection 被剥离
                "x-claude-code-session-id",
                "x-stainless-arch",
                "x-stainless-os",
                "anthropic-beta", // 原位，值被合并重排
                "anthropic-dangerous-direct-browser-access",
                "anthropic-version", // 原位
                "x-app",
                "x-client-request-id", // 客户端没带，无原位可循，追加末尾
            ],
            "转发头序被打乱"
        );

        // 值确实被覆盖了，不是原样透传。
        assert_eq!(out["authorization"], "Bearer sk-ant-oat01-REAL");
        assert!(
            out["anthropic-beta"].to_str().unwrap().contains(config::OAUTH_BETA_HEADER),
            "anthropic-beta 未合并 oauth"
        );
    }

    /// 形态开关全关 = 只注入鉴权，其余头逐项原样：beta 不重排、不塞 oauth，
    /// 也不补任何客户端没带的头。实测上游只强制 `Authorization`，故这条路径必须真能走通。
    #[test]
    fn all_flags_off_only_injects_auth() {
        let flags = store::ForwardFlags {
            spoof_identity: false,
            spoof_device_id: false,
            billing_cch: false,
            fill_client_headers: false,
            merge_beta: false,
            system_shape: false,
            orig_header_case: false,
            thinking_signature_retry: false,
            simulate_cc: false,
            fill_metadata: false,
            rate_limit_retry: false,
            cache_scope_global: false,
            cache_ttl_1h: false,
            nonstream_as_sse: false,
            strip_extra_fields: false,
            tool_name_mimic: false,
        };
        let out =
            build_forward_headers(&incoming_headers(), "sk-ant-oat01-REAL", flags, None, None);

        // 头序与来访一致，且末尾不再追加 x-client-request-id。
        assert_eq!(
            names(&out),
            vec![
                "accept",
                "accept-encoding",
                "authorization",
                "connection",
                "content-type",
                "x-claude-code-session-id",
                "x-stainless-arch",
                "x-stainless-os",
                "anthropic-beta",
                "anthropic-dangerous-direct-browser-access",
                "anthropic-version",
                "x-app",
            ],
            "全关时不应补头"
        );
        // 唯一必需的改动仍然生效。
        assert_eq!(out["authorization"], "Bearer sk-ant-oat01-REAL");
        // beta 原样转发：既不重排也不塞 oauth。
        assert_eq!(out["anthropic-beta"], "claude-code-20250219,effort-2025-11-24");
        assert!(
            !out["anthropic-beta"].to_str().unwrap().contains(config::OAUTH_BETA_HEADER),
            "merge_beta 关闭后不应塞 oauth beta"
        );
    }

    /// 客户端什么都没带时，`fill_client_headers` 决定补不补——关掉就真的一个都不补，
    /// 只留鉴权（`accept-encoding` 由上游 client 的 default_headers 兜底，不在这一层）。
    #[test]
    fn fill_off_adds_nothing_for_bare_client() {
        let mut bare = super::HeaderMap::new();
        bare.insert(
            super::HeaderName::from_static("content-type"),
            HeaderValue::from_static("application/json"),
        );

        let on = build_forward_headers(&bare, "tok", all_on(), None, None);
        assert_eq!(
            names(&on),
            vec![
                "content-type",
                "anthropic-version",
                "anthropic-beta",
                "accept-encoding",
                "x-client-request-id",
                "authorization"
            ],
            "开启时应补齐这四个头"
        );

        let flags = store::ForwardFlags { fill_client_headers: false, ..all_on() };
        let off = build_forward_headers(&bare, "tok", flags, None, None);
        assert_eq!(
            names(&off),
            vec!["content-type", "anthropic-beta", "authorization"],
            "关闭后只该有客户端原有的头 + beta + 鉴权"
        );
        assert!(!off.contains_key("x-client-request-id"));
        assert!(!off.contains_key(header::ACCEPT_ENCODING));
        assert!(!off.contains_key("anthropic-version"));
    }

    /// 覆盖失败时必须把 `authorization` 摘掉，不能把来访者的接入 key 漏给上游。
    /// （这条只有在「照常转发再覆盖」的写法下才存在，剥离式写法天然没有这个洞。）
    #[test]
    fn never_leaks_client_key_upstream() {
        // token 里塞进换行——`HeaderValue::from_str` 会拒绝，走到移除分支。
        let out = build_forward_headers(&incoming_headers(), "bad\ntoken", all_on(), None, None);
        assert!(!out.contains_key("authorization"), "构造失败时应移除该头: {out:?}");
        // 任何路径下都不得把接入 key 转发出去。
        for (_, v) in out.iter() {
            assert!(
                !v.to_str().unwrap_or("").contains("luban-CLIENT-KEY"),
                "接入 key 泄漏到上游: {out:?}"
            );
        }
        assert!(!out.contains_key("x-api-key"), "x-api-key 不应转发");
    }

    /// 起一个本地 HTTP 服务，用给定的响应字节应答，并把收到的请求头原样返回。
    fn serve_once(response: Vec<u8>) -> (std::net::SocketAddr, std::thread::JoinHandle<String>) {
        use std::io::{BufRead, BufReader, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut r = BufReader::new(&stream);
            let mut raw = String::new();
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let end = line == "\r\n";
                raw.push_str(&line);
                if end {
                    break;
                }
            }
            (&stream).write_all(&response).unwrap();
            raw
        });
        (addr, h)
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    /// 上游客户端必须**透明解压**，否则用量嗅探拿到的是压缩字节、什么都解析不出来。
    ///
    /// 这正是线上花费统计消失的成因：v0.2.12 恢复转发 `accept-encoding` 让上游开始压缩响应，
    /// 但 reqwest 没开解压 feature，于是 `UsageSniffer` 被整个跳过——model、token、cost 全空。
    /// 本测试同时盯住两件事：解压 feature 在不在，以及请求侧声明的取值是否仍是官方那个。
    #[tokio::test]
    async fn upstream_client_decodes_gzip_and_keeps_official_accept_encoding() {
        // 一段真实形态的 SSE，压成 gzip 后由服务端返回。
        const SSE: &str = "event: message_start\n\
            data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-sonnet-5\",\
            \"usage\":{\"input_tokens\":123,\"cache_read_input_tokens\":456}}}\n\n";
        let body = gzip(SSE.as_bytes());
        let mut resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\n\
             content-encoding: gzip\r\ncontent-length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        resp.extend_from_slice(&body);

        let (addr, server) = serve_once(resp);
        let up = crate::web::upstream_client()
            .unwrap()
            .post(format!("http://{addr}/v1/messages"))
            .send()
            .await
            .unwrap();

        // wreq 解码后会把 content-encoding / content-length 一并摘掉。
        assert!(
            up.headers().get(header::CONTENT_ENCODING).is_none(),
            "解码后不该再有 content-encoding：{:?}",
            up.headers()
        );
        let bytes = up.bytes().await.unwrap();
        assert_eq!(&bytes[..], SSE.as_bytes(), "响应体应已是明文");

        // 明文喂给嗅探器就能拿到 model 与用量——这是花费统计的全部输入。
        let mut sniffer = UsageSniffer::new(true, false);
        sniffer.feed(&bytes);
        sniffer.finish();
        assert_eq!(sniffer.model.as_deref(), Some("claude-sonnet-5"));
        assert_eq!(sniffer.input_tokens, Some(123));
        assert_eq!(sniffer.cache_read_tokens, Some(456));
        assert!(sniffer.has_usage());

        // 请求侧仍是官方取值，不是解压中间件那个 `zstd,gzip,deflate,br`。
        // 这条请求没经过 build_forward_headers，走的正是 default_headers 兜底那条路
        // ——和 luban 自身的刷新/profile 请求同一条。
        let raw = server.join().unwrap().to_ascii_lowercase();
        assert!(
            raw.contains(&format!("accept-encoding: {}\r\n", config::CC_ACCEPT_ENCODING)),
            "accept-encoding 应为官方取值:\n{raw}"
        );
    }

    /// 上游用了我们没开的编码时，只能跳过嗅探——但不得崩、不得把压缩字节当明文解析。
    #[test]
    fn unknown_encoding_is_skipped_not_misparsed() {
        let mut s = UsageSniffer::new(true, true);
        s.feed(&gzip(b"data: {\"usage\":{\"input_tokens\":999}}\n"));
        s.finish();
        assert!(!s.has_usage(), "解不开的响应体不应被当明文解析出用量");
        assert_eq!(s.model, None);
    }

    /// 起个裸 TCP「上游」，用 [`crate::web::upstream_client`] 那份**真配置**打一发，
    /// 返回请求的原始字节（不做大小写归一化——这里正是要看拼写的）。
    async fn capture_wire(orig_case: bool) -> String {
        use std::io::{BufRead, BufReader, Write};

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut r = BufReader::new(&stream);
            let mut raw = String::new();
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap() == 0 {
                    break;
                }
                let end = line == "\r\n";
                raw.push_str(&line);
                if end {
                    break;
                }
            }
            (&stream).write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").unwrap();
            raw
        });

        let req = crate::web::upstream_client()
            .unwrap()
            .post(format!("http://{addr}/v1/messages?beta=true"))
            .headers(build_forward_headers(
                &incoming_headers(),
                "sk-ant-oat01-REAL",
                all_on(),
                None,
                None,
            ))
            .body(r#"{"model":"claude-sonnet-5"}"#);
        let req = if orig_case { req.orig_headers(super::orig_header_case()) } else { req };
        let _ = req.send().await;

        server.join().unwrap()
    }

    /// 取出线上的头名，保留原始拼写与顺序。
    fn wire_names(raw: &str) -> Vec<&str> {
        raw.lines()
            .skip(1) // 请求行
            .filter(|l| !l.is_empty())
            .map(|l| l.split(':').next().unwrap())
            .collect()
    }

    /// 线上字节的看门狗：头名的**拼写与顺序**都要与官方客户端一致
    /// （基准是 `cap/raw/00006` 的原始报文头，claude-cli/2.1.220 直连）。
    ///
    /// 这条同时钉住三件事，任一退化都会失败：
    /// 1. 分裂大小写——标准头首字母大写、`anthropic-*`/`x-app`/`x-client-request-id` 全小写、
    ///    `X-Stainless-OS` 的 `OS` 全大写（机械 title-case 会写成 `X-Stainless-Os`）。
    /// 2. `User-Agent` 落在 `Content-Type` 与 `X-Claude-Code-Session-Id` 之间，而
    ///    `Connection`/`Host`/`Accept-Encoding`/`Content-Length` 是队尾四个——这是官方线序，
    ///    不是字母序（曾照 `cap/040.json` 的字母序排过，那是抓包工具重排的产物）。
    /// 3. 显式的 `Connection: keep-alive` 确实发出（客户端库默认认为 HTTP/1.1 隐含、不发）。
    #[tokio::test]
    async fn wire_bytes_match_official_header_form() {
        let raw = capture_wire(true).await;
        assert_eq!(
            wire_names(&raw),
            &[
                "Accept",
                "Authorization",
                "Content-Type",
                "User-Agent", // 来访没带，由 upstream_client 兜底，靠 CC_HEADER_ORDER 归位
                "X-Claude-Code-Session-Id",
                "X-Stainless-Arch",
                "X-Stainless-OS",
                "anthropic-beta",
                "anthropic-dangerous-direct-browser-access",
                "anthropic-version",
                "x-app",
                "x-client-request-id",
                // 队尾四个：客户端库自己追加的那些，官方也在这个位置。
                "Connection",
                "Host",
                "Accept-Encoding",
                "Content-Length",
            ],
            "线上头名的拼写或顺序与官方形态不符:\n{raw}"
        );
        assert!(raw.contains("Connection: keep-alive"), "显式的 Connection 头被吞掉了:\n{raw}");
    }

    /// `orig_header_case` 关掉后的形态。**注意它并不等于换 wreq 之前那份**：
    ///
    /// - 头名退回全小写（这点与 reqwest 时代一致）；
    /// - 但 `default_headers` 里的 `user-agent`/`accept-encoding` 被**前置到队首**，
    ///   来访客户端的头序因此被打散（reqwest 是把它们并进原位/队尾的）；
    /// - `host`/`content-length` 仍在队尾。
    ///
    /// 也就是说这条 off 路径比开着**更不像**官方客户端，它的用途只是出问题时能二分
    /// （「是 OrigHeaderMap 引入的问题，还是别处」），不是一个可用的形态选择。
    #[tokio::test]
    async fn wire_bytes_fall_back_to_lowercase_when_off() {
        let raw = capture_wire(false).await;
        let wire = wire_names(&raw);

        let tail = wire.len() - 2;
        assert_eq!(
            &wire[..tail],
            &[
                // default_headers 被前置，不在来访客户端给它们的位置上
                "user-agent",
                "accept-encoding",
                // 以下按来访头序
                "accept",
                "authorization",
                "connection",
                "content-type",
                "x-claude-code-session-id",
                "x-stainless-arch",
                "x-stainless-os",
                "anthropic-beta",
                "anthropic-dangerous-direct-browser-access",
                "anthropic-version",
                "x-app",
                "x-client-request-id",
            ],
            "关掉开关后的形态与实测不符:\n{raw}"
        );
        // 位置不可控，只断言这两个确实在末尾。
        let mut appended = wire[tail..].to_vec();
        appended.sort_unstable();
        assert_eq!(appended, ["content-length", "host"], "\n{raw}");
    }

    fn test_cred() -> crate::credentials::Credential {
        crate::credentials::Credential {
            id: 1,
            label: "t".into(),
            tier: None,
            org_type: None,
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: u64::MAX,
            priority: 0,
            disabled: false,
            device_limit: 0,
            ban_reason: None,
            account_uuid: Some(ACCOUNT_UUID.into()),
            resume_at: None,
            created_at: 0,
            updated_at: 0,
        }
    }

    /// API-key 模式客户端的三块 `system`，形状取自 `cap/raw/00002`（内容缩短）：
    /// billing header 无断点、身份句带 5m 断点、合并块 = 基座 ‖ `\n\n` ‖ 锚点开头的其余部分。
    /// 尾部那条 role=system 的消息也带一个 5m 断点，和真实客户端一样。
    const API_SHAPE_BODY: &str = concat!(
        r#"{"model":"claude-opus-5","messages":[{"role":"system","content":[{"type":"text","#,
        r#""text":"deferred tools","cache_control":{"type":"ephemeral"}}]}],"#,
        r#""system":[{"type":"text","text":"x-anthropic-billing-header: cc_entrypoint=cli;"},"#,
        r#"{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude.","#,
        r#""cache_control":{"type":"ephemeral"}},"#,
        r#"{"type":"text","text":"\nBASE — 基座\n\nWrite code that reads like the surrounding "#,
        r#"code: match its comment density, naming, and idiom.\n\nREST","#,
        r#""cache_control":{"type":"ephemeral"}}],"#,
        r#""metadata":{"user_id":"{\"device_id\":\"dddd\",\"account_uuid\":\"\",\"session_id\":\"ssss\"}"}}"#
    );

    /// 三块改写成官方的四块，且逐字段与 `cap/raw/00006` 的形态一致：
    /// 身份句不再带断点、基座 `{type,ttl:1h,scope:global}`、其余 `{type,ttl:1h}`，
    /// 消息里的断点也补上 `ttl`。切开处那个 `\n\n` 两边都不保留。
    #[test]
    fn aligns_system_to_official_four_blocks() {
        let out =
            rewrite_body(&Bytes::from(API_SHAPE_BODY), &test_cred(), "fp", all_on(), None, None);
        let s = String::from_utf8(out.to_vec()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let sys = v["system"].as_array().unwrap();

        assert_eq!(sys.len(), 4, "应拆成四块: {s}");
        assert!(sys[0].get("cache_control").is_none(), "billing header 不该有断点: {s}");
        assert!(sys[1].get("cache_control").is_none(), "身份句上的断点应去掉: {s}");
        assert_eq!(sys[2]["text"], serde_json::json!("\nBASE — 基座"), "基座切错: {s}");
        assert!(
            sys[3]["text"].as_str().unwrap().starts_with("Write code that reads like"),
            "其余部分应从锚点开始: {s}"
        );
        assert!(sys[3]["text"].as_str().unwrap().ends_with("\n\nREST"), "其余部分被截断: {s}");

        // 键序也要对：type → text → cache_control，cache_control 内 type → ttl → scope
        // （逐字节取自 `cap/raw/00006`）。
        assert!(
            s.contains(r#""cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}"#),
            "基座的 cache_control 形态不对: {s}"
        );
        // 官方三个断点**都**带 ttl，只有基座带 scope——包括来访自己标在消息上的那个：
        // 只补 system 那两个会得到「两个有、一个没有」这种官方不产生的组合，见
        // [`super::fill_cache_ttl`]。
        assert_eq!(
            s.matches(r#""cache_control":{"type":"ephemeral","ttl":"1h"}"#).count(),
            2,
            "system 末块与消息断点都该带 ttl、不带 scope: {s}"
        );
        assert!(
            !s.contains(r#""cache_control":{"type":"ephemeral"}"#),
            "不该再有裸 ephemeral（半对齐）: {s}"
        );

        // 关掉 `cache_ttl_1h` 即回到「沿用客户端时长」：一个 ttl 都不写。
        let no_ttl = store::ForwardFlags { cache_ttl_1h: false, ..all_on() };
        let out =
            rewrite_body(&Bytes::from(API_SHAPE_BODY), &test_cred(), "fp", no_ttl, None, None);
        let s = String::from_utf8(out.to_vec()).unwrap();
        assert!(!s.contains(r#""ttl""#), "关掉后不该替客户端写 ttl: {s}");
        assert!(s.contains(r#""cache_control":{"type":"ephemeral","scope":"global"}"#), "{s}");
    }

    /// 一份 body 里可能**同时**含多条锚点，此时必须切在最早的那个上。
    ///
    /// 实例是 fable-5（`cap/raw/00035` 直连 ↔ `00037` 经 luban）：它自己的锚点
    /// `# Communicating with the user` 在合并块偏移 1212，而 opus 那句
    /// `Write code that reads like…` 也在正文里、偏移 3284。按表序先到先得会切在 3282，
    /// 基座凭空多出 2072 字节；取最早命中才得到官方那 1210B 的基座。
    #[test]
    fn splits_at_earliest_anchor_when_several_match() {
        let raw = Bytes::from(API_SHAPE_BODY.replace(
            r#"\nBASE — 基座\n\nWrite code that reads like the surrounding code: match its comment density, naming, and idiom.\n\nREST"#,
            r#"\nBASE — 基座\n\n# Communicating with the user\n\nWrite code that reads like the surrounding code: match its comment density, naming, and idiom.\n\nREST"#,
        ));
        let out = rewrite_body(&raw, &test_cred(), "fp", all_on(), None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();

        assert_eq!(sys.len(), 4, "应拆成四块: {v}");
        assert_eq!(sys[2]["text"], serde_json::json!("\nBASE — 基座"), "该切在最早的锚点上: {v}");
        assert!(
            sys[3]["text"].as_str().unwrap().starts_with("# Communicating with the user"),
            "其余部分应从最早那个锚点开始: {v}"
        );
    }

    /// 锚点是**按模型族**的：sonnet-5 的基座后面跟的不是 opus 那句，而是 `# Text output …`
    /// （`cap/raw/00009` 直连 10676B 基座 ↔ `00012` 经 luban 合并块偏移 10678）。
    /// haiku-4.5 与 sonnet-5 共用基座，命中的也是这一条。
    #[test]
    fn aligns_sonnet_shape_by_its_own_anchor() {
        let raw = Bytes::from(API_SHAPE_BODY.replace(
            "Write code that reads like the surrounding code: match its comment density, naming, and idiom.",
            "# Text output (does not apply to tool calls)",
        ));
        let out = rewrite_body(&raw, &test_cred(), "fp", all_on(), None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();

        assert_eq!(sys.len(), 4, "sonnet 锚点应能切块: {v}");
        assert_eq!(sys[2]["text"], serde_json::json!("\nBASE — 基座"), "基座切错: {v}");
        assert!(
            sys[3]["text"].as_str().unwrap().starts_with("# Text output"),
            "其余部分应从 sonnet 锚点开始: {v}"
        );
    }

    /// 锚点匹配不到（未知模型族/新版本改了措辞）时**不动结构**，退回三块原样转发——
    /// 宁可不拆，也不切在错误的位置上。其余两项改写照常。
    #[test]
    fn leaves_system_alone_when_anchor_missing() {
        let raw = Bytes::from(API_SHAPE_BODY.replace("Write code that reads like", "改了措辞的"));
        let out = rewrite_body(&raw, &test_cred(), "fp", all_on(), None, None);
        let s = String::from_utf8(out.to_vec()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();

        assert_eq!(v["system"].as_array().unwrap().len(), 3, "不该拆块: {s}");
        assert!(!s.contains("\"ttl\""), "不拆块时不应注入 ttl: {s}");
        assert!(!s.contains("\"scope\""), "不拆块时不应标 scope: {s}");
        assert!(s.contains("cch=00000"), "其余改写仍应生效: {s}");
    }

    /// 客户端本来就是订阅形态（四块）时不动 `system`——它已经是目标形态了。
    #[test]
    fn leaves_official_four_block_shape_alone() {
        let raw = Bytes::from(
            r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_entrypoint=cli; cch=0848d;"},
                          {"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."},
                          {"type":"text","text":"base","cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}},
                          {"type":"text","text":"Write code that reads like the surrounding code: match its comment density, naming, and idiom.","cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#,
        );
        let out = rewrite_body(&raw, &test_cred(), "fp", all_on(), None, None);
        assert_eq!(out, raw, "四块形态应原样返回");
    }

    /// 一份 body JSON 文本里 `system` 的块数——拆没拆块看这个，不要去数 `cache_control`：
    /// 拆块会同时**去掉**身份句上那个多余断点，总数不变（3 → 3），数不出差别。
    fn sys_len(body: &str) -> usize {
        serde_json::from_str::<serde_json::Value>(body).unwrap()["system"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0)
    }

    /// 三项 body 改写全关 = **逐字节原样透传**：不重新序列化，故连缩进、换行、转义写法
    /// 这些 serde 会归一化掉的细节都保持不变（重新序列化本身就是个形态 tell）。
    #[test]
    fn body_flags_off_passes_through_byte_for_byte() {
        // 刻意带上多余空白与换行：一旦走了 serde 往返，这些都会被抹平。
        let raw = Bytes::from(format!(" {}\n", API_SHAPE_BODY));
        let flags = store::ForwardFlags {
            spoof_identity: false,
            spoof_device_id: false,
            billing_cch: false,
            fill_client_headers: false,
            merge_beta: false,
            system_shape: false,
            orig_header_case: false,
            thinking_signature_retry: false,
            simulate_cc: false,
            fill_metadata: false,
            rate_limit_retry: false,
            cache_scope_global: false,
            cache_ttl_1h: false,
            nonstream_as_sse: false,
            strip_extra_fields: false,
            tool_name_mimic: false,
        };
        let out = rewrite_body(&raw, &test_cred(), "fp", flags, None, None);
        assert_eq!(out, raw, "全关时必须原样返回");

        // 逐项开一个，就只有那一项生效，其余仍不动。
        let only_cch = store::ForwardFlags { billing_cch: true, ..flags };
        let s = String::from_utf8(
            rewrite_body(&raw, &test_cred(), "fp", only_cch, None, None).to_vec(),
        )
        .unwrap();
        assert!(s.contains("cch=00000"), "只开 cch 时应补 cch: {s}");
        assert_eq!(sys_len(&s), 3, "system_shape 关着不应拆块: {s}");
        assert!(s.contains(r#"\"account_uuid\":\"\""#), "spoof 关着应保留空 uuid: {s}");

        // 拆块只需要 system_shape：它标的是裸 `{"type":"ephemeral"}`，GA 能力，不吃任何 beta。
        // cache_scope_global 开着但 merge_beta 关着：scope 仍不该出现。
        let shape_only =
            store::ForwardFlags { system_shape: true, cache_scope_global: true, ..flags };
        let s = String::from_utf8(
            rewrite_body(&raw, &test_cred(), "fp", shape_only, None, None).to_vec(),
        )
        .unwrap();
        assert_eq!(sys_len(&s), 4, "只开 system_shape 也该拆成四块: {s}");
        assert!(!s.contains(r#""scope""#), "scope 要 merge_beta 补的 beta 认，此时不该出现: {s}");

        // `scope:"global"` 才连着 merge_beta（prompt-caching-scope beta 由它补）。
        let with_beta = store::ForwardFlags { merge_beta: true, ..shape_only };
        let s = String::from_utf8(
            rewrite_body(&raw, &test_cred(), "fp", with_beta, None, None).to_vec(),
        )
        .unwrap();
        assert!(s.contains(r#""scope":"global""#), "两个开关都开时才标 global: {s}");
        assert!(!s.contains("cch="), "billing_cch 关着不应补 cch: {s}");

        // 单独关掉 cache_scope_global：照样拆块，只是不标 global。
        let no_scope = store::ForwardFlags { cache_scope_global: false, ..with_beta };
        let s = String::from_utf8(
            rewrite_body(&raw, &test_cred(), "fp", no_scope, None, None).to_vec(),
        )
        .unwrap();
        assert_eq!(sys_len(&s), 4, "关 scope 不影响拆块: {s}");
        assert!(!s.contains(r#""scope""#), "关掉后不该标 global: {s}");
    }

    /// 改写后 body 的 key 顺序必须与入站逐字节一致，只允许新增字段追加在末尾。
    ///
    /// serde_json 默认 `Map = BTreeMap`，会把整个 body（含嵌套对象）的 key 按字母序重排，
    /// 得到官方客户端不会产生的排列。靠 `preserve_order` feature 兜住，本测试是它的看门狗：
    /// 一旦该 feature 被摘掉，这里立刻失败。
    #[test]
    fn preserves_key_order() {
        // 客户端的真实字段次序，取自 cap/raw/00002 的原始报文体：顶层是
        // model→messages→system→tools→metadata→max_tokens→…→stream，system 块是 type→text，
        // cache_control 是 type→ttl→scope（luban 自己写的那份没有 ttl），metadata.user_id 内层是
        // device_id→account_uuid→session_id。字母序全都不是这样。
        //
        // （cap/*.json 里看到的字母序是抓包工具重新序列化的产物，不是线上的样子。）
        let raw = concat!(
            r#"{"model":"claude-opus-5","messages":[],"#,
            r#""system":[{"type":"text","text":"x-anthropic-billing-header: cc_entrypoint=cli;"},"#,
            r#"{"type":"text","text":"ident","cache_control":{"type":"ephemeral"}},"#,
            r#"{"type":"text","text":"base\n\nWrite code that reads like the surrounding code: "#,
            r#"match its comment density, naming, and idiom.","cache_control":{"type":"ephemeral"}}],"#,
            r#""tools":[],"#,
            r#""metadata":{"user_id":"{\"device_id\":\"dddd\",\"account_uuid\":\"\",\"session_id\":\"ssss\"}"},"#,
            r#""max_tokens":64000,"stream":true}"#
        );
        let out = rewrite_body(&Bytes::from(raw), &test_cred(), "fp", all_on(), None, None);
        let s = String::from_utf8(out.to_vec()).unwrap();

        // 三项改写都生效了（否则会走 body.clone() 早退，测试空过）。
        assert!(s.contains("cch=00000"), "应补 cch: {s}");
        assert!(s.contains(r#""scope":"global""#), "应对齐 system 形态: {s}");
        assert!(s.contains(&format!(r#"\"account_uuid\":\"{}\""#, ACCOUNT_UUID)), "应填 uuid: {s}");

        // 顶层顺序不变，未被字母序重排（重排后 max_tokens/messages 会跑到 model 前）。
        let mut at = 0;
        for k in ["model", "messages", "system", "tools", "metadata", "max_tokens", "stream"] {
            let needle = format!("\"{k}\":");
            let pos = s[at..].find(&needle).unwrap_or_else(|| panic!("顶层 key {k} 顺序错乱: {s}"));
            at += pos + needle.len();
        }

        // 嵌套对象同样不重排：system 块是 type→text（字母序会变成 text→type），
        // 拆块后新建的两块也按这个键序写回，cache_control 内是 type→ttl→scope
        // （字母序会变成 scope→ttl→type）。
        assert!(s.contains(r#"{"type":"text","text":"base""#), "system 块 key 被重排: {s}");
        assert!(
            s.contains(r#""cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}"#),
            "cache_control key 被重排: {s}"
        );

        // 内层 user_id 仍走定点替换，device_id→account_uuid→session_id 原序。
        assert!(
            s.contains(r#"\"device_id\":\""#)
                && s.find(r#"\"device_id\":\""#) < s.find(r#"\"account_uuid\":\""#),
            "内层 user_id key 被重排: {s}"
        );
    }

    fn body_with_system0(text: &str) -> serde_json::Value {
        serde_json::json!({"system": [{"type": "text", "text": text}]})
    }

    /// 补出的 billing header 与订阅模式的真实形态一致（抓包 040 的 `; cch=…;` 形态）。
    #[test]
    fn adds_cch_in_official_shape() {
        let mut v = body_with_system0(
            "x-anthropic-billing-header: cc_version=2.1.218.0b9; cc_entrypoint=cli;",
        );
        assert!(ensure_billing_cch(&mut v));
        assert_eq!(
            v["system"][0]["text"],
            serde_json::json!(
                "x-anthropic-billing-header: cc_version=2.1.218.0b9; cc_entrypoint=cli; cch=00000;"
            )
        );
    }

    /// 已带 cch（订阅模式客户端）不重复追加；非 billing 块不动。
    #[test]
    fn cch_is_idempotent_and_scoped() {
        let mut has = body_with_system0(
            "x-anthropic-billing-header: cc_version=2.1.218.2d7; cc_entrypoint=cli; cch=0848d;",
        );
        assert!(!ensure_billing_cch(&mut has));

        let mut other =
            body_with_system0("You are Claude Code, Anthropic's official CLI for Claude.");
        assert!(!ensure_billing_cch(&mut other));

        let mut empty = serde_json::json!({"messages": []});
        assert!(!ensure_billing_cch(&mut empty));
    }

    fn err_body(etype: &str, msg: &str) -> Vec<u8> {
        serde_json::json!({"type": "error", "error": {"type": etype, "message": msg}})
            .to_string()
            .into_bytes()
    }

    /// 账号级错误照旧停用。
    #[test]
    fn bans_on_real_account_errors() {
        let cases = [
            (StatusCode::UNAUTHORIZED, err_body("authentication_error", "invalid bearer token")),
            (StatusCode::FORBIDDEN, err_body("permission_error", "This account has been disabled")),
            (
                StatusCode::BAD_REQUEST,
                err_body("invalid_request_error", "Your account was suspended"),
            ),
            // 主语不止 account：组织级停用同样是封号。
            (
                StatusCode::FORBIDDEN,
                err_body("permission_error", "This organization has been deactivated"),
            ),
            // OAuth 刷新失败没有主语词，靠独立特征词命中。
            (StatusCode::BAD_REQUEST, err_body("invalid_request_error", "invalid_grant")),
        ];
        for (status, body) in cases {
            assert!(
                detect_account_ban(status, &body).is_some(),
                "应判定为账号级错误: {status} {}",
                String::from_utf8_lossy(&body)
            );
        }
    }

    /// 非账号问题的 4xx 不得停用——这类误杀会把健康账号一个个扣掉。
    #[test]
    fn does_not_ban_on_non_account_errors() {
        let cases = [
            // Pro 账号请求 Opus / beta 未开通：能力问题，不是封号。
            (
                StatusCode::FORBIDDEN,
                err_body("permission_error", "Your account does not have access to claude-opus-5"),
            ),
            // 裸 401（CDN/网关拦截，非 Anthropic 错误 JSON）。
            (StatusCode::UNAUTHORIZED, b"<html>401 Unauthorized</html>".to_vec()),
            // 客户端请求错误。
            (
                StatusCode::BAD_REQUEST,
                err_body("invalid_request_error", "max_tokens: must be <= 64000"),
            ),
            // 特征词碰巧出现在「端点不支持」里：账号是好的。
            (
                StatusCode::UNAUTHORIZED,
                err_body(
                    "authentication_error",
                    "OAuth authentication is currently not supported for this endpoint",
                ),
            ),
            // 上游回显请求字段名，字段名里含状态词。曾在 v0.2.69 把整池账号逐个误禁：
            // 客户端每重试一次就扣掉一个号，而账号本身完全健康。
            (
                StatusCode::BAD_REQUEST,
                err_body(
                    "invalid_request_error",
                    "\"thinking.type.disabled\" is not supported for this model. Thinking defaults to adaptive mode when not specified; use \"thinking.type.enabled\" with \"budget_tokens\" for extended thinking.",
                ),
            ),
            // 有主语没状态词：额度/权限问题，不是封号。
            (
                StatusCode::BAD_REQUEST,
                err_body("invalid_request_error", "Your account has insufficient credits"),
            ),
        ];
        for (status, body) in cases {
            assert!(
                detect_account_ban(status, &body).is_none(),
                "不应停用: {status} {}",
                String::from_utf8_lossy(&body)
            );
        }
    }

    /// 只混淆 BLOCKED_TOOL_NAMES 里的名字，其余（官方名/MCP前缀/任意第三方名）原样透传。
    /// 判据实测：`skill_manage`/`skill_view`/`skills_list` 原样是 400，加 `mcp__` 前缀豁免。
    #[test]
    fn tool_map_skips_official_mcp_and_server_tools() {
        let body = serde_json::json!({"tools": [
            {"name": "Bash"},                                      // 官方 → 保留
            {"name": "mcp__hermes__skill_manage"},                 // MCP前缀 → 保留
            {"type": "web_search_20250305", "name": "web_search"}, // server tool → 保留
            {"name": "delegate_task"},                             // 非blocklist → 保留
            {"name": "skill_manage"},                              // blocklist → 混淆
        ]});
        let map = build_tool_name_map(Some(&body)).expect("blocklist命中就该有映射");
        assert_eq!(map.forward.len(), 1, "只混淆blocklist里的: {:?}", map.forward);
        assert!(map.forward.contains_key("skill_manage"));
        for kept in ["Bash", "mcp__hermes__skill_manage", "web_search", "delegate_task"] {
            assert!(!map.forward.contains_key(kept), "{kept} 该保留原名");
        }
        // 假名必须走已验证豁免的 MCP 命名空间。
        for fake in map.forward.values() {
            assert!(
                fake.starts_with("mcp__luban__"),
                "假名必须是 mcp__luban__ 前缀: {fake}"
            );
        }

        // blocklist 以外全不命中 → 无映射。
        let clean = serde_json::json!({"tools": [
            {"name": "Bash"}, {"name": "mcp__x__y"}, {"name": "delegate_task"},
        ]});
        assert!(build_tool_name_map(Some(&clean)).is_none());
        assert!(build_tool_name_map(Some(&serde_json::json!({"tools": []}))).is_none());
        assert!(build_tool_name_map(Some(&serde_json::json!({}))).is_none());
    }

    /// 同一组工具名两次构造得到同一套假名——否则每轮请求的假名都变，上游 prompt cache 全丢。
    #[test]
    fn tool_map_is_stable_for_the_same_tool_set() {
        let body = serde_json::json!({"tools": [
            {"name": "skill_manage"}, {"name": "skill_view"}, {"name": "skills_list"},
        ]});
        let a = build_tool_name_map(Some(&body)).unwrap();
        let b = build_tool_name_map(Some(&body)).unwrap();
        assert_eq!(a.forward, b.forward);

        // 工具集变了假名就该变（否则新旧两套名字会撞在一起）。
        let other = serde_json::json!({"tools": [{"name": "skill_manage"}]});
        let c = build_tool_name_map(Some(&other)).unwrap();
        assert_ne!(a.forward.get("skill_manage"), c.forward.get("skill_manage"));
    }

    /// 来访若恰好已有一个和生成假名同名的 MCP 工具，第三方工具仍必须得到映射，
    /// 不能为了避免撞名就把真名漏给上游。
    #[test]
    fn tool_map_resolves_declared_mcp_alias_collision() {
        let one = serde_json::json!({"tools": [{"name": "skill_manage"}]});
        let first = build_tool_name_map(Some(&one)).unwrap();
        let occupied = first.forward["skill_manage"].clone();
        let collided = serde_json::json!({"tools": [
            {"name": "skill_manage"},
            {"name": occupied},
        ]});
        let map = build_tool_name_map(Some(&collided)).expect("撞名不得让映射消失");
        let alias = &map.forward["skill_manage"];
        assert_ne!(alias, &occupied);
        assert!(alias.starts_with(&format!("{occupied}_")), "应以稳定后缀解决撞名: {alias}");
    }

    /// 请求侧三处必须同时改：`tools[]`、`tool_choice`、历史里的 `tool_use`。
    /// 漏掉第三处的话上游会因为 `tool_use` 引用未声明的工具名而拒掉整条请求。
    #[test]
    fn applies_tool_names_to_all_three_places() {
        let mut v = serde_json::json!({
            "tools": [{"name": "skill_manage"}, {"name": "Bash"}],
            "tool_choice": {"type": "tool", "name": "skill_manage"},
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "name": "skill_manage", "input": {}},
                    {"type": "text", "text": "skill_manage 只是正文，不该动"},
                ]},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "x"}]},
            ],
        });
        let snapshot = v.clone();
        let map = build_tool_name_map(Some(&snapshot)).unwrap();
        assert!(apply_tool_names(&mut v, &map));
        let fake = map.forward["skill_manage"].clone();

        assert_eq!(v["tools"][0]["name"], serde_json::json!(fake));
        assert_eq!(v["tools"][1]["name"], serde_json::json!("Bash"), "白名单不该动");
        assert_eq!(v["tool_choice"]["name"], serde_json::json!(fake));
        assert_eq!(v["messages"][0]["content"][0]["name"], serde_json::json!(fake));
        assert!(
            v["messages"][0]["content"][1]["text"].as_str().unwrap().contains("skill_manage"),
            "正文里的同名字符串不该被请求侧改写"
        );
    }

    /// 回程还原：假名换回真名，且**必须扛得住分块从假名中间切开**。
    /// 切断那次还原不了的话，客户端会拿到假名、下一轮带着假名回来，上游再回一个 400。
    #[test]
    fn restores_tool_names_across_chunk_boundaries() {
        let body = serde_json::json!({"tools": [
            {"name": "skill_manage"}, {"name": "skill_view"}, {"name": "skills_list"},
        ]});
        let map = build_tool_name_map(Some(&body)).unwrap();
        let fake = map.forward["skill_manage"].clone();
        let wire = format!(r#"data: {{"type":"tool_use","name":"{fake}"}}"#) + "\n\n";

        // 一次性还原。
        assert_eq!(
            String::from_utf8(map.restore(wire.as_bytes())).unwrap(),
            wire.replace(&fake, "skill_manage")
        );

        // 逐字节喂（最坏的分块），滑动窗口必须拼回同样的结果，且尾巴要 flush 出来。
        let mut pending = Vec::new();
        let mut out = Vec::new();
        for b in wire.as_bytes() {
            out.extend_from_slice(&map.feed(&mut pending, &[*b]));
        }
        out.extend_from_slice(&map.flush(&mut pending));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            wire.replace(&fake, "skill_manage"),
            "分块还原结果必须与整段一致"
        );
    }

    /// 短假名是长假名的子串时，必须先替长的——否则长假名会被先吃掉一截。
    #[test]
    fn restore_replaces_longer_aliases_first() {
        let map = super::ToolNameMap {
            forward: Default::default(),
            reverse: vec![
                ("fetch_abc00_long".to_string(), "REAL_LONG".to_string()),
                ("fetch_abc00".to_string(), "REAL_SHORT".to_string()),
            ],
            max_fake: "fetch_abc00_long".len(),
        };
        assert_eq!(
            String::from_utf8(map.restore(b"x fetch_abc00_long y fetch_abc00 z")).unwrap(),
            "x REAL_LONG y REAL_SHORT z"
        );
    }

    /// 官方从不发的顶层字段要剥掉，客户端真正要的语义不能动。
    ///
    /// 判据取自 `cap/raw/00006`/`00009`：两份直连抓包都没有 `tool_choice`，
    /// `thinking` 也都是裸的 `{"type":"adaptive"}`。
    #[test]
    fn strips_only_the_fields_official_never_sends() {
        // 等价于缺省的 tool_choice + thinking.display：都该剥。
        let mut v = serde_json::json!({
            "model": "claude-opus-5",
            "tool_choice": {"type": "auto"},
            "thinking": {"type": "adaptive", "display": "summarized"},
        });
        assert!(strip_extra_fields(&mut v));
        assert!(v.get("tool_choice").is_none(), "官方不发 tool_choice: {v}");
        assert_eq!(v["thinking"], serde_json::json!({"type": "adaptive"}), "display 应剥掉: {v}");

        // 强制选工具 / 强制用工具 / 关并行：都是客户端要的语义，一个都不能动。
        for keep in [
            serde_json::json!({"type": "tool", "name": "Bash"}),
            serde_json::json!({"type": "any"}),
            serde_json::json!({"type": "auto", "disable_parallel_tool_use": true}),
        ] {
            let mut v = serde_json::json!({ "tool_choice": keep.clone() });
            assert!(!strip_extra_fields(&mut v), "不该动: {keep}");
            assert_eq!(v["tool_choice"], keep);
        }

        // 官方形态本身：走一遍什么也不改（对真实 CC 是空操作）。
        let mut official = serde_json::json!({
            "model": "claude-opus-5",
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "high"},
        });
        let before = official.clone();
        assert!(!strip_extra_fields(&mut official));
        assert_eq!(official, before);
    }

    /// 剥字段走的是 [`super::rewrite_body`] 这条统一路径，且开关关掉即原样透传。
    #[test]
    fn strip_extra_fields_is_wired_and_switchable() {
        let body = br#"{"model":"claude-opus-5","tool_choice":{"type":"auto"},"thinking":{"type":"adaptive","display":"summarized"},"messages":[]}"#;
        let only_strip = store::ForwardFlags {
            strip_extra_fields: true,
            ..store::ForwardFlags {
                spoof_identity: false,
                spoof_device_id: false,
                billing_cch: false,
                fill_client_headers: false,
                merge_beta: false,
                system_shape: false,
                orig_header_case: false,
                thinking_signature_retry: false,
                simulate_cc: false,
                fill_metadata: false,
                rate_limit_retry: false,
                cache_scope_global: false,
                cache_ttl_1h: false,
                nonstream_as_sse: false,
                strip_extra_fields: false,
                tool_name_mimic: false,
            }
        };
        let out = rewrite_body(&Bytes::from(&body[..]), &test_cred(), "fp", only_strip, None, None);
        let s = String::from_utf8(out.to_vec()).unwrap();
        assert!(!s.contains("tool_choice"), "{s}");
        assert!(!s.contains("display"), "{s}");

        let off = store::ForwardFlags { strip_extra_fields: false, ..only_strip };
        let out = rewrite_body(&Bytes::from(&body[..]), &test_cred(), "fp", off, None, None);
        assert_eq!(out.as_ref(), &body[..], "关掉后必须逐字节透传");
    }

    /// 「被判成第三方应用」的那条 400 要认出来，普通 400 不能误认。
    ///
    /// 同一条报文还必须**不**被 [`super::detect_account_ban`] 判成封号——账号是好的，
    /// 被拒的是请求形态；误停用等于每撞一次这个 400 就白扣一个号。
    #[test]
    fn detects_third_party_rejection_without_banning() {
        let real = err_body(
            "invalid_request_error",
            "Third-party apps now draw from your extra usage, not your plan limits. Add more at claude.ai/settings/usage and keep going",
        );
        assert!(is_third_party_rejection(&real));
        assert!(
            detect_account_ban(StatusCode::BAD_REQUEST, &real).is_none(),
            "第三方判定不是账号级错误，不得停用凭证"
        );

        // 同一族的另一条文案（额度真的用光）。
        let drained =
            err_body("invalid_request_error", "You're out of extra usage. Add more to keep going");
        assert!(is_third_party_rejection(&drained));

        for other in [
            err_body("invalid_request_error", "max_tokens: must be <= 64000"),
            err_body("authentication_error", "invalid bearer token"),
            b"<html>400 Bad Request</html>".to_vec(),
        ] {
            assert!(
                !is_third_party_rejection(&other),
                "不该误认: {}",
                String::from_utf8_lossy(&other)
            );
        }
    }

    /// 摘要要留住形态判据（工具名与类型、system 块数与断点、顶层 key 顺序），
    /// 同时不把用户对话正文带进日志。
    #[test]
    fn request_digest_keeps_shape_and_drops_user_text() {
        let body = serde_json::json!({
            "model": "claude-opus-5",
            "system": [
                {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude.",
                 "cache_control": {"type": "ephemeral", "ttl": "1h"}},
            ],
            "tools": [
                {"name": "delegate_task", "description": "hand work to a subagent",
                 "input_schema": {"type": "object", "properties": {"prompt": {"type": "string"}}}},
                {"type": "web_search_20250305", "name": "web_search"},
            ],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "我的银行卡号是 1234"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "name": "delegate_task", "input": {"prompt": "secret"}},
                ]},
            ],
            "stream": true,
        });
        let dumped = request_digest(&body).to_string();

        // 形态判据留住了。
        assert!(dumped.contains("delegate_task"), "工具名是要查的那个维度: {dumped}");
        assert!(dumped.contains("web_search_20250305"), "server tool 的 type 要能看见: {dumped}");
        assert!(dumped.contains("ephemeral"), "缓存断点要原样留着: {dumped}");
        assert!(dumped.contains("claude-opus-5") && dumped.contains("\"stream\":true"));
        assert!(dumped.contains("tool_use(delegate_task)"), "历史里的工具名同样要看: {dumped}");

        // 用户正文没进去。
        assert!(!dumped.contains("1234"), "用户对话正文不得进日志: {dumped}");
        assert!(!dumped.contains("secret"), "工具入参不得进日志: {dumped}");

        // 顶层 key 顺序原样（preserve_order），顺序本身也是判据。
        let parsed: serde_json::Value = serde_json::from_str(&dumped).unwrap();
        let keys: Vec<String> = parsed.as_object().unwrap().keys().cloned().collect();
        assert_eq!(keys, ["model", "system", "tools", "messages", "stream"]);
    }

    /// 出站头摘要不得带出鉴权值——日志文件会被随手贴出来排查。
    #[test]
    fn header_dump_redacts_credentials() {
        assert!(is_secret_header("authorization"));
        assert!(is_secret_header("x-api-key"));
        assert!(!is_secret_header("anthropic-beta"));
    }

    /// 截断按字符不按字节：请求体里有中文，按字节切会 panic。
    #[test]
    fn head_truncates_by_char() {
        assert_eq!(head("abc", 10), "abc");
        assert_eq!(head("中文中文中", 2), "中文…(+3)");
    }

    /// 补齐的 x-client-request-id 是标准 uuid v4 形态。
    #[test]
    fn generates_uuid_v4() {
        let u = uuid_v4();
        let parts: Vec<&str> = u.split('-').collect();
        assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
        assert!(u.chars().all(|c| c.is_ascii_hexdigit() || c == '-'), "非法字符: {u}");
        assert_eq!(parts[2].as_bytes()[0], b'4', "version 位应为 4: {u}");
        assert!(matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'), "variant 位不对: {u}");
        assert_ne!(u, uuid_v4(), "每请求应不同");
    }

    // 真实 CC 抓包形态：字段顺序 device_id → account_uuid → session_id。
    const CC: &str = r#"{"device_id":"dddd","account_uuid":"aaaa","session_id":"ssss"}"#;

    #[test]
    fn replaces_value_and_preserves_order() {
        let s = replace_json_str_field(CC, "account_uuid", "NEW").unwrap();
        let s = replace_json_str_field(&s, "device_id", "DEV").unwrap();
        assert_eq!(s, r#"{"device_id":"DEV","account_uuid":"NEW","session_id":"ssss"}"#);
    }

    #[test]
    fn fills_empty_account_uuid() {
        let empty = r#"{"device_id":"dddd","account_uuid":"","session_id":"ssss"}"#;
        let s = replace_json_str_field(empty, "account_uuid", "FILLED").unwrap();
        assert_eq!(s, r#"{"device_id":"dddd","account_uuid":"FILLED","session_id":"ssss"}"#);
    }

    #[test]
    fn missing_field_returns_none_no_insert() {
        assert!(replace_json_str_field(CC, "not_here", "X").is_none());
    }

    /// 请求体顶层 `speed` 字段能被读出；缺字段/非法 JSON 返回 None（不阻断转发）。
    ///
    /// 入参是**已解析**的 body（handler 全程只解析一次，见 [`super::handle`]），故「非法
    /// JSON」在这里表现为 `None`——解析失败那步已经在上游发生了。
    #[test]
    fn reads_speed_from_request_body() {
        let parse = |s: &str| serde_json::from_str::<serde_json::Value>(s).ok();
        let with = parse(r#"{"model":"claude-opus-5","speed":"fast","messages":[]}"#);
        assert_eq!(request_speed(with.as_ref()).as_deref(), Some("fast"));
        let without = parse(r#"{"model":"claude-opus-5","messages":[]}"#);
        assert_eq!(request_speed(without.as_ref()), None);
        assert_eq!(parse("not json"), None, "非法 JSON 在解析那步就是 None");
        assert_eq!(request_speed(None), None);
    }

    /// 上游 SSE 的 `usage.speed` 会被嗅探到——这是计费的权威来源（fast 被限流会回落）。
    #[test]
    fn sniffs_speed_from_response_usage() {
        let mut s = UsageSniffer::new(true, false);
        s.feed(
            b"data: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5\",\
              \"usage\":{\"input_tokens\":10,\"speed\":\"fast\"}}}\n",
        );
        s.finish();
        assert_eq!(s.speed.as_deref(), Some("fast"));
        assert_eq!(s.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(s.input_tokens, Some(10));

        // 非流式 JSON 响应同样能取到。
        let mut s2 = UsageSniffer::new(false, false);
        s2.feed(br#"{"model":"claude-opus-5","usage":{"output_tokens":5,"speed":"standard"}}"#);
        s2.finish();
        assert_eq!(s2.speed.as_deref(), Some("standard"));
    }

    // ---------- thinking 签名兜底 ----------

    /// 只认「signature + thinking 同现」这一种 400，别的 `invalid_request_error` 一律不碰——
    /// 误判的代价是给每个普通请求错误都白搭一次上游往返。
    #[test]
    fn detects_only_the_thinking_signature_400() {
        let hit = br#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.1.content.0: Invalid `signature` in `thinking` block"}}"#;
        assert!(super::is_thinking_signature_error(hit));

        for miss in [
            // 普通请求形态错误。
            &br#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: must be greater than 0"}}"#[..],
            // 提到了 thinking 但不是签名问题（工具续跑那条）——降级救不了它，不该触发。
            &br#"{"type":"error","error":{"type":"invalid_request_error","message":"a final `assistant` message must start with a thinking block"}}"#[..],
            // 非 JSON 的拦截页：整段当 message 扫，同样不该命中。
            &b"<html>403 Forbidden</html>"[..],
        ] {
            assert!(!super::is_thinking_signature_error(miss), "不该命中: {}", String::from_utf8_lossy(miss));
        }
    }

    /// thinking 原文搬进 text、redacted_thinking 直接删，其余块与 key 序原样不动。
    #[test]
    fn demotes_thinking_to_text() {
        let raw = concat!(
            r#"{"model":"claude-opus-5","messages":["#,
            r#"{"role":"user","content":[{"type":"text","text":"hi"}]},"#,
            r#"{"role":"assistant","content":["#,
            r#"{"type":"thinking","thinking":"想了想","signature":"AAAA"},"#,
            r#"{"type":"redacted_thinking","data":"ZZZZ"},"#,
            r#"{"type":"text","text":"答案"}]}]}"#
        );
        let out = super::demote_thinking_blocks(&Bytes::from(raw)).expect("应有可降级的块");
        let s = String::from_utf8(out.to_vec()).unwrap();

        assert!(!s.contains("\"thinking\""), "thinking 块应已消失: {s}");
        assert!(!s.contains("AAAA"), "签名应已丢弃: {s}");
        assert!(!s.contains("ZZZZ"), "redacted_thinking 应整块删掉: {s}");
        assert!(
            s.contains("<previous_thinking>\\n想了想\\n</previous_thinking>"),
            "推理原文应搬进 text: {s}"
        );
        assert!(s.contains(r#"{"type":"text","text":"答案"}"#), "原有 text 块应原样保留: {s}");
        // 降级块自己也照官方内容块的 type→text 键序写。
        assert!(
            s.contains(r#"{"type":"text","text":"<previous_thinking>"#),
            "降级块 key 被重排: {s}"
        );
    }

    /// user 轮不碰（它本来就没有 thinking 块，扫到也不该动），没得降级时返回 None——
    /// 避免为一条另有原因的 400 白发一次重试。
    #[test]
    fn skips_when_nothing_to_demote() {
        let raw = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
        assert!(super::demote_thinking_blocks(&Bytes::from(raw)).is_none());
        // 非 JSON、以及没有 messages 的请求体都不该 panic。
        assert!(super::demote_thinking_blocks(&Bytes::from_static(b"not json")).is_none());
        assert!(super::demote_thinking_blocks(&Bytes::from_static(br#"{"model":"x"}"#)).is_none());
    }

    /// 整轮只有 thinking 的 assistant 消息原样留着：降级完 `content` 会是空数组，
    /// 那是上游必拒的形态，发出去反而把「多一次往返」变成「多一次注定失败的往返」。
    #[test]
    fn keeps_assistant_turn_that_would_become_empty() {
        let raw = concat!(
            r#"{"messages":[{"role":"assistant","content":["#,
            r#"{"type":"thinking","thinking":"  ","signature":"AAAA"}]},"#,
            r#"{"role":"assistant","content":[{"type":"thinking","thinking":"实打实","signature":"BBBB"},"#,
            r#"{"type":"text","text":"答案"}]}]}"#
        );
        let out = super::demote_thinking_blocks(&Bytes::from(raw)).expect("第二轮可降级");
        let s = String::from_utf8(out.to_vec()).unwrap();
        assert!(s.contains("AAAA"), "空 thinking 那轮应原样留着: {s}");
        assert!(!s.contains("BBBB"), "第二轮仍应降级: {s}");
    }

    // ---------- 非 CC 请求的模拟（Simulation） ----------

    /// 一条普通客户端会发的请求：没有 system、没有 metadata，头也不是 CC 那套。
    const PLAIN_BODY: &str = concat!(
        r#"{"model":"claude-opus-5","max_tokens":1024,"#,
        r#""messages":[{"role":"user","content":"hi"}],"stream":true}"#
    );

    fn sim_for(body: &str) -> super::Simulation {
        super::Simulation::detect(
            parsed(&Bytes::from(body.to_string())).as_ref(),
            all_on(),
            &test_cred(),
            "fp",
        )
        .expect("普通请求应判为需要模拟")
    }

    /// 模拟串交给 `merge_beta` 之后，必须**逐字节**等于官方那串——这是
    /// [`config::CC_BETA_SIMULATED`] / [`config::CC_BETA_SIMULATED_HAIKU`] 唯一的正确性依据。
    ///
    /// 两族分开验：haiku 不发 `mid-conversation-system`/`effort`，且 `claude-code-20250219`
    /// 在**队尾**。共用一份种子串就会给 haiku 发出一个真实客户端不产生的排列。
    #[test]
    fn simulated_beta_matches_official() {
        // cap/raw/00009（sonnet-5 直连）；opus-5/fable-5 只多出计价相关的那几项。
        const OFFICIAL: &str = "claude-code-20250219,oauth-2025-04-20,\
             interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
             advanced-tool-use-2025-11-20,effort-2025-11-24,extended-cache-ttl-2025-04-11";
        // cap/raw/00031（haiku-4.5 直连）：oauth 在最前、claude-code 在第 6 位。
        const OFFICIAL_HAIKU: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,\
             redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
             claude-code-20250219,advanced-tool-use-2025-11-20,extended-cache-ttl-2025-04-11";

        for (model, official) in [
            ("claude-sonnet-5", OFFICIAL),
            ("claude-opus-5", OFFICIAL),
            ("claude-fable-5", OFFICIAL),
            ("gpt-4o", OFFICIAL), // 认不出的模型退回主串
            ("claude-haiku-4-5-20251001", OFFICIAL_HAIKU),
        ] {
            let seed = super::cc_beta_seed(model);
            assert_eq!(merge_beta(Some(&super::simulated_beta(seed, None))), official, "{model}");
        }

        // 计价语义相关的三项刻意不发，见 config::CC_BETA_SIMULATED。
        assert!(!OFFICIAL.contains("context-1m"), "1M 上下文不该由 luban 替客户端声明");
        assert!(!OFFICIAL.contains("fallback"), "额度回补/服务端换模型不该由 luban 替客户端声明");

        // 客户端自己要的 beta 不丢，去重后追加在官方串之后。
        let with_client = merge_beta(Some(&super::simulated_beta(
            config::CC_BETA_SIMULATED,
            Some("output-128k-2025-02-19, effort-2025-11-24"),
        )));
        assert!(
            with_client.contains("output-128k-2025-02-19"),
            "客户端的 beta 被丢了: {with_client}"
        );
        assert_eq!(with_client.matches("effort-2025-11-24").count(), 1, "重复项: {with_client}");
    }

    /// 普通请求 → 官方四块 system：billing / 身份句 / 基座（global）/ 客户端原文。
    /// 基座按模型族选，且 `system` 落在 `messages` 之后（官方 key 序）。
    #[test]
    fn simulates_official_system_for_plain_request() {
        let body = Bytes::from(
            r#"{"model":"claude-sonnet-5","messages":[],"system":"你是助手","max_tokens":8}"#
                .to_string(),
        );
        let sim = super::Simulation::detect(parsed(&body).as_ref(), all_on(), &test_cred(), "fp")
            .unwrap();
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
        let s = String::from_utf8(out.to_vec()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();

        assert_eq!(sys.len(), 4, "应是官方的四块: {s}");
        assert!(
            sys[0]["text"].as_str().unwrap().starts_with("x-anthropic-billing-header:"),
            "第 0 块应是 billing header: {s}"
        );
        assert!(
            sys[0]["text"].as_str().unwrap().contains("cch="),
            "cch 应由 ensure_billing_cch 补上"
        );
        assert_eq!(sys[1]["text"], config::CC_SYSTEM_IDENTITY, "第 1 块必须是那句身份声明");
        assert!(sys[1].get("cache_control").is_none(), "身份句不带断点（官方如此）");
        assert_eq!(sys[2]["text"], config::CC_SYSTEM_BASE_SONNET, "sonnet 族应取 sonnet 基座");
        assert_eq!(sys[2]["cache_control"]["scope"], "global");
        assert_eq!(sys[3]["text"], "你是助手", "客户端原 system 应原样留在末块");
        assert_eq!(sys[3]["cache_control"]["type"], "ephemeral");
        assert!(sys[3]["cache_control"].get("scope").is_none(), "只有基座标 global");
        // ttl 默认对齐官方：三个断点都是 1h（见 [`super::cache_control`]）。
        assert_eq!(sys[2]["cache_control"]["ttl"], "1h", "基座该带 ttl: {s}");
        assert_eq!(sys[3]["cache_control"]["ttl"], "1h", "末块也该带 ttl: {s}");

        // key 序按官方 `model → messages → system → tools → metadata → max_tokens` 落位，
        // 补出来的几个字段不该被追加到队尾。本例没开 thinking，故不补 `context_management`
        // （见 [`super::ensure_context_management`]：那个字段依赖 thinking）。
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec!["model", "messages", "system", "metadata", "max_tokens"],
            "key 序: {s}"
        );

        // 换模型族即换基座；认不出的模型只注入前两块。
        assert_eq!(sim_for(PLAIN_BODY).base, Some(config::CC_SYSTEM_BASE_OPUS), "opus 族基座");
        assert_eq!(
            sim_for(r#"{"model":"claude-haiku-4-5-20251001","messages":[]}"#).base,
            Some(config::CC_SYSTEM_BASE_SONNET),
            "haiku 与 sonnet-5 的基座 sha256 相同，共用一份"
        );
        assert!(
            sim_for(r#"{"model":"gpt-4o","messages":[]}"#).base.is_none(),
            "认不出的模型不猜基座"
        );
    }

    /// 回归 2026-08-07 的拒绝日志：客户端的顶层顺序是
    /// `model, system, messages, max_tokens, stream, tools, metadata, output_config`，即使
    /// system 和工具名都已整形，这个顺序仍把第三方客户端指纹原样带了出去。
    #[test]
    fn simulated_request_reorders_existing_top_level_keys() {
        let body = Bytes::from(
            r#"{"model":"claude-sonnet-5","system":"third party","messages":[],"max_tokens":65536,"stream":true,"tools":[{"name":"skill_manage"}],"metadata":{},"output_config":{"effort":"high"}}"#
                .to_string(),
        );
        let parsed_body = parsed(&body);
        let sim = super::Simulation::detect(parsed_body.as_ref(), all_on(), &test_cred(), "fp")
            .expect("该请求应走模拟路径");
        let map = build_tool_name_map(parsed_body.as_ref()).unwrap();
        let out = super::rewrite_body(
            &body,
            &test_cred(),
            "fp",
            all_on(),
            Some(&sim),
            None,
            false,
            Some(&map),
        );
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "model",
                "messages",
                "system",
                "tools",
                "metadata",
                "max_tokens",
                "output_config",
                "stream",
            ],
            "模拟后顶层键序必须与官方抓包一致: {}",
            String::from_utf8_lossy(&out)
        );
        assert!(
            v["tools"][0]["name"].as_str().unwrap().starts_with("mcp__luban__"),
            "拒绝日志里的普通假名也应改成 MCP 形态: {v}"
        );
    }

    /// 没有 system 的请求同样成立：三块（billing / 身份句 / 基座），且末块拿到断点。
    #[test]
    fn simulates_system_when_client_sent_none() {
        let body = Bytes::from(PLAIN_BODY.to_string());
        let sim = sim_for(PLAIN_BODY);
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();
        assert_eq!(sys.len(), 3, "没有客户端 system 就只有前三块: {v}");
        assert_eq!(sys[2]["cache_control"]["scope"], "global");
    }

    /// 模拟路径要补 `context_management`：`cap/raw` 八份抓包逐字节相同，而声明它的
    /// `context-management-2025-06-27` 已在两份 seed 里，不补就是「头上声明了、体里没有」。
    ///
    /// **但只在客户端自己开了 thinking 时补**——`clear_thinking` 依赖它，没开硬补上游回
    /// `` `clear_thinking_20251015` strategy requires `thinking` to be enabled or adaptive ``。
    /// 抓包八份全开着 thinking，这层依赖看不出来，v0.2.51 即因此让普通请求 400。
    #[test]
    fn simulated_body_carries_official_context_management() {
        const OFFICIAL: &str =
            r#""context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}"#;
        // 开着 thinking 的来访（官方 opus/sonnet/fable 那族的形态）。
        let thinking_body = concat!(
            r#"{"model":"claude-opus-5","max_tokens":1024,"#,
            r#""messages":[{"role":"user","content":"hi"}],"#,
            r#""thinking":{"type":"adaptive"},"stream":true}"#
        );
        let body = Bytes::from(thinking_body.to_string());
        let sim = sim_for(thinking_body);
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
        let text = String::from_utf8(out.to_vec()).unwrap();
        assert!(text.contains(OFFICIAL), "取值要与官方逐字节相同: {text}");

        // 官方位置：`thinking` 之后、`stream` 之前。
        let at = text.find(OFFICIAL).unwrap();
        assert!(at > text.find(r#""thinking""#).unwrap(), "该排在 thinking 之后: {text}");
        assert!(at < text.find(r#""stream""#).unwrap(), "该排在 stream 之前: {text}");
        assert!(at > text.find(r#""metadata""#).unwrap(), "该排在 metadata 之后: {text}");

        // 头上那份声明确实在，否则补了体就是反向的自相矛盾。
        assert!(sim.beta.contains("context-management-2025-06-27"), "seed 里该有对应的 beta");

        // haiku 那族的 `{"type":"enabled","budget_tokens":N}` 同样算开着。
        let haiku = concat!(
            r#"{"model":"claude-haiku-4-5-20251001","max_tokens":32000,"#,
            r#""messages":[{"role":"user","content":"hi"}],"#,
            r#""thinking":{"budget_tokens":31999,"type":"enabled"}}"#
        );
        let b = Bytes::from(haiku.to_string());
        let sim = sim_for(haiku);
        let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
        assert!(String::from_utf8(out.to_vec()).unwrap().contains(OFFICIAL), "enabled 也该补");

        // 没开 thinking 的三种写法都不补——补了上游直接 400。
        for body in [
            PLAIN_BODY.to_string(),
            concat!(
                r#"{"model":"claude-opus-5","max_tokens":16,"#,
                r#""messages":[{"role":"user","content":"hi"}],"thinking":{"type":"disabled"}}"#
            )
            .to_string(),
            concat!(
                r#"{"model":"claude-opus-5","max_tokens":16,"#,
                r#""messages":[{"role":"user","content":"hi"}],"thinking":null}"#
            )
            .to_string(),
        ] {
            let b = Bytes::from(body.clone());
            let sim = sim_for(&body);
            let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
            let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
            assert!(v.get("context_management").is_none(), "没开 thinking 却补了: {body}");
        }
    }

    /// 模拟路径要补官方那**第三个**缓存断点：`cap/raw` 八份抓包每条恰好 3 个，前两个在
    /// `system`，第三个恒在最后一条消息的最后一块上（六份非 haiku 落在末尾那条 `role:"system"`
    /// 消息，两份 haiku 没那条消息就落在 `user` 末块——规则是位置不是角色）。
    /// 顺带把裸字符串 `content` 收成官方那样的块数组，否则断点无处可挂。
    #[test]
    fn simulated_body_carries_official_message_breakpoint() {
        // 字符串 content：要转成块数组，断点落在末块。
        let body = Bytes::from(PLAIN_BODY.to_string());
        let sim = sim_for(PLAIN_BODY);
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let blocks = v["messages"][0]["content"].as_array().expect("content 该收成块数组");
        assert_eq!(blocks[0]["type"], "text", "转出来的该是官方那种文本块");
        assert_eq!(blocks[0]["text"], "hi", "正文一个字都不该变");
        assert_eq!(blocks.last().unwrap()["cache_control"]["type"], "ephemeral", "末块该有断点");
        // 消息这个断点不带 `scope`（官方只在基座标），但跟着开关带 `ttl`。
        assert!(
            blocks.last().unwrap()["cache_control"].get("scope").is_none(),
            "只有基座标 global"
        );
        assert_eq!(blocks.last().unwrap()["cache_control"]["ttl"], "1h");

        // 多轮对话：断点只落在**最后一条**消息上，前面的不动。
        let multi = concat!(
            r#"{"model":"claude-opus-5","max_tokens":16,"messages":["#,
            r#"{"role":"user","content":"a"},{"role":"assistant","content":"b"},"#,
            r#"{"role":"user","content":"c"}]}"#
        );
        let b = Bytes::from(multi.to_string());
        let sim = sim_for(multi);
        let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        for (i, m) in msgs.iter().enumerate() {
            let last = m["content"].as_array().unwrap().last().unwrap();
            assert_eq!(
                last.get("cache_control").is_some(),
                i == 2,
                "断点只该在最后一条消息上，第 {i} 条不对: {v}"
            );
        }

        // 客户端自己标过就不再多标一个；总数封顶 4，满了不补。
        // （`ttl` 会由 [`super::fill_cache_ttl`] 补齐——三个断点要么都有、要么都没有。）
        let mine = concat!(
            r#"{"model":"claude-opus-5","max_tokens":16,"messages":[{"role":"user","content":["#,
            r#"{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]}]}"#
        );
        let b = Bytes::from(mine.to_string());
        let sim = sim_for(mine);
        let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["messages"][0]["content"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral", "ttl": "1h"}),
            "客户端那个断点只该补 ttl，不该多出 scope 或被换掉 type: {v}"
        );
        assert_eq!(v["messages"][0]["content"][0]["text"], "hi", "正文一个字都不该动: {v}");
        assert!(super::count_cache_control(&v) <= super::MAX_CACHE_BREAKPOINTS, "断点超上限: {v}");

        // 非模拟路径**不新标断点**：CC 形态的来访自己就标好了第三个断点，替它再标一个只会
        // 多占预算。唯一会动的是给那个断点补 `ttl`（[`super::fill_cache_ttl`]），正文与断点
        // 位置都不变。
        let cc = Bytes::from(API_SHAPE_BODY);
        let before: serde_json::Value = serde_json::from_slice(&cc).unwrap();
        let out = rewrite_body(&cc, &test_cred(), "fp", all_on(), None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            super::count_cache_control(&v["messages"]),
            super::count_cache_control(&before["messages"]),
            "非模拟路径不该给 messages 新加断点: {v}"
        );
        assert_eq!(
            v["messages"][0]["content"][0]["text"], before["messages"][0]["content"][0]["text"],
            "正文不该被动: {v}"
        );
        assert_eq!(
            v["messages"][0]["content"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral", "ttl": "1h"}),
            "来访那个断点该补上 ttl，与 system 两个保持一致: {v}"
        );

        // 末块不是**非空 text** 时一律不标：抓包只有 text 的样本，而 `thinking` 那种块
        // 上游还要验签名，往它上面挂 cache_control 是拿能发的请求去赌没样本的组合。
        for (label, tail) in [
            ("thinking 块", r#"{"type":"thinking","thinking":"想","signature":"AAAA"}"#),
            ("空 text 块", r#"{"type":"text","text":""}"#),
            (
                "image 块",
                r#"{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA"}}"#,
            ),
        ] {
            let body = format!(
                r#"{{"model":"claude-opus-5","max_tokens":16,"messages":[{{"role":"user","content":"hi"}},{{"role":"assistant","content":[{tail}]}}]}}"#
            );
            let b = Bytes::from(body.clone());
            let sim = sim_for(&body);
            let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
            let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
            let last = v["messages"].as_array().unwrap().last().unwrap();
            let blk = last["content"].as_array().unwrap().last().unwrap();
            assert!(blk.get("cache_control").is_none(), "{label} 不该被标断点: {v}");
        }

        // 空串 content 不转成空 text 块（那种块上游会拒），原样留着。
        let empty = concat!(
            r#"{"model":"claude-opus-5","max_tokens":16,"#,
            r#""messages":[{"role":"user","content":"hi"},{"role":"assistant","content":""}]}"#
        );
        let b = Bytes::from(empty.to_string());
        let sim = sim_for(empty);
        let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["messages"][1]["content"], "", "空串不该被转成空 text 块: {v}");
    }

    /// 客户端自己带了 `context_management` 就一个字节都不动——那是它自己的编辑策略。
    /// CC 形态的来访（非模拟路径）则根本不补。
    #[test]
    fn context_management_respects_client_and_skips_non_simulated() {
        let mine = concat!(
            r#"{"model":"claude-opus-5","max_tokens":1024,"#,
            r#""messages":[{"role":"user","content":"hi"}],"#,
            r#""context_management":{"edits":[]},"stream":true}"#
        );
        let body = Bytes::from(mine.to_string());
        let sim = sim_for(mine);
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["context_management"]["edits"].as_array().unwrap().len(),
            0,
            "客户端的被改写了"
        );

        // 非模拟路径不补：那条路是尽量原样透传，来访本来就是 CC 形态、自己会带。
        let cc = Bytes::from(API_SHAPE_BODY);
        let out = rewrite_body(&cc, &test_cred(), "fp", all_on(), None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert!(v.get("context_management").is_none(), "非模拟路径不该补: {v}");
    }

    /// 已经是 CC 形态的请求一个字节都不该多改——判据是 `system` 里那句身份声明，
    /// 字符串形态与数组形态都认。
    #[test]
    fn leaves_cc_shaped_request_alone() {
        let cc = Bytes::from(API_SHAPE_BODY);
        assert!(
            super::Simulation::detect(parsed(&cc).as_ref(), all_on(), &test_cred(), "fp").is_none(),
            "CC 形态不该走模拟路径"
        );
        let as_string = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","system":"{}","messages":[]}}"#,
            config::CC_SYSTEM_IDENTITY
        ));
        assert!(
            super::Simulation::detect(parsed(&as_string).as_ref(), all_on(), &test_cred(), "fp")
                .is_none()
        );

        // 开关关掉、或 merge_beta 关掉（模拟出来的 beta 没人落位）时也不模拟。
        let plain = Bytes::from(PLAIN_BODY.to_string());
        let off = store::ForwardFlags { simulate_cc: false, ..all_on() };
        assert!(
            super::Simulation::detect(parsed(&plain).as_ref(), off, &test_cred(), "fp").is_none()
        );
        let no_beta = store::ForwardFlags { merge_beta: false, ..all_on() };
        assert!(
            super::Simulation::detect(parsed(&plain).as_ref(), no_beta, &test_cred(), "fp")
                .is_none()
        );
        // 解析不了的请求体不 panic、也不模拟。
        assert!(
            super::Simulation::detect(
                parsed(&Bytes::from_static(b"not json")).as_ref(),
                all_on(),
                &test_cred(),
                "fp"
            )
            .is_none()
        );
    }

    /// 客户端已经用满 4 个缓存断点时不再加——加了整条请求会被上游拒，那是把「形态更像」
    /// 换成「根本发不出去」。断点在别处（tools）时同样算数。
    #[test]
    fn respects_cache_breakpoint_budget() {
        let tool = r#"{"name":"t","cache_control":{"type":"ephemeral"}}"#;
        let body = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"tools":[{tool},{tool},{tool},{tool}]}}"#
        ));
        let sim = super::Simulation::detect(parsed(&body).as_ref(), all_on(), &test_cred(), "fp")
            .unwrap();
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(super::count_cache_control(&v), 4, "断点数不得超过 4: {v}");
        assert!(v["system"][2].get("cache_control").is_none(), "预算用完时基座不带断点");
        // 内容照发，只是少一次缓存复用。
        assert_eq!(v["system"][2]["text"], config::CC_SYSTEM_BASE_OPUS);
    }

    /// 客户端把 `system` 拆成多块时并成官方末块的一块——3+N 块会被上游判第三方应用、
    /// 改扣超额池（`Third-party apps now draw from your extra usage`）。
    ///
    /// 合并腾出来的断点预算要算进去：客户端那 4 个断点合并后只剩 1 个，基座该拿到断点。
    #[test]
    fn merges_client_system_blocks_into_official_tail() {
        let blk = |t: &str| {
            format!(r#"{{"type":"text","text":"{t}","cache_control":{{"type":"ephemeral"}}}}"#)
        };
        let body = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"system":[{},{},{},{}]}}"#,
            blk("a"),
            blk("b"),
            blk("c"),
            blk("d")
        ));
        let sim = super::Simulation::detect(parsed(&body).as_ref(), all_on(), &test_cred(), "fp")
            .unwrap();
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();

        assert_eq!(sys.len(), 4, "客户端的 4 块应并成末块一块: {v}");
        assert_eq!(sys[3]["text"], "a\n\nb\n\nc\n\nd", "正文一个字都不该丢");
        assert_eq!(sys[3]["cache_control"]["type"], "ephemeral", "末块断点取合并前的最后一个");
        assert_eq!(sys[2]["text"], config::CC_SYSTEM_BASE_OPUS);
        assert_eq!(sys[2]["cache_control"]["scope"], "global", "合并腾出的预算该给基座");
        assert_eq!(super::count_cache_control(&v), 2, "断点数: {v}");

        // 空块并不进来（发一个空文本块上游不收），只剩前三块。
        let empty = Bytes::from(
            r#"{"model":"claude-opus-5","messages":[],"system":[{"type":"text","text":""},{"type":"text","text":"  "}]}"#
                .to_string(),
        );
        let sim = super::Simulation::detect(parsed(&empty).as_ref(), all_on(), &test_cred(), "fp")
            .unwrap();
        let out = rewrite_body(&empty, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["system"].as_array().unwrap().len(), 3, "全空的块应丢掉: {v}");
    }

    /// 自称 CC（`system` 里有那句身份声明）却发了 5 块以上的第三方客户端：既不模拟、也不走
    /// 三块拆分器，只能靠封顶兜住，否则块数超 4 照样按第三方额度扣。
    #[test]
    fn caps_system_blocks_for_cc_shaped_client() {
        let body = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"h"}},{{"type":"text","text":"{}"}},{{"type":"text","text":"c"}},{{"type":"text","text":"d"}},{{"type":"text","text":"e","cache_control":{{"type":"ephemeral"}}}}]}}"#,
            config::CC_SYSTEM_IDENTITY
        ));
        assert!(
            super::Simulation::detect(parsed(&body).as_ref(), all_on(), &test_cred(), "fp")
                .is_none(),
            "CC 形态不该走模拟"
        );
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();
        assert_eq!(sys.len(), 4, "5 块应压回 4 块: {v}");
        assert_eq!(sys[3]["text"], "d\n\ne", "第 4 块起并成一块");
        assert_eq!(sys[3]["cache_control"]["type"], "ephemeral", "末块断点保留");

        // 4 块及以内不动结构：官方形态与 API-key 的三块形态都不该被这条碰到。
        let four = Bytes::from(API_SHAPE_BODY);
        let before: serde_json::Value = serde_json::from_slice(&four).unwrap();
        let mut after = before.clone();
        assert!(!super::cap_system_blocks(&mut after), "3 块不该被改");
        assert_eq!(before, after);

        // 开关关掉就不封顶。
        let off = store::ForwardFlags { system_shape: false, ..all_on() };
        let out = rewrite_body(&body, &test_cred(), "fp", off, None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["system"].as_array().unwrap().len(), 5, "关掉开关应原样转发: {v}");
    }

    /// 模拟路径补 `metadata.user_id`：键序与 CC 一致，session_id 与请求头同值且逐设备稳定；
    /// 客户端自己带了 user_id 就不新造（交给 spoof_identity 原格式改写）。
    #[test]
    fn injects_cc_metadata_only_when_absent() {
        let body = Bytes::from(PLAIN_BODY.to_string());
        let sim = sim_for(PLAIN_BODY);
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let user_id = v["metadata"]["user_id"].as_str().unwrap();
        let inner: serde_json::Value = serde_json::from_str(user_id).unwrap();

        assert_eq!(
            inner.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["device_id", "account_uuid", "session_id"],
            "键序应与 CC 一致: {user_id}"
        );
        assert_eq!(inner["account_uuid"], ACCOUNT_UUID);
        assert_eq!(inner["device_id"], test_cred().spoof_device_id("fp").unwrap());
        assert_eq!(inner["session_id"], sim.session_id, "两处 session_id 必须同值");
        assert_eq!(sim.session_id, sim_for(PLAIN_BODY).session_id, "同设备同账号应恒定");

        // 客户端自己带了 user_id：不新造，仍由 spoof_identity 定点改写。
        let with_meta = Bytes::from(
            r#"{"model":"claude-opus-5","messages":[],"metadata":{"user_id":"user_aa_account_bb_session_cc"}}"#
                .to_string(),
        );
        let sim2 =
            super::Simulation::detect(parsed(&with_meta).as_ref(), all_on(), &test_cred(), "fp")
                .unwrap();
        let out2 = rewrite_body(&with_meta, &test_cred(), "fp", all_on(), Some(&sim2), None);
        let v2: serde_json::Value = serde_json::from_slice(&out2).unwrap();
        assert_eq!(
            v2["metadata"]["user_id"],
            format!(
                "user_{}_account_{ACCOUNT_UUID}_session_cc",
                test_cred().spoof_device_id("fp").unwrap()
            ),
            "扁平串形态应原格式改写，而不是被换成 CC 的 JSON 形态"
        );
    }

    /// `spoof_device_id` 关掉时只换 account 段，来访自带的 `device_id` 原样保留。
    ///
    /// **判据取自真实抓包对**：`cap/raw/00002`（API-key 模式经 luban）与 `00006`（订阅模式
    /// 直连）是同机、同客户端、同模型、相隔 28 秒的两条请求，两者的 `device_id` **完全相同**
    /// （`832cb7e6…`），只有 `account_uuid` 不同（空串 ↔ 真 uuid）。故「补 account、留 device」
    /// 正是官方两种模式之间真实存在的那一处差别，见 [`store::ForwardFlags::spoof_device_id`]。
    #[test]
    fn keeps_client_device_id_when_spoof_device_off() {
        const CLIENT_DEVICE: &str =
            "832cb7e697190bc475b926c7994ef183a0f8a58e29818f182e11f924e1ea2870";
        let off = store::ForwardFlags { spoof_device_id: false, ..all_on() };

        // 格式一：CC 内嵌 JSON（键序与官方一致）。
        let body = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"metadata":{{"user_id":"{{\"device_id\":\"{CLIENT_DEVICE}\",\"account_uuid\":\"\",\"session_id\":\"ssss\"}}"}}}}"#
        ));
        let out = rewrite_body(&body, &test_cred(), "fp", off, None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let inner: serde_json::Value =
            serde_json::from_str(v["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(inner["device_id"], CLIENT_DEVICE, "关掉后 device_id 该原样保留");
        assert_eq!(inner["account_uuid"], ACCOUNT_UUID, "account 段照样要补——那才是两模式的差别");
        assert_eq!(inner["session_id"], "ssss", "session 段一如既往不动");

        // 开着时（默认）仍换成派生值：本开关不改变既有行为。
        let on = rewrite_body(&body, &test_cred(), "fp", all_on(), None, None);
        let v_on: serde_json::Value = serde_json::from_slice(&on).unwrap();
        let inner_on: serde_json::Value =
            serde_json::from_str(v_on["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(inner_on["device_id"], test_cred().spoof_device_id("fp").unwrap());

        // 格式二：扁平串——device 段同样保留，仍以扁平串回写。
        let flat = Bytes::from(
            r#"{"model":"claude-opus-5","messages":[],"metadata":{"user_id":"user_aa_account_bb_session_cc"}}"#
                .to_string(),
        );
        let out = rewrite_body(&flat, &test_cred(), "fp", off, None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["metadata"]["user_id"], format!("user_aa_account_{ACCOUNT_UUID}_session_cc"));

        // 模拟路径不受本开关影响：那条路来访压根没有 device_id，只能派生——否则产出的是
        // 一份没有 device_id 的 metadata，官方从不发那种形态。
        let bare = Bytes::from(PLAIN_BODY.to_string());
        let sim = super::Simulation::detect(parsed(&bare).as_ref(), off, &test_cred(), "fp")
            .expect("裸请求仍应走模拟");
        let out = rewrite_body(&bare, &test_cred(), "fp", off, Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let inner: serde_json::Value =
            serde_json::from_str(v["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(
            inner["device_id"],
            test_cred().spoof_device_id("fp").unwrap(),
            "模拟路径必须派生，不受开关影响"
        );
    }

    /// CC 形态但不带 `metadata.user_id` 的来访（第三方 CC 兼容客户端）：照样补一份官方身份，
    /// **且头体两处的 session_id 逐字节相同**。
    ///
    /// 判据逐条取自 `cap/raw/00006`（claude-cli/2.1.220 直连，opus-5）的原始报文：
    /// ```text
    /// "metadata":{"user_id":"{\"device_id\":\"832cb7…2870\",\"account_uuid\":\"edded6bb-…\",\"session_id\":\"bc201916-d0bc-4b4e-adba-caf41fb58746\"}"}
    /// X-Claude-Code-Session-Id: bc201916-d0bc-4b4e-adba-caf41fb58746
    /// ```
    /// 即：内层是紧凑 JSON 字符串、键序 device_id→account_uuid→session_id、device_id 是
    /// 64 位小写 hex、session_id 是 uuid 且与那个头**同值**。`00009`（sonnet-5）同形。
    #[test]
    fn cc_shaped_without_metadata_gets_aligned_identity() {
        // CC 形态：system 里有那句身份声明，故 detect 返回 None（不模拟）。
        let body = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"{}"}}]}}"#,
            config::CC_SYSTEM_IDENTITY
        ));
        assert!(
            super::Simulation::detect(parsed(&body).as_ref(), all_on(), &test_cred(), "fp")
                .is_none(),
            "CC 形态不该走模拟"
        );
        assert!(
            !super::body_has_user_id(parsed(&body).as_ref()),
            "这条来访本来就没有 metadata.user_id"
        );

        // 来访没带会话 id 头 → 派生一个，头体同步补。
        let client = super::HeaderMap::new();
        let sid = super::bare_session_id(
            &client,
            all_on(),
            None,
            true,
            super::body_has_user_id(parsed(&body).as_ref()),
            &test_cred(),
            "fp",
        )
        .expect("CC 形态 + 无 metadata 应补身份");
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), None, Some(sid.as_str()));
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let user_id = v["metadata"]["user_id"].as_str().expect("应补出 metadata.user_id");
        let inner: serde_json::Value = serde_json::from_str(user_id).unwrap();

        assert_eq!(
            inner.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["device_id", "account_uuid", "session_id"],
            "键序应与 00006 一致: {user_id}"
        );
        assert!(!user_id.contains(": "), "内层须是紧凑 JSON（无空白），同 00006");
        let device_id = inner["device_id"].as_str().unwrap();
        assert_eq!(device_id.len(), 64, "device_id 同 00006 是 64 位 hex");
        assert!(
            device_id.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "device_id 须是小写 hex: {device_id}"
        );
        let session_id = inner["session_id"].as_str().unwrap();
        let seg: Vec<usize> = session_id.split('-').map(str::len).collect();
        assert_eq!(seg, vec![8, 4, 4, 4, 12], "session_id 同 00006 是 uuid 形态: {session_id}");

        // 头体同值——00006 里这两处逐字节相同，这正是本条路径最容易做错的地方。
        let headers =
            build_forward_headers(&client, "sk-ant-oat01-REAL", all_on(), None, Some(&sid));
        assert_eq!(
            headers.get("x-claude-code-session-id").unwrap().to_str().unwrap(),
            session_id,
            "头与 metadata 里的 session_id 必须逐字节相同"
        );

        // 来访自己带了那个头 → 用它的值，不另派生（否则头体对不上）。
        let mut with_sid = super::HeaderMap::new();
        with_sid.insert(
            super::HeaderName::from_static("x-claude-code-session-id"),
            HeaderValue::from_static("bc201916-d0bc-4b4e-adba-caf41fb58746"),
        );
        let sid2 =
            super::bare_session_id(&with_sid, all_on(), None, true, false, &test_cred(), "fp")
                .unwrap();
        assert_eq!(sid2, "bc201916-d0bc-4b4e-adba-caf41fb58746", "应沿用来访自己的会话 id");
        let out2 = rewrite_body(&body, &test_cred(), "fp", all_on(), None, Some(sid2.as_str()));
        let v2: serde_json::Value = serde_json::from_slice(&out2).unwrap();
        let inner2: serde_json::Value =
            serde_json::from_str(v2["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(inner2["session_id"], sid2, "体里要用来访那个值");
        let headers2 = build_forward_headers(&with_sid, "tok", all_on(), None, Some(&sid2));
        assert_eq!(
            headers2.get("x-claude-code-session-id").unwrap().to_str().unwrap(),
            sid2,
            "客户端原值不该被覆盖"
        );
    }

    /// 补出来的 metadata 必须与官方报文**逐字节同形**（只有取值不同）。
    ///
    /// 金标准逐字取自 `cap/raw/00006_101505.964.req.raw`（claude-cli/2.1.220 直连 opus-5）
    /// 的请求体原文，位置在 `tools` 之后、`max_tokens` 之前：
    /// ```text
    /// …,"metadata":{"user_id":"{\"device_id\":\"832cb7e6…2870\",\"account_uuid\":\"edded6bb-2521-4a68-94cb-241bb4d96bb9\",\"session_id\":\"bc201916-d0bc-4b4e-adba-caf41fb58746\"}"},"max_tokens":64000,…
    /// ```
    /// 抓包不入库（`cap/` 未跟踪），故把这串固化在这里——与基座字节数、beta 串同一做法。
    /// 逐字节比对是为了钉住**转义写法**：内层是「字符串里的 JSON」，序列化器只要把
    /// `\"` 写成别的形式（或插进任何空白），出去的就不是官方那串了。
    #[test]
    fn injected_metadata_matches_raw_capture_bytes() {
        let body = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"{}"}}],"max_tokens":64000}}"#,
            config::CC_SYSTEM_IDENTITY
        ));
        let sid = "bc201916-d0bc-4b4e-adba-caf41fb58746";
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), None, Some(sid));
        let text = String::from_utf8(out.to_vec()).unwrap();

        let expected = format!(
            r#""metadata":{{"user_id":"{{\"device_id\":\"{}\",\"account_uuid\":\"{ACCOUNT_UUID}\",\"session_id\":\"{sid}\"}}"}}"#,
            test_cred().spoof_device_id("fp").unwrap()
        );
        assert!(text.contains(&expected), "与 00006 的 metadata 形态不符\n实际: {text}");
        // 位置也照抓包：metadata 在 max_tokens 之前。
        assert!(
            text.find(r#""metadata""#) < text.find(r#""max_tokens""#),
            "metadata 应落在 max_tokens 之前（同 00006 的 key 序）: {text}"
        );
    }

    /// 不该补身份的四种情形——补错了都会造出「官方不产生的形态」，比不补更糟。
    #[test]
    fn bare_session_skipped_when_it_would_break_shape() {
        let bare = super::HeaderMap::new();
        let call = |flags, sim, billable, has_user_id, cred: &crate::credentials::Credential| {
            super::bare_session_id(&bare, flags, sim, billable, has_user_id, cred, "fp")
        };
        let sim = sim_for(PLAIN_BODY);

        // 1) 走模拟那条路：session_id 在 Simulation 里，不能再派生一个。
        assert!(call(all_on(), Some(&sim), true, false, &test_cred()).is_none());
        // 2) 身份伪装关着：这是总开关。
        let no_spoof = store::ForwardFlags { spoof_identity: false, ..all_on() };
        assert!(call(no_spoof, None, true, false, &test_cred()).is_none());
        // 3) 非计费路径（count_tokens）：出站体原样透传，补了也发不出去，只剩个孤头。
        assert!(call(all_on(), None, false, false, &test_cred()).is_none());
        // 4) 来访已经有 user_id：交给 spoof_identity 原格式改写，两条路只能有一条动它。
        assert!(call(all_on(), None, true, true, &test_cred()).is_none());
        // 5) 凭证没有 account_uuid：造不出自洽身份，连头也不补（否则头有体无）。
        let no_uuid = crate::credentials::Credential { account_uuid: None, ..test_cred() };
        assert!(call(all_on(), None, true, false, &no_uuid).is_none());
        // 6) 本功能自己的开关关着。
        let no_fill = store::ForwardFlags { fill_metadata: false, ..all_on() };
        assert!(call(no_fill, None, true, false, &test_cred()).is_none());
    }

    /// 模拟模式下来访那套头一个不留：UA/x-app/x-stainless-* 全是官方取值，
    /// 客户端自带的非官方头（`x-my-tool`）不转发，`anthropic-beta` 取并集。
    #[test]
    fn simulated_headers_replace_client_headers() {
        let mut client = super::HeaderMap::new();
        for (k, v) in [
            ("user-agent", "python-httpx/0.27.0"),
            ("accept", "text/event-stream"),
            ("x-my-tool", "cherry-studio"),
            ("anthropic-beta", "output-128k-2025-02-19"),
        ] {
            client.insert(super::HeaderName::from_static(k), HeaderValue::from_static(v));
        }
        let sim = sim_for(PLAIN_BODY);
        let out = build_forward_headers(&client, "sk-ant-oat01-REAL", all_on(), Some(&sim), None);
        let get = |k: &str| out.get(k).and_then(|v| v.to_str().ok()).unwrap_or_default();

        assert_eq!(get("user-agent"), config::CC_USER_AGENT);
        assert_eq!(get("accept"), "application/json", "官方即便流式也发 application/json");
        assert_eq!(get("x-app"), "cli");
        assert_eq!(get("x-stainless-os"), "MacOS");
        assert_eq!(get("anthropic-version"), "2023-06-01");
        assert_eq!(get("accept-encoding"), config::CC_ACCEPT_ENCODING);
        assert_eq!(get("x-claude-code-session-id"), sim.session_id, "会话 id 与 metadata 同值");
        assert!(!get("x-client-request-id").is_empty(), "每请求一个 uuid");
        assert_eq!(get("authorization"), "Bearer sk-ant-oat01-REAL");
        assert!(out.get("x-my-tool").is_none(), "客户端的非官方头不该带到上游");
        assert!(get("anthropic-beta").contains("output-128k-2025-02-19"), "客户端 beta 不该丢");
        assert!(get("anthropic-beta").contains(config::OAUTH_BETA_HEADER));

        // 表里的头全在，且没有多出表外的头（除四个由 HTTP 客户端自己追加的）。
        for (name, _) in config::CC_SIM_HEADERS {
            assert!(out.contains_key(*name), "缺头 {name}");
        }
    }

    /// 裸客户端的日志设备标识：只在真伪装过时才有值，且带 `sim:` 前缀以免被当成真实设备。
    #[test]
    fn logs_simulated_device_only_when_spoofed() {
        let sim = sim_for(PLAIN_BODY);
        let expect = format!("sim:{}", test_cred().spoof_device_id("fp").unwrap());
        let id = super::sim_device_id(Some(&sim), None, all_on(), &test_cred(), "fp").unwrap();
        assert_eq!(id, expect);

        // CC 形态补身份那条路（sim 为 None、bare_session 有值）同样把这个 id 发了出去，
        // 日志要记它——否则这段流量在库里只留下 `-`，无从聚合。
        let bare = super::sim_device_id(None, Some("sess"), all_on(), &test_cred(), "fp").unwrap();
        assert_eq!(bare, expect, "两条补身份的路径记的是同一个 id");

        // 两条路都没走（来访是 CC 形态且自带 metadata）→ 出站体里根本没有这个 id，不该记。
        assert!(super::sim_device_id(None, None, all_on(), &test_cred(), "fp").is_none());
        // spoof_identity 关着时同理：ensure_cc_metadata 不会写 metadata。
        let no_spoof = store::ForwardFlags { spoof_identity: false, ..all_on() };
        assert!(super::sim_device_id(Some(&sim), None, no_spoof, &test_cred(), "fp").is_none());
        // 凭证没有 account_uuid 就派生不出来，退回 `-`。
        let no_uuid = crate::credentials::Credential { account_uuid: None, ..test_cred() };
        assert!(super::sim_device_id(Some(&sim), None, all_on(), &no_uuid, "fp").is_none());
    }

    /// 429 作用域判定的回归用例，**头的取值逐字节取自两次真实的 fable-5 429**
    /// （基础 5h/7d 都有余量，满掉的只有 `7d_oi`——fable 专用的超额池）。
    ///
    /// 这条用例存在的理由：第二版判定把「任一窗口被拒/打满」一律判账号级，于是 fable
    /// 吃满超额池就把整个账号冷却 24 小时——实测 7d_oi 仍 rejected 期间同一账号的
    /// sonnet/opus 照常 200，账号级冷却纯属误伤。见 [`super::rate_limit_scope`] 的演化史。
    #[test]
    fn rate_limit_scope_reads_every_window_not_just_5h_7d() {
        let hdr = |pairs: &[(&str, &str)]| {
            let mut h = super::HeaderMap::new();
            for (k, v) in pairs {
                h.insert(
                    super::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    HeaderValue::from_str(v).unwrap(),
                );
            }
            super::RateLimitInfo::from_headers(&h)
        };
        let fable = Some("claude-fable-5");
        let real = hdr(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-representative-claim", "seven_day_overage_included"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.08"),
            ("anthropic-ratelimit-unified-7d-status", "allowed_warning"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.76"),
            ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
            ("anthropic-ratelimit-unified-7d_oi-utilization", "1.01"),
            ("retry-after", "228721"),
        ]);
        let scope = super::rate_limit_scope(&real, fable);
        assert_eq!(scope.model(), fable, "只有超额池（7d_oi）满 → 模型级，账号其余模型照常");
        // retry-after 优先且原样吃下（63 小时直指超额池的重置时刻）：睡满它、到点自己回池，
        // 中途放出去只会白撞 429——上限只挡明显异常的头，见 [`MAX_RATE_LIMIT_COOLDOWN_SECS`]。
        assert_eq!(real.cooldown(false).as_secs(), 228721);

        // 第二次抓包（2026-07-30，#54）：多了 overage-status 与 org_level_disabled，
        // 判定应当相同。overage 窗口被拒同样不算账号级。
        let real2 = hdr(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-representative-claim", "seven_day_overage_included"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.2"),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.7"),
            ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
            ("anthropic-ratelimit-unified-7d_oi-utilization", "1.02"),
            ("anthropic-ratelimit-unified-overage-status", "rejected"),
            ("retry-after", "304802"),
        ]);
        assert_eq!(super::rate_limit_scope(&real2, fable).model(), fable);

        // 基础窗口自己满掉才是账号级：5h 被拒 → 所有模型一起让位。
        let exhausted = hdr(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-utilization", "1.0"),
            ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
        ]);
        assert!(super::rate_limit_scope(&exhausted, fable).account_level());
        // 没有任何逐窗口明细时，unified-status=rejected 兜底判账号级——宁可保守。
        let unified_only = hdr(&[("anthropic-ratelimit-unified-status", "rejected")]);
        assert!(super::rate_limit_scope(&unified_only, fable).account_level());

        // 所有窗口都还有余量却被拒 → 这才是模型容量限制，只冷却该模型且不吃 reset。
        let far = crate::credentials::now_secs() as i64 + 4 * 3600;
        let capacity = hdr(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.32"),
            ("anthropic-ratelimit-unified-5h-reset", &far.to_string()),
            ("anthropic-ratelimit-unified-reset", &far.to_string()),
        ]);
        let scope = super::rate_limit_scope(&capacity, fable);
        assert_eq!(scope.model(), fable, "窗口都没满应判模型级");
        assert_eq!(capacity.cooldown(false).as_secs(), 30, "模型级不该拿 reset 当冷却");
        assert!(capacity.cooldown(true).as_secs() > 3000, "账号级才按 reset 冷却");

        // retry-after 两档都优先；读不出模型名保守退回账号级；什么头都没有用默认值。
        let with_retry = hdr(&[("retry-after", "7")]);
        assert_eq!(with_retry.cooldown(false).as_secs(), 7);
        assert_eq!(with_retry.cooldown(true).as_secs(), 7);
        let bare = hdr(&[]);
        assert!(super::rate_limit_scope(&bare, None).account_level());
        assert_eq!(bare.cooldown(true).as_secs(), 60);

        // 7d 窗口耗尽要睡满 7 天（冷却是硬门禁，中途放出去只会白撞）；离谱的头才被上限挡下。
        let seven_d = hdr(&[("retry-after", &(7 * 24 * 3600).to_string())]);
        assert_eq!(seven_d.cooldown(true).as_secs(), 7 * 24 * 3600);
        let absurd = hdr(&[("retry-after", "999999999")]);
        assert_eq!(absurd.cooldown(true).as_secs(), super::MAX_RATE_LIMIT_COOLDOWN_SECS as u64);
    }

    /// 落库展示用的全窗口快照：三张分开收集的表（status / utilization / reset）要按窗口名
    /// 合并回一份，且**窗口名不写死**——`7d_oi` 那类没有专用列的必须在里面，那正是这一列
    /// 存在的理由（见 [`store::QuotaWindow`]）。
    #[test]
    fn snapshot_windows_merge_every_reported_window() {
        let hdr = |pairs: &[(&str, &str)]| {
            let mut h = super::HeaderMap::new();
            for (k, v) in pairs {
                h.insert(
                    super::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    HeaderValue::from_str(v).unwrap(),
                );
            }
            super::RateLimitInfo::from_headers(&h)
        };
        // 逐字取自第二次真实的 fable-5 429（同 rate_limit_scope_reads_every_window_not_just_5h_7d）。
        let info = hdr(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-representative-claim", "seven_day_overage_included"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.2"),
            ("anthropic-ratelimit-unified-5h-reset", "9000"),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.7"),
            ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
            ("anthropic-ratelimit-unified-7d_oi-utilization", "1.02"),
            // 只报了 status、没有 utilization/reset 的窗口也不能漏。
            ("anthropic-ratelimit-unified-overage-status", "rejected"),
            ("retry-after", "304802"),
        ]);
        let windows = info.windows();
        let by = |n: &str| windows.iter().find(|w| w.name == n).unwrap_or_else(|| panic!("缺 {n}"));

        assert_eq!(windows.len(), 4, "5h / 7d / 7d_oi / overage 四个都要在：{windows:?}");
        assert_eq!(by("5h").utilization, Some(0.2));
        assert_eq!(by("5h").reset, Some(9_000));
        assert_eq!(by("5h").status.as_deref(), Some("allowed"));
        // 没有专用列的那个——这一列的全部意义所在。
        assert_eq!(by("7d_oi").utilization, Some(1.02));
        assert_eq!(by("7d_oi").status.as_deref(), Some("rejected"));
        assert_eq!(by("7d_oi").reset, None, "上游没给 reset 就该是空，不许编");
        // 三张表里只出现在 status 那张的窗口同样要被带出来。
        assert_eq!(by("overage").status.as_deref(), Some("rejected"));
        assert_eq!(by("overage").utilization, None);
        // 顺序即上游响应头里首次出现的顺序，前端照着渲染就是原序。
        assert_eq!(
            windows.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(),
            ["5h", "7d", "7d_oi", "overage"]
        );

        // 不带任何限流头的响应给出空列表——落库那侧靠它判断「要不要覆盖快照」。
        assert!(hdr(&[]).windows().is_empty());
        // 不带窗口名的 `…-unified-status` / `…-unified-reset` 不得造出一个名字为空的假窗口。
        let unified_only = hdr(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-reset", "9000"),
        ]);
        assert!(unified_only.windows().is_empty(), "{:?}", unified_only.windows());
    }

    /// **fable 撞 429 绝不能停用整个账号。**
    ///
    /// 这是一条真实事故的护栏：fable 走的是超额池（`7d_oi`），它满了的时候基础 5h/7d 还空着，
    /// 同一账号的 sonnet/opus 照常 200。把这种 429 判成账号级，等于因为一个模型没容量就把整个
    /// 号从调度池里摘掉——现在账号级还会**落库停用**，误伤代价比以前的进程内冷却大得多，
    /// 所以这里直接钉住 [`super::park_rate_limited`] 的落点，而不只是钉判定函数。
    #[test]
    fn model_level_429_never_disables_the_account() {
        let hdr = |kv: &[(&str, &str)]| {
            let mut h = super::HeaderMap::new();
            for (k, v) in kv {
                h.insert(
                    super::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    HeaderValue::from_str(v).unwrap(),
                );
            }
            super::RateLimitInfo::from_headers(&h)
        };
        let store = store::CredentialStore::open_in_memory().unwrap();
        let cred = store.insert("a", None, "at", "rt", u64::MAX, None, None).unwrap();
        let fable = Some("claude-fable-5");

        // 实测形态：只有超额池满，基础窗口都有余量。
        let oi_full = hdr(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-representative-claim", "seven_day_overage_included"),
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.20"),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.70"),
            ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
            ("anthropic-ratelimit-unified-7d_oi-utilization", "1.02"),
            ("retry-after", "304802"),
        ]);
        let scope = super::rate_limit_scope(&oi_full, fable);
        assert_eq!(scope.model(), fable, "超额池满只该判模型级");
        super::park_rate_limited(&store, &cred, &scope, oi_full.cooldown(false));

        let after = store.get(cred.id).unwrap().unwrap();
        assert!(!after.disabled, "fable 撞 429 不该停用整个账号");
        assert!(after.resume_at.is_none(), "更不该写恢复时刻——账号压根没被停");
        assert_eq!(after.ban_reason, None, "卡片上不该显示成这个号出了问题");
        // 但 fable 自己确实要让位，而 sonnet 照常可用。
        let pick = |m| {
            store.select_for_device(store::Select { model: Some(m), ..Default::default() }).is_ok()
        };
        assert!(!pick("claude-fable-5"), "fable 应被模型级冷却挡下");
        assert!(pick("claude-sonnet-5"), "同一个号的 sonnet 不该被牵连");

        // 对照组：基础窗口真耗尽才落库停用，并写下到点自动恢复的时刻。
        let base_gone = hdr(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-utilization", "1.0"),
            ("retry-after", "3600"),
        ]);
        let scope = super::rate_limit_scope(&base_gone, fable);
        assert!(scope.account_level(), "基础窗口耗尽才是账号级");
        super::park_rate_limited(&store, &cred, &scope, base_gone.cooldown(true));

        let after = store.get(cred.id).unwrap().unwrap();
        assert!(after.disabled, "额度真耗尽才关调度开关");
        let resume_at = after.resume_at.expect("应写下自动恢复时刻");
        let wait = resume_at as i64 - crate::credentials::now_secs() as i64;
        assert!((3595..=3600).contains(&wait), "恢复时刻应取上游给的等待时间，实得 {wait}");
        assert!(after.ban_reason.unwrap().contains("1h"), "停用原因该写清楚还要等多久");
    }

    /// 冷却睡到**上游返回的那个重置时刻**，不是写死的 5 小时/7 天：没有 `retry-after` 时，
    /// 取被拒的那个基础窗口自己的 `*-reset`，而不是 `unified-reset`、也不是最早的那个。
    #[test]
    fn account_cooldown_sleeps_until_the_exhausted_window_reset() {
        let hdr = |kv: &[(&str, &str)]| {
            let mut h = super::HeaderMap::new();
            for (k, v) in kv {
                h.insert(
                    super::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                    HeaderValue::from_str(v).unwrap(),
                );
            }
            super::RateLimitInfo::from_headers(&h)
        };
        let now = crate::credentials::now_secs() as i64;
        let at = |secs: i64| (now + secs).to_string();

        // 5h 打满、7d 还有余量：该睡到 5h 自己的 reset（这里剩 2 小时，不是「5 小时」），
        // 而不是 unified-reset 说的 9 小时、也不是 7d 的 30 小时。
        //
        // `retry-after` 也故意给了，且比 5h reset 少一分钟——线上真实的对不上就长这样：
        // 它是相对秒数（上游向下取整、还要锚回本地时钟），reset 是绝对时刻，两者口径不同。
        // 账号级必须吃 reset，否则卡片会一边写「12:20 重置」一边写「12:19 恢复」。
        let five_h_gone = hdr(&[
            ("anthropic-ratelimit-unified-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-utilization", "1.0"),
            ("anthropic-ratelimit-unified-5h-reset", &at(2 * 3600)),
            ("anthropic-ratelimit-unified-7d-status", "allowed"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.4"),
            ("anthropic-ratelimit-unified-7d-reset", &at(30 * 3600)),
            ("anthropic-ratelimit-unified-reset", &at(9 * 3600)),
            ("retry-after", &(2 * 3600 - 60).to_string()),
        ]);
        assert!(super::rate_limit_scope(&five_h_gone, Some("claude-sonnet-5")).account_level());
        let secs = five_h_gone.cooldown(true).as_secs() as i64;
        assert!(
            (2 * 3600 - 5..=2 * 3600).contains(&secs),
            "应睡到 5h 窗口的 reset（而非 retry-after 的 {}），实得 {secs}",
            2 * 3600 - 60
        );
        // 模型级那档没有「哪个窗口满了」可言，仍旧只认 retry-after。
        assert_eq!(five_h_gone.cooldown(false).as_secs() as i64, 2 * 3600 - 60);

        // 两个基础窗口都满 → 取**最晚**的那个：5h 到点了 7d 照样拦着，早醒只是白撞一发。
        let both_gone = hdr(&[
            ("anthropic-ratelimit-unified-5h-status", "rejected"),
            ("anthropic-ratelimit-unified-5h-reset", &at(2 * 3600)),
            ("anthropic-ratelimit-unified-7d-status", "rejected"),
            ("anthropic-ratelimit-unified-7d-reset", &at(50 * 3600)),
        ]);
        let secs = both_gone.cooldown(true).as_secs() as i64;
        assert!((50 * 3600 - 5..=50 * 3600).contains(&secs), "应睡到较晚的 7d reset，实得 {secs}");

        // 满的只有超额池：那不是账号额度耗尽，它的 reset 不该被当成账号冷却
        // （判定本身也是模型级，这里只钉住 reset 口径不被超额窗口污染）。
        let oi_gone = hdr(&[
            ("anthropic-ratelimit-unified-5h-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-reset", &at(3 * 3600)),
            ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
            ("anthropic-ratelimit-unified-7d_oi-reset", &at(60 * 3600)),
        ]);
        let secs = oi_gone.cooldown(true).as_secs() as i64;
        assert!(
            (3 * 3600 - 5..=3 * 3600).contains(&secs),
            "超额池的 reset 不该当账号冷却，实得 {secs}"
        );
    }

    /// 基座资产是逐字节从抓包取出来的，别被编辑器/格式化工具动过。
    #[test]
    fn system_base_assets_are_verbatim() {
        assert_eq!(config::CC_SYSTEM_BASE_OPUS.len(), 1214, "opus 族基座字节数（cap/raw/00006）");
        assert_eq!(
            config::CC_SYSTEM_BASE_SONNET.len(),
            10682,
            "sonnet 族基座字节数（cap/raw/00009）"
        );
        assert_eq!(config::CC_SYSTEM_IDENTITY.len(), 57, "身份句字节数");
        for base in [config::CC_SYSTEM_BASE_OPUS, config::CC_SYSTEM_BASE_SONNET] {
            assert!(
                base.starts_with("\nYou are an interactive agent"),
                "开头那个 \\n 是官方就有的"
            );
            assert!(!base.ends_with('\n'), "结尾多出的换行是编辑器加的，官方没有");
        }
        // 基座是「切点之前」那一段，锚点属于其余段，不该出现在基座里。
        for anchor in config::CC_SYSTEM_BASE_ANCHORS {
            assert!(!config::CC_SYSTEM_BASE_OPUS.contains(anchor), "基座里不该有拆块锚点");
        }
    }

    /// 由 `k=v` 造一份限流头解析结果，给下面两个测试共用。
    fn rl_headers(pairs: &[(&str, &str)]) -> super::RateLimitInfo {
        let mut h = super::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                super::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        super::RateLimitInfo::from_headers(&h)
    }

    /// 测试结果里的额度快照直接来自本次响应的限流头（200 与 429 都带）；而响应压根没有这些
    /// 头时给 `None` 而不是一坨全空对象——CDN 拦截页、网关错误就是那样，前端不该被迫自己
    /// 再判一遍「是不是全空」。
    #[test]
    fn probe_quota_reads_ratelimit_headers() {
        let hdr = rl_headers;

        let info = hdr(&[
            ("anthropic-ratelimit-unified-status", "allowed_warning"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.32"),
            ("anthropic-ratelimit-unified-5h-reset", "1800000000"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.76"),
            ("anthropic-ratelimit-unified-representative-claim", "7d"),
            ("retry-after", "228721"),
        ]);
        let q = super::ProbeQuota::from_info(&info).expect("有限流头就该有快照");
        assert_eq!(q.unified_status.as_deref(), Some("allowed_warning"));
        assert_eq!(q.rl_5h_utilization, Some(0.32));
        assert_eq!(q.rl_5h_reset, Some(1_800_000_000));
        assert_eq!(q.rl_7d_utilization, Some(0.76));
        assert_eq!(q.rl_representative.as_deref(), Some("7d"));
        assert_eq!(q.retry_after_secs, Some(228_721), "429 的等待时间原样带出，不夹");

        // 非限流类的 anthropic- 头会被 RateLimitInfo 收进 raw，但解析不出任何额度字段。
        assert!(
            super::ProbeQuota::from_info(&hdr(&[("anthropic-version", "2023-06-01")])).is_none()
        );
        assert!(super::ProbeQuota::from_info(&hdr(&[])).is_none());
    }

    /// 测试要能让**卡片**跟着更新：卡片上的额度快照来自 `latest_quota`，而那读的是
    /// `usage_logs` 里最新一条带限流信息的行。所以探测必须落一条日志——否则测出来的额度
    /// 只活在弹窗里，卡片照旧显示上一次真实请求时的旧数，两处对不上。
    ///
    /// 同时钉住另外两件事：这条日志按**实际用量**计价（测试真的花了钱，不记等于让累计花费
    /// 虚低），且以 `device_id = "probe"` 标出，翻日志时能与真实流量分开。
    #[test]
    fn probe_usage_log_feeds_the_card_quota() {
        // Arc 包着：落库现在走 spawn_blocking（见 `spawn_usage_log`），要能把 store 交出去。
        // 这个测试不在 tokio 运行时里，故 `Handle::try_current` 失败、退回就地同步写——
        // 下面的断言因此仍能立刻读到结果。
        let store = std::sync::Arc::new(crate::store::CredentialStore::open_in_memory().unwrap());
        let cred = store.insert("t", None, "a", "r", 0, None, None).unwrap();
        let info = rl_headers(&[
            ("anthropic-ratelimit-unified-status", "allowed"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.32"),
            ("anthropic-ratelimit-unified-5h-reset", "1800000000"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.76"),
        ]);
        // 上游 200 的响应体形状（只留计价要用的字段）。
        let body = Bytes::from(
            r#"{"model":"claude-opus-5-20260115","usage":{"input_tokens":320,"output_tokens":1}}"#,
        );
        super::ProbeLog {
            store: &store,
            cred: &cred,
            req_model: "claude-opus-5",
            started: &std::time::Instant::now(),
            out_ua: Some(config::CC_USER_AGENT.into()),
        }
        .record(StatusCode::OK, &body, &info);

        let q = store.latest_quota(cred.id).unwrap().expect("卡片应能读到这次测试的额度");
        assert_eq!(q.rl_5h_utilization, Some(0.32));
        assert_eq!(q.rl_7d_utilization, Some(0.76));
        assert_eq!(q.unified_status.as_deref(), Some("allowed"));

        let logs = store.list_usage_logs(10).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].device_id.as_deref(), Some("probe"), "日志里要能认出这是测试");
        assert_eq!(logs[0].model.as_deref(), Some("claude-opus-5-20260115"), "模型以上游回报为准");
        assert_eq!(logs[0].input_tokens, Some(320));
        // opus $5/MTok 输入 + $25/MTok 输出：320×5 + 1×25 = 1625 微美元。
        assert_eq!(logs[0].cost_usd, Some(0.001625), "按实际用量计价，不是记 0");
        // 测试没有来访客户端，但确实按官方形态发了出去：入站空、出站照实。
        assert_eq!(logs[0].ua, None, "测试不来自任何客户端，入站 UA 必须为空");
        assert_eq!(logs[0].ua_out.as_deref(), Some(config::CC_USER_AGENT), "出站照实记");
    }

    /// 两份 UA 各存各的：入站记来访那份、出站记实际发出去那份，`-` 占位一律还原成 NULL
    /// （存进去就成了一个真实存在的 UA，按 UA 分组时会凭空多出一类）。
    #[test]
    fn client_ua_lands_in_the_usage_log() {
        let store = std::sync::Arc::new(crate::store::CredentialStore::open_in_memory().unwrap());
        let cred = store.insert("t", None, "a", "r", 0, None, None).unwrap();
        let log = |ua: &str, ua_out: &str| {
            drop(super::ReqLog {
                started: std::time::Instant::now(),
                ttft_ms: None,
                method: "POST".into(),
                path: "/v1/messages?beta=true".into(),
                ua: ua.into(),
                ua_out: ua_out.into(),
                cred_id: cred.id,
                cred_label: cred.label.clone(),
                device_id: None,
                status: 200,
                sse_aggregated: false,
                sniffer: super::UsageSniffer::new(false, false),
                req_speed: None,
                req_model: None,
                ratelimit: rl_headers(&[]),
                stream_broke: None,
                store: store.clone(),
            })
        };
        // 非模拟路径：来访那份原样转发，两列相同。
        log(config::CC_USER_AGENT, config::CC_USER_AGENT);
        // 模拟路径：来访是第三方客户端，出站换成官方那串——正是分两列才看得见的东西。
        log("python-httpx/0.27.0", config::CC_USER_AGENT);
        // 两边都没有（裸请求且开关关到不补头）。
        log("-", "-");

        // 倒序：后写的那条在前。
        let logs = store.list_usage_logs(10).unwrap();
        assert_eq!(logs.len(), 3);
        assert_eq!(logs[0].ua, None, "没带 UA 的请求不该存成 `-`");
        assert_eq!(logs[0].ua_out, None);
        assert_eq!(logs[1].ua.as_deref(), Some("python-httpx/0.27.0"), "来访那份是第三方客户端");
        assert_eq!(logs[1].ua_out.as_deref(), Some(config::CC_USER_AGENT), "出站换成了官方那串");
        assert_eq!(logs[2].ua.as_deref(), Some(config::CC_USER_AGENT));
        assert_eq!(logs[2].ua_out.as_deref(), Some(config::CC_USER_AGENT));
    }

    /// 透传流路径（`sse_aggregated=false`，绝大多数请求走这条）上，上游在 200 的流中途
    /// 改口报错：客户端已经收到 200 头，改不动，但**记账**要按真实结果走。
    ///
    /// 这条曾是纯盲区。线上实例的原始形态是：`message_start` 与 `message_delta` 都到了，
    /// 随后上游发 `event: error`，我们原样透传，客户端报错，而服务端只留下一行
    /// `forwarded status=200 has_usage=true`，还照常算了花费——唯一的线索是
    /// `output_tokens` 小得离谱（实测那次是 2）。
    #[test]
    fn mid_stream_error_is_billed_as_the_mapped_status() {
        let store = std::sync::Arc::new(crate::store::CredentialStore::open_in_memory().unwrap());
        let cred = store.insert("t", None, "a", "r", 0, None, None).unwrap();
        let mut rl = super::ReqLog {
            started: std::time::Instant::now(),
            ttft_ms: None,
            method: "POST".into(),
            path: "/v1/messages?beta=true".into(),
            ua: "-".into(),
            ua_out: "-".into(),
            cred_id: cred.id,
            cred_label: cred.label.clone(),
            device_id: None,
            status: 200,
            sse_aggregated: false,
            sniffer: super::UsageSniffer::new(true, false),
            req_speed: None,
            req_model: None,
            ratelimit: rl_headers(&[]),
            stream_broke: None,
            store: store.clone(),
        };
        rl.sniffer.feed(
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5\",\"usage\":{\"input_tokens\":2,\"cache_read_input_tokens\":47030}}}\n\n",
        );
        rl.sniffer.feed(
            b"event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
        );
        drop(rl);

        let logs = store.list_usage_logs(10).unwrap();
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].status, 529, "流中途报错不能记成 200——失败会从成功率里消失");
        assert_eq!(logs[0].model.as_deref(), Some("claude-opus-5"), "用量照旧嗅探，不受影响");
        assert_eq!(logs[0].cache_read_tokens, Some(47030));
    }

    /// `message_stop` 是流正常收尾的唯一标志。缺了它、又没有 error 事件、连接层也没报错，
    /// 是最安静的那种断流：这一层看什么都正常，客户端拿到的却是半截回复
    /// （Claude Code 报 `Connection closed mid-response`）。
    #[test]
    fn message_stop_marks_a_complete_stream() {
        let mut truncated = super::UsageSniffer::new(true, false);
        truncated.feed(
            b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"model\":\"claude-opus-5\"}}\n\n",
        );
        truncated.feed(
            b"event: message_delta\ndata: {\"type\":\"message_delta\",\"usage\":{\"output_tokens\":14}}\n\n",
        );
        assert!(!truncated.saw_message_stop, "流断在半路，收尾时要告警");
        assert_eq!(truncated.output_tokens, Some(14), "已生成的部分照旧计入用量");
        // 断点定位：光看 output_tokens 分不出「刚开口就断」和「只差收尾」，事件类型才行。
        assert_eq!(truncated.last_event.as_deref(), Some("message_delta"));
        assert_eq!(truncated.events, 2);

        let mut complete = super::UsageSniffer::new(true, false);
        complete.feed(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
        assert!(complete.saw_message_stop);
        assert_eq!(complete.last_event.as_deref(), Some("message_stop"));
    }

    /// 非流式响应体里的 `{"type":"error"}` 不走流内那套：那条路的 4xx 由
    /// [`super::detect_account_ban`] 一侧处理，这里再记一份会让同一个错误告警两次。
    #[test]
    fn nonstream_error_body_is_not_taken_as_a_stream_error() {
        let mut s = super::UsageSniffer::new(false, false);
        s.feed(br#"{"type":"error","error":{"type":"invalid_request_error","message":"nope"}}"#);
        s.finish();
        assert!(s.stream_error.is_none());
    }

    /// 日志用的 UA 取值：缺失/空串取 `-`，过长按 char 截断（不能按字节切，会劈开多字节 UTF-8）。
    #[test]
    fn client_ua_falls_back_and_truncates() {
        let ua = |v: Option<&str>| {
            let mut h = super::HeaderMap::new();
            if let Some(v) = v {
                h.insert(super::header::USER_AGENT, HeaderValue::from_str(v).unwrap());
            }
            super::ua_of(&h)
        };
        assert_eq!(ua(None), "-", "没有该头");
        assert_eq!(ua(Some("   ")), "-", "空白等于没带");
        assert_eq!(ua(Some(config::CC_USER_AGENT)), config::CC_USER_AGENT, "正常那串原样保留");
        let long = "a".repeat(300);
        assert_eq!(ua(Some(&long)).len(), 120, "超长的截到 120");
        // 非 ASCII 的头值 `to_str()` 直接失败，落回 `-`——所以库里存的 UA 恒为可见 ASCII。
        let cjk = super::HeaderValue::from_bytes("中文客户端".as_bytes()).unwrap();
        let mut h = super::HeaderMap::new();
        h.insert(super::header::USER_AGENT, cjk);
        assert_eq!(super::ua_of(&h), "-");
    }

    /// 出站 URL 上那个 `?beta=true`：官方 `cap/raw` 八份抓包的请求行全带，Anthropic 公开 API
    /// 里却没有这个参数——它是 CC 客户端自己的标记，故只在模拟路径上补。
    #[test]
    fn appends_official_beta_query() {
        let base = "https://api.anthropic.com/v1/messages";
        assert_eq!(super::ensure_beta_query(base), format!("{base}?beta=true"), "没有查询串就加 ?");
        assert_eq!(
            super::ensure_beta_query(&format!("{base}?foo=1")),
            format!("{base}?foo=1&beta=true"),
            "已有查询串就接 &"
        );

        // 客户端自己写了 beta= 的一律不动——包括它显式关掉的情形。
        for already in ["?beta=true", "?beta=false", "?foo=1&beta=true", "?beta=true&foo=1"] {
            let url = format!("{base}{already}");
            assert_eq!(super::ensure_beta_query(&url), url, "客户端自己的 beta= 被改写了");
        }
        // `betas=`/`xbeta=` 不是 `beta=`，不该被当成已有。
        assert!(super::ensure_beta_query(&format!("{base}?betas=1")).ends_with("&beta=true"));
        assert!(super::ensure_beta_query(&format!("{base}?xbeta=1")).ends_with("&beta=true"));
    }

    /// 连通性测试发出去的那条请求本身必须是**官方形态**：`system` 是官方那几块（含上游对
    /// OAuth 凭证唯一强制的那句身份声明）、`metadata` 是该凭证自洽的身份、`anthropic-beta`
    /// 带 `oauth-2025-04-20`、`Authorization` 是该凭证的 token。
    ///
    /// 真正盯的是**测试与真实转发共用同一套改写**：`probe` 只给一个裸 body，剩下的全交给
    /// [`super::rewrite_body`]/[`super::build_forward_headers`]。若哪天有人图省事在 probe 里
    /// 手抄一份 system，改写规则一变就会得到「测试通过但转发失败」——那比没有这个功能更糟。
    #[test]
    fn probe_request_is_official_shaped() {
        let cred = test_cred();
        let sim = super::Simulation {
            base: super::cc_system_base("claude-opus-5"),
            beta: super::cc_beta_seed("claude-opus-5"),
            session_id: "sess".into(),
        };
        let out = rewrite_body(
            &super::probe_body("claude-opus-5"),
            &cred,
            "fp",
            all_on(),
            Some(&sim),
            None,
        );
        let s = String::from_utf8(out.to_vec()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();

        // 顶层 key 序：官方是 model→messages→system→metadata→max_tokens。
        // probe 不开 thinking（一条 1 token 的探测不需要），故也不带 `context_management`
        // ——那个字段依赖 thinking，硬补上游回 400，见 [`super::ensure_context_management`]。
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["model", "messages", "system", "metadata", "max_tokens"], "\n{s}");
        assert!(v.get("context_management").is_none(), "没开 thinking 就不该补: {s}");
        assert_eq!(v["max_tokens"], 1, "测试只要 1 个 token，别把额度花在正文上");

        // 官方前三块：billing header、身份句、按模型族选出的基座。测试请求没有「客户端自己
        // 的 system」，故第四块不存在。
        let blocks = v["system"].as_array().unwrap();
        assert_eq!(blocks.len(), 3, "\n{s}");
        assert!(blocks[0]["text"].as_str().unwrap().starts_with("x-anthropic-billing-header:"));
        assert_eq!(blocks[1]["text"], config::CC_SYSTEM_IDENTITY, "缺这句就用不了订阅额度");
        assert_eq!(blocks[2]["text"], config::CC_SYSTEM_BASE_OPUS, "opus 族基座");

        // 身份：伪装 metadata 用的是这个凭证的 account_uuid，不是空串。
        let user_id = v["metadata"]["user_id"].as_str().unwrap();
        assert!(user_id.contains(ACCOUNT_UUID), "metadata 应带该凭证的 account_uuid: {user_id}");

        let headers = super::build_forward_headers(
            &super::HeaderMap::new(),
            "tok",
            all_on(),
            Some(&sim),
            None,
        );
        let beta = headers.get("anthropic-beta").unwrap().to_str().unwrap();
        assert!(
            beta.split(',').any(|p| p == config::OAUTH_BETA_HEADER),
            "OAuth 鉴权必需这一项: {beta}"
        );
        assert_eq!(headers.get(header::AUTHORIZATION).unwrap(), "Bearer tok");
        assert_eq!(
            headers.get(header::USER_AGENT).unwrap(),
            config::CC_USER_AGENT,
            "测试请求同样按官方客户端形态发"
        );
    }
    // ---------- 非流式改流式 + SSE 聚合 ----------

    /// `stream` 的判定口径：只有布尔 `true` 算流式。字符串 `"true"`、数字、缺失都不是——
    /// 上游那边它们同样回整段 JSON，判据跟着响应形态走才不会错配。
    #[test]
    fn stream_requested_only_counts_boolean_true() {
        let case = |body: &str| super::stream_requested(&serde_json::from_str(body).unwrap());
        assert!(case(r#"{"stream":true}"#));
        assert!(!case(r#"{"stream":false}"#));
        assert!(!case(r#"{"model":"claude-opus-5"}"#), "字段缺失 = 非流式");
        assert!(!case(r#"{"stream":"true"}"#), "字符串不算");
        assert!(!case(r#"{"stream":1}"#), "数字不算");
    }

    /// 流式化把 `stream` 置成 `true`，且落在官方 key 序该在的位置（队尾）：
    /// 来访带了就原位改值，没带就追加——两条路都与官方线序一致。
    #[test]
    fn forces_stream_true_and_keeps_key_order() {
        let keys = |bytes: &Bytes| {
            let v: serde_json::Value = serde_json::from_slice(bytes).unwrap();
            v.as_object().unwrap().keys().cloned().collect::<Vec<_>>()
        };
        // 只开流式化这一项，确保观察到的差异只来自它。
        let only_stream = store::ForwardFlags {
            spoof_identity: false,
            system_shape: false,
            billing_cch: false,
            ..all_on()
        };
        let call = |body: &str| {
            super::rewrite_body(
                &Bytes::from(body.to_string()),
                &test_cred(),
                "fp",
                only_stream,
                None,
                None,
                true,
                None,
            )
        };

        // 1) 来访压根没带 `stream`：追加到末尾（官方线序里它就是最后一个）。
        let out = call(r#"{"model":"claude-opus-5","messages":[],"max_tokens":64}"#);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["stream"], serde_json::json!(true));
        assert_eq!(keys(&out), vec!["model", "messages", "max_tokens", "stream"]);

        // 2) 来访带了 `stream:false`：原位改值，位置不动。
        let out = call(r#"{"model":"claude-opus-5","stream":false,"max_tokens":64}"#);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["stream"], serde_json::json!(true));
        assert_eq!(keys(&out), vec!["model", "stream", "max_tokens"], "已有字段不该挪位置");

        // 3) 开关关着：一个字节都不动（哪怕 body 是非流式的）。
        let untouched = super::rewrite_body(
            &Bytes::from(r#"{"model":"claude-opus-5","stream":false}"#.to_string()),
            &test_cred(),
            "fp",
            only_stream,
            None,
            None,
            false,
            None,
        );
        assert_eq!(
            untouched,
            Bytes::from(r#"{"model":"claude-opus-5","stream":false}"#.to_string())
        );
    }

    /// 把一串 SSE 文本喂给聚合器；`chunk` 是每次喂的字节数，用来构造跨块断行。
    fn aggregate(sse: &str, chunk: usize) -> super::Aggregated {
        let mut agg = super::SseAggregator::default();
        for part in sse.as_bytes().chunks(chunk.max(1)) {
            agg.feed(part);
        }
        agg.finish()
    }

    fn aggregated_message(sse: &str, chunk: usize) -> serde_json::Value {
        match aggregate(sse, chunk) {
            super::Aggregated::Message(v) => v,
            super::Aggregated::UpstreamError(e) => panic!("不该判成上游错误: {e}"),
            super::Aggregated::Incomplete(why) => panic!("不该判成不完整: {why}"),
        }
    }

    /// 一条典型的文本流：文本增量拼接、`message_delta` 的 stop_reason 与 usage 合进顶层。
    ///
    /// **逐字节喂一遍**：真实网络下 SSE 的分块与行边界毫无关系，聚合器必须自己攒行。
    #[test]
    fn aggregates_a_text_stream() {
        let sse = concat!(
            "event: message_start\n",
            r#"data: {"type":"message_start","message":{"id":"msg_1","type":"message","role":"assistant","model":"claude-opus-5","content":[],"stop_reason":null,"usage":{"input_tokens":10,"cache_read_input_tokens":5,"output_tokens":1}}}"#,
            "\n\n",
            "event: ping\ndata: {\"type\":\"ping\"}\n\n",
            "event: content_block_start\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"你好"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"，世界"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "\n\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn","stop_sequence":null},"usage":{"output_tokens":42}}"#,
            "\n\n",
            r#"data: {"type":"message_stop"}"#,
            "\n\n",
        );

        for chunk in [1usize, 7, 4096] {
            let v = aggregated_message(sse, chunk);
            assert_eq!(v["id"], "msg_1", "chunk={chunk}");
            assert_eq!(v["type"], "message");
            assert_eq!(v["content"][0]["type"], "text");
            assert_eq!(v["content"][0]["text"], "你好，世界", "文本增量要按序拼接");
            assert_eq!(v["stop_reason"], "end_turn", "message_delta 的字段合进顶层");
            assert_eq!(v["usage"]["output_tokens"], 42, "usage 逐键覆盖");
            assert_eq!(v["usage"]["input_tokens"], 10, "message_start 里没被覆盖的键要留着");
            assert_eq!(v["usage"]["cache_read_input_tokens"], 5);
        }
    }

    /// tool_use 的入参是分片 JSON 串，攒到 `content_block_stop` 整体解析；
    /// thinking 块的正文与签名各自拼接；未知块类型原样透传。
    #[test]
    fn aggregates_tool_use_thinking_and_unknown_blocks() {
        let sse = concat!(
            r#"data: {"type":"message_start","message":{"id":"msg_2","content":[],"usage":{}}}"#,
            "\n\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":""}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"先想一下"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig-"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"abc"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "\n\n",
            r#"data: {"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather","input":{}}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"\"上海\"}"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_stop","index":1}"#,
            "\n\n",
            r#"data: {"type":"content_block_start","index":2,"content_block":{"type":"some_future_block","payload":{"k":1}}}"#,
            "\n\n",
            r#"data: {"type":"content_block_stop","index":2}"#,
            "\n\n",
            r#"data: {"type":"message_stop"}"#,
            "\n\n",
        );
        let v = aggregated_message(sse, 5);

        assert_eq!(v["content"][0]["thinking"], "先想一下");
        assert_eq!(v["content"][0]["signature"], "sig-abc", "签名同样是分片拼接");
        assert_eq!(v["content"][1]["name"], "get_weather");
        assert_eq!(
            v["content"][1]["input"],
            serde_json::json!({"city": "上海"}),
            "分片 JSON 要在 content_block_stop 时整体解析成 input"
        );
        assert_eq!(
            v["content"][2],
            serde_json::json!({"type":"some_future_block","payload":{"k":1}}),
            "认不出来的块类型原样收下——上游新增块类型时这里不该跟着改"
        );
    }

    /// 认不出来的 `delta.type` 不能把整条响应带崩：那一块的内容丢掉，其余照常攒完。
    /// （丢内容这件事本身在 [`super::SseAggregator::apply_delta`] 里另打 warn。）
    #[test]
    fn unknown_delta_type_does_not_break_aggregation() {
        let sse = concat!(
            r#"data: {"type":"message_start","message":{"id":"msg_3","content":[],"usage":{}}}"#,
            "\n\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"future_delta","whatever":"x"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"ok"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "\n\n",
            r#"data: {"type":"message_stop"}"#,
            "\n\n",
        );
        let v = aggregated_message(sse, 64);
        assert_eq!(v["content"][0]["text"], "ok", "认识的增量照样要攒上");
    }

    /// 流中 `event: error`：整份 error 负载原样交出去（回程拿它当响应体；状态码另按
    /// [`super::error_status`] 映射，见 `mid_stream_error_maps_to_the_non_streaming_status`）。
    #[test]
    fn mid_stream_error_payload_is_surfaced_as_is() {
        let sse = concat!(
            r#"data: {"type":"message_start","message":{"id":"msg_4","content":[],"usage":{}}}"#,
            "\n\n",
            "event: error\n",
            r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            "\n\n",
        );
        match aggregate(sse, 3) {
            super::Aggregated::UpstreamError(e) => {
                assert_eq!(e["error"]["type"], "overloaded_error");
                assert_eq!(e["type"], "error", "整份 data 原样带走，形状与非流式错误体一致");
            }
            other => panic!(
                "应判成上游错误，实际: {}",
                match other {
                    super::Aggregated::Message(_) => "Message",
                    super::Aggregated::Incomplete(_) => "Incomplete",
                    super::Aggregated::UpstreamError(_) => unreachable!(),
                }
            ),
        }
    }

    /// 流中错误的状态码映射：与非流式那条路上同一个错误该有的状态码一致——开不开这个功能，
    /// 客户端看到的状态码都一样。认不出来的类型兜底 500，**不能是 200**：那会把一次失败
    /// 记成成功，客户端与统计两边都被带偏。
    #[test]
    fn mid_stream_error_maps_to_the_non_streaming_status() {
        let status = |kind: &str| {
            super::error_status(&serde_json::json!({"type":"error","error":{"type":kind}})).as_u16()
        };
        assert_eq!(status("invalid_request_error"), 400);
        assert_eq!(status("authentication_error"), 401);
        assert_eq!(status("permission_error"), 403);
        assert_eq!(status("billing_error"), 403);
        assert_eq!(status("not_found_error"), 404);
        assert_eq!(status("request_too_large"), 413);
        assert_eq!(status("timeout_error"), 408);
        assert_eq!(status("rate_limit_error"), 429);
        assert_eq!(status("api_error"), 500);
        assert_eq!(status("overloaded_error"), 529, "529 不在常量表里，按数字构造");
        assert_eq!(status("something_new_2027"), 500, "认不出来的一律 500");
        // 连 `error` 字段都没有的畸形负载同样按 500，绝不退回 200。
        assert_eq!(super::error_status(&serde_json::json!({"type":"error"})).as_u16(), 500);
    }

    /// 端到端：上游在流中报 overloaded → 客户端拿到 529 + 那份错误 JSON 原文。
    #[tokio::test]
    async fn mid_stream_error_reaches_the_client_with_a_mapped_status() {
        let sse = concat!(
            r#"data: {"type":"message_start","message":{"id":"msg_err","content":[],"usage":{}}}"#,
            "\n\n",
            "event: error\n",
            r#"data: {"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            "\n\n",
        );
        let (status, ctype, body) = relay_sse(sse).await;

        assert_eq!(status.as_u16(), 529, "上游那个 200 不能照搬——它其实是一次失败");
        assert_eq!(ctype.as_deref(), Some("application/json"));
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["type"], "overloaded_error", "错误原文原样交给客户端");
        assert_eq!(v["error"]["message"], "Overloaded");
    }

    /// 没收到 `message_stop` 就断了 → 判不完整（回程 502）。
    ///
    /// **绝不能把攒了一半的内容当完整响应回去**：客户端拿到的会是一条看着正常、实则被截断的
    /// Message，比一个明确的错误糟得多——它会被当成模型的真实输出写进会话历史。
    #[test]
    fn truncated_stream_is_incomplete_not_a_partial_message() {
        let cut = concat!(
            r#"data: {"type":"message_start","message":{"id":"msg_5","content":[],"usage":{}}}"#,
            "\n\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"半句"}}"#,
            "\n\n",
        );
        assert!(matches!(aggregate(cut, 9), super::Aggregated::Incomplete(_)));
        // 一个事件都没来（比如连上就断）同样是不完整，不是空 Message。
        assert!(matches!(aggregate("", 1), super::Aggregated::Incomplete(_)));
    }
    /// 端到端走一遍聚合回程：起一个吐 SSE 的本地上游，`aggregate_sse` 必须回一条
    /// `content-type: application/json` 的整段 Message——客户端本来就是按非流式发的，
    /// 它认的是这个形态。
    #[tokio::test]
    async fn aggregated_response_is_a_single_json_message() {
        let sse = concat!(
            "event: message_start\n",
            r#"data: {"type":"message_start","message":{"id":"msg_e2e","type":"message","role":"assistant","model":"claude-sonnet-5","content":[],"usage":{"input_tokens":9}}}"#,
            "\n\n",
            "event: content_block_start\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"pong"}}"#,
            "\n\n",
            r#"data: {"type":"content_block_stop","index":0}"#,
            "\n\n",
            r#"data: {"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":3}}"#,
            "\n\n",
            r#"data: {"type":"message_stop"}"#,
            "\n\n",
        );
        let (status, ctype, body) = relay_sse(sse).await;

        assert_eq!(status, super::StatusCode::OK);
        assert_eq!(
            ctype.as_deref(),
            Some("application/json"),
            "上游那份 text/event-stream 必须被替掉"
        );
        let v: serde_json::Value =
            serde_json::from_slice(&body).expect("回给客户端的必须是整段 JSON");
        assert_eq!(v["id"], "msg_e2e");
        assert_eq!(v["content"][0]["text"], "pong");
        assert_eq!(v["stop_reason"], "end_turn");
        assert_eq!(v["usage"]["output_tokens"], 3);
        assert_eq!(v["usage"]["input_tokens"], 9);
    }

    /// 流断在半路 → 502，且**不带**攒了一半的内容：截断的 Message 会被客户端当成模型的
    /// 真实输出写进会话历史，比一个明确的错误糟得多。
    #[tokio::test]
    async fn truncated_upstream_stream_yields_502() {
        let sse = concat!(
            r#"data: {"type":"message_start","message":{"id":"msg_cut","content":[],"usage":{}}}"#,
            "\n\n",
            r#"data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":""}}"#,
            "\n\n",
            r#"data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"半句"}}"#,
            "\n\n",
        );
        let (status, _, body) = relay_sse(sse).await;

        assert_eq!(status, super::StatusCode::BAD_GATEWAY);
        assert!(!String::from_utf8_lossy(&body).contains("半句"), "截断的内容不该回给客户端");
    }

    /// 起一个吐 `sse` 的本地上游，取回响应交给 [`super::aggregate_sse`]，
    /// 返回 (状态码, content-type, 响应体)。
    async fn relay_sse(sse: &str) -> (super::StatusCode, Option<String>, Bytes) {
        let mut resp = format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\n\r\n",
            sse.len()
        )
        .into_bytes();
        resp.extend_from_slice(sse.as_bytes());
        let (addr, server) = serve_once(resp);
        let up = crate::web::upstream_client()
            .unwrap()
            .post(format!("http://{addr}/v1/messages"))
            .send()
            .await
            .unwrap();

        let out = super::aggregate_sse(up, req_log(), None).await;
        server.join().unwrap();
        let status = out.status();
        let ctype =
            out.headers().get(header::CONTENT_TYPE).and_then(|v| v.to_str().ok()).map(String::from);
        let body = axum::body::to_bytes(out.into_body(), usize::MAX).await.unwrap();
        (status, ctype, body)
    }

    /// 聚合路径要一份 `ReqLog`（它在 Drop 里落日志与用量）；这里给一份最小可用的。
    fn req_log() -> super::ReqLog {
        let store = std::sync::Arc::new(crate::store::CredentialStore::open_in_memory().unwrap());
        let cred = store.insert("t", None, "a", "r", 0, None, None).unwrap();
        super::ReqLog {
            started: std::time::Instant::now(),
            ttft_ms: None,
            method: "POST".into(),
            path: "/v1/messages".into(),
            ua: "-".into(),
            ua_out: "-".into(),
            cred_id: cred.id,
            cred_label: cred.label,
            device_id: None,
            status: 200,
            sse_aggregated: false,
            sniffer: super::UsageSniffer::new(true, false),
            req_speed: None,
            req_model: None,
            ratelimit: rl_headers(&[]),
            stream_broke: None,
            store,
        }
    }
}
