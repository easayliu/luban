//! 请求本身：发出前的探测与规范化、`tengu_api_query`、首字节。

use super::*;

impl EventBuilder<'_> {
    /// 发请求前的 git 探测、子代理选型、线程回放与工具搜索核验。
    pub(super) fn emit_pre_request(&mut self) {
        let env = self.env;
        let (call, shape) = (env.call, env.shape);
        let TurnFacts {
            ref base_identity,
            kind,
            ref display_model,
            git_outcome,
            new_prompt,
            query_depth,
            v280,
            v285,
            v291,
            ref main_effort,
            ref ignored_suggestion,
            thread_step,
            ..
        } = *env.f;
        let EventEnv { t0, builtin_agent, .. } = *env;
        let query_source = env.query_source.as_str();
        // auto 模式下**每条**带工具的请求发出前都探一次 git 状态，不只是新输入那条
        // （`cap/2.1.280` 第二个会话：主线程续轮、猜下一句、子代理每一步与它的摘要请求前都有，
        // 紧跟在攒附件之后）。新输入那条已经在上面报过。
        if v280
            && !new_prompt
            && shape.permission_mode == "auto"
            && shape.tools_count > 0
            && kind != Kind::WebSearchTool
        {
            self.push(
                ms(t0, -2),
                "tengu_auto_mode_git_state_probe",
                {
                    let mut probe = json!({ "duration_ms": query_depth % 2, "wait_ms": 0, "outcome": git_outcome, "truncated": false });
                    // 2.1.291 多两项：进程里头一回探才是 true（每次输入那条在前头，这里恒 false）。
                    if v291 {
                        probe["first_in_process"] = json!(false);
                        probe["repo_visibility_lookup"] = json!("none");
                    }
                    probe
                },
            );
        }
        // 续轮与侧查询在发请求前也判一次工具搜索模式（首次输入的那条在模板里）；它排在
        // 规范化那串之前（`cap/2.1.260-2` 与 `cap/2.1.280/00048` 都是）。
        if !new_prompt {
            self.push(
                ms(t0, -1),
                "tengu_tool_search_mode_decision",
                tool_search_decision(shape, display_model, kind.is_agent()),
            );
            // 2.1.293 标题生成（haiku-5-5）头一次用这个模型：紧跟工具搜索判定（同一毫秒，
            // `cap/auto-2.1.293-20261008-full` B0 会话 -20ms 那组）。
            if env.f.sleepy && kind == Kind::Title {
                self.push(
                    ms(t0, -1),
                    "tengu_sleepy_snowflake_applied",
                    json!({ "model": display_model, "source": "growthbook", "value": "all" }),
                );
            }
        }
        // 2.1.285：每条线程的头两条请求前，客户端把上下文宣告、提醒折叠、工具入参回显这几样
        // 记一次、回放一次（`cap/2.1.285`：主线程首条那串在模板里，第二条只有两条 `*_replayed`；
        // 子代理首步 06:56:08.494–.497 是整串，第二步 06:56:20.389 / .392 又是那两条回放）。
        // 回放那两条夹着工具搜索判定：折叠在前、回显在后。
        // 子代理首步之前，主线程那边先选定子代理类型、解析它的模型（`cap/2.1.285` 06:56:08.488：
        // `agent_tool_selected` 挂在主线程上，`subagent_model_resolve` 已经挂在子代理支线上）。
        if v285 && kind == Kind::Subagent && thread_step == Some(0) {
            let ts = ms(t0, -11);
            let family = shape.model.trim_start_matches("claude-").split('-').next().unwrap_or("");
            let selected = json!({
                "agent_type": call.agent.agent_type.as_deref().unwrap_or("general-purpose"),
                "model": &shape.model,
                "source": if builtin_agent.is_some() { "built-in" } else { "custom" },
                "is_built_in_agent": builtin_agent.is_some(),
                "is_resume": false,
                "is_async": true,
                "is_fork": false,
                "agent_depth": 1,
                "agent_system_prompt_chars": shape.system_chars
            });
            // 2.1.291 在 `is_fork` 后面多报主会话与子代理各自的 effort，**没有就不报**：opus 主线程起
            // 的 Explore 两项都是 high，haiku 主线程起的 haiku Explore 两项都不在
            // （`cap/auto-2.1.291-20261006-full` 的 E4 会话）。主会话那项取主线程最近一条，子代理那项
            // 取子代理这条请求自己的。
            let mut selected = selected;
            if v291 {
                let mut pairs = Vec::new();
                if let Some(e) = main_effort.as_deref() {
                    pairs.push(("session_effort", json!(e)));
                }
                if let Some(e) = shape.effort.as_deref() {
                    pairs.push(("subagent_effort", json!(e)));
                }
                insert_after(&mut selected, "is_fork", pairs);
            }
            self.main_side.push((
                ts,
                base_identity.event("tengu_agent_tool_selected", ts, &env.ctx(ts), selected),
            ));
            let resolve = json!({
                "feature_name": "subagent_model_resolve",
                "source": "spawn",
                "precedence": "frontmatter",
                "requested_family": family,
                "resolved_family": family,
                "requested_model": family,
                "resolved_model": &shape.model
            });
            self.push_dd(ts, "tengu_feature_ok", resolve);
        }
        // `/compact`、`/btw` 也是一次输入：挂着的建议在提交那一刻算 ignored（`07:57:01.971`）。
        if matches!(kind, Kind::Compact | Kind::SideQuestion)
            && let Some((rid, shown_end, chars)) = &ignored_suggestion
        {
            let submit = ms(t0, -11);
            // 敲的是斜杠命令本身：`/compact` 8 个字，`/btw ` 加问句。
            let typed = if kind == Kind::Compact { 8 } else { shape.prompt_len + 5 };
            self.push(
                submit,
                "tengu_prompt_suggestion",
                ignored_suggestion_meta(rid, *shown_end, *chars, submit, typed),
            );
        }
        // `/btw` 插问发出前同样回放一次提醒折叠与工具入参回显（`cap/auto-2.1.285-20260930`
        // 08:00:15.238 / .242）。
        if v285 && kind == Kind::SideQuestion {
            self.push(
                ms(t0, -5),
                "tengu_reminder_fold_replayed",
                json!({ "fold": false, "cached": false, "matched": true, "cachedSource": "payload" }),
            );
            self.push(
                ms(t0, -1),
                "tengu_wire_tool_input_echo_replayed",
                json!({ "echo": true, "cached": true, "matched": true, "cachedSource": "payload" }),
            );
        }
        if v285 && let Some(step) = thread_step {
            let recorded = step == 0 && kind == Kind::Subagent;
            let replayed = step <= 1 && (kind == Kind::Subagent || step == 1);
            if recorded {
                let tr = ms(t0, -5);
                self.push_dd(tr, "tengu_feature_ok", feature("context_git_detect"));
                for (kind_name, tokens) in [("session_context", 82), ("date", 9)] {
                    self.push(
                        tr,
                        "tengu_context_announcement",
                        json!({
                            "announcement_type": kind_name,
                            "token_estimate": tokens,
                            "updates_copy": false,
                            "query_source": query_source
                        }),
                    );
                }
                let t4 = ms(t0, -4);
                self.push(
                    t4,
                    "tengu_reminder_fold_recorded",
                    json!({ "recorded": true, "fold": false, "carriedForward": false, "inherited": false, "cached": false, "cachedSource": "payload" }),
                );
                self.push(
                    t4,
                    "tengu_wire_tool_input_echo_recorded",
                    json!({ "recorded": true, "echo": true, "carriedForward": false, "inherited": false, "cached": true, "cachedSource": "payload" }),
                );
                self.push(
                    t4,
                    "tengu_context_rendering_recorded",
                    json!({ "rendering": "announced", "carriedForward": false }),
                );
            }
            if replayed {
                self.push(
                    ms(t0, -3),
                    "tengu_reminder_fold_replayed",
                    json!({ "fold": false, "cached": false, "matched": true, "cachedSource": "payload" }),
                );
                self.push(
                    ms(t0, if new_prompt { 0 } else { -1 }),
                    "tengu_wire_tool_input_echo_replayed",
                    json!({ "echo": true, "cached": true, "matched": true, "cachedSource": "payload" }),
                );
            }
        }
        // 每条带工具的请求发出前核一次声明的工具集（主线程、猜下一句、离开摘要、子代理都有，
        // 标题那类无工具的没有）。`deferredLate` 两份抓包 55 条恒为 3、其余计数恒为 0；
        // 没有 `ToolSearch` 的形态抓包里没见过，按字段名报 0 与 `toolSearchAbsent: true`。
        // 子代理只在工具表里有 `ToolSearch` 时才核（`cap/2.1.280` Explore 七条都有；
        // `cap/2.1.285` 的 claude-code-guide 没有 `ToolSearch`，十二条请求一条都没有）。
        // `-p` 的第一条请求不核（`cap/auto-2.1.285-20260930` 十个 `-p` 进程的首条都没有）。
        if v280
            && shape.tools_count > 0
            && kind != Kind::WebSearchTool
            && !(shape.sdk && new_prompt)
            && (!kind.is_agent() || shape.has_tool_search)
        {
            self.push(ms(t0, -1), "tengu_declared_tool_set_held", {
                let mut held = json!({
                    "deferredLate": if shape.has_tool_search { 3 } else { 0 },
                    "redeclared": 0,
                    "fromRecord": 0,
                    "queryDepth": query_depth,
                    "toolSearchAbsent": !shape.has_tool_search
                });
                // 2.1.285 末尾多两项，11 条恒为 false / 0（`cap/2.1.285`）。
                if v285 {
                    insert_after(
                        &mut held,
                        "toolSearchAbsent",
                        vec![
                            ("noDeferredChannel", json!(false)),
                            ("unclassifiedDepartures", json!(0)),
                        ],
                    );
                }
                held
            });
        }
    }

    /// 请求本身：规范化、system 边界、tether 判定、缓存断点与 `tengu_api_query`。
    pub(super) fn emit_request(&mut self) {
        let env = self.env;
        let shape = env.shape;
        let TurnFacts {
            kind,
            ref display_model,
            ref previous_request_id,
            v285,
            v291,
            first_main,
            ref tether,
            ref betas_full,
            ..
        } = *env.f;
        let EventEnv {
            t0,
            build_age_mins,
            modern,
            pre_count,
            api_system,
            model_held,
            classifier_held,
            ref effort,
            ..
        } = *env;
        let query_source = env.query_source.as_str();
        self.push(
            ms(t0, -1),
            "tengu_api_before_normalize",
            json!({ "preNormalizedMessageCount": pre_count }),
        );
        self.push(
            t0,
            "tengu_api_after_normalize",
            json!({
                "postNormalizedMessageCount": shape.messages_len,
                "apiSystemMessageCount": api_system
            }),
        );
        // 2.1.258 那版 5 条以上消息会钉住分叉点（markerCount 2），2.1.260 起恒为 1；
        // 猜下一句那条不写缓存。
        let pinned = !modern && shape.messages_len > 2;
        // 2.1.285 的离开回顾钉住分叉点，断点仍是一个（`cap/2.1.285/00099`、`00160` 都是
        // `forkPointPinned: true, markerCount: 1`；2.1.280 的 `00048`、`00088` 是 false）。
        let fork_pinned = v285 && kind == Kind::AwaySummary;
        let breakpoints = json!({
            "totalMessageCount": shape.messages_len,
            "cachingEnabled": shape.has_cache_control,
            // fork 出来的查询不写缓存：猜下一句、子代理摘要与离开回顾都是 true（`cap/2.1.280`、
            // `cap/2.1.285`）。
            "skipCacheWrite": kind.is_fork(),
            "forkPointPinned": pinned || fork_pinned,
            "markerCount": if pinned { 2 } else { 1 }
        });
        // 发请求前的顺序照 `cap/2.1.280/00036`：边界、首块、边界，然后 tether 判定与回声审计，
        // 再是缓存断点和 api_query。
        if kind.has_boundary_marker() && shape.system_blocks >= 2 {
            let boundary = json!({
                "blockCount": shape.system_blocks,
                "staticBlockLength": shape.static_len,
                "dynamicBlockLength": shape.dynamic_len
            });
            self.push(t0, "tengu_sysprompt_boundary_found", boundary.clone());
            if shape.sys0_len > 0 {
                self.push(
                    t0,
                    "tengu_sysprompt_block",
                    json!({ "length": shape.sys0_len, "hash": &shape.sys0_hash }),
                );
            }
            self.push(t0, "tengu_sysprompt_boundary_found", boundary);
        } else if kind.is_agent() {
            // 子代理：标记缺失、首块、标记缺失，块数恒报 6（见 [`Kind::has_boundary_marker`]）。
            let missing = json!({ "promptBlockCount": 6 });
            self.push(t0, "tengu_sysprompt_missing_boundary_marker", missing.clone());
            if shape.sys0_len > 0 {
                self.push(
                    t0,
                    "tengu_sysprompt_block",
                    json!({ "length": shape.sys0_len, "hash": &shape.sys0_hash }),
                );
            }
            self.push(t0, "tengu_sysprompt_missing_boundary_marker", missing);
        } else if v285 {
            // 2.1.285 的无工具侧查询（标题、WebFetch 页面处理）是缺标记、首块、缺标记，与子代理
            // 同序（`cap/2.1.285/00040` 06:34:37.218、`00137` 06:56:14.139）。
            let missing = json!({ "promptBlockCount": shape.system_blocks });
            if shape.system_blocks > 0 {
                self.push(t0, "tengu_sysprompt_missing_boundary_marker", missing.clone());
            }
            if shape.sys0_len > 0 {
                self.push(
                    t0,
                    "tengu_sysprompt_block",
                    json!({ "length": shape.sys0_len, "hash": &shape.sys0_hash }),
                );
            }
            if shape.system_blocks > 0 {
                self.push(t0, "tengu_sysprompt_missing_boundary_marker", missing);
            }
        } else {
            if shape.sys0_len > 0 {
                self.push(
                    t0,
                    "tengu_sysprompt_block",
                    json!({ "length": shape.sys0_len, "hash": &shape.sys0_hash }),
                );
            }
            if shape.system_blocks > 0 {
                let missing = json!({ "promptBlockCount": shape.system_blocks });
                self.push(t0, "tengu_sysprompt_missing_boundary_marker", missing.clone());
                self.push(t0, "tengu_sysprompt_missing_boundary_marker", missing);
            }
        }
        if let Some(t) = &tether {
            let decision = json!({
                "decision": t.decision,
                "reason": t.reason,
                "sourceCategory": kind.category(),
                "threadUnsupported": false,
                "modelHeldStateless": model_held,
                "relayHeldStateless": false,
                "classifierHeldStateless": classifier_held,
                "dropHeldStateless": false,
                "evictedOther": false,
                "changedModel": t.changed_model,
                "changedSystem": false,
                "changedTools": t.changed_tools,
                "changedBetas": t.changed_betas,
                "changedLatchedHeaders": t.changed_latched,
                "changedThinking": t.changed_thinking,
                "changedToolChoice": false,
                "changedEffort": t.changed_effort,
                "changedExtraBody": false,
                "turnsInThread": t.turns,
                "messageCount": shape.messages_len,
                "prevMessageCount": t.prev_messages,
                "firstChangedIndex": -1,
                "deltaMessageCount": t.delta,
                "claimedCompact": false,
                "claimedSnip": false,
                "claimedToolResultClear": false,
                "claimedRewind": false,
                "claimedClear": false,
                "claimedAbortStrip": false,
                "unclaimedHistoryChange": t.reason == "history_changed"
            });
            let mut decision = decision;
            if v285 {
                insert_after(
                    &mut decision,
                    "classifierHeldStateless",
                    vec![("creditRetryStateless", json!(false))],
                );
            }
            self.push_dd_snake(t0, "tengu_tether_decision", decision);
            // 回声审计：客户端把上游回过来的 assistant 轮次原样带回去了没有。经代理转发的
            // 历史就是客户端自己拼的那份，按「全部原样」报。
            if t.echo {
                let turns = shape.assistant_messages;
                let echo = json!({
                    "sourceCategory": kind.category(),
                    "messageCount": shape.messages_len,
                    "anchorDiverged": false,
                    "turnsReceived": turns,
                    "turnsIdentical": turns,
                    "turnsDiverged": 0,
                    "reordered": 0,
                    "turnSplit": 0,
                    "turnDropped": 0,
                    "addedToolUseGettask": 0,
                    "addedToolUsePoll": 0,
                    "addedToolUseOther": 0,
                    "addedOther": 0,
                    "droppedToolUse": 0,
                    "droppedThinking": 0,
                    "droppedText": 0,
                    "droppedOther": 0,
                    "toolNameChanged": 0,
                    "callerDropped": 0
                });
                self.push_dd_snake(t0, "tengu_tether_echo_audit", echo);
            }
        }
        self.push(t0, "tengu_api_cache_breakpoints", breakpoints.clone());
        let mut query = Map::new();
        query.insert("model".into(), json!(&display_model));
        query.insert("messagesLength".into(), json!(shape.messages_len));
        query.insert("temperature".into(), json!(shape.temperature));
        query.insert("provider".into(), json!("firstParty"));
        query.insert("buildAgeMins".into(), json!(build_age_mins));
        query.insert("betas".into(), json!(&betas_full));
        query.insert("permissionMode".into(), json!(shape.permission_mode));
        query.insert("querySource".into(), json!(query_source));
        env.chain_fields(&mut query);
        query.insert("thinkingType".into(), json!(&shape.thinking_type));
        if let Some(e) = &effort {
            query.insert("effortValue".into(), json!(e));
        }
        query.insert("fastMode".into(), json!(shape.fast_mode));
        if let Some(prev) = &previous_request_id {
            query.insert("previousRequestId".into(), json!(prev));
        }
        self.push(t0, "tengu_api_query", Value::Object(query));
        self.push(ms(t0, 1), "tengu_api_cache_breakpoints", breakpoints);
        // 2.1.291：会话首条主线程请求发出后约 40 ~ 120ms 头一回写会话记录（交互式与 `-p` 都有，
        // `cap/auto-2.1.291-20261006-full` 每个会话恰好一条，`/clear` 之后的新会话再一条）。
        if v291 && first_main {
            self.push_dd(ms(t0, 80), "tengu_feature_ok", feature("session_transcript_write"));
        }
    }

    /// 首字节、首段正文与模型切换。
    pub(super) fn emit_first_token(&mut self) {
        let env = self.env;
        let (call, shape) = (env.call, env.shape);
        let TurnFacts {
            ref display_model,
            failed,
            aborted,
            ref turn_origin,
            model_changed,
            ref previous_message_id,
            ref chain_id,
            query_depth,
            turn_started,
            first_text_in_turn,
            first_text_interrupted,
            tool_calls_before,
            v277,
            first_main,
            ref betas_full,
            ..
        } = *env.f;
        let EventEnv { t_first, t_end, .. } = *env;
        let query_source = env.query_source.as_str();
        // 首字节到达。失败那条没有这一条：官方的 `tengu_feature_ok{api_request}` 是请求
        // **成功返回**之后才打的，失败走的是下面的 `tengu_feature_bad{api_request}`。
        if !failed && (!aborted || call.ttft_ms.is_some()) {
            self.push_dd(t_first, "tengu_feature_ok", feature("api_request"));
            // 会话首条主线程请求的首字节处，客户端头一回用上这几项能力各报一次，与 beta 一一
            // 对应（`cap/2.1.285/00040` 06:34:19.500，紧跟 `api_request`；`cap/2.1.280` 两个会话
            // 各一组）。
            if first_main {
                for (beta, name) in [
                    (config::CC_BETA_PER_TURN_CONTROL, "api_per_turn_effort"),
                    ("mid-conversation-tool-changes-", "mcp_late_tool_additions"),
                    ("mid-conversation-system-clear-at-", "api_kept_reminder_clear_at"),
                ] {
                    let prefix = beta.trim_end_matches(|c: char| c.is_ascii_digit() || c == '-');
                    if betas_full.split(',').any(|b| b.trim().starts_with(prefix)) {
                        self.push_dd(t_first, "tengu_feature_ok", feature(name));
                    }
                }
            }
        }
        if first_text_interrupted {
            let mut first_text = json!({
                "first_text_wait_end": "interrupted",
                "user_wait_before_first_text_ms": 0,
                "user_waits_before_first_text": 0,
                "queryChainId": &chain_id,
                "terminal_reason": "aborted_streaming",
                "prompt_submit_to_send_ms": 22,
                "prompt_queued_ms": 0
            });
            if v277
                && turn_origin != "human"
                && let Some(o) = first_text.as_object_mut()
            {
                o.shift_remove("prompt_submit_to_send_ms");
                o.shift_remove("prompt_queued_ms");
            }
            self.push(ms(t_end, 5), "tengu_turn_first_text", first_text);
        }
        if first_text_in_turn {
            let t_paint = ms(t_first, 30);
            let mut first_text = json!({
                    "first_text_wait_end": "painted",
                    "ttfvt_first_text_paint_ms": (t_paint - turn_started).num_milliseconds().max(0),
                    "first_text_path": if query_depth == 0 { "direct" } else { "after_tool_use" },
                    "requests_before_first_text": query_depth + 1,
                    "tool_calls_before_first_text": tool_calls_before,
                    "first_text_assistant_message_id": call.message_id.as_deref().unwrap_or(""),
                    "first_text_request_id": call.request_id.as_deref().unwrap_or(""),
                    "first_text_render_path": "block_complete",
                    "user_wait_before_first_text_ms": 0,
                    "user_waits_before_first_text": 0,
                    "queryChainId": &chain_id,
                    "prompt_submit_to_send_ms": 24,
                    "prompt_queued_ms": 0
            });
            // 不是用户敲的那轮（后台任务通知）没有「提交到发出」这两项（`cap/2.1.280`、
            // `cap/2.1.285` 的 task-notification 轮各一条）。
            if v277
                && turn_origin != "human"
                && let Some(o) = first_text.as_object_mut()
            {
                o.shift_remove("prompt_submit_to_send_ms");
                o.shift_remove("prompt_queued_ms");
            }
            self.push(t_paint, "tengu_turn_first_text", first_text);
        }

        // 换模型后缓存全 miss，客户端会收到上游的缓存诊断并上报。
        if model_changed && let Some(rid) = &call.request_id {
            self.push(
                t_end,
                "tengu_prompt_cache_diagnosis_received",
                json!({
                    "diagnosisType": "model_changed",
                    "tokensMissed": call.cache_creation_tokens,
                    "requestId": rid,
                    "previousMessageId": previous_message_id.as_deref().unwrap_or(""),
                    "model": &display_model,
                    "isCowork": false,
                    "is1hCacheTTL": shape.cache_ttl_1h,
                    "querySource": query_source,
                    "queryDepth": query_depth
                }),
            );
        }
    }
}
