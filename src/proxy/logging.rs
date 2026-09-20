use axum::body::Bytes;
use axum::http::{HeaderMap, StatusCode};

use crate::store;

use super::ban::parse_upstream_error;
use super::body::{extract_device_id, extract_session_id, ua_of};
use super::learned_rules::{
    EmptyReplyMemory, REFUSAL_REPLY_BYTES, record_app_request, remember_empty_reply,
    remember_refused_prompt,
};
use super::rate_limit::RateLimitInfo;
use super::session_link::{CcSessionKey, CcSessionLink};
use super::upstream::{
    InFlightGuard, SessionConcurrencyGuard, Upstream, UpstreamRouteGuard, error_status,
};
use super::{AppState, header_opt};

/// 随响应流一起存活；流结束/断开时在 Drop 里输出一条转发日志（含 TTFT、总耗时与用量）并落库。
pub(super) struct ReqLog {
    pub(super) started: std::time::Instant,
    pub(super) ttft_ms: Option<u128>,
    pub(super) method: String,
    pub(super) path: String,
    /// **来访**客户端自报的 `User-Agent`（已截断，见 [`ua_of`]）。
    ///
    /// 存在的理由：`path` 记的是来访原样的路径查询串（`?beta=true` 是官方 CC 自己带的，
    /// luban 只在出站 URL 上补，见 [`ensure_beta_query`]），于是「带 metadata.user_id 却
    /// 没有 `?beta=true`」这类第三方 CC 兼容客户端在日志里和官方客户端长得一样。UA 是
    /// 分辨它们最省事的一项。
    pub(super) ua: String,
    /// **实际发给上游**的那份 `User-Agent`（见 [`build_forward_headers`]）。
    ///
    /// 与 `ua` 分开记而不是只留一份：模拟路径整套头换成官方的（[`official_headers`]），
    /// 出站恒为 [`config::CC_USER_AGENT`]；非模拟路径原样转发来访那份。于是两列一比就知道
    /// 这条走没走模拟——只存一份的话，要么看不见真实客户端是谁，要么看不见上游收到的是什么。
    pub(super) ua_out: String,
    pub(super) cred_id: i64,
    pub(super) cred_label: String,
    /// 完整 device_id；日志里只展示前 8 位（脱敏）。
    pub(super) device_id: Option<String>,
    pub(super) status: u16,
    /// 这条来访本来是非流式、被改成流式发给上游再聚合回整段 JSON（见
    /// [`store::ForwardFlags::nonstream_as_sse`]）。
    ///
    /// 日志与 `usage_logs` 两处都记：它解释了同一条记录里 `ttft_ms` 与 `total_ms` 为什么会
    /// 差很多——TTFT 记的是上游首字节，而客户端是在末尾一次性收到整段的。没有这个标记的话，
    /// 这类记录在明细里看着就像一次「首字节极快、总耗时极长」的异常请求。
    pub(super) sse_aggregated: bool,
    /// 增量嗅探到的响应用量。
    pub(super) sniffer: UsageSniffer,
    /// 请求体里声明的速度档；仅在响应未回报 `usage.speed` 时兜底。
    pub(super) req_speed: Option<String>,
    /// 请求体里声明的模型名；仅在响应没带 `usage`（4xx/5xx，尤其是 429）时兜底。
    /// 否则那些记录只留下 `model=-`，排查「哪个模型被拒得多」时等于没有信息。
    pub(super) req_model: Option<String>,
    /// 上游返回的订阅账号限流快照。
    pub(super) ratelimit: RateLimitInfo,
    /// 转发途中上游把流掐了（传输层错误，非 `event: error`）时的错误描述。
    ///
    /// 与 [`UsageSniffer::stream_error`] 分开记：那个是上游**说**自己出错了，这个是连接
    /// 本身断了，两者排查方向不同（前者看上游侧原因，后者看网络/超时）。此前
    /// [`stream_upstream`] 里这个分支是 `if let Ok` 的隐式丢弃——错误原样交给 axum，
    /// 客户端拿到一条截断的流，服务端侧一行日志都没有。
    pub(super) stream_broke: Option<String>,
    /// luban 给这条入站请求发的 id，见 [`handle`]。
    pub(super) request_id: String,
    /// 来访请求头里带的 id（若有），见 [`client_request_id`]。
    pub(super) client_request_id: Option<String>,
    /// 上游**最后一次**响应头里的 `request-id`；换号/重试后以最终那一发为准。
    pub(super) upstream_request_id: Option<String>,
    /// 取证字段（出口、形态摘要、上游错误文案、改写标签……），见 [`store::Forensics`]。
    pub(super) forensics: store::Forensics,
    /// 逐请求遥测要用的请求侧材料（出站体、出站 beta、会话 id、组织 id）；`None` 即这条
    /// 不上报（开关关着、非计费路径、非 2xx）。响应侧的量在 Drop 时从 `sniffer` 取，
    /// 一起交给 [`crate::telemetry::Telemetry::record`]。
    pub(super) telemetry: Option<crate::telemetry::Capture>,
    /// 走模拟路径时这条请求用的会话 id；收尾时把本次的上游 `request-id` 与回复
    /// `message.id` 记回该会话，供**下一条**请求写 `cc_prev_req` 与
    /// `diagnostics.previous_message_id`，见 [`CcSessionLink::record`]。
    ///
    /// 非模拟路径为 `None`：真实 CC 来访自己维护这条链，luban 不该替它记，更不该拿自己
    /// 记的那份去覆盖。
    pub(super) cc_session: Option<String>,
    /// 这条请求所属的「零输出请求类」（模型 + `max_tokens`），见 [`empty_reply_class`]；
    /// `None` 即不属于任何一类（带 tools、多轮、非计费路径……），收尾时不学。
    pub(super) empty_reply_key: Option<(String, i64)>,
    /// 这条请求的「模型 + 提示词哈希」（[`prompt_digest`]），上游拒答（`stop_reason:
    /// "refusal"`）时按它学，见 [`known_refused_prompt`]；非计费路径为 `None`。
    pub(super) prompt_key: Option<(String, String)>,
    /// 这条请求的「模型 + system 哈希」（[`app_system_digest`]），只有**识别不了会话**的来访（没有
    /// 会话 id 也没有 device_id）且带 system 时才有；上游拒答时按它学，见 [`known_app_refusal`]。
    pub(super) app_key: Option<(String, String)>,
    /// 零输出请求类与被拒答提示词的记忆表，收尾时按上游回复的种类往里记，
    /// 见 [`ReqLog::note_unanswered_reply`]。
    pub(super) empty_replies: EmptyReplyMemory,
    /// 模拟路径替客户端注进去、客户端自己**没声明**的官方工具名（[`cc_tools_to_inject`]）；
    /// 非模拟路径与没注的为空。收尾时与回复里的 `tool_use` 名对一遍：模型若调了其中一个，
    /// 客户端会收到一个自己不认识的工具调用——这是注入策略的已知代价，此前只在文档里写着
    /// 「概率低」，没有任何地方量过。流水的 `shape` 只记 tool_use 的个数不记名字，
    /// `toolUseContentLengths` 那张表又把名字归了类，两处都答不了「调的是不是注入的」。
    pub(super) injected_tools: Vec<&'static str>,
    pub(super) store: std::sync::Arc<store::CredentialStore>,
    /// 在途计数句柄，见 [`InFlightGuard`]：只为让计数活到流结束，字段本身不读。
    pub(super) _in_flight: InFlightGuard,
    /// 会话并发在途的句柄，见 [`SessionConcurrencyGuard`]：让计数活到流结束。
    pub(super) _session_concurrency: SessionConcurrencyGuard,
    /// 「账号 + 模型」在飞格的句柄，见 [`UpstreamRouteGuard`]：同样只为让那一格活到流结束。
    pub(super) _route_load: UpstreamRouteGuard,
}

