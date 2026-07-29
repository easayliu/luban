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

    // 1) 校验来访 API Key（未配置则放行）。生效 key：环境覆盖优先，否则用库中配置。
    if let Some(expected) = effective_client_key(&state) {
        if !client_authorized(&headers, &expected) {
            tracing::warn!(%method, path = %path_and_query, "拒绝：无效的接入 API Key");
            return (StatusCode::UNAUTHORIZED, "无效的 API Key").into_response();
        }
    }

    // 2) 提取 device_id（在请求体 metadata.user_id 里；兼容 CC 内嵌 JSON 与扁平串两种格式）。
    let device_id = extract_device_id(&body);

    // 2.1) 这条路径是否消耗订阅额度——决定要不要卡设备身份、要不要改写出站体。
    //      判定吃 `uri.path()` 而非上面那个带查询串的 `path_and_query`：豁免要精确匹配。
    let billable = is_billable_messages(uri.path());

    // 2.2) 无有效设备身份（无 metadata / 无法识别的 user_id 格式）→ 计费路径默认直接拒绝：
    //      这类请求既无法做身份伪装、也无从计入设备上限（会绕过 device_limit）。
    //      网页可关掉该校验（放行裸客户端），此时它们退化为不绑定、不占名额的负载均衡挑选。
    if device_id.is_none() {
        if billable && state.store.require_device_id() {
            tracing::warn!(%method, path = %path_and_query, "拒绝：请求无有效设备身份（metadata.user_id 缺失或格式无法识别）");
            return (StatusCode::FORBIDDEN, "缺少有效的设备身份（metadata.user_id）")
                .into_response();
        }
        tracing::debug!(%method, path = %path_and_query, billable, "放行无设备身份的请求");
    }

    // 3) 按 device_id 粘性选出凭证的 access_token（必要时刷新）。
    // 请求的模型名：冷却按「账号 + 模型」分格，fable 那类模型级 429 不该拖累整个账号。
    let req_model = request_model(&body);
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
            tracing::warn!(%method, path = %path_and_query, error = %e, "拒绝转发");
            // 裸请求速率达上限 → 429 且带 `retry-after`：这里的等待时间是可算的（窗口长度），
            // 给出来客户端才知道该等多久，而不是立刻重试再撞一次。
            if let Some(rl) = e.downcast_ref::<store::BareRateLimited>() {
                let retry = rl.retry_after_secs.to_string();
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
    // 6) 转发前改写 body：system 形态对齐（拆成官方的 4 块 + 断点全上 1h + 基座标 scope=global）
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
            "客户端识别头"
        );
    }
    // 请求侧的速度档（顶层 `speed` 字段，配套 anthropic-beta: fast-mode-*）。
    // 仅作兜底：以上游 `usage.speed` 为准，那里才反映实际生效的档位。
    let req_speed = request_speed(&body);

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
    let (upstream, resp) = loop {
        let sim = Simulation::detect(&body, flags, &cred, &device_fp);
        let out = build_forward_headers(&headers, &token, flags, sim.as_ref());
        let upstream = Upstream {
            state: &state,
            method: method.clone(),
            url: url.clone(),
            headers: out,
            flags,
            billable,
            sim,
        };
        let resp = upstream.send(upstream.shape(&body, &cred, &device_fp)).await;

        // 只认「上游明确回 429」这一种：连不上/超时那类换个号一样连不上，重试只是浪费时间。
        let limited = match &resp {
            Ok(up) if up.status() == StatusCode::TOO_MANY_REQUESTS => {
                Some(RateLimitInfo::from_headers(up.headers()))
            }
            _ => None,
        };
        let Some(info) = limited else { break (upstream, resp) };
        // 额度真耗尽 → 冷却整个账号；窗口没跑满却被拒（实测只有 fable 这样）→ 只冷却这个模型。
        let scope = rate_limit_scope(&info, req_model.as_deref());
        let cooldown = info.cooldown(scope.account_level());
        tracing::warn!(
            cred = format!("#{} {}", cred.id, cred.label),
            model = %req_model.as_deref().unwrap_or("-"),
            scope = scope.label(),
            cooldown_secs = cooldown.as_secs(),
            ratelimit = %info.raw,
            "上游 429"
        );

        // 冷却与重试同受一个开关：关掉即完全退回「原样透传 429」的既有行为。
        if max_retry == 0 {
            break (upstream, resp);
        }
        state.store.mark_rate_limited(cred.id, scope.model(), cooldown);
        tried.push(cred.id);
        if retried >= max_retry {
            tracing::warn!(
                cred = format!("#{} {}", cred.id, cred.label),
                retried,
                "上游 429，已达换号重试次数上限，透传该响应"
            );
            break (upstream, resp);
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
                    from = format!("#{} {}", cred.id, cred.label),
                    to = format!("#{} {}", next_cred.id, next_cred.label),
                    cooldown_secs = cooldown.as_secs(),
                    attempt = retried + 1,
                    "上游 429：该号已进入冷却，改用其它账号重试"
                );
                (token, cred) = (next_token, next_cred);
                retried += 1;
            }
            // 没有别的号可用（都试过/都停用了）：保留最初那条 429 原样透传，别把它变成 503。
            Err(e) => {
                tracing::warn!(
                    cred = format!("#{} {}", cred.id, cred.label),
                    error = %e,
                    "上游 429，但没有可换的账号，原样透传"
                );
                break (upstream, resp);
            }
        }
    };
    // 请求日志里记哪个设备：客户端自己带了就记它的，裸客户端记出站那份**伪装** device_id。
    // 不记的话这段流量在日志里只留下 `device=-`，既看不出是谁、也无从聚合。见 [`sim_device_id`]。
    // 取最终那一轮的凭证与模拟参数——换过号的话，实际发出去的就是那份。
    let logged_device = device_id
        .clone()
        .or_else(|| sim_device_id(upstream.sim.as_ref(), flags, &cred, &device_fp));

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
                    "上游响应带无法解码的 content-encoding：用量嗅探与账号级错误判定都会被跳过（该编码需在 wreq feature 里开启）"
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
                cred_id: cred.id,
                cred_label: cred.label.clone(),
                device_id: logged_device,
                status: status.as_u16(),
                sniffer: UsageSniffer::new(is_stream, compressed),
                req_speed,
                req_model: req_model.clone(),
                ratelimit,
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
                                cred = format!("#{} {}", cred.id, cred.label),
                                status = status.as_u16(),
                                error_type = %etype.as_deref().unwrap_or("-"),
                                message = %message.chars().take(500).collect::<String>(),
                                "上游返回 4xx"
                            );
                        }
                        // 压缩体读不出内容，宁可漏判也不误判（乱码可能碰巧命中特征词）。
                        if let Some(reason) =
                            (!compressed).then(|| detect_account_ban(status, &bytes)).flatten()
                        {
                            tracing::warn!(
                                cred = format!("#{} {}", cred.id, cred.label),
                                status = status.as_u16(),
                                reason = %reason,
                                "检测到账号级错误，自动停用该凭证"
                            );
                            if let Err(e) = state.store.mark_banned(cred.id, &reason) {
                                tracing::warn!(error = %e, "自动停用凭证失败");
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
                                    cred = format!("#{} {}", cred.id, cred.label),
                                    "上游拒绝 thinking 块签名（会话历史多半由其它账号签发），降级重试开关已关闭，原样透传"
                                );
                            } else if let Some(up) =
                                retry_demoted_thinking(&upstream, &cred, &device_fp, &body, &mut rl)
                                    .await
                            {
                                return stream_upstream(up, rl);
                            }
                        }
                        builder.body(Body::from(bytes)).unwrap_or_else(|e| {
                            (StatusCode::BAD_GATEWAY, e.to_string()).into_response()
                        })
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "读取上游错误响应体失败");
                        builder.body(Body::empty()).unwrap_or_else(|e| {
                            (StatusCode::BAD_GATEWAY, e.to_string()).into_response()
                        })
                    }
                };
            }

            stream_upstream(up, rl)
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
                "上游请求失败"
            );
            (StatusCode::BAD_GATEWAY, format!("上游请求失败[{kind}]: {detail}")).into_response()
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
            rewrite_body(body, cred, device_fp, self.flags, self.sim.as_ref())
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
    let mut builder = Response::builder().status(up.status());
    for (k, v) in up.headers().iter() {
        if is_resp_forwardable(k) {
            builder = builder.header(k, v);
        }
    }
    builder
}

