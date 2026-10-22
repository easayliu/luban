//! 收尾：成功、失败、失败那一轮的 turn_end，以及 `/model` 探测与子代理的 beta 回填。

use super::*;

impl EventBuilder<'_> {
    /// 成功收尾：`tengu_api_success` 及随它的那几条。
    pub(super) fn emit_success(&mut self) {
        let env = self.env;
        let (call, shape) = (env.call, env.shape);
        let TurnFacts {
            ref identity,
            kind,
            has_1m,
            ref display_model,
            ref resp_model,
            failed,
            aborted,
            is_main,
            ref turn_skill,
            post_compaction,
            ref turn_origin,
            ref agent,
            ref previous_request_id,
            time_since_last,
            message_tokens,
            ref default_model,
            tools_changed,
            ref chain_id,
            query_depth,
            v270,
            v277,
            v280,
            v285,
            v291,
            v305,
            first_main,
            ref snapshot,
            ref dd_model,
            ref betas_full,
            ..
        } = *env.f;
        let EventEnv {
            ttft,
            total,
            t_end,
            build_age_mins,
            builtin_agent,
            cache_ttl,
            modern,
            retry_pad,
            body_chars,
            gzip_skip,
            ref effort,
            req_hash,
            ..
        } = *env;
        let query_source = env.query_source.as_str();
        // 收尾：成功报 `tengu_api_success`、失败报 `tengu_api_error`——两条并列，且互斥。
        //
        // 官方对失败请求发的是 `tengu_api_error`（`ate()` 分类 + 上游文案，没有任何用量）
        // 外加一条 `tengu_feature_bad{api_request}`；成功那条独有的
        // `tengu_tool_schema_sizes` 与标题生成收尾也只在成功时发（前者就在官方
        // `tengu_api_success` 那个函数里）。被客户端取消的两样都不报。
        if !failed && !aborted {
            // 收尾：tengu_api_success（键序照抓包）。
            let mut success = Map::new();
            let mut put = |k: &str, v: Value| {
                success.insert(k.to_string(), v);
            };
            put("model", json!(&resp_model));
            // 2.1.285 起报这条请求头上的 `anthropic-dispatch-id`（12 条恒为 `v2d`，见
            // [`config::CC_DISPATCH_ID`]）。
            if v285 {
                put("dispatch", json!(env.dispatch()));
            }
            if has_1m {
                put("preNormalizedModel", json!(&display_model));
            }
            put("betas", json!(&betas_full));
            if v285 {
                put("echoWireToolInputs", json!(true));
            }
            put("messageCount", json!(shape.messages_len));
            put("messageTokens", json!(message_tokens));
            put("inputTokens", json!(call.input_tokens));
            put("outputTokens", json!(call.output_tokens));
            put("cachedInputTokens", json!(call.cache_read_tokens));
            put("uncachedInputTokens", json!(call.cache_creation_tokens));
            // 2.1.291：缓存写入按 ttl 拆开报（`cap/auto-2.1.291-20261006-full` 主线程每条都有，
            // 两项之和等于 `uncachedInputTokens`）。上游没给拆分时按这条请求的断点 ttl 归到一边。
            if v291 {
                let (m5, h1) = match (call.cache_creation_5m_tokens, call.cache_creation_1h_tokens)
                {
                    (Some(a), Some(b)) => (a, b),
                    _ if shape.cache_ttl_1h => (0, call.cache_creation_tokens),
                    _ => (call.cache_creation_tokens, 0),
                };
                put("cache_creation_5m_input_tokens", json!(m5));
                put("cache_creation_1h_input_tokens", json!(h1));
            }
            // 2.1.285：缓存没盖住的尾段，即最后一条带断点的消息之后那几条（[`tail_tokens_est`] 估
            // token）。`cap/2.1.285` 与 `cap/auto-2.1.285-20260930` 里五种取值：
            //
            // - 不带断点（标题那类）：`caching_off`，整段消息都算没盖住，估算全 0；
            // - thread 续轮（主线程与子代理）：`threaded_continue`；
            // - fork 出来的查询（猜下一句、插问、压缩、离开回顾、子代理摘要）：
            //   `fork_tail_skip_cache_write`，尾段条数与估算记在 `resentTailTokensEst`（猜下一句恒
            //   341、插问 336、压缩 1775）；
            // - 主线程尾段全是 system 消息（fable-5-1 不走 thread，工具续轮末尾那条只发一次的
            //   system 提示，`00385`、`00391`）：`sent_once_text`，条数报 0、估算记在
            //   `sentOnceTailTokensEst`，`plainInputExcessTokens` 扣掉它（32 − 32 = 0）；
            // - 其余带断点的：`none`，全 0，`plainInputExcessTokens` 等于 `inputTokens`。
            //
            // 前三种不报 `plainInputExcessTokens`。2.1.291 起没有 `threaded_continue` 这一项（306 源码的
            // 取值表里没有，291~306 抓包一条也没有），thread 续轮照 `none` 报。官方 `ffn` 另要求
            // `usage.iterations` 不超过一轮、三项输入合计不少于 4096 才报 `plainInputExcessTokens`。
            if v285 {
                let est = shape.tail_tokens_est;
                let fork = matches!(
                    kind,
                    Kind::AgentSummary
                        | Kind::AwaySummary
                        | Kind::Suggestion
                        | Kind::SideQuestion
                        | Kind::Compact
                );
                let (reason, tail, resent, sent_once) = if !shape.has_cache_control {
                    ("caching_off", shape.messages_len, 0, 0)
                } else if !v291 && shape.thread_type.as_deref() == Some("continue") {
                    ("threaded_continue", 0, 0, 0)
                } else if fork {
                    ("fork_tail_skip_cache_write", shape.tail_messages.max(1), est, 0)
                } else if shape.tail_all_system {
                    ("sent_once_text", 0, 0, est)
                } else {
                    ("none", 0, 0, 0)
                };
                put("uncoveredTailReason", json!(reason));
                put("uncoveredTailMessages", json!(tail));
                put("resentTailTokensEst", json!(resent));
                put("sentOnceTailTokensEst", json!(sent_once));
                put("unexcusedTailTokensEst", json!(0));
                let input_total =
                    call.input_tokens + call.cache_read_tokens + call.cache_creation_tokens;
                if matches!(reason, "none" | "sent_once_text")
                    && call.iterations <= 1
                    && input_total >= 4096
                {
                    put(
                        "plainInputExcessTokens",
                        json!((call.input_tokens.max(0) as u64).saturating_sub(sent_once)),
                    );
                }
            }
            put("durationMs", json!(total));
            put("durationMsIncludingRetries", json!(env.duration_including_retries()));
            put("attempt", json!(env.retry.attempt()));
            put("ttftMs", json!(ttft));
            // 首个内容块到达：比首字节晚 0–1ms（`cap/2.1.280` 九条里七条 +1、两条 +0）。
            if v270 {
                put("firstContentMs", json!(ttft + i64::from(retry_pad % 2)));
            }
            if v305 && let Some(n) = call.stream_events {
                put("streamEventCount", json!(n));
            }
            // 2.1.285：发请求前客户端这一侧的开销，主线程 6–46ms（首条 25），标题那类侧查询
            // 不报。按 request-id 取个稳定值，同 `retry_pad`。
            if v285 && !kind.is_side_query() {
                put("queryOverheadMs", json!(6 + i64::from(req_hash % 41)));
            }
            put("buildAgeMins", json!(build_age_mins));
            put("provider", json!("firstParty"));
            put("requestId", json!(call.request_id.as_deref().unwrap_or("")));
            if v270 && let Some(crid) = call.client_request_id.as_deref().filter(|c| !c.is_empty())
            {
                put("clientRequestId", json!(crid));
            }
            // 子代理首条：是哪条主线程请求拉起的它（`cap/2.1.280`：`invokingRequestId` 指回
            // 调了 Agent 的那条，`invocationKind: spawn`）；之后的各条不带。
            if let Some(inv) = agent.as_ref().and_then(|a| a.invoking_request_id.as_deref())
                && kind == Kind::Subagent
            {
                put("invokingRequestId", json!(inv));
                put("invocationKind", json!("spawn"));
            }
            put("stop_reason", json!(call.stop_reason.as_deref().unwrap_or("end_turn")));
            if let Some(e) = &effort {
                put("effort_level", json!(e));
            }
            // 只有主线程带（子代理、猜下一句、标题都没有），取值同本轮的 input_prompt。
            if v277 && is_main {
                put("turn_origin", json!(&turn_origin));
            }
            // 只有主线程报「是不是默认模型/默认 effort」；子代理（`cap/2.1.280` Explore 六条）、
            // 猜下一句和标题那类都不报。
            if is_main {
                // `[1m]` 不算换了模型：`/model opus[1m]` 之后官方照报 true（`cap/auto-2.1.285-20260930/00235`）。
                let bare = |m: &str| m.trim_end_matches("[1m]").to_string();
                put("is_default_model", json!(bare(display_model) == bare(default_model)));
                put("default_model", json!(&default_model));
                if let Some(e) = &effort {
                    // 2.1.280 起按模型的默认 effort 比（`cap/2.1.285/00040`、`00079`，
                    // `cap/2.1.280/00167`）；更老的版本没有样本，照旧报「就是默认」。
                    let default = if v280 { default_effort_of(display_model) } else { e.as_str() };
                    put("is_default_effort", json!(e == default));
                    put("default_effort_level", json!(default));
                }
            }
            put("costUSD", json!(call.cost_usd.unwrap_or(0.0)));
            put("didFallBackToNonStreaming", json!(false));
            // `-p` 打印模式（`00441` 等十条）：非交互、print、没有 TTY。
            put("isNonInteractiveSession", json!(shape.sdk));
            put("print", json!(shape.sdk));
            put("isTTY", json!(!shape.sdk));
            put("querySource", json!(query_source));
            if kind.has_chain() {
                put("queryChainId", json!(&chain_id));
                put("queryDepth", json!(query_depth));
            }
            put("permissionMode", json!(shape.permission_mode));
            put("globalCacheStrategy", json!("system_prompt"));
            if shape.has_cache_control {
                put("prompt_cache_ttl", json!(cache_ttl));
                // 订阅用户的 1h 缓存报 `subscriber`；子代理那份是 5m，报 `default`（`cap/2.1.280`）。
                put(
                    "prompt_cache_ttl_reason",
                    json!(if shape.cache_ttl_1h { "subscriber" } else { "default" }),
                );
            }
            put("textContentLength", json!(call.text_chars));
            // 官方的判据是「这条回复里有没有思考块」（`redacted_thinking` 与空思考块都算），嗅探器
            // 按块类型记（`saw_thinking`）。此前「整条没有正文」也算，可 `tool_use` 收尾的回复并不
            // 必带思考块：`cap/auto-2.1.285-20260930` 31 条没有思考块的官方都不报这一项。
            if call.saw_thinking || call.thinking_chars > 0 {
                put("thinkingContentLength", json!(call.thinking_chars));
            }
            put("narrationBlockCount", json!(0));
            // `toolUseContentLengths`：这条回复里每个工具的入参 JSON 字符数之和，键按首次
            // 出现排序，整张表**序列化成一个字符串**塞进事件（与 `toolSchemaCharLengths`
            // 同一种写法）；一个 `tool_use` 块都没有时整个字段不出现。
            if !call.tool_use_lens.is_empty() {
                let mut table = Map::new();
                for (name, len) in &call.tool_use_lens {
                    table.insert(name.clone(), json!(len));
                }
                put("toolUseContentLengths", json!(Value::Object(table).to_string()));
            }
            put("imageBlockCount", json!(shape.image_blocks));
            put("imageTotalPixels", json!(shape.image_pixels));
            put("imageTotalBytes", json!(shape.image_bytes));
            put("documentBlockCount", json!(shape.doc_blocks));
            put("documentTotalBytes", json!(shape.doc_bytes));
            put("inputTextCharLength", json!(shape.input_text_chars));
            put("estimatedInputTokens", json!(shape.estimated_tokens));
            put("systemCharLength", json!(shape.system_chars));
            if let Some((source, hash)) = &snapshot {
                put("systemPromptSource", json!(source));
                put("snapshotHash", json!(hash));
            } else if modern && kind.has_boundary() {
                put("systemPromptSource", json!("live_unrecorded"));
            }
            put("toolsCharLength", json!(shape.tools_chars));
            put("toolsCount", json!(shape.tools_count));
            put("deferredToolsCount", json!(shape.deferred_tools));
            put("toolSchemasHash", json!(&shape.tools_hash));
            put("requestBodyEncoding", json!("identity"));
            put("requestBodyChars", json!(body_chars));
            // 2.1.285：组请求体的耗时，每条都报，0 或 1ms。
            if v285 {
                put("requestPrepareMs", json!((req_hash / 41) % 2));
            }
            put("gzipSkipReason", json!(gzip_skip));
            put("fastMode", json!(shape.fast_mode));
            if let Some(prev) = &previous_request_id {
                put("previousRequestId", json!(prev));
            }
            if post_compaction {
                put("isPostCompaction", json!(true));
            }
            if let Some(skill) = &turn_skill {
                put("attributionSkill", json!(skill));
            }
            // 内置子代理多报一项是哪类子代理，排在链字段之后（`cap/2.1.280` Explore 六条都有，
            // 它的摘要请求没有）。自定义子代理那边报的是触发它的 skill（`attributionSkill`），
            // 代理这一侧不知道是哪个 skill，不报。
            if kind == Kind::Subagent
                && v270
                && let Some(t) = builtin_agent
            {
                put("attributionAgent", json!(t));
            }
            if let Some(ms_since) = time_since_last {
                put("timeSinceLastApiCallMs", json!(ms_since));
            }
            let success = Value::Object(success);
            // tether 收尾排在 api_success 之前、同一毫秒（`cap/2.1.277`、`cap/2.1.280` 全部如此）。
            // 失败那条抓包里只见过流被中断的 `aborted`，代理这边的失败形态没有样本，不报。
            if let Some(live) = env.live_outcome("ok") {
                self.push_dd_snake(t_end, "tengu_tether_live_outcome", live);
            }
            // 缓存保活：这条主线程回复接替了一条续命成功过的记录，客户端停掉那条的续命时报一次，夹在
            // tether 收尾与 api_success 之间、同一毫秒（`cap/auto-2.1.300-20261015-full` A0）。
            if call.cache_keepalive_ok {
                self.push_dd(t_end, "tengu_feature_ok", feature("api_cache_keepalive"));
            }
            self.push(t_end, "tengu_api_success", success.clone());
            // 心跳（交互式主线程）：回复里调了 `Agent`，派出去那一刻界面转入 `subagent_running`，
            // 落在这条收尾前 2ms（`cap/auto-2.1.305-20261021-full` A8b .811 / .813、A19 .213 / .215，
            // A8a 早 18ms）。登记在拉起它的这条上，而不是子代理首条上——那条落地时主线程多半又完成了
            // 续轮，早于已折算时刻的切换会被当过时丢掉。派出去的子代理还在跑时，续轮的流一收尾界面又回到
            // 等工具那段，收尾前 3ms 切回 `subagent_running`（A8a .198 / .201、A8b .118 / .121、
            // A19 .432 / .435）；一轮结束再切回来的那下在下面停手处。
            if kind == Kind::Main && !shape.sdk {
                if env.f.delegated_running {
                    self.beat(
                        ms(t_end, -3),
                        heartbeat::Trigger::Run(Some(heartbeat::SUBAGENT_RUNNING)),
                    );
                }
                if call.tool_use_lens.iter().any(|(name, _)| name == "Agent" || name == "Task") {
                    self.beat(
                        ms(t_end, -2),
                        heartbeat::Trigger::Run(Some(heartbeat::SUBAGENT_RUNNING)),
                    );
                }
            }
            // 心跳：主循环（交互式主线程与 `-p` 的 sdk 主线程）请求成功那一刻报 `main_request`。
            // 没有 request-id 的不登记（官方 `tEt` 遇到 `dV()` 为空就返回，时刻也不记）。
            if kind == Kind::Main
                && let Some(id) = call.request_id.clone().filter(|id| !id.is_empty())
            {
                self.beat(t_end, heartbeat::Trigger::MainRequest(id));
            }
            // Datadog 那份比 event_logging 少两项：工具长度表的 hash 与各工具入参长度
            // （`cap/2.1.260-2`、`2.1.277`、`2.1.280` 三份的 Datadog 批次里一条都没有）。
            let mut dd_success = snake_flat(&success);
            if let Some(o) = dd_success.as_object_mut() {
                o.shift_remove("tool_schemas_hash");
                o.shift_remove("tool_use_content_lengths");
            }
            self.dd.push(identity.dd_entry(
                "tengu_api_success",
                &env.ctx(t_end),
                dd_model,
                dd_success,
            ));

            // 工具集变了才报一次（无工具的侧查询也算一种：`{}` 那份）。
            if tools_changed {
                self.push(
                    t_end,
                    "tengu_tool_schema_sizes",
                    json!({
                        "toolSchemasHash": &shape.tools_hash,
                        "toolSchemaCharLengths": &shape.tool_lens,
                        "toolsCharLength": shape.tools_chars,
                        "toolsCount": shape.tools_count,
                        "deferredToolsCount": shape.deferred_tools
                    }),
                );
            }
            // 首条主线程请求收尾时记一次这个会话发出去的请求形态（`cap/2.1.285/00040`
            // 06:34:21.881，2.1.280 没有）：system 角色消息、工具变更头、保留提醒、inline 工具这几项
            // 跟着对应的 beta 走（`inlineTools` 2.1.293 起随 `inline-tools` 为 true，此前恒 false）。
            // `-p` 的只在带 `mid-conversation-system` 的模型上记（fable / opus 那两条有、haiku 那条没有）；
            // 2.1.291 起交互式也是——haiku-4.5 的会话一条都没有（`cap/auto-2.1.291-20261006-full` B3 / E4，
            // `cap/auto-2.1.293-20261008-full` 的 haiku-4-5 会话），带它的 haiku-5-5 有。
            // 2.1.299 起 `mid-conversation-system` 转正不发头了，同样那几族照发它的子项 `-clear-at`，
            // `systemTurns` 仍是 true（`cap/auto-2.1.299-20261014-full` 每个会话一条），故两项按前缀一起认。
            let v291 = version_at_least(&env.f.version, "2.1.291");
            if first_main
                && v285
                && (!(shape.sdk || v291) || betas_full.contains("mid-conversation-system-"))
            {
                let has = |p: &str| betas_full.split(',').any(|b| b.trim().starts_with(p));
                self.push(
                    ms(t_end, 1),
                    "tengu_wire_shape_recorded",
                    json!({
                        "systemTurns": has("mid-conversation-system-"),
                        "toolChangeHeader": has("mid-conversation-tool-changes-"),
                        "inlineTools": has(config::CC_BETA_INLINE_TOOLS
                            .trim_end_matches(|c: char| c.is_ascii_digit() || c == '-')),
                        "keptReminders": has("mid-conversation-system-clear-at-"),
                        "overwrote": false
                    }),
                );
            }
            if kind == Kind::Title {
                // 2.1.285 多两项：输入有没有被截断（`cap/2.1.285/00040`，都是 false）。
                let title = if v285 {
                    json!({ "success": true, "input_capped": false, "input_over_cap": false })
                } else {
                    json!({ "success": true })
                };
                self.push(t_end, "tengu_session_title_generated", title);
            }
            if kind == Kind::RenameName {
                self.push(ms(t_end, 4), "tengu_agent_name_set", json!({ "source": "auto" }));
            }
        }
    }

    /// 同一请求先前几次失败（[`ApiCall::prior_attempts`]）在客户端那头报的重发事件，落在各自失败收尾后 2ms
    /// （`cap/auto-2.1.306-20261021-retry`）：头一次 5xx / 连接层失败换 `v2p` 原样重发，报
    /// `tengu_dispatch_header_fallback`；之后每次失败报 `tengu_api_retry`（`attempt` 从 1 数，`delayMs` 是到下一次
    /// 发出的间隔）。最终那条的收尾照常由 [`Self::emit_success`] / [`Self::emit_failure`] 报。
    pub(super) fn emit_retries(&mut self) {
        let env = self.env;
        let (call, shape) = (env.call, env.shape);
        if call.prior_attempts.is_empty() {
            return;
        }
        let query_source = env.query_source.clone();
        let mut attempt = 0u32;
        for (i, a) in call.prior_attempts.iter().enumerate() {
            let start: DateTime<Utc> = a.started_at.into();
            let end = ms(start, a.total_ms as i64);
            let t = ms(end, 2);
            if i == 0 && env.retry.fallback {
                if !env.f.v285 {
                    continue;
                }
                let mut fb = json!({
                    "model": &shape.model,
                    "dispatch": a.dispatch.as_deref().unwrap_or(config::CC_DISPATCH_ID),
                    "resend_dispatch": "v2p",
                    "reason": if a.status.is_some() { "5xx" } else { "conn_err" },
                    "status": a.status.map_or_else(|| "none".to_string(), |s| s.to_string()),
                    "query_source": &query_source
                });
                if let Some(r) = a.request_id.as_deref().filter(|r| !r.is_empty()) {
                    fb["request_id"] = json!(r);
                }
                self.push(t, "tengu_dispatch_header_fallback", fb);
                continue;
            }
            attempt += 1;
            let next: DateTime<Utc> =
                call.prior_attempts.get(i + 1).map_or(call.started_at, |n| n.started_at).into();
            let failure = CallFailure {
                status: a.status,
                error_type: a.error_type.clone(),
                message: a.message.clone(),
                in_band: a.in_band,
            };
            let kind_name = error_kind(&failure);
            let mut m = Map::new();
            m.insert("attempt".into(), json!(attempt));
            m.insert("delayMs".into(), json!((next - end).num_milliseconds().max(0)));
            m.insert(
                "error".into(),
                json!(format!(
                    "API error: type={kind_name} status={}",
                    a.status.map_or_else(|| "none".to_string(), |s| s.to_string())
                )),
            );
            m.insert("errorType".into(), json!(&kind_name));
            if env.f.v305 {
                m.insert(
                    "error_message_hash".into(),
                    json!(error_message_hash(
                        a.status,
                        a.error_type.as_deref(),
                        &a.message,
                        a.request_id.as_deref()
                    )),
                );
                if let Some((ctor, stack, top)) = error_trace(&env.f.version, a.status) {
                    m.insert("error_constructor".into(), json!(ctor));
                    m.insert("error_stack_hash".into(), json!(stack));
                    m.insert("error_top_frame".into(), json!(top));
                }
            }
            if let Some(st) = a.status {
                m.insert("status".into(), json!(st));
            }
            m.insert("provider".into(), json!("firstParty"));
            m.insert("attempt_duration_ms".into(), json!(a.total_ms));
            self.push(t, "tengu_api_retry", Value::Object(m));
        }
    }

    /// 失败收尾：`tengu_feature_bad{api_request}` + `tengu_api_error`。
    pub(super) fn emit_failure(&mut self) {
        let env = self.env;
        let (call, shape) = (env.call, env.shape);
        let TurnFacts {
            kind,
            ref display_model,
            ref previous_request_id,
            message_tokens,
            ref chain_id,
            query_depth,
            ..
        } = *env.f;
        let EventEnv { total, t_end, body_chars, gzip_skip, ref effort, .. } = *env;
        let query_source = env.query_source.as_str();
        // 失败收尾：`tengu_feature_bad{api_request}` + `tengu_api_error`，两条都进 Datadog
        // （官方那份 Datadog 白名单里 `tengu_api_error` 与 `tengu_feature_bad` 都在）。
        if let Some(fail) = &call.failure {
            let kind_name = error_kind(fail);
            let code = api_request_error_code(fail);
            let bad = json!({ "feature_name": "api_request", "error_code": code });
            self.push_dd(t_end, "tengu_feature_bad", bad);

            let mut err = Map::new();
            let mut put = |k: &str, v: Value| {
                err.insert(k.to_string(), v);
            };
            put("model", json!(&display_model));
            if env.f.v305 {
                put("dispatch", json!(env.dispatch()));
            }
            // 上游文案原样报（官方截断在 4000 字），除非连 request-id 都没拿到——那种情形
            // 官方换成 `API error: type=… status=…` 这句合成文案。
            let has_request_id = call.request_id.as_deref().is_some_and(|r| !r.is_empty());
            let message = if has_request_id && !fail.message.trim().is_empty() {
                let m = fail.message.trim();
                match m.char_indices().nth(4_000) {
                    Some((cut, _)) => format!("{}\u{2026}<truncated>", &m[..cut]),
                    None => m.to_string(),
                }
            } else {
                format!(
                    "API error: type={kind_name} status={}",
                    fail.status.map_or_else(|| "none".to_string(), |s| s.to_string())
                )
            };
            put("error", json!(message));
            // 错误的哈希与栈（官方 `Dp`，`cap/auto-2.1.306-20261021-retry`），见 [`error_message_hash`] / [`error_trace`]。
            if env.f.v305 {
                put(
                    "error_message_hash",
                    json!(error_message_hash(
                        fail.status,
                        fail.error_type.as_deref(),
                        &fail.message,
                        call.request_id.as_deref()
                    )),
                );
                if let Some((ctor, stack, top)) = error_trace(&env.f.version, fail.status) {
                    put("error_constructor", json!(ctor));
                    put("error_stack_hash", json!(stack));
                    put("error_top_frame", json!(top));
                }
            }
            // `status` 是十进制串而不是数字（官方 `FP()` 就是 `String(status)`）；流内错误
            // 那种 SDK 侧没有状态码，整个字段不出现。
            if let Some(st) = fail.status {
                put("status", json!(st.to_string()));
            }
            put("errorType", json!(&kind_name));
            if let Some(e) = &effort {
                put("effort_level", json!(e));
            }
            put("messageCount", json!(shape.messages_len));
            put("messageTokens", json!(message_tokens));
            put("durationMs", json!(total));
            put("durationMsIncludingRetries", json!(env.duration_including_retries()));
            put("attempt", json!(env.retry.attempt()));
            put("provider", json!("firstParty"));
            if has_request_id {
                put("requestId", json!(call.request_id.as_deref().unwrap_or("")));
            }
            if let Some(crid) = call.client_request_id.as_deref().filter(|c| !c.is_empty()) {
                put("clientRequestId", json!(crid));
            }
            put("didFallBackToNonStreaming", json!(false));
            put("requestBodyEncoding", json!("identity"));
            put("requestBodyChars", json!(body_chars));
            put("gzipSkipReason", json!(gzip_skip));
            if kind.has_chain() {
                put("queryChainId", json!(&chain_id));
                put("queryDepth", json!(query_depth));
            }
            put("querySource", json!(query_source));
            put("fastMode", json!(shape.fast_mode));
            if let Some(prev) = &previous_request_id {
                put("previousRequestId", json!(prev));
            }
            let err = Value::Object(err);
            self.push_dd_snake(t_end, "tengu_api_error", err);
        }
    }

    /// 失败的请求也是一轮的终点：turn_end 与之后的那串。
    pub(super) fn emit_failed_turn_end(&mut self) {
        let env = self.env;
        let (call, shape) = (env.call, env.shape);
        let TurnFacts {
            ref identity,
            turn_over,
            failed,
            aborted,
            emit_first_turn,
            is_main,
            turn_started,
            prompt_seq,
            v280,
            v285,
            v291,
            ref dd_model,
            ..
        } = *env.f;
        let EventEnv { total, t_end, tpl, pre_count, ref effort, .. } = *env;
        let error_kind_name = env.error_kind_name.as_str();
        // 心跳：主线程这一轮停手，界面从「干活」回到闲着，落在 `turn_end` 之后十几毫秒
        // （`cap/auto-2.1.305-20261021-full` B1：+64、+19、+15）。`-p` 没有界面运行态，登记了也不动。
        let idle_at = if is_main && turn_over && failed {
            Some(ms(t_end, 16))
        } else if is_main && aborted {
            Some(ms(t_end, 18))
        } else if is_main && turn_over {
            Some(ms(t_end, 17))
        } else {
            None
        };
        // 后台子代理还在跑时的一轮结束：`turn_end` 之后 2ms 先从 `subagent_running` 切回 `turn_running`，
        // 再照常停手（A8a .213 → .215 → .228，A8b .127 → .129 → .143，A19 .444 → .447 → .462）。
        if is_main && turn_over && !failed && !aborted && env.f.delegated_running && !shape.sdk {
            self.beat(ms(t_end, 4), heartbeat::Trigger::Run(Some(heartbeat::TURN_RUNNING)));
        }
        if let Some(t) = idle_at {
            self.beat(t, heartbeat::Trigger::Run(None));
        }
        if is_main && turn_over && failed {
            // 请求失败也是一轮的终点，但没有 stop hook、没有 `tengu_feature_ok{turn}`、
            // 也没有首轮那串——那些都挂在「跑完了」这条路上。
            self.push(
                ms(t_end, 1),
                "tengu_turn_end",
                env.turn_end(
                    "api_error",
                    (ms(t_end, 1) - turn_started).num_milliseconds().max(total),
                    Some(if error_kind_name.is_empty() { "unknown" } else { error_kind_name }),
                ),
            );
            if shape.sdk {
                self.emit_sdk_result(SdkOutcome::Failed);
            }
        } else if is_main && aborted && shape.sdk {
            // `-p` / SDK 里被中止：没有界面上的 Esc、也不撤回输入，只有 turn_end 与 `terminated` 的结果
            // （官方 `subtype: error_during_execution` 且被中止时改报 `terminated`）。
            if let Some(live) = env.live_outcome("aborted") {
                self.push_dd_snake(ms(t_end, 2), "tengu_tether_live_outcome", live);
            }
            let t3 = ms(t_end, 3);
            self.push(
                t3,
                "tengu_turn_end",
                env.turn_end(
                    "aborted_streaming",
                    (t3 - turn_started).num_milliseconds().max(total),
                    None,
                ),
            );
            self.emit_sdk_result(SdkOutcome::Aborted);
        } else if is_main && aborted {
            // 按 Esc 打断（`cap/auto-2.1.285-20260930` 08:01:42.630–.635）：取消、tether 收尾报
            // `aborted`、turn、`turn_end{aborted_streaming}`，没有 stop hook；一个字都还没出的话
            // 客户端把这一轮撤回（`conversation_rewind{auto_restore_cancel}`），输入框里恢复原文。
            let stream_mode = if call.text_chars > 0 {
                "responding"
            } else if call.saw_thinking {
                "thinking"
            } else {
                "requesting"
            };
            let mut cancel = json!({ "source": "escape", "streamMode": stream_mode });
            if let Some(e) = &effort {
                cancel["effort_level"] = json!(e);
            }
            cancel["message_id"] = json!(call.message_id.as_deref().unwrap_or(""));
            self.push(t_end, "tengu_cancel", cancel);
            if let Some(live) = env.live_outcome("aborted") {
                self.push_dd_snake(ms(t_end, 2), "tengu_tether_live_outcome", live);
            }
            let t3 = ms(t_end, 3);
            for name in ["shoji_engine", "turn"] {
                self.push_dd(t3, "tengu_feature_ok", feature(name));
            }
            self.push(
                t3,
                "tengu_turn_end",
                env.turn_end(
                    "aborted_streaming",
                    (t3 - turn_started).num_milliseconds().max(total),
                    None,
                ),
            );
            if call.text_chars == 0 {
                let t5 = ms(t_end, 5);
                let before = pre_count + 1;
                self.push(
                    t5,
                    "tengu_conversation_rewind",
                    json!({
                        "preRewindMessageCount": before,
                        "postRewindMessageCount": before - 4,
                        "messagesRemoved": 4,
                        "rewindToMessageIndex": before - 4,
                        "source": "auto_restore_cancel"
                    }),
                );
                self.push_dd(t5, "tengu_feature_ok", feature("repl_rewind_conversation"));
            }
        } else if is_main && turn_over {
            self.take_tpl(&tpl.turn, t_end);
            if shape.sdk {
                self.emit_sdk_result(SdkOutcome::Success);
            }
            if emit_first_turn {
                self.take_tpl(&tpl.first_turn, t_end);
            }
            let t1 = ms(t_end, 1);
            let t2 = ms(t_end, 2);
            // 2.1.280 起每轮收尾都报一次「猜下一句」为什么没出（`cap/2.1.285` 14 轮 14 条，
            // `cap/2.1.280` 同样逐轮有）：这一轮
            // 大半在写缓存（首轮、换模型之后）是 `cache_cold`，其余是 `unfocused`——终端不在
            // 前台，客户端也就没发猜下一句那条请求，与代理这边看到的一致。分界取「缓存写入超过
            // 读取的一成」，14 条全对得上（写 884 / 读 87942 那两条是 unfocused）。
            //
            // 2.1.285（`cap/auto-2.1.285-20260930` 35 条）改成按「猜下一句」那条请求的结果报，一条
            // `unfocused` 都没有：这一轮大半在写缓存就不发那条请求，当场报 `cache_cold`；会话第一轮
            // 报 `early_conversation`（auto 模式那个会话的首轮仍是 `cache_cold`）；其余的轮次客户端
            // 会发那条请求，这里不报，等它的结果——正文为空报 `empty`、被新输入顶掉报 `aborted`、
            // 出了建议则在下一次输入时报 `ignored`（见 [`ignored_suggestion_meta`]）。`-p` 模式不猜。
            // 2.1.291：每轮收尾、猜下一句那条之前多一条消息展示钩子的统计（交互式才有，`-p` 没有；
            // `cap/auto-2.1.291-20261006-full` 46 条，flushCount 绝大多数是 1，耗时 1 ~ 8ms、
            // 只刷一次时 total 与 max 相同）。
            if v291 && !shape.sdk {
                let d = 2 + i64::from(env.req_hash % 6);
                self.push(
                    t1,
                    "tengu_message_display_hooks",
                    json!({ "flushCount": 1, "errorCount": 0, "totalDurationMs": d, "maxDurationMs": d }),
                );
            }
            if v280 && !shape.sdk {
                let cold = call.cache_creation_tokens * 10 > call.cache_read_tokens;
                let early = v285 && prompt_seq == 1 && shape.permission_mode != "auto";
                let suggestion = if early {
                    Some(
                        json!({ "source": "cli", "outcome": "suppressed", "reason": "early_conversation", "prompt_id": "user_intent" }),
                    )
                } else if cold {
                    Some(
                        json!({ "source": "cli", "outcome": "suppressed", "reason": "cache_cold", "cacheColdBy": "cache_write", "prompt_id": "user_intent" }),
                    )
                } else if v285 {
                    None
                } else {
                    Some(
                        json!({ "source": "cli", "outcome": "suppressed", "reason": "unfocused", "prompt_id": "user_intent" }),
                    )
                };
                if let Some(suggestion) = suggestion {
                    self.push(t1, "tengu_prompt_suggestion", suggestion);
                }
            }
            self.push(t1, "tengu_feature_ok", feature("hook_stop_handler"));
            self.push(t2, "tengu_feature_ok", feature("turn"));
            self.push(
                t2,
                "tengu_turn_end",
                env.turn_end("completed", (t2 - turn_started).num_milliseconds().max(total), None),
            );
            self.dd.push(identity.dd_entry(
                "tengu_feature_ok",
                &env.ctx(t1),
                dd_model,
                feature("hook_stop_handler"),
            ));
            self.dd.push(identity.dd_entry(
                "tengu_feature_ok",
                &env.ctx(t2),
                dd_model,
                feature("turn"),
            ));
        }
    }

    /// 子代理收尾那条 `tengu_agent_tool_completed` 的顶层 `betas` 换成它这条请求自己那份会话级 beta。
    ///
    /// 它自己的 `tengu_api_query` / `tengu_api_success` 不在这里换：API 事件的顶层报这条请求的**完整**
    /// beta 串，与 meta 里那份同值，生成时已经跟着 meta 走了（`cap/2.1.285` 起直到
    /// `cap/auto-2.1.308-20261022-gaps`，各版子代理 API 事件顶层与 meta 都相同：claude-code-guide 跑 haiku-4.5
    /// 时 11 项、跑 haiku-5-5 时 16 项）。此前在这里一并换成过滤后那份（6 项），是错的。
    pub(super) fn emit_agent_own_betas(&mut self) {
        let env = self.env;
        let TurnFacts { kind, ref betas_own, ref betas_session, .. } = *env.f;
        // `take_tpl` 借着 `self.tpl_events`/`self.tpl_dd`，到这里已经不再用它，可以并进主队列。
        if kind.is_agent() && betas_session != betas_own {
            for (_, e) in self.events.iter_mut() {
                if e["event_data"]["event_name"].as_str() == Some("tengu_agent_tool_completed")
                    && let Some(d) = e.get_mut("event_data").and_then(|d| d.as_object_mut())
                {
                    d.insert("betas".into(), json!(&betas_own));
                }
            }
        }
    }

    /// `/model` 的「Hi」探测只报一条精简的 `tengu_api_success`。
    pub(super) fn emit_model_validation(&mut self) {
        let env = self.env;
        let call = env.call;
        let TurnFacts { ref identity, kind, ref resp_model, failed, ref dd_model, .. } = *env.f;
        let EventEnv { total, t_end, body_chars, gzip_skip, .. } = *env;
        let query_source = env.query_source.as_str();
        // `/model` 的「Hi」探测：官方只有这一条字段极少的 `tengu_api_success`（`cap/auto-2.1.285-20260930`
        // 四条逐字同序），前后的规范化、断点、api_query 与收尾一概没有。
        if kind == Kind::ModelValidation {
            self.events.clear();
            self.dd.clear();
            if !failed {
                let ok = json!({
                    "requestId": call.request_id.as_deref().unwrap_or(""),
                    "clientRequestId": call.client_request_id.as_deref().unwrap_or(""),
                    "querySource": query_source,
                    "model": &resp_model,
                    "inputTokens": call.input_tokens,
                    "outputTokens": call.output_tokens,
                    "cachedInputTokens": call.cache_read_tokens,
                    "uncachedInputTokens": call.cache_creation_tokens,
                    "durationMs": total,
                    "durationMsIncludingRetries": total,
                    "attempt": 1,
                    "dispatch": config::CC_DISPATCH_ID,
                    "stop_reason": call.stop_reason.as_deref().unwrap_or("max_tokens"),
                    "requestBodyEncoding": "identity",
                    "requestBodyChars": body_chars,
                    "gzipSkipReason": gzip_skip
                });
                self.events.push((
                    t_end,
                    identity.event("tengu_api_success", t_end, &env.ctx(t_end), ok.clone()),
                ));
                self.dd.push(identity.dd_entry(
                    "tengu_api_success",
                    &env.ctx(t_end),
                    dd_model,
                    snake_flat(&ok),
                ));
            }
        }
    }
}

