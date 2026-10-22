//! 分叉请求（猜下一句、侧问、压缩、离开回顾、代理摘要）与子代理的收尾。

use super::*;
use crate::telemetry::files::MAIN_READER;

impl EventBuilder<'_> {
    /// 猜下一句：被新输入顶掉的，与跑完的。分叉里调了工具的（一律被拒，客户端带着拒绝结果接着发）是几条
    /// 请求，收尾在最后一条（见 [`SuggestionFork`]）。
    pub(super) fn emit_suggestion(&mut self) {
        let env = self.env;
        let (call, shape) = (env.call, env.shape);
        let TurnFacts {
            kind,
            failed,
            aborted,
            turn_over,
            prev_main_depth,
            ref main_chain,
            ref chain_id,
            query_depth,
            ref suggestion_fork,
            safety_events,
            ref display_model,
            v277,
            v280,
            v285,
            v296,
            ..
        } = *env.f;
        let EventEnv { t0, total, t_end, fork_messages, .. } = *env;
        if kind != Kind::Suggestion {
            return;
        }
        // 分叉里接着发的那条：客户端先把拒绝结果与这一步算出的附件攒进分叉再发（`cap/auto-2.1.307-20261022-full`
        // A19 .431：只有 `total_tokens_reminder`，没有主线程续轮前后那两条 query 计数）。附件计算耗时那条官方
        // 每个附件只抽 5%（`su`），不补。
        if shape.suggestion_step > 0 {
            let mut attachments = json!({ "attachment_types": ["total_tokens_reminder"] });
            if v285 {
                let query_source = env.query_source.as_str();
                fill_attachment_estimates(&mut attachments, query_source, shape.sdk, display_model);
            }
            self.push(ms(t0, -4), "tengu_attachments", attachments);
        }
        // 整个分叉的时长从头一条发出算；前面几条的用量与消息数，加上接着发时攒进去的拒绝结果与附件。
        let span = suggestion_fork.as_ref().map_or(total, |f| {
            (t_end - DateTime::<Utc>::from(f.started)).num_milliseconds().max(total)
        });
        let (mut usage, mut messages) = suggestion_fork
            .as_ref()
            .map_or(([0; 4], 0), |f| (f.usage, f.messages + suggestion_resumed_messages(shape)));
        let hit_rate = |u: &[i64; 4]| {
            let total_in = u[0] + u[2] + u[3];
            if total_in > 0 { json!(u[2] as f64 / total_in as f64) } else { json!(0) }
        };
        // 猜下一句：自己就是一轮（`cap/2.1.260-2` 09:43:21），收尾多一条 fork 统计，没有 tips。
        // 失败的那条只有上面的错误串（fork 统计报的是这一发的用量，没有用量就没有它）。
        // 猜下一句被新输入顶掉（`07:52:58.552`）：fork 统计（这一发的用量全 0，多一条打断的消息）、
        // `suppressed/aborted`、`turn_end{aborted_streaming}`，没有 stop hook。
        if aborted {
            let fork = json!({
                "forkLabel": "prompt_suggestion",
                "querySource": "prompt_suggestion",
                "durationMs": span,
                "messageCount": messages + 1,
                "relayEligible": true,
                "inputTokens": usage[0],
                "outputTokens": usage[1],
                "cacheReadInputTokens": usage[2],
                "cacheCreationInputTokens": usage[3],
                "serviceTier": "standard",
                "cacheCreationEphemeral1hTokens": 0,
                "cacheCreationEphemeral5mTokens": 0,
                "cacheHitRate": hit_rate(&usage),
                "queryChainId": &main_chain,
                "queryDepth": prev_main_depth
            });
            self.push(t_end, "tengu_fork_agent_query", fork);
            self.push(
                t_end,
                "tengu_prompt_suggestion",
                json!({ "source": "cli", "outcome": "suppressed", "reason": "aborted", "prompt_id": "user_intent" }),
            );
            self.push(t_end, "tengu_turn_end", env.turn_end("aborted_streaming", span, None));
        }
        if failed || aborted {
            return;
        }
        // 回复里调了工具：分叉自己的 canUseTool 一律拒（官方 `M`：deny「No tools needed for suggestion」），每个
        // 工具一条 `can_use_tool_rejected`（`deniedBy` 与 `decisionReasonType` 都是 `other`，深度与 request-id
        // 是这条自己的），2.1.307 起紧跟一条配置拒绝的 `tengu_safety_event`；流式执行，都在这条的 api_success 之前
        // （A19：.427 两条、.428 成功）。客户端接着发，收尾那串等最后一条。
        if !turn_over {
            let t_rej = ms(t_end, -1);
            let message_id = call.message_id.as_deref().unwrap_or_default();
            for c in &call.tool_calls {
                let mut can_use = json!({
                    "messageID": message_id,
                    "toolName": &c.name,
                    "deniedBy": "other",
                    "decisionReasonType": "other"
                });
                if v296 {
                    can_use["preToolHookDurationMs"] = json!(1);
                    can_use["permissionDurationMs"] = json!(1);
                }
                can_use["queryChainId"] = json!(chain_id);
                can_use["queryDepth"] = json!(query_depth);
                if let Some(r) = &call.request_id {
                    can_use["requestId"] = json!(r);
                }
                self.push(t_rej, "tengu_tool_use_can_use_tool_rejected", can_use);
                if safety_events {
                    let safety =
                        safety_event(Denial::Config, &c.name, &c.id, shape.permission_mode);
                    self.push(t_rej, "tengu_safety_event", safety);
                }
            }
            return;
        }
        for (t, v) in usage.iter_mut().zip([
            call.input_tokens,
            call.output_tokens,
            call.cache_read_tokens,
            call.cache_creation_tokens,
        ]) {
            *t += v;
        }
        messages += fork_messages;
        let t1 = ms(t_end, 1);
        let mut fork = json!({
            "forkLabel": "prompt_suggestion",
            "querySource": "prompt_suggestion",
            "durationMs": span + 7,
            "messageCount": messages
        });
        // 2.1.277 起多一项 `relayEligible`（猜下一句恒 true，子代理摘要恒 false）。
        if v277 {
            fork["relayEligible"] = json!(true);
        }
        let rest = json!({
            "inputTokens": usage[0],
            "outputTokens": usage[1],
            "cacheReadInputTokens": usage[2],
            "cacheCreationInputTokens": usage[3],
            "serviceTier": "standard",
            "cacheCreationEphemeral1hTokens": 0,
            "cacheCreationEphemeral5mTokens": 0,
            "cacheHitRate": hit_rate(&usage),
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
            self.push(t1, "tengu_turn_end", env.turn_end("completed", span + 7, None));
            self.push(t1, "tengu_fork_agent_query", fork);
            let name = "prompt_suggestion_generate";
            self.push_dd(t1, "tengu_feature_ok", feature(name));
            // 回来的正文是空的：没有建议可出（`07:50:09.353`，紧跟 `prompt_suggestion_generate`）。看整个分叉：前面
            // 调工具那条回复里带的正文也是候选（官方 `q()` 挑头一个非空的）。
            let suggested = call.text_chars > 0
                || suggestion_fork.as_ref().is_some_and(|f| f.candidate_chars > 0);
            if v285 && !suggested {
                self.push(
                    t1,
                    "tengu_prompt_suggestion",
                    json!({ "source": "cli", "outcome": "suppressed", "reason": "empty", "prompt_id": "user_intent" }),
                );
            }
        } else {
            self.push(t1, "tengu_fork_agent_query", fork);
            self.push(t1, "tengu_turn_end", env.turn_end("completed", span + 7, None));
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
        // 心跳：敲 `/compact`（提交前一秒上下）、提交即转入 `compacting`（`cap/auto-2.1.305-20261021-full` A18：
        // .459 / .446）。这两下不看压缩成没成——官方开始时就切进 `compacting`，压缩收尾（`finally`）才切出来；
        // 失败或被取消就在收尾后回到闲着，只有成功才在下面报 `compaction` 并清场。敲字那条挂上一轮 prompt
        // （A18：.459 的 `user_input` 是上一轮的 id，同秒的 `compacting` 才是这一轮的）。
        if kind == Kind::Compact {
            let t0 = env.t0;
            self.beat_before_prompt(ms(t0, -1_000), heartbeat::Trigger::UserInput);
            self.beat(ms(t0, -6), heartbeat::Trigger::Run(Some(heartbeat::COMPACTING)));
            if failed || aborted {
                self.beat(ms(t_end, 2), heartbeat::Trigger::Run(None));
            }
        }
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
                let restored = self.emit_compact_restore(t2);
                let t3 = ms(t2, 23);
                // 心跳：压缩成功，收尾与逐出提示同一毫秒报 `compaction` 并清场（A18：.382 / .382）。
                self.beat(t3, heartbeat::Trigger::Reset("compaction"));
                // 压缩收尾同一刻依次是 `compact_reactive`、`reactive_compact_succeeded`、逐出提示与 `compaction`
                // 心跳；命令那三条晚 2ms（`cap/auto-2.1.305-20261021-full` A0：.382 / .384）。
                self.push_dd(t3, "tengu_feature_ok", feature("compact_reactive"));
                self.emit_compact_succeeded(t3, restored);
                self.files.compacted(MAIN_READER);
                self.push(
                    t3,
                    "tengu_cache_eviction_hint",
                    json!({
                        "scope": "compaction",
                        "last_request_id": prev_main_request_id.as_deref().unwrap_or("")
                    }),
                );
                let t4 = ms(t3, 2);
                for name in ["cmd_compact", "cmd_dispatch"] {
                    self.push_dd(t4, "tengu_feature_ok", feature(name));
                }
                self.push(
                    t4,
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
                if env.f.v300 {
                    auto_mode_decision_v300(&mut decision);
                }
                if env.f.v307 {
                    auto_mode_decision_v307(&mut decision);
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
            if env.f.v303 {
                insert_after(&mut ok, "messageID", vec![("toolUseID", json!(&tu.id))]);
            }
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
            // 2.1.300：子代理一结束，它的进度摘要器跟着停，报一条 `tengu_agent_summary_stopped`，与收尾
            // 同一刻、排在 `lively_waffle` 前（`cap/auto-2.1.300-20261015-full` 六条：Explore / 通用 / Plan
            // 都有）。摘要器从子代理起跑就计时，时长比 `turn_end` 多 6~14ms、比 `agent_tool_completed` 少
            // 22~35ms。`calls` / `calls_failed` / `lines_accepted` 按这条支线上实际经过的摘要请求统计（每
            // 30 秒才可能分叉一次；抓包里的子代理都不到 30 秒，三项恒 0，大于 0 的情形没有样本）。
            // `config_flag` 按子代理拉起时会话主模型的族挑（可执行文件 `nd`：fable / opus 各一个，其余一个；
            // 摘要器启动时读当前主模型，中途 `/model` 换过的以那时为准）。
            // `config_source`、等待与（2.1.303 起的）模式、输入上限取这个号 eval 里那个开关
            // （[`crate::proxy::agent_summary_config`]）；抓包那个号三个开关都没下发，六条都是 `fallback`、
            // `[30]` 秒、`full`、2000。
            let summary_ms = elapsed + 10;
            if env.f.v300 {
                let session_family = model_family(&done.session_model);
                let agent_family = model_family(display_model);
                // 收尾报的是摘要器**最后一次读到**的配置，不是此刻号的最新 eval：它启动时读一次，之后每次
                // 计时器触发都重读；触发时刻按官方的排法从最后一条摘要结束（没发过就是起跑）往后推，摘要
                // 请求的耗时也算进去（[`crate::proxy::agent_summary_config::last_read_at`]）。
                let spawn = done.started.unwrap_or(call.started_at);
                let stop = spawn + std::time::Duration::from_millis(summary_ms.max(0) as u64);
                let read_at = crate::proxy::agent_summary_config::last_read_at(
                    call.cred_id,
                    session_family,
                    done.summary_last_end.unwrap_or(spawn),
                    done.summary_last_sent.unwrap_or(spawn),
                    done.summaries,
                    stop,
                );
                let cfg = crate::proxy::agent_summary_config::config_at(
                    call.cred_id,
                    session_family,
                    read_at,
                );
                let calls = done.summaries;
                let mut stopped = json!({
                    "config_flag": crate::proxy::agent_summary_config::flag_for_family(session_family),
                    "config_source": &cfg.source,
                    "session_model_family": session_family,
                    "agent_model_family": agent_family,
                    "agent_model": &display_model,
                    "is_sdk_host": false,
                    "is_resume": false,
                    "duration_ms": summary_ms,
                    "calls": calls,
                    "calls_failed": done.summaries_failed,
                    "lines_accepted": done.summaries_accepted,
                    "first_wait_ms": cfg.wait_ms(0),
                    "next_wait_ms": cfg.wait_ms(calls)
                });
                // 2.1.303 多六项（`cap/auto-2.1.303-20261019-full` 六条：`full` / `full` / `false` / 2000 /
                // `false` / 0）。官方（`z` / `te` / `he`）：计时器每次触发先 `J = te()`（配置是 short 且子代理
                // 是 opus / fable 即 short，否则 full），**再**看到没到期——没到期、内容没变而不发也照样改了
                // `J`；收尾 `mode` 报 `J`（一次没触发就现算 `te()`），`config_mode` 报那时的配置，`mode_changed`
                // 看发过的摘要里有没有不同于 `mode` 的，`calls_kept_on_full` 是逐条按发出时配置累计的（见
                // [`AgentState::summaries_kept_on_full`]）。所以：最后一次读配置正是最后一条摘要发出那一刻时，
                // `mode` 就是那条的模式，且那条是 short 即证明当时配置是 short（eval 没见过或没更新也按
                // short）；之后的触发若没赶上新的 eval，读到的还是同一份配置，同样按那条算；赶上了新 eval
                // 的，按那次读到的配置现算。子代理模型中途被换（`agent_model_swapped`）代理看不到，报 false。
                if env.f.v303 {
                    let full_calls = calls - done.summaries_short;
                    let same_as_last_send = done.summary_last_sent.is_some_and(|sent| {
                        !crate::proxy::agent_summary_config::changed_between(
                            call.cred_id,
                            sent,
                            read_at,
                        )
                    });
                    let last_short = done.summary_last_short.filter(|_| same_as_last_send);
                    let config_short = cfg.short || last_short == Some(true);
                    let mode_short = last_short
                        .unwrap_or(config_short && matches!(agent_family, "opus" | "fable"));
                    let mode_changed =
                        if mode_short { full_calls > 0 } else { done.summaries_short > 0 };
                    let mode_name = |short: bool| if short { "short" } else { "full" };
                    insert_before(
                        &mut stopped,
                        "config_flag",
                        vec![
                            ("mode", json!(mode_name(mode_short))),
                            ("config_mode", json!(mode_name(config_short))),
                            ("mode_changed", json!(mode_changed)),
                            ("max_input_tokens", json!(cfg.max_input_tokens)),
                        ],
                    );
                    insert_after(
                        &mut stopped,
                        "agent_model",
                        vec![("agent_model_swapped", json!(false))],
                    );
                    insert_after(
                        &mut stopped,
                        "calls",
                        vec![("calls_kept_on_full", json!(done.summaries_kept_on_full))],
                    );
                    // 2.1.307 在 `agent_model` 与 `agent_model_swapped` 之间多 writer 三项
                    // （`cap/auto-2.1.307-20261022-full` 六条都没配：`writer_model` 同 `agent_model`、
                    // `writer_failed` false、`config_writer` 不出现）。官方 `he` / `fe`：writer 按子代理的族取
                    // （只有 fable / opus），收尾是 short、配了、换过的没失败过、且等于客户端的小快模型才算换了；
                    // `config_writer` 报配的那项（官方 `wt`：认不出的名字报 `nonconforming`）。客户端的小快模型
                    // 可以被 `ANTHROPIC_SMALL_FAST_MODEL` / `ANTHROPIC_DEFAULT_HAIKU_MODEL` 改掉，代理看不到：short
                    // 摘要换上过 writer 的，小快模型就是它（[`AgentState::small_fast`]）；没换上过的按默认的小快模型
                    // 猜，除非它在配着时没换上（[`AgentState::not_small_fast`]）。
                    if env.f.v307 {
                        let config_writer = cfg.writer_for(agent_family);
                        let is_small_fast = |w: &str| match done.small_fast.as_deref() {
                            Some(s) => w == s,
                            None => {
                                w == crate::config::CC_SMALL_FAST_MODEL
                                    && !done.not_small_fast.iter().any(|x| x == w)
                            }
                        };
                        let writer = config_writer.filter(|w| {
                            mode_short && !done.summary_writer_failed && is_small_fast(w)
                        });
                        let writer_model = writer.unwrap_or(display_model.as_str());
                        let mut keys =
                            vec![("writer_model", json!(telemetry_model_name(writer_model)))];
                        if let Some(w) = config_writer {
                            keys.push(("config_writer", json!(telemetry_model_name(w))));
                        }
                        keys.push(("writer_failed", json!(done.summary_writer_failed)));
                        insert_after(&mut stopped, "agent_model", keys);
                    }
                }
                self.push(t2, "tengu_agent_summary_stopped", stopped);
            }
            if handback.is_some() {
                self.push_dd(t2, "tengu_feature_ok", feature("agent_handback_pointer_notice"));
            }
            if !v285 || handback.is_some() {
                let lively = feature_with("lively_waffle", json!({ "flagged": false }), env.f.v293);
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
                    // 2.1.300 起中间多停一个摘要器，收尾比 `turn_end` 晚得多一些（同上，22~35ms 落在它之后）。
                    "duration_ms": if env.f.v300 { summary_ms + 28 } else { elapsed + 16 },
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

/// 遥测里报模型名的写法（官方 `wt`）：合 `^[A-Za-z0-9._:[\]-]{1,100}$`，或是 `名字@八位日期` 后面可带
/// `[1m]` 这类上下文后缀（名字 1~91 个同样的字符），就原样报，否则报 `nonconforming`。
fn telemetry_model_name(name: &str) -> &str {
    let plain = |s: &str, max: usize| {
        (1..=max).contains(&s.len())
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._:[]-".contains(&b))
    };
    let dated = || {
        let Some((head, tail)) = name.split_once('@') else { return false };
        // 先认八位数字，再切：前八个字节都是 ASCII 数字，第八字节处必是字符边界。
        if tail.len() < 8 || !tail.as_bytes()[..8].iter().all(u8::is_ascii_digit) {
            return false;
        }
        let rest = &tail[8..];
        let window = |r: &str| {
            let Some(inner) = r.strip_prefix('[').and_then(|r| r.strip_suffix(']')) else {
                return false;
            };
            let digits = inner.trim_end_matches(['m', 'M']);
            inner.len() == digits.len() + 1
                && (1..=3).contains(&digits.len())
                && digits.bytes().all(|b| b.is_ascii_digit())
        };
        plain(head, 91) && (rest.is_empty() || window(rest))
    };
    if plain(name, 100) || dated() { name } else { "nonconforming" }
}

#[cfg(test)]
mod tests {
    use super::telemetry_model_name as name;

    /// 官方 `wt`：合规的模型名原样报（含 `@日期` 与 `[1m]` 后缀那种），其余报 `nonconforming`。
    #[test]
    fn model_names_follow_the_official_patterns() {
        for ok in [
            "claude-haiku-5-5",
            "claude-opus-5-5[1m]",
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "claude-sonnet-4@20250514",
            "claude-sonnet-4@20250514[1m]",
        ] {
            assert_eq!(name(ok), ok);
        }
        for bad in ["", "claude haiku", "模型", &"x".repeat(101), "a@2025051", "a@20250514[1x]"] {
            assert_eq!(name(bad), "nonconforming", "{bad}");
        }
    }
}
