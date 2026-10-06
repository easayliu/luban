//! 分叉请求（猜下一句、侧问、压缩、离开回顾、代理摘要）与子代理的收尾。

use super::*;

impl EventBuilder<'_> {
    /// 猜下一句：被新输入顶掉的，与跑完的。
    pub(super) fn emit_suggestion(&mut self) {
        let env = self.env;
        let call = env.call;
        let TurnFacts {
            kind,
            failed,
            aborted,
            prev_main_depth,
            ref main_chain,
            v277,
            v280,
            v285,
            ..
        } = *env.f;
        let EventEnv { total, t_end, fork_messages, .. } = *env;
        // 猜下一句：自己就是一轮（`cap/2.1.260-2` 09:43:21），收尾多一条 fork 统计，没有 tips。
        // 失败的那条只有上面的错误串（fork 统计报的是这一发的用量，没有用量就没有它）。
        // 猜下一句被新输入顶掉（`07:52:58.552`）：fork 统计（用量全 0）、`suppressed/aborted`、
        // `turn_end{aborted_streaming}`，没有 stop hook。
        if kind == Kind::Suggestion && aborted {
            let fork = json!({
                "forkLabel": "prompt_suggestion",
                "querySource": "prompt_suggestion",
                "durationMs": total,
                "messageCount": 1,
                "relayEligible": true,
                "inputTokens": 0,
                "outputTokens": 0,
                "cacheReadInputTokens": 0,
                "cacheCreationInputTokens": 0,
                "serviceTier": "standard",
                "cacheCreationEphemeral1hTokens": 0,
                "cacheCreationEphemeral5mTokens": 0,
                "cacheHitRate": 0,
                "queryChainId": &main_chain,
                "queryDepth": prev_main_depth
            });
            self.push(t_end, "tengu_fork_agent_query", fork);
            self.push(
                t_end,
                "tengu_prompt_suggestion",
                json!({ "source": "cli", "outcome": "suppressed", "reason": "aborted", "prompt_id": "user_intent" }),
            );
            self.push(t_end, "tengu_turn_end", env.turn_end("aborted_streaming", total, None));
        }
        if kind == Kind::Suggestion && !failed && !aborted {
            let t1 = ms(t_end, 1);
            let total_in = call.input_tokens + call.cache_read_tokens + call.cache_creation_tokens;
            let hit_rate =
                if total_in > 0 { call.cache_read_tokens as f64 / total_in as f64 } else { 0.0 };
            let mut fork = json!({
                "forkLabel": "prompt_suggestion",
                "querySource": "prompt_suggestion",
                "durationMs": total + 7,
                "messageCount": fork_messages
            });
            // 2.1.277 起多一项 `relayEligible`（猜下一句恒 true，子代理摘要恒 false）。
            if v277 {
                fork["relayEligible"] = json!(true);
            }
            let rest = json!({
                "inputTokens": call.input_tokens,
                "outputTokens": call.output_tokens,
                "cacheReadInputTokens": call.cache_read_tokens,
                "cacheCreationInputTokens": call.cache_creation_tokens,
                "serviceTier": "standard",
                "cacheCreationEphemeral1hTokens": 0,
                "cacheCreationEphemeral5mTokens": 0,
                "cacheHitRate": hit_rate,
                "queryChainId": &main_chain,
                "queryDepth": prev_main_depth
            });
            if let (Some(o), Some(r)) = (fork.as_object_mut(), rest.as_object()) {
                o.extend(r.clone());
            }
            // 2.1.280 的顺序是 stop hook、turn、turn_end、fork 统计，最后才是
            // `prompt_suggestion_generate`（`cap/2.1.280` 三条逐条如此）；之前的版本三个
            // feature 连着报在前头。
            let names: &[&str] = if v280 {
                &["hook_stop_handler", "turn"]
            } else {
                &["hook_stop_handler", "prompt_suggestion_generate", "turn"]
            };
            for name in names {
                self.push_dd(t1, "tengu_feature_ok", feature(name));
            }
            if v280 {
                self.push(t1, "tengu_turn_end", env.turn_end("completed", total + 7, None));
                self.push(t1, "tengu_fork_agent_query", fork);
                let name = "prompt_suggestion_generate";
                self.push_dd(t1, "tengu_feature_ok", feature(name));
                // 回来的正文是空的：没有建议可出（`07:50:09.353`，紧跟 `prompt_suggestion_generate`）。
                if v285 && call.text_chars == 0 {
                    self.push(
                        t1,
                        "tengu_prompt_suggestion",
                        json!({ "source": "cli", "outcome": "suppressed", "reason": "empty", "prompt_id": "user_intent" }),
                    );
                }
            } else {
                self.push(t1, "tengu_fork_agent_query", fork);
                self.push(t1, "tengu_turn_end", env.turn_end("completed", total + 7, None));
            }
        }
    }

    /// 侧问与压缩的收尾。
    pub(super) fn emit_side_question_or_compact(&mut self) {
        let env = self.env;
        let (call, shape) = (env.call, env.shape);
        let TurnFacts {
            kind,
            ref display_model,
            failed,
            aborted,
            ref prev_main_request_id,
            ref previous_message_id,
            query_depth,
            ..
        } = *env.f;
        let EventEnv { total, t_end, fork_messages, .. } = *env;
        let query_source = env.query_source.as_str();
        // `/btw` 插问与 `/compact`：自己算一轮辅助调用。插问是 stop hook、缓存诊断
        // （`unavailable`，分叉请求上游不给诊断）、turn、fork 统计（没有链字段）、turn_end
        // （`cap/auto-2.1.285-20260930` 08:00:19.400–.401）；压缩是 stop hook、turn、turn_end、
        // fork 统计（`reactive-compact`，链另起、深度 -1），再是缓存逐出提示与压缩命令收尾
        // （07:57:42.737–.756）。
        if matches!(kind, Kind::SideQuestion | Kind::Compact) && !failed && !aborted {
            let t1 = ms(t_end, 1);
            let total_in = call.input_tokens + call.cache_read_tokens + call.cache_creation_tokens;
            let hit_rate =
                if total_in > 0 { call.cache_read_tokens as f64 / total_in as f64 } else { 0.0 };
            let compact = kind == Kind::Compact;
            let mut fork = json!({
                "forkLabel": if compact { "reactive-compact" } else { "side_question" },
                "querySource": query_source,
                "durationMs": total + if compact { 10 } else { 37 },
                "messageCount": fork_messages,
                "relayEligible": true,
                "inputTokens": call.input_tokens,
                "outputTokens": call.output_tokens,
                "cacheReadInputTokens": call.cache_read_tokens,
                "cacheCreationInputTokens": call.cache_creation_tokens,
                "serviceTier": "standard",
                "cacheCreationEphemeral1hTokens": 0,
                "cacheCreationEphemeral5mTokens": 0,
                "cacheHitRate": hit_rate
            });
            if compact {
                fork["queryChainId"] = json!(uuid_v4());
                fork["queryDepth"] = json!(-1);
            }
            self.push_dd(t1, "tengu_feature_ok", feature("hook_stop_handler"));
            if !compact && let Some(rid) = &call.request_id {
                self.push(
                    t_end,
                    "tengu_prompt_cache_diagnosis_received",
                    json!({
                        "diagnosisType": "unavailable",
                        "tokensMissed": -1,
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
            let t2 = ms(t1, 1);
            self.push_dd(t2, "tengu_feature_ok", feature("turn"));
            let elapsed = total + if compact { 9 } else { 36 };
            if compact {
                self.push(t2, "tengu_turn_end", env.turn_end("completed", elapsed, None));
                self.push(ms(t2, 1), "tengu_fork_agent_query", fork);
                let t3 = ms(t2, 17);
                self.push(
                    t3,
                    "tengu_cache_eviction_hint",
                    json!({
                        "scope": "compaction",
                        "last_request_id": prev_main_request_id.as_deref().unwrap_or("")
                    }),
                );
                for name in ["compact_reactive", "cmd_compact", "cmd_dispatch"] {
                    self.push_dd(t3, "tengu_feature_ok", feature(name));
                }
                self.push(
                    t3,
                    "tengu_input_command",
                    json!({ "input": "compact", "invocation_trigger": "user-slash" }),
                );
            } else {
                self.push(t2, "tengu_fork_agent_query", fork);
                self.push(t2, "tengu_turn_end", env.turn_end("completed", elapsed, None));
            }
        }
    }

    /// 离开回顾的收尾。
    pub(super) fn emit_away_summary(&mut self) {
        let env = self.env;
        let call = env.call;
        let TurnFacts { kind, failed, aborted, ref main_chain, .. } = *env.f;
        let EventEnv { total, t_end, fork_messages, .. } = *env;
        // 离开回顾：同猜下一句，自己算一轮辅助调用；fork 统计挂回主线程那条链、深度 0，
        // `relayEligible: true`，最后是 `away_summary_generate`（`cap/2.1.285/00099`、`00160`，
        // `cap/2.1.280/00048`、`00088` 同序）。
        if kind == Kind::AwaySummary && !failed && !aborted {
            let t1 = ms(t_end, 1);
            for name in ["hook_stop_handler", "turn"] {
                self.push_dd(t1, "tengu_feature_ok", feature(name));
            }
            self.push(t1, "tengu_turn_end", env.turn_end("completed", total + 6, None));
            let total_in = call.input_tokens + call.cache_read_tokens + call.cache_creation_tokens;
            let hit_rate =
                if total_in > 0 { call.cache_read_tokens as f64 / total_in as f64 } else { 0.0 };
            self.push(
                t1,
                "tengu_fork_agent_query",
                json!({
                    "forkLabel": "away_summary",
                    "querySource": "away_summary",
                    "durationMs": total + 6,
                    "messageCount": fork_messages,
                    "relayEligible": true,
                    "inputTokens": call.input_tokens,
                    "outputTokens": call.output_tokens,
                    "cacheReadInputTokens": call.cache_read_tokens,
                    "cacheCreationInputTokens": call.cache_creation_tokens,
                    "serviceTier": "standard",
                    "cacheCreationEphemeral1hTokens": 0,
                    "cacheCreationEphemeral5mTokens": 0,
                    "cacheHitRate": hit_rate,
                    "queryChainId": &main_chain,
                    "queryDepth": 0
                }),
            );
            let name = "away_summary_generate";
            self.push_dd(t1, "tengu_feature_ok", feature(name));
        }
    }

    /// 子代理摘要请求的收尾。
    pub(super) fn emit_agent_summary(&mut self) {
        let env = self.env;
        let call = env.call;
        let TurnFacts { kind, failed, aborted, ref agent, .. } = *env.f;
        let EventEnv { total, t_end, fork_messages, .. } = *env;
        // 子代理的摘要请求：同猜下一句，自己算一轮辅助调用，fork 统计挂回子代理那条链、
        // 深度恒为 1（`cap/2.1.280` 1 条、`cap/2.1.277` 5 条）。
        if kind == Kind::AgentSummary && !failed && !aborted {
            let t1 = ms(t_end, 1);
            for name in ["hook_stop_handler", "turn"] {
                self.push_dd(t1, "tengu_feature_ok", feature(name));
            }
            self.push(t1, "tengu_turn_end", env.turn_end("completed", total + 5, None));
            let total_in = call.input_tokens + call.cache_read_tokens + call.cache_creation_tokens;
            let hit_rate =
                if total_in > 0 { call.cache_read_tokens as f64 / total_in as f64 } else { 0.0 };
            let agent_chain = agent.as_ref().map(|a| a.chain_id.clone()).unwrap_or_default();
            self.push(
                t1,
                "tengu_fork_agent_query",
                json!({
                    "forkLabel": "agent_summary",
                    "querySource": "agent_summary",
                    "durationMs": total + 5,
                    "messageCount": fork_messages,
                    "relayEligible": false,
                    "inputTokens": call.input_tokens,
                    "outputTokens": call.output_tokens,
                    "cacheReadInputTokens": call.cache_read_tokens,
                    "cacheCreationInputTokens": call.cache_creation_tokens,
                    "serviceTier": "standard",
                    "cacheCreationEphemeral1hTokens": 0,
                    "cacheCreationEphemeral5mTokens": 0,
                    "cacheHitRate": hit_rate,
                    "queryChainId": agent_chain,
                    "queryDepth": 1
                }),
            );
        }
    }

    /// SubagentHandback 当场执行的工具串，与子代理跑完时的收尾。
    pub(super) fn emit_agent_steps(&mut self) {
        let env = self.env;
        let (call, shape) = (env.call, env.shape);
        let TurnFacts {
            ref display_model,
            ref chain_id,
            query_depth,
            v285,
            usage_before,
            ref handback,
            ref agent_done,
            ref session_cwd,
            ..
        } = *env.f;
        let EventEnv { total, t_end, builtin_agent, ref effort, .. } = *env;
        // 子代理跑完（它自己的 `end_turn`）：stop hook、turn、turn_end，随后是
        // `agent_tool_completed` 与支线收尾那几条（`cap/2.1.280` 10:16:10.901–.906）。
        // SubagentHandback 当场执行的那串工具事件：判决、放行、执行、成功，都挂在这条请求上
        // （它的 messageID / requestId / 深度），排在 api_success 之前。
        if let Some(hb) = &handback {
            let msg = call.message_id.as_deref().unwrap_or("");
            let rid = call.request_id.as_deref().unwrap_or("");
            let mut tu = ToolUse { id: hb.id.clone(), ..Default::default() };
            tu.apply_call(&hb.name, &hb.input, hb.verdict.as_ref());
            let th = ms(t_end, -32);
            if shape.permission_mode == "auto"
                && let Some(verdict) = tu.verdict.as_deref()
            {
                let mut decision = auto_mode_decision_meta(
                    &tu,
                    verdict,
                    None,
                    msg,
                    0,
                    usage_before,
                    tu.id.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(u32::from(b))),
                    session_cwd.as_deref(),
                );
                if env.f.v291 {
                    auto_mode_decision_v291(&mut decision);
                }
                self.push_dd_snake(th, "tengu_auto_mode_decision", decision);
            }
            let tg = ms(t_end, -3);
            self.push_dd(tg, "tengu_feature_ok", feature("permission_auto_approve_config"));
            self.push(
                tg,
                "tengu_tool_use_can_use_tool_allowed",
                json!({
                    "messageID": msg,
                    "toolName": &tu.name,
                    "queryChainId": &chain_id,
                    "queryDepth": query_depth,
                    "requestId": rid
                }),
            );
            self.push(
                tg,
                "tengu_tool_use_granted_in_config",
                json!({ "messageID": msg, "isMcp": false, "toolName": &tu.name, "sandboxEnabled": false }),
            );
            let td = ms(t_end, -2);
            self.push_dd(td, "tengu_feature_ok", feature("tool_subagent_handback"));
            let mut ok = json!({
                "messageID": msg,
                "toolName": &tu.name,
                "isMcp": false,
                "subagent_type": call.agent.agent_type.as_deref().unwrap_or("general-purpose"),
                "is_built_in_agent": builtin_agent.is_some()
            });
            if let Some(e) = &effort {
                ok["effort_level"] = json!(e);
            }
            for (k, v) in [
                ("durationMs", json!(28)),
                ("rssDeltaBytes", json!(5_308_416)),
                ("heapUsedDeltaBytes", json!(0)),
                ("externalDeltaBytes", json!(0)),
                ("preToolHookDurationMs", json!(0)),
                ("permissionDurationMs", json!(30)),
                // 回给子代理的那句固定提示（94 字，三条抓包相同）。
                ("toolResultSizeBytes", json!(94)),
                ("toolResultTokensEst", json!(15)),
                ("toolResultMediaBlocks", json!(0)),
                ("toolResultWillPersist", json!(false)),
                ("toolInputSizeBytes", json!(tu.input_len)),
                ("queryChainId", json!(&chain_id)),
                ("queryDepth", json!(query_depth)),
                ("requestId", json!(rid)),
            ] {
                ok[k] = v;
            }
            self.push_dd_snake(td, "tengu_tool_use_success", ok);
        }
        if let Some(done) = &agent_done {
            let t1 = ms(t_end, 1);
            // 以 SubagentHandback 收尾的没有 stop hook，turn 之后先报一条「工具结果结束了这一轮」。
            let names: &[&str] =
                if handback.is_some() { &["turn"] } else { &["hook_stop_handler", "turn"] };
            for name in names {
                self.push_dd(t1, "tengu_feature_ok", feature(name));
            }
            if handback.is_some() {
                self.push(
                    t1,
                    "tengu_mcp_tool_result_ended_turn",
                    json!({ "queryChainId": &chain_id, "queryDepth": query_depth, "source": "tool" }),
                );
            }
            let started: DateTime<Utc> = done.started.unwrap_or(call.started_at).into();
            let elapsed = (t1 - started).num_milliseconds().max(total);
            self.push(t1, "tengu_turn_end", env.turn_end("completed", elapsed, None));
            // 2.1.285 的收尾比 turn_end 晚 60 来毫秒、没有 `lively_waffle`（`cap/2.1.285`
            // 06:57:29.511 → .574）；2.1.280 的 `lively_waffle` 带 `flagged: false`。以 SubagentHandback
            // 收尾的紧接着报，照旧有 `lively_waffle`，另多一条交还提示（07:51:29.455）。
            let t2 = ms(t1, if v285 && handback.is_none() { 63 } else { 3 });
            if handback.is_some() {
                self.push_dd(t2, "tengu_feature_ok", feature("agent_handback_pointer_notice"));
            }
            if !v285 || handback.is_some() {
                let lively = json!({ "feature_name": "lively_waffle", "flagged": false });
                self.push_dd(t2, "tengu_feature_ok", lively);
            }
            self.push(
                t2,
                "tengu_agent_tool_completed",
                json!({
                    "agent_type": call.agent.agent_type.as_deref().unwrap_or("general-purpose"),
                    "model": &display_model,
                    "prompt_char_count": done.prompt_chars,
                    // 子代理最后那条回复的正文字数（`cap/2.1.285`：2272，正是那条 textContentLength；
                    // `cap/2.1.280` 那条以 SubagentHandback 工具收尾，正文 0）。
                    "response_char_count": call.text_chars,
                    "assistant_message_count": done.steps,
                    "total_tool_uses": done.tool_uses,
                    "duration_ms": elapsed + 16,
                    "total_tokens": done.prev_total,
                    "is_built_in_agent": builtin_agent.is_some(),
                    "is_async": true,
                    "agent_depth": 1,
                    "final_model": &display_model,
                    "model_swapped": false
                }),
            );
            self.push(
                t2,
                "tengu_cache_eviction_hint",
                json!({
                    "scope": "subagent_end",
                    "last_request_id": call.request_id.as_deref().unwrap_or("")
                }),
            );
            let t3 = ms(t2, 1);
            for name in ["melodic_wolf", "task_local_agent", "subagent_complete"] {
                self.push_dd(t3, "tengu_feature_ok", feature(name));
            }
        }
    }
}
