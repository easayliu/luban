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
                put("dispatch", json!(config::CC_DISPATCH_ID));
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
            // 前三种不报 `plainInputExcessTokens`。
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
                } else if shape.thread_type.as_deref() == Some("continue") {
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
                if matches!(reason, "none" | "sent_once_text") {
                    put(
                        "plainInputExcessTokens",
                        json!((call.input_tokens.max(0) as u64).saturating_sub(sent_once)),
                    );
                }
            }
            put("durationMs", json!(total));
            put("durationMsIncludingRetries", json!(total + 1 + i64::from(retry_pad)));
            put("attempt", json!(1));
            put("ttftMs", json!(ttft));
            // 首个内容块到达：比首字节晚 0–1ms（`cap/2.1.280` 九条里七条 +1、两条 +0）。
            if v270 {
                put("firstContentMs", json!(ttft + i64::from(retry_pad % 2)));
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
            put("imageTotalPixels", json!(0));
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
            self.push(t_end, "tengu_api_success", success.clone());
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
            // 06:34:21.881，2.1.280 没有）：system 角色消息、工具变更头、保留提醒这三项跟着对应的
            // beta 走。
            // `-p` 的只在带 `mid-conversation-system` 的模型上记（fable / opus 那两条有、haiku 那条没有）。
            if first_main
                && v285
                && (!shape.sdk || betas_full.contains("mid-conversation-system-2"))
            {
                let has = |p: &str| betas_full.split(',').any(|b| b.trim().starts_with(p));
                self.push(
                    ms(t_end, 1),
                    "tengu_wire_shape_recorded",
                    json!({
                        "systemTurns": has("mid-conversation-system-2"),
                        "toolChangeHeader": has("mid-conversation-tool-changes-"),
                        "inlineTools": false,
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
        let EventEnv { total, t_end, retry_pad, body_chars, gzip_skip, ref effort, .. } = *env;
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
            put("durationMsIncludingRetries", json!(total + 1 + i64::from(retry_pad)));
            put("attempt", json!(1));
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
            ref resp_model,
            turn_over,
            failed,
            aborted,
            emit_first_turn,
            is_main,
            started_wall,
            turn_started,
            prompt_seq,
            v280,
            v285,
            v291,
            ref dd_model,
            sess_turn_tools,
            sess_turn_api_calls,
            sess_turn_api_ms,
            ..
        } = *env.f;
        let EventEnv { total, t_first, t_end, tpl, pre_count, ref effort, .. } = *env;
        let error_kind_name = env.error_kind_name.as_str();
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
            // `-p` 跑完这一轮进程就退出（`00448` 08:06:45.632–46.376）：首字耗时与结果两条（从进程
            // 启动算），最后一条会话结束的缓存逐出提示；中间那几条静态的退出收尾在模板里。
            if shape.sdk {
                let t4 = ms(t_end, 4);
                let start: DateTime<Utc> = started_wall.into();
                self.push(
                    t4,
                    "tengu_sdk_ttft",
                    json!({
                        "ttft_ms": (t_first - start).num_milliseconds().max(0),
                        "model": &resp_model,
                        "tool_pool_reused": false
                    }),
                );
                self.push(
                    t4,
                    "tengu_sdk_result",
                    json!({
                        "subtype": "success",
                        "is_error": false,
                        "num_turns": sess_turn_api_calls,
                        "duration_ms": (t4 - start).num_milliseconds().max(0),
                        "duration_api_ms": sess_turn_api_ms + 3,
                        "saw_retry": false,
                        "saw_compact": false,
                        "tool_use_count": sess_turn_tools,
                        "mcp_tool_calls": 0,
                        "toolsearch_calls": 0,
                        "builtin_tool_calls": sess_turn_tools,
                        "turn_index": 0,
                        "mcp_pending_at_start": 0,
                        "mcp_pending_at_end": 0
                    }),
                );
                self.push(
                    ms(t_end, 748),
                    "tengu_cache_eviction_hint",
                    json!({
                        "scope": "session_end",
                        "last_request_id": call.request_id.as_deref().unwrap_or("")
                    }),
                );
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

    /// 子代理自己那几类事件的顶层 `betas` 换回它这条请求的。
    pub(super) fn emit_agent_own_betas(&mut self) {
        let env = self.env;
        let TurnFacts { kind, ref betas_own, ref betas_session, .. } = *env.f;
        // `take_tpl` 借着 `self.tpl_events`/`self.tpl_dd`，到这里已经不再用它，可以并进主队列。
        if kind.is_agent() && betas_session != betas_own {
            for (_, e) in self.events.iter_mut() {
                let own = matches!(
                    e["event_data"]["event_name"].as_str(),
                    Some("tengu_api_query" | "tengu_api_success" | "tengu_agent_tool_completed")
                );
                if own && let Some(d) = e.get_mut("event_data").and_then(|d| d.as_object_mut()) {
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