/// `-p` / SDK 这一轮怎么结束的，见 [`EventBuilder::emit_sdk_result`]。
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SdkOutcome {
    Success,
    Failed,
    Aborted,
}

impl EventBuilder<'_> {
    /// `-p` 跑完这一轮进程就退出（`cap/auto-2.1.306-20261021-full` D1 七条）：首字耗时（只有成功才报）与
    /// 结果两条，最后一条会话结束的缓存逐出提示；中间那几条静态的退出收尾在模板里。
    ///
    /// 两个时长都从**本轮开始**算（官方 `submitMessage` 起点，落在 `input_prompt` 前二三十毫秒，比
    /// luban 记的 `turn_started` 早约 42ms：七条里六条 `duration_ms − turn_end.duration_ms` 是 38～46），
    /// 首字量到第一条 assistant 消息产出——SDK 整条消息收完才吐，比那条回复流收尾早约 5ms
    /// （`ttft_ms` 比 `duration_ms` 少 18～26）。工具按名字分 MCP / ToolSearch / 内置（官方 `ux`）。
    /// 请求失败（重试用尽或不可重试）报的仍是 `subtype: success`、`is_error: true`，另带 `retry_status` /
    /// `api_error_status`，失败的尝试不算进 `duration_api_ms`，之前没有成功过的主请求就没有会话结束的逐出提示与心跳
    /// （`cap/auto-2.1.306-20261021-retry` R2）；被中止报 `terminated`。
    pub(super) fn emit_sdk_result(&mut self, outcome: SdkOutcome) {
        let env = self.env;
        let TurnFacts {
            ref resp_model,
            ref prev_main_request_id,
            turn_started,
            turn_first_reply_end,
            v299,
            v300,
            sess_turn_tools,
            sess_turn_mcp,
            sess_turn_toolsearch,
            sess_turn_api_calls,
            sess_turn_api_ms,
            sess_turn_retry,
            ..
        } = *env.f;
        let EventEnv { t_end, .. } = *env;
        let t4 = ms(t_end, 4);
        let start = ms(turn_started, -42);
        if outcome == SdkOutcome::Success {
            let first = turn_first_reply_end.unwrap_or(t_end);
            self.push(
                t4,
                "tengu_sdk_ttft",
                json!({
                    "ttft_ms": (ms(first, -5) - start).num_milliseconds().max(0),
                    "model": resp_model,
                    "tool_pool_reused": false
                }),
            );
        }
        let (subtype, is_error) = match outcome {
            SdkOutcome::Success => ("success", false),
            SdkOutcome::Failed => ("success", true),
            SdkOutcome::Aborted => ("terminated", true),
        };
        let api_ms = if sess_turn_api_ms > 0 { sess_turn_api_ms + 3 } else { 0 };
        let builtin = sess_turn_tools.saturating_sub(sess_turn_mcp + sess_turn_toolsearch);
        let mut result = json!({
            "subtype": subtype,
            "is_error": is_error,
            "num_turns": sess_turn_api_calls,
            "duration_ms": (t4 - start).num_milliseconds().max(0),
            "duration_api_ms": api_ms,
            "saw_retry": sess_turn_retry.is_some(),
            "saw_compact": false,
            "tool_use_count": sess_turn_tools,
            "mcp_tool_calls": sess_turn_mcp,
            "toolsearch_calls": sess_turn_toolsearch,
            "builtin_tool_calls": builtin,
            "turn_index": 0,
            "mcp_pending_at_start": 0,
            "mcp_pending_at_end": 0
        });
        // 本轮重试过就在 `saw_compact` 之后报各次重试状态码的最大值（整轮累计，首条重试过、之后工具续轮正常收尾的
        // 照样报），请求最终失败再报它的状态码（R2：529 / 529）。
        let failed_status = env.call.failure.as_ref().and_then(|f| f.status);
        let extra: Vec<(&str, Value)> = [
            sess_turn_retry.map(|s| ("retry_status", json!(s))),
            failed_status
                .filter(|_| outcome == SdkOutcome::Failed)
                .map(|s| ("api_error_status", json!(s))),
        ]
        .into_iter()
        .flatten()
        .collect();
        if !extra.is_empty() {
            insert_after(&mut result, "saw_compact", extra);
        }
        // 2.1.299：末尾多报进程里加载了几个 mod（`cap/auto-2.1.299-20261014-full` 的 `-p` 全是 0）。
        if v299 {
            result["installed_mods"] = json!(0);
        }
        // 2.1.300：再多 debug 输出被丢掉的条数，stderr 一项、调试文件一项（`-p` 的 6 条全是 0）。
        if v300 {
            result["debug_stderr_dropped_total"] = json!(0);
            result["debug_file_dropped_total"] = json!(0);
        }
        self.push(t4, "tengu_sdk_result", result);
        // 逐出提示报的是最后一条**成功**的主请求（官方 `dV()`）：失败或被中止的那条都不算，之前也没成功过就整条
        // 不报，心跳的 `session_end` 一样报不出来（R2）。
        let last_ok = match outcome {
            SdkOutcome::Success => env.call.request_id.as_deref(),
            SdkOutcome::Failed | SdkOutcome::Aborted => prev_main_request_id.as_deref(),
        };
        if last_ok.is_some() {
            self.push(
                ms(t_end, 748),
                "tengu_cache_eviction_hint",
                json!({
                    "scope": "session_end",
                    "last_request_id": last_ok.unwrap_or("")
                }),
            );
        }
        // 心跳的 `session_end` 与它同一毫秒、排在它后面（`cap/auto-2.1.305-20261021-full` D1）。
        self.beat(ms(t_end, 748), heartbeat::Trigger::Reset("session_end"));
    }
}