impl Drop for ReqLog {
    fn drop(&mut self) {
        self.sniffer.finish();
        // 下面两个分支会把 `stream_broke` take 掉；遥测那一步要知道这条有没有断，先记下。
        let stream_broke = self.stream_broke.clone();
        // 流内错误（`event: error` 裹在 200 里）：客户端拿到的是 200 + 错误负载，SDK 那头
        // 没有状态码，遥测要按 `in_band_<type>` 报，见 [`crate::telemetry::CallFailure`]。
        // 400/401/403 那条路也会把错误体喂给嗅探器，故这里以「改写前是不是 200」为判据。
        let in_band_error =
            self.sniffer.stream_error.is_some() && self.status == StatusCode::OK.as_u16();
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
            if self.forensics.error_type.is_none() {
                let err = payload.get("error");
                self.forensics.error_type =
                    err.and_then(|e| e.get("type")).and_then(|t| t.as_str()).map(str::to_string);
                self.forensics.error_message = Some(
                    err.and_then(|e| e.get("message"))
                        .and_then(|m| m.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| payload.to_string()),
                );
            }
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
        // 200 却没回答——`output_tokens = 0`（上游收了输入的钱、一个字没回）或
        // `stop_reason: "refusal"`（内容分类器拒答，正文为空或半截）——且流是完整收尾的：
        // 状态码与用量列都看不出它回了什么，这里把截下的响应体开头记进流水、打一行 warn，
        // 并按种类学进记忆表，之后本地拒，见 [`ReqLog::note_unanswered_reply`]。断流 / 没等到
        // `message_stop` 的不算：那是没收完，不是没回。
        if self.status == StatusCode::OK.as_u16()
            && (self.sniffer.output_tokens == Some(0) || self.sniffer.refused())
            && stream_broke.is_none()
            && (!self.sniffer.is_stream || self.sniffer.saw_message_stop || self.sse_aggregated)
        {
            self.note_unanswered_reply();
        }
        // 识别不了会话的应用：这条到过上游、拿到 200 却不是（可学的）拒答——只给应用的请求总数
        // 记一笔，拒答比例的分母就是它。拒答那条已在 note_unanswered_reply 里连分子一起记了。
        if self.status == StatusCode::OK.as_u16()
            && let Some((model, digest)) = self.app_key.take()
        {
            record_app_request(&self.empty_replies, &model, &digest, None);
        }
        // 请求模型拒答、上游按 `fallbacks` 换了模型作答：标签记下，`model` 列与计价都是
        // 作答的那个（响应顶层 `model` 已经是它）。
        if let Some(to) = self.sniffer.fallback_to.clone() {
            tracing::info!(
                cred_id = self.cred_id, cred = %self.cred_label,
                requested = %self.req_model.as_deref().unwrap_or("-"),
                served_by = %to,
                request_id = %self.request_id,
                "the requested model refused; upstream served the reply from the fallback model"
            );
            let tags = self.forensics.rewrites.get_or_insert_with(String::new);
            if !tags.is_empty() {
                tags.push(',');
            }
            tags.push_str(REWRITE_SERVED_BY_FALLBACK);
        }
        // 回复里要客户端执行的 tool_use 名单，与注入名单对一遍。名字挂在下面那条 `forwarded`
        // 上（没有就是 `-`），调到注入工具的另打一行 warn 并在 `rewrites` 列打标签——导出
        // 与库里按标签就能数出「多少次、哪些客户端」，不必翻日志。
        let tool_uses = self.sniffer.tool_use_names();
        let injected_called: Vec<&str> =
            tool_uses.iter().copied().filter(|n| self.injected_tools.contains(n)).collect();
        if !injected_called.is_empty() {
            tracing::warn!(
                cred_id = self.cred_id, cred = %self.cred_label,
                ua = %self.ua,
                model = %self.sniffer.model.as_deref().or(self.req_model.as_deref()).unwrap_or("-"),
                called = %injected_called.join(","),
                tool_uses = %tool_uses.join(","),
                request_id = %self.request_id,
                "the model called an injected CC tool the client never declared"
            );
            let tags = self.forensics.rewrites.get_or_insert_with(String::new);
            if !tags.is_empty() {
                tags.push(',');
            }
            tags.push_str(REWRITE_INJECTED_TOOL_CALLED);
        }
        let tool_uses_col =
            if tool_uses.is_empty() { "-".to_string() } else { tool_uses.join(",") };
        // 速度档以上游回报为准（fast 被限流时会回落），响应没带才退回请求声明。
        let speed = self.sniffer.speed.clone().or_else(|| self.req_speed.clone());
        // 模型同理以响应为准（上游可能回落到别的模型），没有才用请求侧声明的那个。
        let model = self.sniffer.model.clone().or_else(|| self.req_model.clone());
        // 输出前就被分类器拒掉的（`stop_reason: "refusal"`、零输出）上游**不计费**：usage 里
        // 报了 token 数但不扣钱（官方 Refusals and fallback 页）。照 usage 算会把一堆 0 输出的
        // 拒答算成真金白银。流到一半被掐的按已产出部分正常计费（输入 + 已流出的输出都算），
        // 走下面那条。「输出前」的判据不只看 `output_tokens`：那个数没解析到（`None`）时不能
        // 当 0——回复里已经见过任何内容块（[`UsageSniffer::saw_output_block`]，`fallback` 切换
        // 标记不算）的，就是流到一半被掐的，得计费。
        let refused_before_output = self.sniffer.refused()
            && self.sniffer.output_tokens.unwrap_or(0) == 0
            && !self.sniffer.saw_output_block;
        let cost_usd = if refused_before_output {
            Some(0.0)
        } else {
            crate::pricing::estimate_usd(crate::pricing::Usage {
                model: model.as_deref(),
                speed: speed.as_deref(),
                input_tokens: self.sniffer.input_tokens,
                output_tokens: self.sniffer.output_tokens,
                cache_creation_total: self.sniffer.cache_creation_tokens,
                cache_5m_tokens: self.sniffer.cache_creation_5m,
                cache_1h_tokens: self.sniffer.cache_creation_1h,
                cache_read_tokens: self.sniffer.cache_read_tokens,
            })
        };
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
            request_id = %self.request_id,
            client_request_id = %self.client_request_id.as_deref().unwrap_or("-"),
            upstream_request_id = %self.upstream_request_id.as_deref().unwrap_or("-"),
            tool_uses = %tool_uses_col,
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
            request_id: Some(self.request_id.clone()),
            upstream_request_id: self.upstream_request_id.clone(),
            forensics: std::mem::take(&mut self.forensics),
        };
        spawn_usage_log(self.store.clone(), rec);

        // 模拟路径的会话链条：把这一条的上游 request-id 与回复 message.id 记回去，
        // 同会话的下一条据此写 `cc_prev_req` 与 `diagnostics.previous_message_id`。
        // 失败的那些照记 request-id——官方那条链上并不跳过报错的请求。
        if let Some(sid) = &self.cc_session {
            CcSessionLink::record(
                CcSessionKey { cred_id: self.cred_id, session_id: sid },
                self.upstream_request_id.as_deref(),
                self.sniffer.message_id.as_deref(),
            );
        }

        // 官方客户端只对**成功拿到用量**的请求发 `tengu_api_success`：中途断流、上游报错、
        // 没有 usage 的一律不报。流式还要看见 `message_stop`：半截流的 `message_start` 也带
        // usage，但客户端那边这条是失败的，不会有 success 事件。聚合路径
        // （[`aggregate_sse`]）另有完整性检查，放行。
        let stream_complete =
            !self.sniffer.is_stream || self.sniffer.saw_message_stop || self.sse_aggregated;
        let ok = self.status == StatusCode::OK.as_u16()
            && has_usage
            && stream_broke.is_none()
            && stream_complete;
        // 失败的那些走 `tengu_api_error`。三类里只报**能确定是上游拒了**的两类：
        //
        // - 非 2xx（含流内 `event: error` 改写出来的那个状态码）；
        // - 上游传输层把流掐了（`stream_broke`，有明确的错误描述）。
        //
        // 「没报错、没断连，就是没等到 `message_stop`」那一类不报：这一层分不清是上游 EOF
        // 还是客户端自己走了（用户按 Ctrl-C 也长这样），替一次用户取消编一条上游错误，
        // 比不报更糟。同理 200 + 有 usage 但流不完整的也不报。
        let bad_status = self.status < 200 || self.status >= 300;
        let failure = (!ok && (bad_status || stream_broke.is_some())).then(|| {
            let (etype, message) = match (&self.forensics.error_type, &self.forensics.error_message)
            {
                (t, Some(m)) => (t.clone(), m.clone()),
                _ => match &self.sniffer.body_error {
                    Some((t, m)) => (t.clone(), m.clone()),
                    None => (None, String::new()),
                },
            };
            crate::telemetry::CallFailure {
                status: (!in_band_error && bad_status).then_some(self.status),
                error_type: etype,
                message: if message.is_empty() {
                    stream_broke.clone().unwrap_or_default()
                } else {
                    message
                },
                in_band: in_band_error,
            }
        });
        if let Some(cap) = self.telemetry.take()
            && (ok || failure.is_some())
        {
            let sink = cap.sink.clone();
            sink.record(crate::telemetry::ApiCall {
                cred_id: self.cred_id,
                account_uuid: cap.account_uuid,
                org_type: cap.org_type,
                body: cap.body,
                betas: cap.betas,
                session_header: cap.session_header,
                ua_out: self.ua_out.clone(),
                organization_id: cap.organization_id,
                started_at: cap.started_at,
                ttft_ms: self.ttft_ms.map(|v| v as u64),
                total_ms: total_ms as u64,
                request_id: self.upstream_request_id.clone(),
                client_request_id: cap.client_request_id,
                message_id: self.sniffer.message_id.clone(),
                stop_reason: self.sniffer.stop_reason.clone(),
                resp_model: self.sniffer.model.clone(),
                input_tokens: self.sniffer.input_tokens.unwrap_or(0),
                output_tokens: self.sniffer.output_tokens.unwrap_or(0),
                cache_read_tokens: self.sniffer.cache_read_tokens.unwrap_or(0),
                cache_creation_tokens: self.sniffer.cache_creation_tokens.unwrap_or(0),
                text_chars: self.sniffer.text_chars,
                thinking_chars: self.sniffer.thinking_chars,
                saw_thinking: self.sniffer.saw_thinking,
                tool_use_lens: self.sniffer.tool_use_lens(),
                cost_usd,
                speed,
                failure,
            });
        }
    }
}

impl ReqLog {
    /// 记一次 luban 侧的改写重试：追加标签，把上游 request-id 换成重试那一发的，并把
    /// **请求侧**那些由 body 算出来的东西一并换成重试实际发出去的那份。
    ///
    /// 重试成功后这条流水按重试那次记账（见 [`retry_demoted_thinking`]），标签是唯一能看出
    /// 「中间还发过一发被拒的」的地方。
    ///
    /// `sent` 不能省。响应侧（状态码、用量、model、stop_reason、限流）调用方已经换成重试
    /// 那一发了，请求侧却还挂着首发那份 body——于是遥测里 `messageCount` /
    /// `inputTextCharLength` / `toolsCount` / 「是不是新输入」全都算的是**一条被上游拒了的
    /// 请求**，而同一条事件里的 requestId 与 token 来自另一条。prefill 那条尤其明显：
    /// [`strip_assistant_prefill`] 直接弹掉末尾的 assistant 轮，`messageCount` 是真的变了。
    /// 取证的 `shape` 列同理——它要回答的是「发出去的到底长什么样」。
    pub(super) fn note_retry(&mut self, tag: &str, headers: &HeaderMap, sent: &Bytes) {
        let tags = self.forensics.rewrites.get_or_insert_with(String::new);
        if !tags.is_empty() {
            tags.push(',');
        }
        tags.push_str(tag);
        if let Some(rid) = header_opt(headers, "request-id") {
            self.upstream_request_id = Some(rid);
        }
        if let Some(cap) = &mut self.telemetry {
            cap.body = sent.clone();
        }
        // 只换形态摘要那一项：身份三项（session/device）重试前后逐字相同——两发都过同一个
        // `upstream.shape()`，只有 messages 内容不同。
        //
        // 这条路上只拿得到字节，故要自己解析一遍。主路径不必（改写那一步把 `Value` 顺手交出来
        // 了，见 [`shape_summary_of`]）；重试是 400 兜底，一条请求最多走一次，不值得为它把
        // 一份解析态的体从改写那里一路拿到这里。
        self.forensics.shape = shape_summary(sent).shape;
    }

