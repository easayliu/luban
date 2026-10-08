//! 上一条回复里的工具：权限判定、执行与结果，以及随后的攒附件。

use super::*;

impl EventBuilder<'_> {
    /// 上一条回复里的工具跑完、这条请求带回结果时的那串工具事件。
    pub(super) fn emit_tool_steps(&mut self) {
        let env = self.env;
        let (call, shape) = (env.call, env.shape);
        let TurnFacts {
            kind,
            ref display_model,
            is_main,
            new_prompt,
            ref agent,
            ref previous_request_id,
            ref prev_main_message_id,
            prev_main_depth,
            prev_end,
            ref chain_id,
            query_depth,
            v285,
            ..
        } = *env.f;
        let EventEnv { t0, .. } = *env;
        let query_source = env.query_source.as_str();
        // 续轮：上一条回复里的工具调用在两次请求之间执行，把权限判定、执行、附件计算那串
        // 补在这条请求之前（`cap/2.1.260-2` 09:43:12–09:43:13）。权限判定发生在上一条回复
        // 流到工具块时，时间戳落在上一条结束之前。
        // 子代理每一步之间也是这一串（`cap/2.1.280` Explore：Bash 的权限判定、执行、成功，
        // 再攒附件），深度与消息 id 取它自己那条支线的上一条。
        let is_sub = kind == Kind::Subagent;
        if (is_main || is_sub) && !new_prompt && !shape.tool_uses.is_empty() {
            let prev_end_dt: DateTime<Utc> =
                prev_end.unwrap_or(call.started_at - Duration::from_secs(1)).into();
            // 工具是上一条主线程回复产生的：事件里的 requestId / messageID 都指上一条。
            let prev_req = previous_request_id.clone().unwrap_or_default();
            let prev_msg = if is_sub {
                agent.as_ref().and_then(|a| a.last_message_id.clone()).unwrap_or_default()
            } else {
                prev_main_message_id.clone().unwrap_or_default()
            };
            let prev_depth = if is_sub { query_depth.saturating_sub(1) } else { prev_main_depth };
            let n = shape.tool_uses.len() as i64;
            let step = ToolStep { is_sub, prev_end_dt, prev_req, prev_msg, prev_depth, n };
            for (i, tu) in shape.tool_uses.iter().enumerate() {
                let i = i as i64;
                self.emit_tool_permission(&step, i, tu);
                self.emit_tool_result(&step, i, tu);
            }
            // 工具都跑完，攒附件再发下一条。这两条的深度仍是上一条的（三个版本的抓包都是
            // 「下一条 api_query 的深度 − 1」：主线程 0→1、子代理 2→3…）。
            let ta = ms(t0, -4);
            // 2.1.285 的子代理续轮不再报附件计算耗时（五条续轮前一条都没有）。
            let labels: &[&str] =
                if v285 && is_sub { &[] } else { &["agent_pending_messages", "memory_update"] };
            for label in labels {
                self.push(
                    ta,
                    "tengu_attachment_compute_duration",
                    json!({ "label": label, "duration_ms": 0, "attachment_size_bytes": 0, "attachment_count": 0 }),
                );
            }
            let mut attachments = json!({ "attachment_types": ["total_tokens_reminder"] });
            if v285 {
                fill_attachment_estimates(&mut attachments, query_source, shape.sdk, display_model);
            }
            let results = shape.tool_uses.len();
            // 先 `query_before_attachments`、再攒附件、再 `query_after_attachments`（`cap/2.1.277`
            // 30 次、`cap/2.1.280` 5 次、`cap/2.1.285` 6 次都是这个顺序）。
            self.push(
                ms(ta, -1),
                "tengu_query_before_attachments",
                json!({
                    "messagesForQueryCount": shape.messages_len + 3,
                    "assistantMessagesCount": shape.assistant_messages,
                    "toolResultsCount": results,
                    "queryChainId": &chain_id,
                    "queryDepth": prev_depth
                }),
            );
            self.push(ta, "tengu_attachments", attachments);
            self.push(
                ms(ta, 1),
                "tengu_query_after_attachments",
                json!({
                    "totalToolResultsCount": results + 1,
                    "fileChangeAttachmentCount": 0,
                    "queryChainId": &chain_id,
                    "queryDepth": prev_depth
                }),
            );
        }
    }

    /// 一个工具调用的权限判定：配置放行、auto 模式的分类器、或弹框让用户点头。
    pub(super) fn emit_tool_permission(&mut self, step: &ToolStep, i: i64, tu: &ToolUse) {
        let env = self.env;
        let shape = env.shape;
        let TurnFacts {
            ref identity,
            ref base_identity,
            ref chain_id,
            v285,
            usage_before,
            ref dd_model,
            ref session_cwd,
            ..
        } = *env.f;
        let EventEnv { t0, .. } = *env;
        let ToolStep { is_sub, prev_end_dt, ref prev_req, ref prev_msg, prev_depth, n, .. } = *step;
        // 权限判定。配置里放行的工具（default 模式下也是，`cap/2.1.280` 的 Agent、
        // `cap/2.1.285` 的 Agent / WebFetch / Read）报 `granted_in_config` →
        // `permission_auto_approve_config` → `can_use_tool_allowed`；键序是
        // `messageID, isMcp, toolName, sandboxEnabled`，只有 auto 模式的 Bash 多
        // `destructive_*` 与 `permission_mode`。非 auto 模式的 Skill 要用户点头：弹框
        // （`show_permission_request`，回复一结束就弹）→ 本次放行
        // （`granted_in_prompt_temporary` + `permission_user_grant`）→ `can_use_tool_allowed`
        // （`cap/2.1.285` 06:55:27.663 / 30.944 / 30.946；2.1.280 那次用户拒了）。
        // 此前只在 auto 模式下报，default 模式一条权限事件都没有。
        let tg = ms(prev_end_dt, -6 - 3 * (n - i));
        let command = tu.input.get("command").and_then(|c| c.as_str()).unwrap_or("");
        let profile = (tu.name == "Bash").then(|| bash_profile(command));
        let read_only = profile.as_ref().is_some_and(|p| bash_read_only(p, session_cwd.as_deref()));
        // auto 模式：服务端分类器给每个工具调用的判决（响应里的 `safeguard_results`），客户端在
        // 放行前报一条 `tengu_auto_mode_decision`（`cap/auto-2.1.285-20260930` 23 条）。要用户
        // 亲自回答的 AskUserQuestion 不报。
        if shape.permission_mode == "auto"
            && tu.name != "AskUserQuestion"
            && let Some(verdict) = tu.verdict.as_deref()
        {
            let td = ms(tg, -1);
            let mut decision = auto_mode_decision_meta(
                tu,
                verdict,
                profile.as_ref(),
                prev_msg,
                i as usize,
                usage_before,
                tu.id.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(u32::from(b))),
                session_cwd.as_deref(),
            );
            if env.f.v291 {
                auto_mode_decision_v291(&mut decision);
            }
            self.push_dd_snake(td, "tengu_auto_mode_decision", decision);
        }
        // default 模式下要用户点头的：Edit / Write / NotebookEdit、非只读的 Bash（`cap/auto-2.1.285-20260930`
        // 08:00:45.839 的 Edit、08:02:04.556 的 Bash），以及非 auto 模式的 Skill。只读的 Bash
        // （`ls`、`git status`……）照配置放行。
        let prompted = (tu.name == "Skill" && shape.permission_mode != "auto")
            || (shape.permission_mode == "default"
                && (matches!(tu.name.as_str(), "Edit" | "Write" | "NotebookEdit")
                    || (tu.name == "Bash" && !read_only)))
            // 要用户亲自作答的两种，任何模式都弹（`07:55:36.016` AskUserQuestion、规划模式下
            // `07:56:18.339` 的 ExitPlanMode）。
            || tu.name == "AskUserQuestion"
            || tu.name == "ExitPlanMode";
        if prompted {
            let shown = ms(prev_end_dt, -5);
            let mut show = json!({
                "messageID": &prev_msg,
                "toolName": &tu.name,
                "isMcp": false
            });
            // 复合命令逐条判过（`subcommandResults`），子代理里的弹框另标来源
            // （`07:58:16.980`、`08:02:04.556`）。
            if profile.as_ref().is_some_and(|p| p.simple_commands > 1) {
                show["decisionReasonType"] = json!("subcommandResults");
            }
            show["sandboxEnabled"] = json!(false);
            // ExitPlanMode 的工具事件跟着下一条请求补，那时已经退出规划模式了；弹框那一刻
            // 仍是 `plan`。
            let shown_mode = if tu.name == "ExitPlanMode" { "plan" } else { shape.permission_mode };
            show["permissionMode"] = json!(shown_mode);
            if is_sub {
                show["requestSource"] = json!("subagent");
            }
            show["originAgentType"] = json!(if is_sub { "subagent" } else { "main" });
            self.push(shown, "tengu_tool_use_show_permission_request", show);
            // 用户点头落在这条请求发出前（两次请求之间就是在等他）。
            let tp = ms(t0, -40).max(ms(shown, 1));
            // 点「Yes」：确认提交，Bash 另报选了第几项。Skill、AskUserQuestion、ExitPlanMode
            // 没有这一步。弹框挂在子代理上时，这两条是主线程界面上的操作，不带子代理身份
            // （`07:58:17.850`）。
            if matches!(tu.name.as_str(), "Edit" | "Write" | "NotebookEdit" | "Bash") {
                let ta = ms(tp, -3);
                let accept = json!({
                    "toolName": &tu.name,
                    "isMcp": false,
                    "has_instructions": false,
                    "instructions_length": 0,
                    "entered_feedback_mode": false
                });
                let mut ui = vec![("tengu_accept_submitted", accept)];
                if tu.name == "Bash" {
                    ui.push((
                        "tengu_permission_request_option_selected",
                        json!({ "option_index": 1 }),
                    ));
                }
                for (name, meta) in ui {
                    if is_sub {
                        self.main_side
                            .push((ta, base_identity.event(name, ta, &env.ctx(ta), meta)));
                    } else {
                        self.push(ta, name, meta);
                    }
                }
            }
            let mut grant = json!({
                "messageID": &prev_msg,
                "isMcp": false,
                "toolName": &tu.name,
                "sandboxEnabled": false,
                "waiting_for_user_permission_ms": (tp - shown).num_milliseconds()
            });
            if tu.name == "Bash" {
                grant["destructive_category"] = json!("none");
                grant["destructive_target_scope"] = json!("none");
                grant["git_destructive_target"] = json!("none");
                grant["permission_mode"] = json!(shape.permission_mode);
            }
            self.push(tp, "tengu_tool_use_granted_in_prompt_temporary", grant.clone());
            if v285 {
                self.dd.push(identity.dd_entry(
                    "tengu_tool_use_granted_in_prompt_temporary",
                    &env.ctx(tp),
                    dd_model,
                    snake_flat(&grant),
                ));
            }
            self.push_dd(tp, "tengu_feature_ok", feature("permission_user_grant"));
            self.push(
                ms(tp, 2),
                "tengu_tool_use_can_use_tool_allowed",
                json!({
                    "messageID": &prev_msg,
                    "toolName": &tu.name,
                    "queryChainId": &chain_id,
                    "queryDepth": prev_depth,
                    "requestId": &prev_req
                }),
            );
        } else {
            let mut granted = json!({
                "messageID": &prev_msg,
                "isMcp": false,
                "toolName": &tu.name,
                "sandboxEnabled": false
            });
            // Bash 放行时带破坏性判定与权限模式，default 模式下也是（08:00:42.542）。
            if tu.name == "Bash" {
                granted["destructive_category"] = json!("none");
                granted["destructive_target_scope"] = json!("none");
                granted["git_destructive_target"] = json!("none");
                granted["permission_mode"] = json!(shape.permission_mode);
            }
            self.push(tg, "tengu_tool_use_granted_in_config", granted);
            self.push_dd(ms(tg, 1), "tengu_feature_ok", feature("permission_auto_approve_config"));
            self.push(
                ms(tg, 2),
                "tengu_tool_use_can_use_tool_allowed",
                json!({
                    "messageID": &prev_msg,
                    "toolName": &tu.name,
                    "queryChainId": &chain_id,
                    "queryDepth": prev_depth,
                    "requestId": &prev_req
                }),
            );
        }
    }

    /// 一个工具调用的执行与结果：进度、完成与 `tengu_tool_use_success`。
    pub(super) fn emit_tool_result(&mut self, step: &ToolStep, i: i64, tu: &ToolUse) {
        let env = self.env;
        let (call, shape) = (env.call, env.shape);
        let TurnFacts { is_main, ref chain_id, shell_snapshot_first, v285, v291, v293, .. } =
            *env.f;
        let EventEnv { t0, builtin_agent, ref effort, ref effort_value, req_hash, .. } = *env;
        let ToolStep { is_sub, prev_end_dt, ref prev_req, ref prev_msg, prev_depth, n, .. } = *step;
        // 工具执行：按调用数把上一条结束到这条发出之间的时间均分。
        let gap = (t0 - prev_end_dt).num_milliseconds().max(50);
        // 2.1.285：工具跑完就发下一条，完成事件贴着这条请求之前（WebFetch 要先等它那条
        // 页面处理回来，`cap/2.1.285` 06:56:20.385 完成、.389 起下一条）；之前的版本按
        // 调用数均分。
        let t_done = if v285 {
            ms(t0, -8 - 4 * (n - 1 - i)).max(ms(prev_end_dt, i + 1))
        } else {
            ms(prev_end_dt, gap * (i + 1) / (n + 1))
        };
        let duration = (gap / (n + 1) - 12).max(1);
        let bash_failed = tu.name == "Bash" && tu.is_error;
        if tu.name == "Bash" {
            if shell_snapshot_first && is_main && i == 0 {
                let mut snap = feature("shell_snapshot_create");
                // 2.1.291 起多报这次快照花了多久（`cap/auto-2.1.291-20261006-full`：1237、722ms）。
                if v291 {
                    snap["duration_ms"] = json!(700 + i64::from(req_hash % 600));
                }
                self.push_dd(ms(t_done, -40), "tengu_feature_ok", snap);
            }
            let command = tu.input.get("command").and_then(|c| c.as_str()).unwrap_or("");
            let backgrounded =
                tu.input.get("run_in_background").and_then(|b| b.as_bool()) == Some(true);
            // 官方量的是命令的原始输出，末尾那个换行在 tool_result 里被去掉了（10 对 9、258 对
            // 257……）；转到后台的那种当场只有一个换行（`00146`）。
            let stdout = if backgrounded { 1 } else { tu.result_len + 1 };
            let mut bash = bash_executed_meta(
                &bash_profile(command),
                stdout,
                Some(&tu.id),
                backgrounded,
                false,
                shape.permission_mode,
            );
            if v293 {
                bash_meta_v293(&mut bash, &tu.result_head);
            }
            if bash_failed {
                // 非零退出（`cap/auto-2.1.285-20260930` 07:58:12.123）：同一份画像报
                // `command_failed`，退出码取结果开头的 `Exit code N`，没有 `was_backgrounded`；
                // 工具那头报 `feature_sad{tool_shell_error}` 与 `tool_use_error`，不报成功。
                if let Some(o) = bash.as_object_mut() {
                    o.shift_remove("was_backgrounded");
                    o.insert("exit_code".into(), json!(exit_code_of(&tu.result_head)));
                }
                self.push_dd_snake(t_done, "tengu_bash_tool_command_failed", bash);
                let mut sad =
                    json!({ "feature_name": "tool_bash", "error_code": "tool_shell_error" });
                // 2.1.293 末尾多一项实验标记（`tengu_started` 也多了同一项）。
                if v293 {
                    sad["tengu_quizzical_giraffe"] = json!("unset");
                }
                self.push_dd(t_done, "tengu_feature_sad", sad);
            } else {
                self.push_dd_snake(t_done, "tengu_bash_tool_command_executed", bash);
                self.push_dd(t_done, "tengu_feature_ok", feature("tool_bash"));
            }
        }
        // 每个工具跑完都有一条 `tool_<工具名>`，紧挨在 `tool_use_success` 前（`cap/2.1.277`
        // `tool_read` / `tool_skill`、`cap/2.1.280` `tool_agent` / `tool_subagent_handback`、
        // `cap/2.1.285` `tool_web_fetch`，条数与 `tool_use_success` 逐个相等）；Bash 那条
        // 在上面跟着命令执行报了。Agent 前另有 `subagent_launch`、Skill 前另有
        // `skill_invoke`（`cap/2.1.285` 06:56:08.489、06:55:30.993）。
        // 结果大到被落盘：先报一条落盘（`cap/2.1.285` 06:56:24.411：原始 60278 字节、
        // 预览 2267 字节、阈值 50000），`tool_use_success` 报的仍是原始大小。
        if v285 && let Some(orig) = tu.persisted_from {
            self.push(
                ms(t_done, -1),
                "tengu_tool_result_persisted",
                json!({
                    "toolName": &tu.name,
                    "originalSizeBytes": orig,
                    "persistedSizeBytes": tu.result_len,
                    "estimatedOriginalTokens": (orig + 2) / 4,
                    "estimatedPersistedTokens": (tu.result_len + 2) / 4,
                    "thresholdUsed": 50_000,
                    "truncatedAtCap": false
                }),
            );
        }
        if tu.name != "Bash" {
            let before = match tu.name.as_str() {
                "Agent" | "Task" => Some("subagent_launch"),
                "Skill" => Some("skill_invoke"),
                _ => None,
            };
            for name in before.into_iter().chain([tool_feature_name(&tu.name).as_str()]) {
                self.push_dd(t_done, "tengu_feature_ok", feature(name));
            }
        }
        let mut success = json!({
            "messageID": &prev_msg,
            "toolName": &tu.name,
            "isMcp": false
        });
        // 子代理里跑的工具多报是哪类子代理（`cap/2.1.280`：`subagent_type: Explore`、
        // `is_built_in_agent: true`，紧跟 `isMcp`）。
        if is_sub {
            success["subagent_type"] =
                json!(call.agent.agent_type.as_deref().unwrap_or("general-purpose"));
            success["is_built_in_agent"] = json!(builtin_agent.is_some());
        }
        let mut rest = json!({
            "effort_level": &effort_value,
            "durationMs": duration,
            "rssDeltaBytes": 1_081_344,
            "heapUsedDeltaBytes": 3_887_104,
            "externalDeltaBytes": 2_870_395,
            "preToolHookDurationMs": 0,
            "permissionDurationMs": 11,
            "toolResultSizeBytes": tu.persisted_from.unwrap_or(tu.result_len),
            "toolInputSizeBytes": if tu.name == "ExitPlanMode" && tu.input_len <= 2 {
                self.file_sizes.get(PLAN_INPUT_KEY).copied().unwrap_or(tu.input_len)
            } else {
                tu.input_len
            }
        });
        // 没有 effort 的模型（haiku 子代理，`cap/2.1.285` 九条）不报这一项。
        if v285
            && effort.is_none()
            && let Some(o) = rest.as_object_mut()
        {
            o.shift_remove("effort_level");
        }
        // 2.1.285：结果的 token 估算（字节数 / 4 四舍五入，11 条里 10 条相等）、媒体块数、
        // 会不会被落盘（超过 50000 字节的那条 60278 为 true，41469 为 false）。
        if v285 {
            let size = tu.persisted_from.unwrap_or(tu.result_len) as u64;
            insert_after(
                &mut rest,
                "toolResultSizeBytes",
                vec![
                    ("toolResultTokensEst", json!((size + 2) / 4)),
                    ("toolResultMediaBlocks", json!(tu.media_blocks)),
                    ("toolResultWillPersist", json!(size > 50_000)),
                ],
            );
        }
        if let (Some(obj), Some(rest)) = (success.as_object_mut(), rest.as_object()) {
            obj.extend(rest.clone());
        }
        tool_success_extras(&mut success, tu, self.file_sizes);
        success["queryChainId"] = json!(&chain_id);
        success["queryDepth"] = json!(prev_depth);
        success["requestId"] = json!(&prev_req);
        if bash_failed {
            let mut err = json!({
                "messageID": &prev_msg,
                "toolName": &tu.name,
                "toolUseID": &tu.id,
                "isMcp": false,
                "toolInputSizeBytes": tu.input_len,
                "durationMs": duration,
                "preToolHookDurationMs": 0,
                "permissionDurationMs": 5
            });
            if is_sub {
                err["subagent_type"] =
                    json!(call.agent.agent_type.as_deref().unwrap_or("general-purpose"));
                err["is_built_in_agent"] = json!(builtin_agent.is_some());
            }
            if let Some(e) = &effort {
                err["effort_level"] = json!(e);
            }
            err["queryChainId"] = json!(&chain_id);
            err["queryDepth"] = json!(prev_depth);
            err["requestId"] = json!(&prev_req);
            err["error"] = json!("ShellError");
            err["error_message_hash"] = json!(&sha256_hex(tu.result_head.as_bytes())[..12]);
            // 栈的摘要与顶帧随每个版本的打包产物变：2.1.285 那份取自 `cap/auto-2.1.285-20260930`，
            // 2.1.291 取自 `cap/auto-2.1.291-20261006-full`（B4 那条失败的 Bash），后者还多了异常的
            // 构造器名（压缩后的类名）；2.1.293（`cap/auto-2.1.293-20261008-full` 的两条失败 Bash）
            // 又没有构造器名了。
            if v293 {
                err["error_stack_hash"] = json!("2e671ed03fc0");
                err["error_top_frame"] = json!("chunk-nwqfvmza.js:3493:5989");
            } else if v291 {
                err["error_constructor"] = json!("Yj");
                err["error_stack_hash"] = json!("bfd227038d85");
                err["error_top_frame"] = json!("chunk-v1gtm86q.js:3481:5962");
            } else {
                err["error_stack_hash"] = json!("739abbe6a843");
                err["error_top_frame"] = json!("chunk-59zy4j10.js:3389:5792");
            }
            err["errorCode"] = json!("ShellError");
            err["rssDeltaBytes"] = json!(1_081_344);
            err["heapUsedDeltaBytes"] = json!(0);
            err["externalDeltaBytes"] = json!(2_856);
            self.push_dd_snake(t_done, "tengu_tool_use_error", err);
        } else {
            self.push_dd_snake(t_done, "tengu_tool_use_success", success);
        }
    }
}