/// 把上游响应包成流式回传：首块到达记 TTFT，边转发边嗅探用量；
/// 流结束（或客户端断开）时 `rl` 在 Drop 里记 total、输出一条日志并落库。
fn stream_upstream(up: wreq::Response, mut rl: ReqLog) -> Response {
    let builder = resp_builder(&up);
    let stream = up.bytes_stream().map(move |chunk| {
        if rl.ttft_ms.is_none() {
            rl.ttft_ms = Some(rl.started.elapsed().as_millis());
        }
        if let Ok(bytes) = &chunk {
            rl.sniffer.feed(bytes);
        }
        chunk
    });
    builder
        .body(Body::from_stream(stream))
        .unwrap_or_else(|e| (StatusCode::BAD_GATEWAY, e.to_string()).into_response())
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
/// 请求数翻倍。真正的解法是别让会话中途换号（见 `device_binding_ttl`），这里只是兜底。
async fn retry_demoted_thinking(
    upstream: &Upstream<'_>,
    cred: &crate::credentials::Credential,
    device_fp: &str,
    client_body: &Bytes,
    rl: &mut ReqLog,
) -> Option<wreq::Response> {
    let label = format!("#{} {}", cred.id, cred.label);
    let Some(demoted) = demote_thinking_blocks(client_body) else {
        tracing::warn!(
            cred = %label,
            "上游拒绝 thinking 块签名，但请求体里没有可降级的 thinking 块，原样透传"
        );
        return None;
    };
    tracing::warn!(
        cred = %label,
        "上游拒绝 thinking 块签名（会话历史多半由其它账号签发）：已把历史 thinking 降级为 text，用同一凭证重试一次"
    );

    let up = match upstream.send(upstream.shape(&demoted, cred, device_fp)).await {
        Ok(up) => up,
        Err(e) => {
            tracing::warn!(error = %error_chain(&e), "降级 thinking 后的重试请求发不出去，透传最初那条 400");
            return None;
        }
    };
    let status = up.status();
    if !status.is_success() {
        // 最常见的是末轮为 `tool_result` 的工具续跑：上游另外要求「最后一条 assistant
        // 消息必须以 thinking 块开头」，降级完照样被拒，只是换了条错误信息。
        tracing::warn!(
            cred = %label,
            status = status.as_u16(),
            "降级 thinking 后重试仍被拒，透传最初那条 400"
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
    cred_id: i64,
    cred_label: String,
    /// 完整 device_id；日志里只展示前 8 位（脱敏）。
    device_id: Option<String>,
    status: u16,
    /// 增量嗅探到的响应用量。
    sniffer: UsageSniffer,
    /// 请求体里声明的速度档；仅在响应未回报 `usage.speed` 时兜底。
    req_speed: Option<String>,
    /// 请求体里声明的模型名；仅在响应没带 `usage`（4xx/5xx，尤其是 429）时兜底。
    /// 否则那些记录只留下 `model=-`，排查「哪个模型被拒得多」时等于没有信息。
    req_model: Option<String>,
    /// 上游返回的订阅账号限流快照。
    ratelimit: RateLimitInfo,
    store: std::sync::Arc<store::CredentialStore>,
}

impl Drop for ReqLog {
    fn drop(&mut self) {
        self.sniffer.finish();
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
            cred = format!("#{} {}", self.cred_id, self.cred_label),
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
            cost_usd = cost_usd.map(|c| format!("{c:.5}")).unwrap_or_else(|| "-".into()),
            "转发"
        );

        let rec = store::UsageRecord {
            cred_id: Some(self.cred_id),
            cred_label: self.cred_label.clone(),
            device_id: self.device_id.clone(),
            model,
            path: self.path.clone(),
            status: self.status,
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
            ratelimit_raw: (!self.ratelimit.raw.is_empty()).then(|| self.ratelimit.raw.clone()),
            cost_usd,
        };
        if let Err(e) = self.store.insert_usage_log(&rec) {
            tracing::warn!(error = %e, "写入用量日志失败");
        }
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
        if !self.is_stream && !self.buf.is_empty() {
            if let Ok(v) = serde_json::from_slice::<serde_json::Value>(&self.buf) {
                self.merge(&v);
            }
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
fn extract_device_id(body: &Bytes) -> Option<String> {
    let json: serde_json::Value = serde_json::from_slice(body).ok()?;
    let user_id = json.get("metadata")?.get("user_id")?.as_str()?;
    // CC 内嵌 JSON 优先。
    if let Ok(inner) = serde_json::from_str::<serde_json::Value>(user_id) {
        if let Some(dev) = inner.get("device_id").and_then(|d| d.as_str()) {
            if !dev.is_empty() {
                return Some(dev.to_string());
            }
        }
    }
    // 退化：扁平串格式，取 device 段。
    let flat = parse_flat_user_id(user_id)?;
    (!flat.device.is_empty()).then_some(flat.device)
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

/// 400 场景下的账号级错误特征词：命中其一才判定为「该账号被上游封禁/停用/授权失效」，
/// 以区别于常规的客户端请求错误（invalid_request_error，如模型名错、body 超长）——避免
/// 客户端一条坏请求重试时把所有账号逐个误禁。命中后原文（截断）存作 `ban_reason`。
const BAN_KEYWORDS: &[&str] = &[
    "disabled",
    "suspended",
    "banned",
    "terminated",
    "deactivated",
    "violat",
    "invalid_grant",
    "oauth",
];

/// 反向豁免：命中其一则**一定不是**账号级问题，无论状态码与特征词如何都不停用。
/// 用于挡住「特征词碰巧出现在非账号报错里」的误杀，见 [`detect_account_ban`]。
const NOT_ACCOUNT_PHRASES: &[&str] =
    &["not supported for this endpoint", "does not support", "unsupported model"];

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
/// 三档都要求响应体确实是 Anthropic 的错误 JSON（能取到 `error.type`）或命中
/// [`BAN_KEYWORDS`]，避免把「非账号问题的 4xx」当成封号，把健康账号打成停用：
/// - 401：`authentication_error` 才停用。裸 401（CDN/网关拦截，无 `error.type`）不停用。
/// - 403：**仅**命中 [`BAN_KEYWORDS`] 时停用。普通 `permission_error`（如 Pro 账号请求
///   Opus、beta 未开通、区域限制）是能力/权限问题而非封号，原样透传即可。
/// - 400：同 403，仅命中特征词时停用；普通 `invalid_request_error` 是客户端请求错误。
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
    let hits_keyword = || BAN_KEYWORDS.iter().any(|k| hay.contains(k));
    match status {
        StatusCode::UNAUTHORIZED => {
            (etype.as_deref() == Some("authentication_error") || hits_keyword()).then(reason)
        }
        StatusCode::FORBIDDEN | StatusCode::BAD_REQUEST => hits_keyword().then(reason),
        _ => None,
    }
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
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        if v == expected {
            return true;
        }
    }
    if let Some(v) = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok()) {
        if v.strip_prefix("Bearer ").map(str::trim) == Some(expected) {
            return true;
        }
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
            Err(e) => tracing::warn!(error = %e, "构造 anthropic-beta 失败，保留来访原值"),
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
            tracing::error!(error = %e, "构造 Authorization 失败，移除该头避免泄漏接入 key");
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
            _ => tracing::error!(header = name, "模拟头构造失败（常量表写错了），跳过该头"),
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
        body: &Bytes,
        flags: store::ForwardFlags,
        cred: &crate::credentials::Credential,
        device_fp: &str,
    ) -> Option<Self> {
        if !flags.simulate_cc || !flags.merge_beta {
            return None;
        }
        let v: serde_json::Value = serde_json::from_slice(body).ok()?;
        if is_cc_shaped(&v) {
            return None;
        }
        let model = v.get("model").and_then(|m| m.as_str()).unwrap_or_default();
        let base = cc_system_base(model);
        let beta = cc_beta_seed(model);
        tracing::debug!(
            model,
            base_bytes = base.map(str::len).unwrap_or(0),
            "非 CC 请求，按官方形态模拟"
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

/// 把非 CC 请求的 `system` 换成官方形态的四块：
///
/// ```text
/// [0] x-anthropic-billing-header: …            无断点（cch 由 ensure_billing_cch 补）
/// [1] You are Claude Code, …（57B）            无断点
/// [2] 官方基座（按模型族）                      {ephemeral, ttl:1h, scope:global}
/// [3] 客户端自己的 system（原样搬来）           {ephemeral, ttl:1h}
/// ```
///
/// 客户端的 `system` 是字符串就裹成一个文本块，是数组就整段搬过来（它自己的块结构、
/// 已有的 `cache_control` 都不动），没有就只有前三块。
///
/// **断点是数着加的**：客户端可能自己就用满了 4 个（比如给每条工具定义都标了缓存），这时
/// 再加就会让整条请求被上游拒——那是把「形态更像」换成「根本发不出去」。预算不够时基座与
/// 末块照发，只是不带断点（少一次缓存复用，不影响正确性）。
fn simulate_system(v: &mut serde_json::Value, sim: &Simulation) -> bool {
    let mut budget = MAX_CACHE_BREAKPOINTS.saturating_sub(count_cache_control(v));
    let client: Vec<serde_json::Value> = match v.get("system") {
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => {
            vec![text_block_bare(s)]
        }
        Some(serde_json::Value::Array(a)) => a.clone(),
        _ => Vec::new(),
    };

    let mut blocks =
        vec![text_block_bare(&billing_header_text()), text_block_bare(config::CC_SYSTEM_IDENTITY)];
    if let Some(base) = sim.base {
        if budget > 0 {
            budget -= 1;
            blocks.push(text_block(base, cache_control(true)));
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
        last.insert("cache_control".into(), cache_control(false));
    }

    insert_top_level(v, "system", serde_json::Value::Array(blocks), &["messages", "model"]);
    // 客户端自己那些断点也对齐到 1h：官方 3/3 全带 ttl，混着 5m 是官方不产生的中间态。
    fill_cache_ttl(v);
    true
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
fn request_model(body: &Bytes) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    Some(v.get("model")?.as_str()?.to_string())
}

/// 读取请求体声明的速度档（顶层 `speed` 字段，如 `"fast"`；配套 header
/// `anthropic-beta: fast-mode-*`）。解析失败或没有该字段时返回 `None`。
fn request_speed(body: &Bytes) -> Option<String> {
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    Some(v.get("speed")?.as_str()?.to_string())
}

/// 转发前改写请求体，各项分别受 [`store::ForwardFlags`] 里的开关控制（默认全开；全关即
/// 请求体逐字节原样转发）：
///
/// 0. **模拟**（`simulate_cc`，仅当 `sim` 为 `Some`，即来访不是 CC 形态）：补上官方
///    `system` 前缀与 `metadata` 身份，见 [`Simulation`]。它先跑——后面几项都是在
///    「已经是 CC 形态」的前提下做微调。
/// 1. **system 形态**（`system_shape`）：把 API-key 模式的 3 块改写成订阅模式的 4 块，
///    见 [`align_system_shape`]。含拆块、断点全上 `ttl:1h`、基座标 `scope:"global"`。
///    模拟路径已经直接产出 4 块，故两者互斥，不叠加。
/// 2. **身份伪装**（`spoof_identity`）：把 `metadata.user_id` 里的 `account_uuid`/`device_id`
///    换成该凭证自洽的身份（真实 account_uuid + 由其稳定派生的 device_id），避免
///    「真账号 + 陌生设备」的矛盾。它也管着模拟路径的 `metadata` 注入——凭空造一份身份，
///    本来就是同一件事。
/// 3. **cch**（`billing_cch`）：给 `x-anthropic-billing-header` 补订阅模式独有的 `cch`。
///
/// **key 顺序**：改写要把 body 重新序列化，serde_json 默认的 `Map = BTreeMap` 会把**整个
/// body**（含 tools/messages/content/cache_control 里每一个对象）的 key 按字母序重排，得到
/// 官方客户端不会产生的排列——集合对了顺序错，一次精确比对即可判定中间有代理。故本 crate
/// 开了 serde_json 的 `preserve_order`（见 Cargo.toml），解析出的顺序原样写回，
/// 新增字段追加在末尾。回归测试见 [`tests::preserves_key_order`]。
///
/// 解析失败或结构异常时原样返回——绝不因改写失败而阻断转发。
fn rewrite_body(
    body: &Bytes,
    cred: &crate::credentials::Credential,
    device_fp: &str,
    flags: store::ForwardFlags,
    sim: Option<&Simulation>,
) -> Bytes {
    // `ttl` 要上游认，前提是 `anthropic-beta` 里有 `extended-cache-ttl-2025-04-11`，而那串
    // 是 `merge_beta` 补的（API-key 模式的客户端自己不发，cap/raw/00002 证实）。两个开关必须
    // 同时开，否则就是「body 里写了 1h、头上没声明」的自相矛盾。
    let shape = flags.system_shape && flags.merge_beta;
    // 全关且不模拟：连解析都不必做，原样返回。
    if sim.is_none() && !shape && !flags.spoof_identity && !flags.billing_cch {
        return body.clone();
    }
    let mut v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return body.clone(),
    };
    let simulated = sim.is_some_and(|sim| simulate_system(&mut v, sim));
    // 模拟已经产出官方的 4 块形态，再走一遍三块拆分器只会切错地方。
    let shaped = shape && !simulated && align_system_shape(&mut v);
    let cch_added = flags.billing_cch && ensure_billing_cch(&mut v);
    tracing::debug!(
        metadata = %v.get("metadata").map(|m| m.to_string()).unwrap_or_else(|| "<无 metadata>".into()),
        "入站 metadata"
    );
    let sim_meta = flags.spoof_identity
        && sim.is_some_and(|sim| ensure_cc_metadata(&mut v, cred, device_fp, &sim.session_id));
    let spoofed = flags.spoof_identity && spoof_identity(&mut v, cred, device_fp);
    tracing::debug!(
        simulated,
        sim_meta,
        shaped,
        spoofed,
        cch_added,
        device_fp = %device_fp,
        spoof_device = %cred.spoof_device_id(device_fp).as_deref().unwrap_or("-"),
        "改写 body"
    );
    if !shaped && !spoofed && !cch_added && !simulated && !sim_meta {
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
/// **只在真伪装过时才记**：要求走了模拟路径（`sim` 为 `Some`）且 `spoof_identity` 开着——
/// 那正是 [`ensure_cc_metadata`] 会把这个 id 写进出站体的条件。否则记出来的是一个上游根本
/// 没见过的 id，比留个 `-` 更误导。
///
/// **前缀不是装饰**：这个值每账号恒定（指纹对裸客户端恒为 `"||"`），所有裸客户端共用同一个，
/// 看着就像「一台设备打了全部请求」。前缀让它在日志与 `usage_logs` 里一眼可辨，不至于被当成
/// 真实设备读。它也**不写设备绑定**，故不占 `device_limit` 名额、不会出现在设备列表里
/// （[`store::CredentialStore::list_devices`] 从 `device_bindings` 出发）。
fn sim_device_id(
    sim: Option<&Simulation>,
    flags: store::ForwardFlags,
    cred: &crate::credentials::Credential,
    device_fp: &str,
) -> Option<String> {
    if sim.is_none() || !flags.spoof_identity {
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

/// 把 `metadata.user_id` 里的 `account_uuid`/`device_id` 换成凭证自洽身份，**保持原格式**：
/// - CC 内嵌 JSON：**字符串级定点替换**这两个字段的值，字段顺序与其余内容原样不动。
///   真实 CC 发的是紧凑 JSON `{"device_id":..,"account_uuid":..,"session_id":..}`。外层 body
///   已靠 serde_json 的 `preserve_order` 保住顺序，但这层仍绕开 serde：内层是**字符串里的
///   JSON**，重新序列化会连空白、转义写法一起归一化，只有定点替换才逐字节不变。
/// - 扁平串 `user_<hash>_account_<acct>_session_<sess>`（如 Windows）：换掉 device 段与
///   account 段，保留 session 段，仍以扁平串回写——不把 Windows 请求伪装成 CC 的 JSON 形态。
///
/// 凭证无 `account_uuid`（如旧库未回填）或 user_id 结构无法识别时不改动，返回 `false`。
fn spoof_identity(
    v: &mut serde_json::Value,
    cred: &crate::credentials::Credential,
    device_fp: &str,
) -> bool {
    let account_uuid = match cred.account_uuid.as_deref() {
        Some(u) if !u.trim().is_empty() => u,
        _ => return false,
    };
    let device_id = match cred.spoof_device_id(device_fp) {
        Some(d) => d,
        None => return false,
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
        if let Some(next) = replace_json_str_field(&s, "device_id", &device_id) {
            s = next;
            changed = true;
        }
        if changed {
            *user_id = serde_json::Value::String(s);
        }
        return changed;
    }

    // 格式二：扁平串——保持格式，只换 device 与 account，保留 session。
    if let Some(flat) = parse_flat_user_id(&inner_str) {
        let rebuilt =
            format!("user_{}_account_{}_session_{}", device_id, account_uuid, flat.session);
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
/// ```
///
/// 故改写是四件事，缺一都会得到真实客户端不产生的中间态，因此**同受一个开关控制**：
/// 1. 在 [`config::CC_SYSTEM_BASE_ANCHOR`] 前的 `\n\n` 处把合并块切成基座 + 其余；
/// 2. 基座标 `{type:ephemeral, ttl:1h, scope:global}`，其余标 `{type:ephemeral, ttl:1h}`；
/// 3. 去掉身份句上那个断点——它的缓存前缀只有 127 字节（约 35 token），远低于最小可缓存长度，
///    本就是空转，官方也不发；
/// 4. body 里剩下的 ephemeral 断点（尾部那条 role=system 的消息等）补 `ttl:1h`，官方 3/3 全带。
///
/// **`scope:global` 只标基座**。之前是「标 text 最长的那块」，在三块形态下必然选中合并块，
/// 而合并块含 `# Environment` 的 cwd/git、技能清单这些本机内容——跨账号不可能撞上，标了换不来
/// 复用，还发出 `{type,scope}`（global 却无 ttl）这种官方不产生的组合。拆开之后基座是纯静态的，
/// 全网同一份，这个标记才真正有意义。
///
/// **代价**：1h 缓存写单价是 5m 的 2 倍。这是形态对齐的一部分（官方就是全 1h），不是优化。
///
/// 保守起见只处理「确实是 API-key 三块形态」：`system` 长度不为 3、锚点匹配不到、或锚点前不是
/// `\n\n`，一律不动结构返回 `false`。客户端本来就是 4 块（订阅形态）时同样不动。
fn align_system_shape(v: &mut serde_json::Value) -> bool {
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
    sys[2] = text_block(&text[..at - 2], cache_control(true));
    sys.push(text_block(&text[at..], cache_control(false)));
    fill_cache_ttl(v);
    true
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

/// 构造 `cache_control`，key 序与官方一致：`type` → `ttl` → `scope`。
fn cache_control(global: bool) -> serde_json::Value {
    let mut cc = serde_json::Map::new();
    cc.insert("type".into(), "ephemeral".into());
    cc.insert("ttl".into(), config::CC_CACHE_TTL.into());
    if global {
        cc.insert("scope".into(), "global".into());
    }
    serde_json::Value::Object(cc)
}

/// 递归给 body 里所有 `cache_control: {"type":"ephemeral"}` 补上 `ttl`（已有则不动）。
/// 追加在 `type` 之后，得到官方的 `{"type":"ephemeral","ttl":"1h"}` 键序。
fn fill_cache_ttl(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            if let Some(cc) = map.get_mut("cache_control").and_then(|c| c.as_object_mut())
                && cc.get("type").and_then(|t| t.as_str()) == Some("ephemeral")
                && !cc.contains_key("ttl")
            {
                cc.insert("ttl".into(), config::CC_CACHE_TTL.into());
            }
            for (_, child) in map.iter_mut() {
                fill_cache_ttl(child);
            }
        }
        serde_json::Value::Array(items) => items.iter_mut().for_each(fill_cache_ttl),
        _ => {}
    }
}

/// 一次 429 该冷却到什么范围，见 [`rate_limit_scope`]。
#[derive(Debug, Clone, PartialEq)]
enum LimitScope {
    /// 额度真的耗尽：该账号所有模型一起让位。
    Account,
    /// 窗口没跑满却被拒（模型容量限制）：只让这一个模型让位。
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

/// 判定一次 429 是「这个账号没额度了」还是「这个模型这会儿没容量」。
///
/// **规则是实测倒逼出来的**，一次真实的 fable-5 429 头长这样：
///
/// ```text
/// unified-status: rejected                            ← 不是 "rate_limited"
/// representative-claim: seven_day_overage_included     ← 指向 7d_oi
/// 5h:    allowed,         utilization=0.08             ← 5h 几乎是空的
/// 7d:    allowed_warning, utilization=0.76
/// 7d_oi: rejected,        utilization=1.01             ← 真正满掉的是这个窗口
/// retry-after: 228721
/// ```
///
/// 第一版判定认死 `status == "rate_limited"` 且只看 5h/7d 两个窗口，于是把上面这条判成了
/// 模型级——只冷却 fable 三十秒，放出去再撞，如此循环。两处教训都写在规则里：
///
/// 1. **状态词不止一个**：`rejected` 与 `rate_limited` 都算被拒（`allowed`/`allowed_warning`
///    才是放行）。只认其中一个等于漏判。
/// 2. **窗口名不能写死**：真正被拒的是 `7d_oi`（7 天含超额），代码里原本根本没解析它。
///    故改成扫**所有** `unified-<窗口>-status/utilization`，任一被拒或 `utilization >= 1`
///    即判账号级——不必知道窗口叫什么，也不必维护 `representative-claim` 到窗口名的映射
///    （`seven_day_overage_included` → `7d_oi` 这种对应关系纯属猜谜）。
///
/// 剩下的「所有窗口都还有余量却仍被拒」才是模型容量限制，只冷却该模型。请求体里读不出模型名
/// 时退回账号级——没有模型可挂，宁可保守。
fn rate_limit_scope(info: &RateLimitInfo, model: Option<&str>) -> LimitScope {
    let Some(model) = model else { return LimitScope::Account };
    let rejected = |s: &str| s.contains("rate_limited") || s.contains("rejected");
    let quota_gone = info.unified_status.as_deref().is_some_and(rejected)
        || info.window_status.iter().any(|(_, s)| rejected(s))
        || info.window_utilization.iter().any(|(_, u)| *u >= 1.0);
    if quota_gone { LimitScope::Account } else { LimitScope::Model(model.to_string()) }
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
    /// **所有** `anthropic-ratelimit-unified-<窗口>-status` 的取值（窗口名原样保留）。
    ///
    /// 刻意不写死窗口名：实测除了 `5h`/`7d`，还有 `7d_oi`（7 天含超额），而**真正被拒的
    /// 正是它**——只解析 5h/7d 会看到「两个窗口都没满」，从而把一次账号级限流误判成模型
    /// 容量限制。窗口种类是上游说了算的，只能全收，见 [`rate_limit_scope`]。
    window_status: Vec<(String, String)>,
    /// 所有 `…-<窗口>-utilization` 的取值。同上，全收。
    window_utilization: Vec<(String, f64)>,
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
                }
            }
        }
        info.raw = pairs.join(", ");
        info
    }

    /// 该凭证被上游 429 之后应冷却多久。
    ///
    /// **账号级**（额度真耗尽）取值优先级：
    /// 1. `retry-after`（秒）——上游对这次拒绝给出的明确等待时间，最可信（实测给的是
    ///    228721 秒 ≈ 63 小时，直指 7 天窗口的重置时刻，说明它确实算得很准）；
    /// 2. 不带窗口名的 `anthropic-ratelimit-unified-reset` 减去当前时刻——上游给的「整体
    ///    什么时候恢复」，比按 `representative-claim` 反查窗口名可靠；
    /// 3. 各窗口 `*-reset` 里**最早**的那个，宁可早醒也不要多睡；
    /// 4. 都没有 → [`DEFAULT_RATE_LIMIT_COOLDOWN_SECS`]。
    ///
    /// **模型级不看任何 reset**：窗口都没跑满，reset 说的是「这个窗口什么时候重置」，跟
    /// 「这个模型什么时候有容量」是两码事，拿它当冷却会让一个好账号的某个模型白白闲置几小时。
    /// 那一档只认 `retry-after`，没有就用 [`DEFAULT_MODEL_COOLDOWN_SECS`]。
    ///
    /// 结果夹在 `[1s, 24h]`。上限从 6h 放宽到 24h 是实测改的：上游真的会给 63 小时的
    /// `retry-after`，夹到 6h 等于每 6 小时把这个号放出去白撞一次 429。再往上放宽意义不大
    /// ——冷却记在内存里，进程重启就清了。冷却本身也只是**选号提示**（见
    /// [`store::CredentialStore::select_for_device`]），全部号都在冷却时会被忽略。
    fn cooldown(&self, account_level: bool) -> std::time::Duration {
        let now = crate::credentials::now_secs() as i64;
        let earliest_window_reset = self
            .window_reset_candidates()
            .into_iter()
            .filter(|reset| *reset > now)
            .min()
            .map(|reset| reset - now);
        let fallback = if account_level {
            self.unified_reset
                .map(|reset| reset - now)
                .filter(|d| *d > 0)
                .or(earliest_window_reset)
                .unwrap_or(DEFAULT_RATE_LIMIT_COOLDOWN_SECS)
        } else {
            DEFAULT_MODEL_COOLDOWN_SECS
        };
        let secs = self.retry_after.unwrap_or(fallback).clamp(1, 24 * 3600);
        std::time::Duration::from_secs(secs as u64)
    }

    /// 各窗口的 `*-reset`（unix 秒）。目前专用字段只解析了 5h/7d 两个，够用即可——
    /// 它只是 `unified-reset` 缺失时的兜底，而实测那个头一直都在。
    fn window_reset_candidates(&self) -> Vec<i64> {
        [self.five_h_reset, self.seven_d_reset].into_iter().flatten().collect()
    }
}

/// 模型级冷却在没有 `retry-after` 时的时长。
///
/// 取 30 秒：容量限制是「这一阵挤」，不是「这个号没额度了」，躲一小会儿就该让它回来试；
/// 押太久等于把一个健康账号的这个模型白白闲置。
const DEFAULT_MODEL_COOLDOWN_SECS: i64 = 30;

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

#[cfg(test)]
mod tests {
    use super::{
        Bytes, HeaderValue, StatusCode, UsageSniffer, build_forward_headers, config,
        detect_account_ban, ensure_billing_cch, header, is_billable_messages, merge_beta,
        replace_json_str_field, request_speed, store, uuid_v4,
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

    /// 形态开关全开（= 默认，也是加入开关机制之前的既有行为）。
    fn all_on() -> store::ForwardFlags {
        store::ForwardFlags::default()
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
        let out = build_forward_headers(&incoming_headers(), "sk-ant-oat01-REAL", all_on(), None);

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
            billing_cch: false,
            fill_client_headers: false,
            merge_beta: false,
            system_shape: false,
            orig_header_case: false,
            thinking_signature_retry: false,
            simulate_cc: false,
            rate_limit_retry: false,
        };
        let out = build_forward_headers(&incoming_headers(), "sk-ant-oat01-REAL", flags, None);

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

        let on = build_forward_headers(&bare, "tok", all_on(), None);
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
        let off = build_forward_headers(&bare, "tok", flags, None);
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
        let out = build_forward_headers(&incoming_headers(), "bad\ntoken", all_on(), None);
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
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: u64::MAX,
            priority: 0,
            disabled: false,
            device_limit: 0,
            ban_reason: None,
            account_uuid: Some(ACCOUNT_UUID.into()),
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
            super::rewrite_body(&Bytes::from(API_SHAPE_BODY), &test_cred(), "fp", all_on(), None);
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

        // 键序也要对：type → text → cache_control，cache_control 内 type → ttl → scope。
        assert!(
            s.contains(r#""cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}"#),
            "基座的 cache_control 形态不对: {s}"
        );
        assert_eq!(
            s.matches(r#""cache_control":{"type":"ephemeral","ttl":"1h"}"#).count(),
            2,
            "其余块与消息断点都应是 1h: {s}"
        );
        assert!(!s.contains(r#"{"type":"ephemeral"}"#), "还有没补 ttl 的断点: {s}");
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
        let out = super::rewrite_body(&raw, &test_cred(), "fp", all_on(), None);
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
        let out = super::rewrite_body(&raw, &test_cred(), "fp", all_on(), None);
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
        let out = super::rewrite_body(&raw, &test_cred(), "fp", all_on(), None);
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
        let out = super::rewrite_body(&raw, &test_cred(), "fp", all_on(), None);
        assert_eq!(out, raw, "四块形态应原样返回");
    }

    /// 三项 body 改写全关 = **逐字节原样透传**：不重新序列化，故连缩进、换行、转义写法
    /// 这些 serde 会归一化掉的细节都保持不变（重新序列化本身就是个形态 tell）。
    #[test]
    fn body_flags_off_passes_through_byte_for_byte() {
        // 刻意带上多余空白与换行：一旦走了 serde 往返，这些都会被抹平。
        let raw = Bytes::from(format!(" {}\n", API_SHAPE_BODY));
        let flags = store::ForwardFlags {
            spoof_identity: false,
            billing_cch: false,
            fill_client_headers: false,
            merge_beta: false,
            system_shape: false,
            orig_header_case: false,
            thinking_signature_retry: false,
            simulate_cc: false,
            rate_limit_retry: false,
        };
        let out = super::rewrite_body(&raw, &test_cred(), "fp", flags, None);
        assert_eq!(out, raw, "全关时必须原样返回");

        // 逐项开一个，就只有那一项生效，其余仍不动。
        let only_cch = store::ForwardFlags { billing_cch: true, ..flags };
        let s = String::from_utf8(
            super::rewrite_body(&raw, &test_cred(), "fp", only_cch, None).to_vec(),
        )
        .unwrap();
        assert!(s.contains("cch=00000"), "只开 cch 时应补 cch: {s}");
        assert!(!s.contains(r#""ttl""#), "system_shape 关着不应拆块/上 ttl: {s}");
        assert!(s.contains(r#"\"account_uuid\":\"\""#), "spoof 关着应保留空 uuid: {s}");

        // system 形态依赖 merge_beta 补的 extended-cache-ttl beta：只开 system_shape 不生效。
        let shape_only = store::ForwardFlags { system_shape: true, ..flags };
        let out = super::rewrite_body(&raw, &test_cred(), "fp", shape_only, None);
        assert_eq!(out, raw, "merge_beta 关着时不应写出 ttl");

        let with_beta = store::ForwardFlags { merge_beta: true, ..shape_only };
        let s = String::from_utf8(
            super::rewrite_body(&raw, &test_cred(), "fp", with_beta, None).to_vec(),
        )
        .unwrap();
        assert!(s.contains(r#""scope":"global""#), "两个开关都开时应对齐形态: {s}");
        assert!(!s.contains("cch="), "billing_cch 关着不应补 cch: {s}");
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
        // cache_control 是 type→ttl→scope，metadata.user_id 内层是
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
        let out = super::rewrite_body(&Bytes::from(raw), &test_cred(), "fp", all_on(), None);
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
        // 拆块后新建的两块也按这个键序写回，cache_control 内是 type→ttl→scope。
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
        ];
        for (status, body) in cases {
            assert!(
                detect_account_ban(status, &body).is_none(),
                "不应停用: {status} {}",
                String::from_utf8_lossy(&body)
            );
        }
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
    #[test]
    fn reads_speed_from_request_body() {
        let with = Bytes::from(r#"{"model":"claude-opus-5","speed":"fast","messages":[]}"#);
        assert_eq!(request_speed(&with).as_deref(), Some("fast"));
        let without = Bytes::from(r#"{"model":"claude-opus-5","messages":[]}"#);
        assert_eq!(request_speed(&without), None);
        assert_eq!(request_speed(&Bytes::from("not json")), None);
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
        super::Simulation::detect(&Bytes::from(body.to_string()), all_on(), &test_cred(), "fp")
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

    /// 普通请求 → 官方四块 system：billing / 身份句 / 基座（1h + global）/ 客户端原文（1h）。
    /// 基座按模型族选，且 `system` 落在 `messages` 之后（官方 key 序）。
    #[test]
    fn simulates_official_system_for_plain_request() {
        let body = Bytes::from(
            r#"{"model":"claude-sonnet-5","messages":[],"system":"你是助手","max_tokens":8}"#
                .to_string(),
        );
        let sim = super::Simulation::detect(&body, all_on(), &test_cred(), "fp").unwrap();
        let out = super::rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim));
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
        assert_eq!(sys[2]["cache_control"]["ttl"], "1h");
        assert_eq!(sys[2]["cache_control"]["scope"], "global");
        assert_eq!(sys[3]["text"], "你是助手", "客户端原 system 应原样留在末块");
        assert_eq!(sys[3]["cache_control"]["ttl"], "1h");
        assert!(sys[3]["cache_control"].get("scope").is_none(), "只有基座标 global");

        // key 序按官方 `model → messages → system → tools → metadata → max_tokens` 落位，
        // 补出来的两个字段不该被追加到队尾。
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

    /// 没有 system 的请求同样成立：三块（billing / 身份句 / 基座），且末块拿到断点。
    #[test]
    fn simulates_system_when_client_sent_none() {
        let body = Bytes::from(PLAIN_BODY.to_string());
        let sim = sim_for(PLAIN_BODY);
        let out = super::rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim));
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();
        assert_eq!(sys.len(), 3, "没有客户端 system 就只有前三块: {v}");
        assert_eq!(sys[2]["cache_control"]["ttl"], "1h");
    }

    /// 已经是 CC 形态的请求一个字节都不该多改——判据是 `system` 里那句身份声明，
    /// 字符串形态与数组形态都认。
    #[test]
    fn leaves_cc_shaped_request_alone() {
        let cc = Bytes::from(API_SHAPE_BODY);
        assert!(
            super::Simulation::detect(&cc, all_on(), &test_cred(), "fp").is_none(),
            "CC 形态不该走模拟路径"
        );
        let as_string = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","system":"{}","messages":[]}}"#,
            config::CC_SYSTEM_IDENTITY
        ));
        assert!(super::Simulation::detect(&as_string, all_on(), &test_cred(), "fp").is_none());

        // 开关关掉、或 merge_beta 关掉（模拟出来的 beta 没人落位）时也不模拟。
        let plain = Bytes::from(PLAIN_BODY.to_string());
        let off = store::ForwardFlags { simulate_cc: false, ..all_on() };
        assert!(super::Simulation::detect(&plain, off, &test_cred(), "fp").is_none());
        let no_beta = store::ForwardFlags { merge_beta: false, ..all_on() };
        assert!(super::Simulation::detect(&plain, no_beta, &test_cred(), "fp").is_none());
        // 解析不了的请求体不 panic、也不模拟。
        assert!(
            super::Simulation::detect(
                &Bytes::from_static(b"not json"),
                all_on(),
                &test_cred(),
                "fp"
            )
            .is_none()
        );
    }

    /// 客户端已经用满 4 个缓存断点时不再加——加了整条请求会被上游拒，那是把「形态更像」
    /// 换成「根本发不出去」。
    #[test]
    fn respects_cache_breakpoint_budget() {
        let blk = r#"{"type":"text","text":"t","cache_control":{"type":"ephemeral"}}"#;
        let body = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"system":[{blk},{blk},{blk},{blk}]}}"#
        ));
        let sim = super::Simulation::detect(&body, all_on(), &test_cred(), "fp").unwrap();
        let out = super::rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim));
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(super::count_cache_control(&v), 4, "断点数不得超过 4: {v}");
        assert!(v["system"][2].get("cache_control").is_none(), "预算用完时基座不带断点");
        // 内容照发，只是少一次缓存复用。
        assert_eq!(v["system"][2]["text"], config::CC_SYSTEM_BASE_OPUS);
    }

    /// 模拟路径补 `metadata.user_id`：键序与 CC 一致，session_id 与请求头同值且逐设备稳定；
    /// 客户端自己带了 user_id 就不新造（交给 spoof_identity 原格式改写）。
    #[test]
    fn injects_cc_metadata_only_when_absent() {
        let body = Bytes::from(PLAIN_BODY.to_string());
        let sim = sim_for(PLAIN_BODY);
        let out = super::rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim));
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
        let sim2 = super::Simulation::detect(&with_meta, all_on(), &test_cred(), "fp").unwrap();
        let out2 = super::rewrite_body(&with_meta, &test_cred(), "fp", all_on(), Some(&sim2));
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
        let out = build_forward_headers(&client, "sk-ant-oat01-REAL", all_on(), Some(&sim));
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
        let id = super::sim_device_id(Some(&sim), all_on(), &test_cred(), "fp").unwrap();
        assert_eq!(id, format!("sim:{}", test_cred().spoof_device_id("fp").unwrap()));

        // 没走模拟路径（来访本来就是 CC 形态）→ 出站体里根本没有这个 id，不该记。
        assert!(super::sim_device_id(None, all_on(), &test_cred(), "fp").is_none());
        // spoof_identity 关着时同理：ensure_cc_metadata 不会写 metadata。
        let no_spoof = store::ForwardFlags { spoof_identity: false, ..all_on() };
        assert!(super::sim_device_id(Some(&sim), no_spoof, &test_cred(), "fp").is_none());
        // 凭证没有 account_uuid 就派生不出来，退回 `-`。
        let no_uuid = crate::credentials::Credential { account_uuid: None, ..test_cred() };
        assert!(super::sim_device_id(Some(&sim), all_on(), &no_uuid, "fp").is_none());
    }

    /// 429 作用域判定的回归用例，**头的取值逐字节取自一次真实的 fable-5 429**
    /// （账号 5h 只用了 8%、7d 用了 76%，真正满掉的是 `7d_oi` = 1.01）。
    ///
    /// 这条用例存在的理由：第一版判定认死 `status == "rate_limited"` 且只看 5h/7d，
    /// 会把它误判成「模型容量限制」，只冷却 fable 三十秒然后反复撞墙。
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
        assert!(scope.account_level(), "unified-status=rejected + 7d_oi 满 → 账号级");
        // retry-after 优先，但夹到 24h：上游给的 63 小时不该原样吃下，也不该被砍成 6h。
        assert_eq!(real.cooldown(true).as_secs(), 24 * 3600);

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
}