    /// 上游回了 200 却没回答（判据见 [`Drop`] 里的调用处）：响应体开头记进流水的
    /// `response_excerpt`、打一行 warn 把原话带上，再按**种类**学、写穿落库：
    ///
    /// - `stop_reason: "refusal"`（标签 `refusal`）：是内容分类器拒了**这一条提示词**
    ///   （`stop_details.category` 如 `cyber`），与请求形态无关——按 [`Self::prompt_key`]
    ///   （模型 + 提示词哈希）学，之后只拦逐字相同的提示词重发。**不能**按请求类学：
    ///   一条触发拒答的内容会把同形态的所有正常请求一起拦掉（v0.3.89 就犯过这个错，
    ///   `opus-5 + max_tokens 65536` 那一类被一条 cyber 拒答连坐）。且只学
    ///   [`UsageSniffer::classifier_refusal`] 认定的那种：category 为空（模型自己拒的、带
    ///   采样）或带 `recommended_model`（fallback 没跑成）的拒答只记流水、不学——重发可能
    ///   就答了，学了是把能救的请求锁死 7 天；
    /// - 其余零输出（标签 `empty_reply`）：上游对这类请求形态本身不回——按
    ///   [`Self::empty_reply_key`]（模型 + 无 tools 单条消息 + `max_tokens`）学。
    fn note_unanswered_reply(&mut self) {
        let excerpt = self.sniffer.excerpt().unwrap_or_default();
        let model = self.sniffer.model.clone().or_else(|| self.req_model.clone());
        let refused = self.sniffer.refused();
        // 先判定再打日志：一行里就能看出这条是拒答还是零输出、分类器给了什么、学没学。
        // 拒答只学分类器判决（category 非空且没有 recommended_model）；模型自拒带采样，
        // 原样重发可能就答了；带 recommended_model 说明 fallback 没跑成，直接重试可能就成。
        let verdict = self.sniffer.classifier_refusal().map(str::to_string);
        // 拒答要学还得有上游那次的原样响应体（命中时回放的就是它）：体太大、流到一半才被掐
        // 的都没有，见 [`UsageSniffer::refusal_reply`]。
        let reply = if refused && verdict.is_some() { self.sniffer.refusal_reply() } else { None };
        let learn = if !refused {
            if self.empty_reply_key.is_some() { "request_class" } else { "none" }
        } else if verdict.is_some() {
            if self.prompt_key.is_some() && reply.is_some() { "prompt" } else { "none" }
        } else {
            "none"
        };
        let reply_note = match (&reply, refused && verdict.is_some()) {
            (Some(r), _) => {
                if r.sse {
                    "kept_sse"
                } else {
                    "kept_json"
                }
            }
            (None, true) if self.sniffer.saw_output_block => "mid_stream",
            (None, true) if self.sniffer.reply_overflow => "too_large",
            (None, true) => "missing",
            (None, false) => "-",
        };
        tracing::warn!(
            cred_id = self.cred_id, cred = %self.cred_label,
            model = %model.as_deref().unwrap_or("-"),
            input_tokens = self.sniffer.input_tokens.unwrap_or(0),
            output_tokens = self.sniffer.output_tokens.unwrap_or(0),
            stop_reason = %self.sniffer.stop_reason.as_deref().unwrap_or("-"),
            category = %self.sniffer.refusal_category.as_deref().unwrap_or("-"),
            recommended_model = %self.sniffer.refusal_recommended_model.as_deref().unwrap_or("-"),
            request_id = %self.request_id,
            upstream_request_id = %self.upstream_request_id.as_deref().unwrap_or("-"),
            response = %excerpt.chars().take(500).collect::<String>(),
            kind = if refused { "refusal" } else { "empty_reply" },
            learn,
            reply = reply_note,
            "upstream returned 200 without an answer; the reply is kept on the usage log. learn=prompt: an identical resend of this exact prompt gets this upstream refusal replayed locally from now on (classifier verdict; reply=kept_*); learn=request_class: this model + request class is rejected locally; learn=none: nothing learned, a resend goes upstream (reply=mid_stream / too_large: the refusal came after output or the body was too big to replay)"
        );
        self.forensics.response_excerpt = (!excerpt.is_empty()).then(|| excerpt.clone());
        let tags = self.forensics.rewrites.get_or_insert_with(String::new);
        if !tags.is_empty() {
            tags.push(',');
        }
        tags.push_str(if refused { REWRITE_REFUSAL } else { REWRITE_EMPTY_REPLY });
        let mut learned: Vec<store::LearnedRejection> = Vec::new();
        if refused {
            if let Some((category, reply)) = verdict.zip(reply) {
                // 规则文案 = 「[类别] stop_details=<原样 JSON>」：设置页与日志一眼看出是
                // 哪类判决、上游给了什么解释。取 stop_details 而不是响应体开头——开头是
                // usage 样板，判决在流末尾；有 verdict 就一定解析到过 stop_details 对象，
                // 退回 excerpt 只是防御。回给客户端的不是这段文案，是 `reply` 里上游的原样体。
                let message = match &self.sniffer.refusal_details {
                    Some(details) => format!("[{category}] stop_details={details}"),
                    None => format!("[{category}] {excerpt}"),
                };
                // 按提示词学（所有来访）：只拦逐字相同的重发。
                if let Some((model, digest)) = self.prompt_key.take() {
                    learned.extend(remember_refused_prompt(
                        &self.empty_replies,
                        &model,
                        &digest,
                        &message,
                        reply.clone(),
                    ));
                }
                // 按应用学（只有识别不了会话的来访）：给应用记一条拒答，拒答比例够了才学成
                // 「同一模型 + 同一份 system 一律回放」。`take` 掉后 Drop 里那条「只计总数」不再重复计。
                if let Some((model, digest)) = self.app_key.take() {
                    learned.extend(record_app_request(
                        &self.empty_replies,
                        &model,
                        &digest,
                        Some((&message, &reply)),
                    ));
                }
            }
        } else if let Some((model, max_tokens)) = self.empty_reply_key.take() {
            learned.extend(remember_empty_reply(&self.empty_replies, &model, max_tokens, &excerpt));
        }
        if !learned.is_empty()
            && let Err(e) = self.store.remember_rejections(&learned)
        {
            // 写穿落库：进程内表已经更新，落库失败只影响重启后要不要重学，不影响本次。
            tracing::warn!(error = %e, "persisting the learned reply rule failed (kept in memory)");
        }
    }
}

/// 流水 `rewrites` 列里标「上游 200 却零输出」的标签，见 [`ReqLog::note_unanswered_reply`]。
const REWRITE_EMPTY_REPLY: &str = "empty_reply";
/// 流水 `rewrites` 列里标「上游拒答（`stop_reason: "refusal"`）」的标签。
const REWRITE_REFUSAL: &str = "refusal";
/// 流水 `rewrites` 列里标「请求模型拒答、由 fallback 模型作答」的标签。
const REWRITE_SERVED_BY_FALLBACK: &str = "served_by_fallback";
/// 流水 `rewrites` 列里标「模型调了模拟路径注入的、客户端没声明的官方工具」的标签，
/// 见 [`ReqLog::injected_tools`]。
const REWRITE_INJECTED_TOOL_CALLED: &str = "injected_tool_called";

/// 组一份 [`store::BanContext`]：状态码、上游 `error.type`/完整 message、两侧请求 id。
pub(super) fn ban_context(
    reason: &str,
    source: &'static str,
    status: StatusCode,
    body: &[u8],
    request_id: &str,
    upstream_request_id: Option<&str>,
) -> store::BanContext {
    let (etype, message) = parse_upstream_error(body);
    store::BanContext {
        reason: reason.to_string(),
        source,
        status: Some(status.as_u16()),
        error_type: etype,
        error_message: Some(message),
        request_id: Some(request_id.to_string()),
        upstream_request_id: upstream_request_id.map(str::to_string),
    }
}

/// 转发那一刻能确定的取证字段：出口代理、是否模拟、出站体形态摘要、会话 id。
/// 错误文案、第三方判定、改写标签在响应到达后再补（见 [`handle`] 的 4xx 段与 [`ReqLog::note_retry`]）。
/// 在**没有 [`ReqLog`]** 的早退路径上，补上失败请求该有的两件事：把这一发记进模拟会话链，
/// 与报一条 `tengu_api_error`。正常路径上这两件事都在 [`ReqLog::drop`] 里。
///
/// 两个调用点：`upstream.send()` 连接层就失败（`ReqLog` 建都没建），以及 401 那条路
/// ——它把响应体 `bytes()` 掉了，没法再 `break` 出去走正常收尾，只能就地返回。
///
/// 会话链照记：官方那条 `cc_prev_req` 链**不跳过报错的请求**（同 [`ReqLog::drop`] 里的
/// 说明）。连接层失败那条没有上游 request-id，`record` 会自己跳过。
///
/// `organization_id` 取自**上游响应头** `anthropic-organization-id`，事件的 `auth` 块要它
/// （[`crate::telemetry::Identity::auth_block`]：没有就整个 `organization_uuid` 键都不出现，
/// 而订阅/团队账号官方每条都带）。遥测那边按凭证缓存过一份，同一个号只要之前有过一条带
/// 这个头的响应就还补得上；但一个进程里**头一条就是早退 401** 的号没有那份缓存，所以
/// 拿得到就得往下传。连接层失败那条压根没有响应，只能是 `None`。
#[allow(clippy::too_many_arguments)]
pub(super) fn record_early_failure(
    state: &AppState,
    cred: &crate::credentials::Credential,
    upstream: &Upstream<'_>,
    sent: &Bytes,
    started: std::time::Instant,
    flags: store::ForwardFlags,
    billable: bool,
    up_request_id: Option<&str>,
    organization_id: Option<String>,
    failure: crate::telemetry::CallFailure,
) {
    if let Some(sid) = upstream
        .sim
        .as_ref()
        .map(|s| s.session_id.clone())
        .or_else(|| upstream.client_link.as_ref().map(|(sid, _)| sid.clone()))
    {
        CcSessionLink::record(
            CcSessionKey { cred_id: cred.id, session_id: &sid },
            up_request_id,
            None,
        );
    }
    let Some(cap) =
        telemetry_capture(state, cred, upstream, sent, started, flags, billable, organization_id)
    else {
        return;
    };
    cap.record_failure(
        cred.id,
        ua_of(&upstream.headers),
        started.elapsed().as_millis() as u64,
        up_request_id.map(str::to_string),
        failure,
    );
}

/// 逐请求遥测的请求侧材料，见 [`crate::telemetry::Capture`]。开关关着或非计费路径为 `None`。
///
/// 三个调用点共用：正常路径（建 [`ReqLog`] 时）、连接层就失败那条、以及 401 换号那条
/// ——后两条压根没有 `ReqLog`，得自己就地报一条 `tengu_api_error`。字段拼装只有这一份，
/// 免得哪天正常路径加了个字段、另外两条忘了跟。
#[allow(clippy::too_many_arguments)]
pub(super) fn telemetry_capture(
    state: &AppState,
    cred: &crate::credentials::Credential,
    upstream: &Upstream<'_>,
    sent: &Bytes,
    started: std::time::Instant,
    flags: store::ForwardFlags,
    billable: bool,
    organization_id: Option<String>,
) -> Option<crate::telemetry::Capture> {
    // 组织 id 先拿凭证上 profile 存的那份垫底；响应头里的（`organization_id`）随后覆盖。
    state.telemetry.seed_org_uuid(cred.id, cred.org_uuid.as_deref());
    (flags.api_telemetry && billable).then(|| crate::telemetry::Capture {
        sink: state.telemetry.clone(),
        account_uuid: cred.account_uuid.clone(),
        org_type: cred.org_type.clone(),
        body: sent.clone(),
        betas: header_opt(&upstream.headers, "anthropic-beta"),
        session_header: header_opt(&upstream.headers, "x-claude-code-session-id"),
        client_request_id: header_opt(&upstream.headers, "x-client-request-id"),
        organization_id,
        started_at: std::time::SystemTime::now()
            .checked_sub(started.elapsed())
            .unwrap_or_else(std::time::SystemTime::now),
    })
}

pub(super) fn capture_forensics(
    upstream: &Upstream<'_>,
    sent: &Bytes,
    cred: &crate::credentials::Credential,
) -> store::Forensics {
    let mut f = capture_forensics_without_body(upstream, cred);
    fill_shape_forensics(&mut f, shape_summary(sent));
    f
}

/// [`capture_forensics`] 里**不看出站体**的那几项。
///
/// 转发主路径用这个：形态摘要那三项留空，由调用方拿 [`Upstream::shape_outbound`] 交回来的
/// [`ShapeBits`] 经 [`fill_shape_forensics`] 填上。那三项是改写那一步顺手算出来的，这里再从
/// 字节解析一遍就是同一份几 MB 的 JSON 解析两次。
///
/// `session_id` 这里先放出站头里的那个：它与体里那个在官方形态下逐字相同，体里没有
/// `metadata.user_id` 时它也是唯一的来源。体里真有的话，补那一步会用体里的覆盖回来
/// （与此前 `session_from_body.or_else(header)` 同序）。
pub(super) fn capture_forensics_without_body(
    upstream: &Upstream<'_>,
    cred: &crate::credentials::Credential,
) -> store::Forensics {
    store::Forensics {
        proxy: cred.proxy.as_deref().map(store::redact_proxy),
        simulated: upstream.sim.is_some(),
        sim_reason: upstream.sim.as_ref().map(|s| s.reason.tag().to_string()),
        session_id: header_opt(&upstream.headers, "x-claude-code-session-id"),
        ..Default::default()
    }
}

/// 把出站体的取证三项补进取证字段。
///
/// 出站体里取到的 session_id 覆盖已有的（那是出站头里的兜底），取不到就留着原来的——
/// 与 [`capture_forensics`] 此前 `session_from_body.or_else(header)` 的取舍一致。
pub(super) fn fill_shape_forensics(f: &mut store::Forensics, bits: ShapeBits) {
    f.shape = bits.shape;
    if bits.session_id.is_some() {
        f.session_id = bits.session_id;
    }
    f.device_id_out = bits.device_id_out;
}

/// 出站请求体的**结构**摘要（不含任何用户正文），落进流水的 `shape` 列。
///
/// 与 [`request_digest`] 的区别：那个是排障日志用的、逐轮列出消息块，几百轮的会话一条就几 KB；
/// 这个每条请求都落库，只留计数、哈希与顶层参数，大小与会话长度无关。返回值的第二、三项是从
/// `metadata.user_id` 里取出的 session_id 与 device_id（两种格式都认，见 [`extract_session_id`]
/// 与 [`extract_device_id`]），没有则 `None`。这里读的是**出站**体，故取到的 device_id 是上游
/// 实际看到的那个（伪装开着时为派生值），落进 `device_id_out` 列。
///
/// 摘要里各项都是「拿被封的号与活着的号对照」时要看的维度：
/// - `keys`：顶层字段及**顺序**（官方客户端的顺序是固定的，多一个字段、换个顺序都是判据）；
/// - `system`：块数、每块长度与是否带 cache_control、全文 sha256 前 16 位（同一份基座提示词
///   哈希相同，直接按哈希分组就能看出「哪种 system 前缀的号被封」）；
/// - `tools`：数量、名字列表（最多 64 个）与名字串的哈希；
/// - `messages`：条数、末条角色、各类内容块计数（text/tool_use/tool_result/thinking/image…）；
/// - 其余顶层参数（model/max_tokens/stream/thinking/temperature/tool_choice/…）原样。
pub(super) fn shape_summary(sent: &[u8]) -> ShapeBits {
    match serde_json::from_slice::<serde_json::Value>(sent) {
        Ok(v) => shape_summary_of(&v),
        Err(_) => ShapeBits::default(),
    }
}

/// 出站体的取证三项：形态摘要、出站 session_id、出站 device_id。
///
/// 三项同源（都从同一份出站 `Value` 上读），故一并算、一并传。
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ShapeBits {
    pub(super) shape: Option<String>,
    pub(super) session_id: Option<String>,
    pub(super) device_id_out: Option<String>,
}

/// [`shape_summary`] 的本体：吃一份**已经解析好**的出站体。
///
/// 转发主路径走这个，`Value` 由改写那一步顺手交出来（见 [`rewrite_body_out`]）——同一份几 MB
/// 的 JSON 一条请求里只解析一次。只拿得到字节的那几条路（重试改写、早退兜底、连通性测试）
/// 走上面那个包装。
pub(super) fn shape_summary_of(v: &serde_json::Value) -> ShapeBits {
    use sha2::{Digest, Sha256};
    let Some(obj) = v.as_object() else { return ShapeBits::default() };
    // 两种 `metadata.user_id` 格式（内嵌 JSON / 扁平串）都由这两个函数认，别在下面再手写一套。
    let session = extract_session_id(Some(v));
    let device = extract_device_id(Some(v));
    let sha16 = |text: &str| -> String {
        let d = Sha256::digest(text.as_bytes());
        d.iter().take(8).map(|b| format!("{b:02x}")).collect()
    };
    let mut out = serde_json::Map::new();
    out.insert("keys".into(), serde_json::json!(obj.keys().collect::<Vec<_>>()));
    for (k, val) in obj {
        let digest = match k.as_str() {
            "system" => {
                let blocks: Vec<(usize, bool, &str)> = match val {
                    serde_json::Value::String(s) => vec![(s.len(), false, s.as_str())],
                    serde_json::Value::Array(bs) => bs
                        .iter()
                        .map(|b| {
                            let t = b.get("text").and_then(|t| t.as_str()).unwrap_or("");
                            (t.len(), b.get("cache_control").is_some(), t)
                        })
                        .collect(),
                    _ => Vec::new(),
                };
                let all: String = blocks.iter().map(|b| b.2).collect();
                serde_json::json!({
                    "blocks": blocks.iter().map(|b| serde_json::json!({"len": b.0, "cache": b.1})).collect::<Vec<_>>(),
                    "sha": sha16(&all)})
            }
            "tools" => {
                let names: Vec<&str> = val
                    .as_array()
                    .map(|a| {
                        a.iter().filter_map(|t| t.get("name").and_then(|n| n.as_str())).collect()
                    })
                    .unwrap_or_default();
                serde_json::json!({
                    "count": names.len(),
                    "sha": sha16(&names.join(",")),
                    "names": names.iter().take(64).collect::<Vec<_>>()})
            }
            "messages" => {
                let arr = val.as_array().map(|a| a.as_slice()).unwrap_or(&[]);
                let mut kinds: std::collections::BTreeMap<String, usize> = Default::default();
                for m in arr {
                    match m.get("content") {
                        Some(serde_json::Value::Array(bs)) => {
                            for b in bs {
                                let t = b.get("type").and_then(|t| t.as_str()).unwrap_or("?");
                                *kinds.entry(t.to_string()).or_default() += 1;
                            }
                        }
                        Some(serde_json::Value::String(_)) => {
                            *kinds.entry("text".into()).or_default() += 1;
                        }
                        _ => {}
                    }
                }
                serde_json::json!({
                    "count": arr.len(),
                    "last_role": arr.last().and_then(|m| m.get("role")).cloned(),
                    "blocks": kinds})
            }
            "metadata" => {
                let uid = val.get("user_id").and_then(|u| u.as_str());
                serde_json::json!({ "user_id": uid.is_some(), "user_id_len": uid.map(str::len) })
            }
            _ => val.clone(),
        };
        out.insert(k.clone(), digest);
    }
    ShapeBits {
        shape: Some(serde_json::Value::Object(out).to_string()),
        session_id: session,
        device_id_out: device,
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
pub(super) fn spawn_usage_log(
    store: std::sync::Arc<store::CredentialStore>,
    rec: store::UsageRecord,
) {
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
pub(super) struct UsageSniffer {
    is_stream: bool,
    /// 响应体带我们解不开的 `content-encoding`，只能一律不解析（`feed` 直接丢弃）。
    /// 正常路径下恒为 false——wreq 已解码，见 [`handle`] 里 `compressed` 的说明。
    opaque: bool,
    /// SSE 模式下未处理完的行尾；非流式模式下累积的整段响应体。
    buf: Vec<u8>,
    pub(super) model: Option<String>,
    pub(super) input_tokens: Option<i64>,
    pub(super) output_tokens: Option<i64>,
    pub(super) cache_creation_tokens: Option<i64>,
    /// 缓存写细分：5 分钟 / 1 小时档（上游 `usage.cache_creation` 下）。
    pub(super) cache_creation_5m: Option<i64>,
    pub(super) cache_creation_1h: Option<i64>,
    pub(super) cache_read_tokens: Option<i64>,
    /// 上游回报的实际速度档（`usage.speed`，如 `"fast"`）。fast 有独立限流，
    /// 被限流时会回落到标准档，故以响应为准、请求体只作兜底。
    pub(super) speed: Option<String>,
    /// 流中途上游改口报错的那份 `event: error` 负载（整个 data 对象）。
    ///
    /// 这类错误是裹在 **200** 里来的：响应头早已发出，靠状态码看不出任何异常。此前
    /// [`Self::merge`] 只挑 usage/model，它从眼前流过去不留痕迹，于是一次失败在日志与
    /// `usage_logs` 里都是一条 `status=200`——客户端那头报错（如上游发的 `client_gone`），
    /// 服务端这头查无此事，且成功率统计里凭空少了一次失败。收尾时由 [`ReqLog::drop`]
    /// 取走告警。[`aggregate_sse`] 那条路另有 [`SseAggregator`] 就地处理，不走这里。
    pub(super) stream_error: Option<serde_json::Value>,
    /// 见过 `message_stop` —— 流式响应正常收尾的唯一标志。
    ///
    /// 上游的流可能既不报错、也不断连，就是**发到一半 EOF**：`bytes_stream` 平静地返回
    /// `None`，[`Self::stream_error`] 和 [`ReqLog::stream_broke`] 双双为空，这一层看什么
    /// 都正常，而客户端拿到的是半截回复（Claude Code 报 `Connection closed mid-response`）。
    /// [`aggregate_sse`] 靠 [`Aggregated::Incomplete`] 拦住了这一类，透传路径此前没有对应
    /// 的检查——三种断流方式里最安静的那种，恰恰完全无声。
    pub(super) saw_message_stop: bool,
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
    pub(super) last_event: Option<String>,
    pub(super) events: u32,
    /// 响应的 `message.id`（`msg_…`）、最终 `stop_reason`，以及正文里 text / thinking 的
    /// 字符数。都是逐请求遥测（`tengu_api_success` / `tengu_turn_first_text` /
    /// `tengu_prompt_cache_diagnosis_received`）要的量，顺着已经在做的逐行解析记下来。
    message_id: Option<String>,
    stop_reason: Option<String>,
    /// `stop_details.category`（非流式顶层 / `message_delta.delta`）：拒答时分类器给的类别
    /// （`cyber`、`bio`、…）。**为空的拒答是模型自己拒的**（官方：分类器拦截与模型拒绝都走
    /// `stop_reason: "refusal"`，靠这一项区分），带采样、重发可能就答了，不能学。
    refusal_category: Option<String>,
    /// `stop_details.recommended_model`：带 `fallbacks` 的请求，fallback 模型限流/过载时
    /// 上游不跑 fallback、把主模型的拒答原样返回并在这里提示「直接重试这个模型可能成功」。
    /// 有它的拒答是没救成，不是救不了，同样不能学。
    refusal_recommended_model: Option<String>,
    /// `stop_details` 对象**原样**的紧凑 JSON（`{"type":"refusal","category":"cyber",
    /// "explanation":…}`）。拒答规则的文案取的是它，不是响应体开头：流式回复的开头是
    /// `message_start` 那段 usage 样板，拒答的判决在流**末尾**的 `message_delta` 里，按开头
    /// 截 500 字永远够不到——0.3.93 之前学到的 refusal 规则文案就全是一段 usage JSON。
    refusal_details: Option<String>,
    pub(super) text_chars: usize,
    pub(super) thinking_chars: usize,
    /// 回复里出现过 `thinking` / `redacted_thinking` 块。
    ///
    /// `thinkingContentLength` 官方的判据是「这条回复里有没有思考块」，而不是「思考字数
    /// 大于零」（`redacted_thinking` 与被截断的思考块都是 0 字），此前只能拿「没有正文」
    /// 当代理指标。
    pub(super) saw_thinking: bool,
    /// 回复里出现过**任何**输出内容块（`fallback` 切换标记除外——它不是模型产出）。拒答计价
    /// 用：`output_tokens` 没解析到时，靠它分「输出前被拒（不计费）」与「流到一半被掐（已
    /// 流出的照常计费）」；只看 text / thinking / tool_use 三种会漏掉 `server_tool_use` 的结果
    /// 块之类不认识的类型。
    saw_output_block: bool,
    /// 回复里的 `tool_use` 块：按出现顺序记名字与 `input` 的 JSON 串。
    ///
    /// `toolUseContentLengths` 报的是「这条回复里每个工具的入参 JSON 长度之和」，流式下
    /// 入参是 `input_json_delta` 一片片来的，得按内容块序号拼回去。
    tool_uses: Vec<ToolUseBlock>,
    /// 响应体本身就是一份错误 JSON（`{"type":"error","error":{...}}`）时的类型与文案。
    ///
    /// 非流式 4xx/5xx 的整段体本来就攒在 `buf` 里、`finish` 时会解析一次，顺手记下——
    /// `tengu_api_error` 的 `error` 与 `errorType` 要的正是这两项，而
    /// [`capture_forensics`] 那份只在 400/401/403 与裸 429 上填。
    pub(super) body_error: Option<(Option<String>, String)>,
    /// 回复里的 `fallback` 内容块（`{"type":"fallback","from":{"model":..},"to":{"model":..}}`）：
    /// 请求模型拒答、上游换了 `to` 那个模型作答。记最后一次切换的目标。顶层 `model` /
    /// `message_start.message.model` 已经是作答的那个（[`Self::model`]），计价按它。
    pub(super) fallback_to: Option<String>,
    /// 响应体**原样**的开头，最多 [`RESPONSE_EXCERPT_BYTES`] 字节，流式非流式都留。
    ///
    /// 与 `buf` 分开：流式那条路的 `buf` 逐行吃掉不留痕，非流式那条路 `finish` 后才解析。
    /// 这份只在收尾时上游回了「200 却零输出」才用得上（[`ReqLog::note_empty_reply`]）——
    /// 用量列写着 `input=425 output=0`，却没有任何一处能看到上游到底回了什么：是没有
    /// `content` 的空 Message、`stop_reason: "refusal"`、还是别的形状。留下开头这一小段
    /// 就够回答这个问题，代价是每条请求多拷几 KB。
    head: Vec<u8>,
    /// 响应体**原样全文**，供学到拒答时原样回放（[`Self::refusal_reply`]）。只在还没见到任何
    /// 输出内容块、且没超过 [`REFUSAL_REPLY_BYTES`] 时攒：正常回复几 KB 内就到第一个
    /// `content_block_start`，此后不再拷；超上限的清空并标 `reply_overflow`，这条不学。
    pub(super) reply: Vec<u8>,
    pub(super) reply_overflow: bool,
}

/// [`UsageSniffer::head`] 最多留多少字节。零输出的回复本身只有几百字节；8 KiB 连流式的
/// `message_start` + `message_delta` + `message_stop` 三段也装得下。
pub(super) const RESPONSE_EXCERPT_BYTES: usize = 8 * 1024;

/// 回复里一个 `tool_use` 块：内容块序号、工具名、`input` 的 JSON 串。
#[derive(Default, Clone)]
struct ToolUseBlock {
    index: i64,
    /// 遥测用的归类名（[`tool_use_label`]：`mcp__*` 归 `mcp_tool`、`skill__*` 归 `skill_tool`）。
    name: String,
    /// 上游回的原名，一字不改——回答「模型到底调了哪个」用的是它，归类名答不了。
    /// 回程还原假名在嗅探之后（[`stream_upstream`] 先喂嗅探器再过 [`restore_tool_names_stream`]），
    /// 故客户端自有工具在这里是 `mcp__luban__*` 那个假名，注入的官方工具是官方名。
    raw_name: String,
    /// 是客户端要去执行的 `tool_use`（`server_tool_use` / `mcp_tool_use` 由上游自己跑完，
    /// 客户端只看结果，不算）。
    client_side: bool,
    /// 流式下是拼起来的 `partial_json`；非流式下是 `input` 直接序列化的结果。
    json: String,
    /// 见过 `input_json_delta`：此后 `json` 是增量拼的，别再被 `content_block_start`
    /// 里那个空 `{}` 盖回去。
    from_delta: bool,
}

impl UsageSniffer {
    pub(super) fn new(is_stream: bool, opaque: bool) -> Self {
        Self { is_stream, opaque, ..Default::default() }
    }

    /// 喂入一块响应字节。
    pub(super) fn feed(&mut self, chunk: &[u8]) {
        if self.opaque {
            return;
        }
        if self.head.len() < RESPONSE_EXCERPT_BYTES {
            let room = RESPONSE_EXCERPT_BYTES - self.head.len();
            self.head.extend_from_slice(&chunk[..chunk.len().min(room)]);
        }
        if !self.reply_overflow && !self.saw_output_block {
            if self.reply.len() + chunk.len() > REFUSAL_REPLY_BYTES {
                self.reply = Vec::new();
                self.reply_overflow = true;
            } else {
                self.reply.extend_from_slice(chunk);
            }
        }
        if self.is_stream {
            // `buf` 先整个挪出来：待解析的行是它的切片，而 [`Self::parse_line`] 要 `&mut self`,
            // 借用检查不允许两者同时存在。挪出来之后两个借用就不相干了。
            //
            // **逐行 `drain` 换成了「扫完一次性 drain」**：`Vec::drain(..=pos)` 每行都要把余下
            // 的字节整段前移，`collect()` 每行还要再分配一个 `Vec`。一个 SSE 块里几十行是常
            // 态，于是块内是 O(行数 × 块长) 的搬运加几十次堆分配——而这段在每个回程流块上都
            // 跑。现在按下标切片逐行解析，搬运只在末尾发生一次，一行都不用另外分配。
            let mut buf = std::mem::take(&mut self.buf);
            buf.extend_from_slice(chunk);
            // 逐个完整行处理，保留最后不完整的一段在 buf 里。
            let mut start = 0;
            while let Some(rel) = buf[start..].iter().position(|&b| b == b'\n') {
                let end = start + rel;
                self.parse_line(&buf[start..end]);
                start = end + 1;
            }
            buf.drain(..start);
            // 防御：异常超长行避免无界增长。
            if buf.len() > 1_000_000 {
                buf.clear();
            }
            self.buf = buf;
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
                // 只在换了类型时才重新分配：一条长回复里连着几千个 `content_block_delta`，
                // 每个都 `to_string()` 就是几千次一模一样的小分配。
                if self.last_event.as_deref() != Some(t) {
                    self.last_event = Some(t.to_string());
                }
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
        // `message_start.message.id` / 非流式顶层 `id`。
        if let Some(id) = v
            .get("message")
            .and_then(|m| m.get("id"))
            .or_else(|| v.get("id"))
            .and_then(|i| i.as_str())
            .filter(|i| i.starts_with("msg_"))
        {
            self.message_id = Some(id.to_string());
        }
        // `message_delta.delta.stop_reason` / 非流式顶层 `stop_reason`（`null` 不算）。
        if let Some(sr) = v
            .get("delta")
            .and_then(|d| d.get("stop_reason"))
            .or_else(|| v.get("stop_reason"))
            .and_then(|s| s.as_str())
        {
            self.stop_reason = Some(sr.to_string());
        }
        // `message_delta.delta.stop_details` / 非流式顶层 `stop_details`：只在拒答时是对象，
        // 其余 stop_reason 下是 `null`。`category` 与 `recommended_model` 都可能为 `null`。
        if let Some(sd) = v
            .get("delta")
            .and_then(|d| d.get("stop_details"))
            .or_else(|| v.get("stop_details"))
            .filter(|s| s.is_object())
        {
            self.refusal_category = sd.get("category").and_then(|c| c.as_str()).map(str::to_string);
            self.refusal_recommended_model =
                sd.get("recommended_model").and_then(|m| m.as_str()).map(str::to_string);
            self.refusal_details = Some(sd.to_string());
        }
        // 正文字符数：流式看 `content_block_delta.delta`，非流式看顶层 `content[]`。
        if let Some(d) = v.get("delta") {
            if let Some(t) = d.get("text").and_then(|t| t.as_str()) {
                self.text_chars += t.encode_utf16().count();
            }
            if let Some(t) = d.get("thinking").and_then(|t| t.as_str()) {
                self.thinking_chars += t.encode_utf16().count();
            }
        }
        // 块的类型与工具入参：流式靠 `content_block_start` 开头、`input_json_delta` 续上。
        match v.get("type").and_then(|t| t.as_str()) {
            Some("content_block_start") => {
                let index = v.get("index").and_then(|i| i.as_i64()).unwrap_or(-1);
                if let Some(cb) = v.get("content_block") {
                    self.note_block(index, cb);
                }
            }
            Some("content_block_delta") => {
                if let Some(pj) =
                    v.get("delta").and_then(|d| d.get("partial_json")).and_then(|p| p.as_str())
                {
                    let index = v.get("index").and_then(|i| i.as_i64()).unwrap_or(-1);
                    if let Some(b) = self.tool_uses.iter_mut().find(|b| b.index == index) {
                        // `content_block_start` 里那个 `input` 是空 `{}` 占位，增量一来就作废。
                        if !b.from_delta {
                            b.json.clear();
                            b.from_delta = true;
                        }
                        // 防御：入参异常大时不再拼（长度已经远超任何真实工具调用）。
                        if b.json.len() < 1_000_000 {
                            b.json.push_str(pj);
                        }
                    }
                }
            }
            _ => {}
        }
        // 响应体本身是错误 JSON：`{"type":"error","error":{"type":…,"message":…}}`。
        // 流式的 `event: error` 也是这个形状，两条路记同一份。
        if v.get("type").and_then(|t| t.as_str()) == Some("error")
            && let Some(err) = v.get("error")
        {
            self.body_error = Some((
                err.get("type").and_then(|t| t.as_str()).map(str::to_string),
                err.get("message")
                    .and_then(|m| m.as_str())
                    .map(str::to_string)
                    .unwrap_or_else(|| err.to_string()),
            ));
        }
        if !self.is_stream
            && let Some(blocks) = v.get("content").and_then(|c| c.as_array())
        {
            for (i, b) in blocks.iter().enumerate() {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    self.text_chars += t.encode_utf16().count();
                }
                if let Some(t) = b.get("thinking").and_then(|t| t.as_str()) {
                    self.thinking_chars += t.encode_utf16().count();
                }
                self.note_block(i as i64, b);
            }
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

    /// 记一个内容块的类型：思考块只记「出现过」，工具块记名字与入参。
    fn note_block(&mut self, index: i64, cb: &serde_json::Value) {
        let ty = cb.get("type").and_then(|t| t.as_str());
        if ty != Some("fallback") {
            self.saw_output_block = true;
        }
        match ty {
            Some("thinking" | "redacted_thinking") => self.saw_thinking = true,
            Some("fallback") => {
                if let Some(to) = cb.get("to").and_then(|t| t.get("model")).and_then(|m| m.as_str())
                {
                    self.fallback_to = Some(to.to_string());
                }
            }
            Some(ty @ ("tool_use" | "server_tool_use" | "mcp_tool_use")) => {
                // 防御：一条回复里的工具块数有上限，别让畸形流把内存撑爆。
                if self.tool_uses.len() >= 256 {
                    return;
                }
                let name = cb.get("name").and_then(|n| n.as_str()).unwrap_or("");
                self.tool_uses.push(ToolUseBlock {
                    index,
                    name: tool_use_label(ty, name),
                    raw_name: name.to_string(),
                    client_side: ty == "tool_use",
                    json: cb.get("input").map(|i| i.to_string()).unwrap_or_else(|| "{}".into()),
                    from_delta: false,
                });
            }
            _ => {}
        }
    }

    /// `toolUseContentLengths` 那张表：工具名 → 入参 JSON 的字符数之和，按首次出现排序。
    ///
    /// 官方量的是 `JSON.stringify(input).length`，即**重新序列化**后的长度，而流式收到的
    /// `partial_json` 是模型原样吐出来的串——两者只在转义与数字写法上可能差几个字符。
    /// 能解析就 parse + 紧凑序列化后再量，解析不了（流断在半截）才退回原串。
    pub(super) fn tool_use_lens(&self) -> Vec<(String, usize)> {
        let mut out: Vec<(String, usize)> = Vec::new();
        for b in &self.tool_uses {
            let len = serde_json::from_str::<serde_json::Value>(&b.json)
                .map(|v| v.to_string())
                .map(|s| s.encode_utf16().count())
                .unwrap_or_else(|_| b.json.encode_utf16().count());
            match out.iter_mut().find(|(n, _)| *n == b.name) {
                Some((_, v)) => *v += len,
                None => out.push((b.name.clone(), len)),
            }
        }
        out
    }

    /// 这条回复里要客户端去执行的 `tool_use` 原名，按首次出现去重。server tool 与
    /// MCP server 那两类不在内：上游自己跑完，客户端拿不到 tool_use。
    pub(super) fn tool_use_names(&self) -> Vec<&str> {
        let mut out: Vec<&str> = Vec::new();
        for b in self.tool_uses.iter().filter(|b| b.client_side) {
            if !out.contains(&b.raw_name.as_str()) {
                out.push(&b.raw_name);
            }
        }
        out
    }

    /// 收尾：非流式模式在此解析累积的整段 JSON。
    pub(super) fn finish(&mut self) {
        if !self.is_stream
            && !self.buf.is_empty()
            && let Ok(v) = serde_json::from_slice::<serde_json::Value>(&self.buf)
        {
            self.merge(&v);
        }
    }

    /// 响应体开头（[`Self::head`]）按 UTF-8 转成文本；一个字节都没收到时 `None`。
    /// 截断落在多字节字符中间时最后那个字符丢掉，不留替换符。
    pub(super) fn excerpt(&self) -> Option<String> {
        if self.head.is_empty() {
            return None;
        }
        let text = match std::str::from_utf8(&self.head) {
            Ok(t) => t.to_string(),
            Err(e) => String::from_utf8_lossy(&self.head[..e.valid_up_to()]).into_owned(),
        };
        Some(text)
    }

    /// 上游这次响应的原样全文，学拒答时连同判决一起记下、之后原样回放（[`replay_refusal`]）。
    ///
    /// `None` 的几种情形都不学这条拒答：体超过 [`REFUSAL_REPLY_BYTES`]；回复里已经出现过输出
    /// 内容块（流到一半才被掐的——分类器看的是采样出来的正文，不是对提示词的确定性判决，
    /// 把半截答案当固定回复回放也不对）；体不是 UTF-8；一个字节都没收到。
    pub(super) fn refusal_reply(&self) -> Option<store::LearnedReply> {
        if self.reply_overflow || self.saw_output_block || self.reply.is_empty() {
            return None;
        }
        let body = String::from_utf8(self.reply.clone()).ok()?;
        Some(store::LearnedReply { sse: self.is_stream, body })
    }

    /// 上游以 `stop_reason: "refusal"` 收尾——分类器或模型自己拒了这条提示词（`stop_details`
    /// 里有类别与解释）。非流式看顶层，流式看 `message_delta.delta`，两处都进 [`Self::merge`]。
    pub(super) fn refused(&self) -> bool {
        self.stop_reason.as_deref() == Some("refusal")
    }

    /// 这条拒答是不是**分类器的确定性判决**——值得按提示词记下来、拦逐字相同的重发：
    /// `stop_reason: "refusal"`、`stop_details.category` 非空（分类器给了类别）、且没有
    /// `recommended_model`（有它说明 fallback 没跑成，直接重试可能就答了）。是则给出类别。
    ///
    /// category 为空的一律不学。官方文档说 `stop_details` 只是参考信息、`category` 与
    /// `explanation` 都可能为 `null`（且要把 `null` 当成一种长期存在的正常取值）——所以「空即
    /// 模型自拒」只是对多数样本成立的近似，不是定义；但学与不学两边的代价不对称：不学最多
    /// 让那条提示词再白跑一次上游，学错则把可能能答的请求锁 7 天。判据宁可收紧。官方文档也
    /// 只说输出前的拒答不计费、不占限流，「重发在账号上添一笔」没有依据。
    fn classifier_refusal(&self) -> Option<&str> {
        if !self.refused() || self.refusal_recommended_model.is_some() {
            return None;
        }
        self.refusal_category.as_deref().filter(|c| !c.is_empty())
    }

    /// 是否解析到任一用量字段。
    pub(super) fn has_usage(&self) -> bool {
        self.input_tokens.is_some()
            || self.output_tokens.is_some()
            || self.cache_creation_tokens.is_some()
            || self.cache_read_tokens.is_some()
    }
}

/// 遥测里那张工具长度表的键：官方对工具名做同一套归一化——`mcp_tool_use` 块与 `mcp__*`
/// 名字都记成 `mcp_tool`、`skill__*` 记成 `skill_tool`，其余原样。
pub(super) fn tool_use_label(block_type: &str, name: &str) -> String {
    if block_type == "mcp_tool_use" || name.starts_with("mcp__") {
        return "mcp_tool".to_string();
    }
    if name.starts_with("skill__") {
        return "skill_tool".to_string();
    }
    name.to_string()
}

#[cfg(test)]
mod tests {
    use crate::proxy::test_support::gzip;
    use crate::proxy::{ShapeBits, UsageSniffer, shape_summary};

    /// 上游用了我们没开的编码时，只能跳过嗅探——但不得崩、不得把压缩字节当明文解析。
    #[test]
    fn unknown_encoding_is_skipped_not_misparsed() {
        let mut s = UsageSniffer::new(true, true);
        s.feed(&gzip(b"data: {\"usage\":{\"input_tokens\":999}}\n"));
        s.finish();
        assert!(!s.has_usage(), "解不开的响应体不应被当明文解析出用量");
        assert_eq!(s.model, None);
    }

    /// 落库的形态摘要：留住对照维度（顶层 key 顺序、system 哈希与块数、工具名、消息块计数、
    /// 顶层参数），取出 session_id，且一个字的用户正文都不进去。
    #[test]
    fn shape_summary_keeps_shape_extracts_session_and_drops_user_text() {
        let body = serde_json::json!({
            "model": "claude-opus-5",
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "我的银行卡号是 1234"}]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "hmm", "signature": "sig"},
                    {"type": "tool_use", "name": "Bash", "input": {"command": "rm -rf secret"}},
                ]},
                {"role": "user", "content": "plain string turn"},
            ],
            "system": [
                {"type": "text", "text": "You are Claude Code.", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "private project notes"},
            ],
            "tools": [{"name": "Bash", "input_schema": {}}, {"name": "Read", "input_schema": {}}],
            "metadata": {"user_id": "user_ab12_account_cd34_session_9f8e7d6c-0000-1111-2222-333344445555"},
            "max_tokens": 32000,
            "stream": true,
            "thinking": {"type": "enabled", "budget_tokens": 1024}});
        let bytes = serde_json::to_vec(&body).unwrap();
        let ShapeBits { shape, session_id: session, device_id_out: device } = shape_summary(&bytes);
        let shape = shape.expect("对象体必有摘要");
        assert_eq!(session.as_deref(), Some("9f8e7d6c-0000-1111-2222-333344445555"));
        assert_eq!(device.as_deref(), Some("ab12"), "扁平串的 device 段落进 device_id_out");
        let v: serde_json::Value = serde_json::from_str(&shape).unwrap();
        assert_eq!(
            v["keys"],
            serde_json::json!([
                "model",
                "messages",
                "system",
                "tools",
                "metadata",
                "max_tokens",
                "stream",
                "thinking"
            ]),
            "顶层 key 顺序是判据，要原样留住"
        );
        assert_eq!(v["system"]["blocks"].as_array().unwrap().len(), 2);
        assert_eq!(v["system"]["blocks"][0]["cache"], true);
        assert_eq!(v["system"]["sha"].as_str().unwrap().len(), 16);
        assert_eq!(v["tools"]["count"], 2);
        assert_eq!(v["tools"]["names"], serde_json::json!(["Bash", "Read"]));
        assert_eq!(v["messages"]["count"], 3);
        assert_eq!(v["messages"]["last_role"], "user");
        assert_eq!(v["messages"]["blocks"]["tool_use"], 1);
        assert_eq!(v["messages"]["blocks"]["thinking"], 1);
        assert_eq!(v["messages"]["blocks"]["text"], 2, "字符串 content 也算一块 text");
        assert_eq!(v["metadata"]["user_id"], true);
        assert_eq!(v["thinking"]["budget_tokens"], 1024);
        assert_eq!(v["max_tokens"], 32000);
        for leak in ["1234", "rm -rf", "private project", "You are Claude", "hmm", "user_ab12"] {
            assert!(!shape.contains(leak), "正文/身份不得进摘要: {leak} in {shape}");
        }
        // 非 JSON 体没有摘要，也不 panic。
        assert_eq!(shape_summary(b"not json"), ShapeBits::default());
    }

    /// CC 内嵌 JSON 格式的 `metadata.user_id`：device_id / session_id 都要认出来；
    /// 没带 metadata 时两项为 `None`，摘要照出。
    #[test]
    fn shape_summary_reads_embedded_json_identity() {
        let body = serde_json::json!({
            "model": "claude-opus-5",
            "messages": [{"role": "user", "content": "hi"}],
            "metadata": {"user_id": "{\"device_id\":\"d230ce6e1111\",\"account_uuid\":\"acct\",\"session_id\":\"sess-1\"}"}});
        let bits = shape_summary(&serde_json::to_vec(&body).unwrap());
        assert!(bits.shape.is_some());
        assert_eq!(bits.session_id.as_deref(), Some("sess-1"));
        assert_eq!(bits.device_id_out.as_deref(), Some("d230ce6e1111"));

        let bare = serde_json::json!({"model": "claude-opus-5", "messages": []});
        let bits = shape_summary(&serde_json::to_vec(&bare).unwrap());
        assert!(bits.shape.is_some(), "没带 metadata 也要有摘要");
        assert_eq!((bits.session_id, bits.device_id_out), (None, None));
    }

    /// 嗅探器留下响应体开头：流式非流式都留、封顶不无界、截在多字节字符中间时丢掉半个字。
    #[test]
    fn the_sniffer_keeps_a_bounded_excerpt_of_the_raw_reply() {
        let mut s = crate::proxy::UsageSniffer::new(false, false);
        assert!(s.excerpt().is_none(), "一个字节都没收到时没有摘录");
        s.feed(br#"{"id":"msg_1","content":[],"stop_reason":"end_turn","#);
        s.feed(br#""usage":{"input_tokens":425,"output_tokens":0}}"#);
        s.finish();
        assert_eq!(s.output_tokens, Some(0));
        assert_eq!(
            s.excerpt().as_deref(),
            Some(
                r#"{"id":"msg_1","content":[],"stop_reason":"end_turn","usage":{"input_tokens":425,"output_tokens":0}}"#
            )
        );

        // 流式：逐行解析吃掉 `buf`，摘录仍是原样的字节。
        let mut st = crate::proxy::UsageSniffer::new(true, false);
        st.feed(b"event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{\"output_tokens\":0}}}\n\n");
        assert!(st.excerpt().unwrap().starts_with("event: message_start\ndata: "));

        // 封顶：超过上限的部分不留；截在「中」字中间时丢掉半个字符，不出替换符。
        let mut big = crate::proxy::UsageSniffer::new(false, false);
        let filler = "x".repeat(crate::proxy::RESPONSE_EXCERPT_BYTES - 1);
        big.feed(filler.as_bytes());
        big.feed("中文".as_bytes());
        let ex = big.excerpt().unwrap();
        assert_eq!(ex.len(), crate::proxy::RESPONSE_EXCERPT_BYTES - 1);
        assert!(ex.ends_with('x'));
        assert!(!ex.contains('\u{FFFD}'));

        // 解不开的编码：什么都不留。
        let mut opaque = crate::proxy::UsageSniffer::new(false, true);
        opaque.feed(b"gzip bytes");
        assert!(opaque.excerpt().is_none());
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

    /// `message_stop` 是流正常收尾的唯一标志。缺了它、又没有 error 事件、连接层也没报错，
    /// 是最安静的那种断流：这一层看什么都正常，客户端拿到的却是半截回复
    /// （Claude Code 报 `Connection closed mid-response`）。
    #[test]
    fn message_stop_marks_a_complete_stream() {
        let mut truncated = crate::proxy::UsageSniffer::new(true, false);
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

        let mut complete = crate::proxy::UsageSniffer::new(true, false);
        complete.feed(b"event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n");
        assert!(complete.saw_message_stop);
        assert_eq!(complete.last_event.as_deref(), Some("message_stop"));
    }

    /// 非流式响应体里的 `{"type":"error"}` 不走流内那套：那条路的 4xx 由
    /// [`crate::proxy::detect_account_ban`] 一侧处理，这里再记一份会让同一个错误告警两次。
    /// 但**类型与文案照记**（`body_error`）——`tengu_api_error` 的 `errorType`/`error`
    /// 要它，而 [`crate::proxy::capture_forensics`] 那份只在 400/401/403 与裸 429 上填。
    #[test]
    fn nonstream_error_body_is_not_taken_as_a_stream_error() {
        let mut s = crate::proxy::UsageSniffer::new(false, false);
        s.feed(br#"{"type":"error","error":{"type":"invalid_request_error","message":"nope"}}"#);
        s.finish();
        assert!(s.stream_error.is_none());
        assert_eq!(
            s.body_error,
            Some((Some("invalid_request_error".into()), "nope".into())),
            "错误体的类型与文案要留下来"
        );
    }

    /// `toolUseContentLengths`：流式下工具入参是 `input_json_delta` 一片片来的，按内容块
    /// 序号拼回去；同名工具累加；键按首次出现排序；`mcp__*` 归成 `mcp_tool`。
    /// 顺带钉住思考块的识别——`redacted_thinking` 一个字都没有，靠字数判定认不出来。
    /// 同一条流无论被切成什么样的块，嗅探结果必须逐项相同。
    ///
    /// 切行那段改过一次（逐行 `drain` + `collect` 换成扫完一次性 `drain`，见 [`UsageSniffer::feed`]），
    /// 而它的正确性全压在「跨块的半行要留到下一块再拼」这一条上。这里拿三种切法对同一条流跑：
    /// 整段一次喂、每行一块、以及**逐字节**喂（每个事件都被切得稀碎，最恶劣的一种）。
    #[test]
    fn the_sniffer_is_indifferent_to_how_the_stream_is_chunked() {
        let events = [
            r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-opus-5","usage":{"input_tokens":11,"cache_read_input_tokens":22}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"t1","name":"Bash","input":{}}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"command\":\"ls\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            // 多字节正文：按字节切一定会切在半个汉字中间。
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"一二三四五"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"end_turn"},"usage":{"output_tokens":33}}"#,
            r#"{"type":"message_stop"}"#,
        ];
        let wire: Vec<u8> =
            events.iter().flat_map(|e| format!("event: x\ndata: {e}\n\n").into_bytes()).collect();

        let run = |chunks: Vec<&[u8]>| {
            let mut s = crate::proxy::UsageSniffer::new(true, false);
            for c in chunks {
                s.feed(c);
            }
            s.finish();
            s
        };
        let whole = run(vec![&wire]);
        let per_byte = run(wire.chunks(1).collect());
        // 7 字节一块：与行长互质，故切点会落在行内各处。
        let ragged = run(wire.chunks(7).collect());

        for (label, got) in [("逐字节", &per_byte), ("7 字节一块", &ragged)] {
            assert_eq!(got.model, whole.model, "{label}");
            assert_eq!(got.message_id, whole.message_id, "{label}");
            assert_eq!(got.input_tokens, whole.input_tokens, "{label}");
            assert_eq!(got.output_tokens, whole.output_tokens, "{label}");
            assert_eq!(got.cache_read_tokens, whole.cache_read_tokens, "{label}");
            assert_eq!(got.stop_reason, whole.stop_reason, "{label}");
            assert_eq!(got.text_chars, whole.text_chars, "{label}");
            assert_eq!(got.events, whole.events, "{label}：事件计数");
            assert_eq!(got.last_event, whole.last_event, "{label}");
            assert_eq!(got.saw_message_stop, whole.saw_message_stop, "{label}");
            assert_eq!(got.tool_use_lens(), whole.tool_use_lens(), "{label}：工具入参长度");
        }
        // 这条流本身得真的被认出来了，否则上面比的是一堆空值。
        assert_eq!(whole.model.as_deref(), Some("claude-opus-5"));
        assert_eq!(whole.events, 8);
        assert_eq!(whole.text_chars, 5, "五个汉字，按 UTF-16 码元数");
        assert!(whole.saw_message_stop);
    }

    #[test]
    fn the_sniffer_collects_tool_use_input_lengths() {
        let mut s = crate::proxy::UsageSniffer::new(true, false);
        let feed = |s: &mut crate::proxy::UsageSniffer, line: &str| {
            s.feed(format!("data: {line}\n\n").as_bytes());
        };
        feed(&mut s, r#"{"type":"message_start","message":{"model":"claude-opus-5"}}"#);
        feed(
            &mut s,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"xx"}}"#,
        );
        feed(
            &mut s,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"tool_use","id":"t1","name":"Bash","input":{}}}"#,
        );
        feed(
            &mut s,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":"{\"command\""}}"#,
        );
        feed(
            &mut s,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"input_json_delta","partial_json":":\"ls\"}"}}"#,
        );
        feed(
            &mut s,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"t2","name":"Bash","input":{}}}"#,
        );
        feed(
            &mut s,
            r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"t3","name":"mcp__ide__getDiagnostics","input":{}}}"#,
        );
        feed(&mut s, r#"{"type":"message_stop"}"#);
        assert!(s.saw_thinking, "redacted_thinking 也算思考块");
        // `{"command":"ls"}` 是 16 个字符；没有增量的那个块是空 `{}` = 2；两个都叫 Bash，加起来 18。
        assert_eq!(s.tool_use_lens(), vec![("Bash".to_string(), 18), ("mcp_tool".to_string(), 2)]);
    }

    /// 对着抓包钉住那个长度：`cap/2.1.260-2/00065` 里那条 `stop=tool_use` 的
    /// `tengu_api_success` 报的是 `toolUseContentLengths: '{"Bash":166}'`，而同一个
    /// `tool_use` 块的 `input`（`00061` 的 `messages[5]`）紧凑序列化正好 166 字符。
    /// 官方量的是 `JSON.stringify(input).length`——不带空格的那份。
    #[test]
    fn tool_use_length_matches_the_capture_when_it_is_present() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/cap/2.1.260-2/00061_174309.489.req.raw");
        let Ok(raw) = std::fs::read(path) else {
            eprintln!("skipped: {path} not present");
            return;
        };
        let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").expect("http headers") + 4;
        let v: serde_json::Value = serde_json::from_slice(&raw[sep..]).unwrap();
        // 抓包里的 `tool_use` 块原样搬成一条非流式回复喂给嗅探器。
        let block = v["messages"][5]["content"]
            .as_array()
            .unwrap()
            .iter()
            .find(|b| b["type"] == "tool_use")
            .expect("那条 assistant 消息里有一个 tool_use 块")
            .clone();
        let thinking = v["messages"][5]["content"][0].clone();
        let body = serde_json::json!({
            "id": "msg_x",
            "model": "claude-opus-5",
            "content": [thinking, block],
            "usage": { "input_tokens": 1, "output_tokens": 1 }
        });
        let mut s = crate::proxy::UsageSniffer::new(false, false);
        s.feed(&serde_json::to_vec(&body).unwrap());
        s.finish();
        assert_eq!(s.tool_use_lens(), vec![("Bash".to_string(), 166)]);
        // 那条回复只有一个空思考块加一个工具块：`textContentLength` 与
        // `thinkingContentLength` 抓包里都是 0，且后者**存在**——靠字数判不出来，靠块类型。
        assert_eq!(s.text_chars, 0);
        assert_eq!(s.thinking_chars, 0);
        assert!(s.saw_thinking, "空思考块也要算");
    }

    /// 非流式回复里的工具块同样要认（`content[]` 直接带完整 `input`）。
    #[test]
    fn the_sniffer_collects_tool_use_lengths_from_a_nonstream_body() {
        let mut s = crate::proxy::UsageSniffer::new(false, false);
        s.feed(
            br#"{"id":"msg_1","model":"claude-opus-5","content":[{"type":"text","text":"hi"},{"type":"tool_use","id":"t1","name":"Read","input":{"file_path":"/a"}}],"usage":{"input_tokens":1}}"#,
        );
        s.finish();
        assert!(!s.saw_thinking);
        // serde 重新序列化后的 `{"file_path":"/a"}` = 18 个字符。
        assert_eq!(s.tool_use_lens(), vec![("Read".to_string(), 18)]);
    }

    /// 回复里的 `tool_use` **原名**另记一份：遥测那张表把 `mcp__*` 归成 `mcp_tool`，答不了
    /// 「模型到底调了哪个」。server tool 与 MCP server 的调用不算——上游自己跑完，客户端拿
    /// 不到那个 tool_use。同名多次只记一次。
    #[test]
    fn the_sniffer_keeps_raw_tool_use_names_for_client_side_blocks() {
        let mut s = crate::proxy::UsageSniffer::new(true, false);
        let block = |i: u32, ty: &str, name: &str| -> Vec<u8> {
            const EV: &str = "event: content_block_start
data: {\"type\":\"content_block_start\",\"index\":IDX,\"content_block\":{\"type\":\"TY\",\"id\":\"tIDX\",\"name\":\"NAME\",\"input\":{}}}

";
            EV.replace("IDX", &i.to_string()).replace("TY", ty).replace("NAME", name).into_bytes()
        };
        s.feed(&block(0, "tool_use", "Bash"));
        s.feed(&block(1, "tool_use", "mcp__luban__query_bas00"));
        s.feed(&block(2, "server_tool_use", "web_search"));
        s.feed(&block(3, "mcp_tool_use", "mcp__ide__getDiagnostics"));
        s.feed(&block(4, "tool_use", "Bash"));
        assert_eq!(s.tool_use_names(), ["Bash", "mcp__luban__query_bas00"]);
        // 遥测那张表照旧按归类名走，不受影响。
        let lens: Vec<String> = s.tool_use_lens().into_iter().map(|(n, _)| n).collect();
        assert_eq!(lens, ["Bash", "mcp_tool", "web_search"]);
    }
}
