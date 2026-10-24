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
            // 问题 3：这一批里有要点头的工具时，上一条回复到这条请求的间隔大头是盯弹框的等待
            // （死时间，不属于任何工具的执行）。先按「间隔减去各工具一份合理执行」估出这段等待，
            // 它只进点头工具的 `permissionDurationMs`，不摊到同批别的工具的时长上。
            let gap = (t0 - prev_end_dt).num_milliseconds().max(50);
            let cwd = env.f.session_cwd.as_deref();
            let prompted_flags: Vec<bool> = shape
                .tool_uses
                .iter()
                .map(|tu| {
                    let cmd = tu.input.get("command").and_then(|c| c.as_str()).unwrap_or("");
                    let ro = tu.name == "Bash" && bash_read_only(&bash_profile(cmd), cwd);
                    tool_is_prompted(tu, shape.permission_mode, ro)
                })
                .collect();
            let prompted = prompted_flags.iter().filter(|x| **x).count() as i64;
            let batch_approval = if prompted > 0 && v285 {
                // 各工具一份合理执行：首条 Bash 含快照、其余很小；剩下的算盯弹框的等待。
                let shell_first = env.f.shell_snapshot_first;
                let exec: i64 = shape
                    .tool_uses
                    .iter()
                    .enumerate()
                    .map(|(j, tu)| {
                        let first_bash = tu.name == "Bash" && shell_first && is_main && j == 0;
                        if first_bash { 700 + i64::from(env.req_hash % 600) + 60 } else { 40 }
                    })
                    .sum();
                (gap - 50 - 20 * (n - 1) - exec).max(0)
            } else {
                0
            };
            let step = ToolStep {
                is_sub,
                prev_end_dt,
                prev_req,
                prev_msg,
                prev_depth,
                n,
                prompted,
                batch_approval,
            };
            for (i, tu) in shape.tool_uses.iter().enumerate() {
                let i = i as i64;
                if tu.is_error && tu.result_head.starts_with(NO_SUCH_TOOL) {
                    self.emit_no_such_tool(&step, i, tu);
                    continue;
                }
                if tu.is_error
                    && let Some(msg) = &tu.tool_error
                    && let Some(detail) = msg.strip_prefix("InputValidationError: ")
                {
                    self.emit_input_schema_error(&step, i, tu, detail);
                    continue;
                }
                if tu.is_error
                    && let Some(msg) = &tu.tool_error
                    && let Some(code) = validate_error_code(&tu.name, msg)
                {
                    self.emit_validate_error(&step, i, tu, msg, code);
                    continue;
                }
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
            // billing-only 模拟没补 `<total_tokens>` 提醒（[`ApiCall::bare`]）。
            if !self.env.call.bare {
                self.push(ta, "tengu_attachments", attachments);
            }
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

    /// 模型调了这一轮没声明的工具（`Glob` 之类）：客户端找不到它，不走权限、不执行，只报
    /// `feature_bad{tool_not_found}` 与 `tool_use_error{NO_SUCH_TOOL}`（官方 `qYr`；
    /// `cap/auto-2.1.306-20261021-full/00387` 的 Glob，2.1.300 / 2.1.305 同形）。
    fn emit_no_such_tool(&mut self, step: &ToolStep, i: i64, tu: &ToolUse) {
        let env = self.env;
        let TurnFacts { ref chain_id, v293, .. } = *env.f;
        let EventEnv { builtin_agent, ref effort, .. } = *env;
        let ToolStep { is_sub, ref prev_req, prev_depth, .. } = *step;
        let t_done = self.unrun_tool_at(step, i);
        // 官方工具名报原名、错误归 `registered_base_tool`；别的名字官方脱敏（`IJ`）：`mcp__` 开头的是 `mcp_tool`，
        // 功能名另叫 `tool_unregistered_mcp`，其余 `unknown_tool`（2.1.306 `qYr`）。
        let known = config::CC_TOOL_NAMES.contains(&tu.name.as_str());
        let (tool_name, error, feature) = if known {
            (
                tu.name.as_str(),
                "No such tool available: registered_base_tool",
                tool_feature_name(&tu.name),
            )
        } else if tu.name.starts_with("mcp__") {
            ("mcp_tool", "No such tool available: mcp_tool", "tool_unregistered_mcp".to_string())
        } else {
            (
                "unknown_tool",
                "No such tool available: unknown_tool",
                tool_feature_name("unknown_tool"),
            )
        };
        let mut bad = json!({ "feature_name": feature, "error_code": "tool_not_found" });
        if v293 {
            bad["tengu_quizzical_giraffe"] = json!("unset");
        }
        self.push_dd(ms(t_done, -1), "tengu_feature_bad", bad);
        let mut err = json!({
            "error": error,
            "errorCode": "NO_SUCH_TOOL",
            "toolName": tool_name,
            "toolNameHash": &sha256_hex(tu.name.as_bytes())[..12],
            "toolUseID": &tu.id,
            "isMcp": tu.name.starts_with("mcp__")
        });
        if is_sub {
            err["subagent_type"] =
                json!(env.call.agent.agent_type.as_deref().unwrap_or("general-purpose"));
            err["is_built_in_agent"] = json!(builtin_agent.is_some());
        }
        if let Some(e) = &effort {
            err["effort_level"] = json!(e);
        }
        err["queryChainId"] = json!(chain_id);
        err["queryDepth"] = json!(prev_depth);
        err["requestId"] = json!(prev_req);
        self.push_dd_snake(t_done, "tengu_tool_use_error", err);
    }

    /// 没跑起来的工具（找不到、验证不过）报错的时刻：贴着这条请求之前，与跑完的工具同一套取法。
    fn unrun_tool_at(&self, step: &ToolStep, i: i64) -> DateTime<Utc> {
        let ToolStep { prev_end_dt, n, .. } = *step;
        if self.env.f.v285 {
            ms(self.env.t0, -8 - 4 * (n - 1 - i)).max(ms(prev_end_dt, i + 1))
        } else {
            ms(prev_end_dt, i + 1)
        }
    }

    /// 入参没过 schema（缺必填参数、类型不对、多了参数……）或根本不是合法 JSON：客户端在验证之前就拦下，回
    /// `<tool_use_error>InputValidationError: …</tool_use_error>`。只报 `feature_sad{tool_input_validation_failed}`
    /// 与 `tool_use_error{InputValidationError}`（官方 `o4r`）：schema 那种是 `ZOD_VALIDATION`，带按文案认出的
    /// `zodIssueCodes`（[`zod_issue_codes`]）与整段文案的哈希；JSON 那种是 `JSON_PARSE`，哈希的是
    /// `"<工具名>: unparsed tool input"`。字段序照 `cap/2.1.2xx` 里 Read 那两条。
    fn emit_input_schema_error(&mut self, step: &ToolStep, i: i64, tu: &ToolUse, detail: &str) {
        let env = self.env;
        let TurnFacts { ref chain_id, v293, .. } = *env.f;
        let EventEnv { builtin_agent, ref effort, .. } = *env;
        let ToolStep { is_sub, ref prev_req, ref prev_msg, prev_depth, .. } = *step;
        let t_done = self.unrun_tool_at(step, i);
        let mut sad = json!({
            "feature_name": tool_feature_name(&tu.name),
            "error_code": "tool_input_validation_failed"
        });
        if v293 {
            sad["tengu_quizzical_giraffe"] = json!("unset");
        }
        self.push_dd(ms(t_done, -1), "tengu_feature_sad", sad);
        let json_parse =
            detail.contains(" was called with input that could not be parsed as JSON.");
        let mut err = json!({ "error": "InputValidationError" });
        if json_parse {
            err["errorCode"] = json!("JSON_PARSE");
            err["errorDetailsHash"] =
                json!(&sha256_hex(format!("{}: unparsed tool input", tu.name).as_bytes())[..12]);
        } else {
            err["errorCode"] = json!("ZOD_VALIDATION");
            err["zodIssueCodes"] = json!(zod_issue_codes(detail));
            err["errorDetailsHash"] = json!(&sha256_hex(detail.as_bytes())[..12]);
        }
        err["messageID"] = json!(prev_msg);
        err["toolName"] = json!(&tu.name);
        err["isMcp"] = json!(tu.name.starts_with("mcp__"));
        if is_sub {
            err["subagent_type"] =
                json!(env.call.agent.agent_type.as_deref().unwrap_or("general-purpose"));
            err["is_built_in_agent"] = json!(builtin_agent.is_some());
        }
        if let Some(e) = &effort {
            err["effort_level"] = json!(e);
        }
        err["toolInputSizeBytes"] = json!(tu.input_len);
        err["queryChainId"] = json!(chain_id);
        err["queryDepth"] = json!(prev_depth);
        err["requestId"] = json!(prev_req);
        self.push_dd_snake(t_done, "tengu_tool_use_error", err);
    }

    /// 已声明的工具在验证这一步被拦下（Edit 改不存在的文件、Write 没先读……，[`validate_error_code`]）：
    /// 不走权限、不执行，只报 `feature_sad{tool_validate_input_rejected}` 与
    /// `tool_use_error{ValidateInputError}`，报错文案脱敏后取哈希（官方 `Rt` / `Dp` / `dZe`）。
    fn emit_validate_error(&mut self, step: &ToolStep, i: i64, tu: &ToolUse, msg: &str, code: u32) {
        let env = self.env;
        let TurnFacts { ref chain_id, v293, .. } = *env.f;
        let EventEnv { builtin_agent, ref effort, .. } = *env;
        let ToolStep { is_sub, ref prev_req, ref prev_msg, prev_depth, .. } = *step;
        let t_done = self.unrun_tool_at(step, i);
        // 子代理写报告文件被拦：校验那一步先报一条，带要写的内容字节数（官方 Write 的 `validateInput`）。
        if tu.name == "Write" && code == 5 {
            let bytes = tu.input.get("content").and_then(|c| c.as_str()).map_or(0, str::len);
            self.push(
                ms(t_done, -2),
                "tengu_subagent_md_report_blocked",
                json!({ "contentBytes": bytes }),
            );
        }
        let mut sad = json!({
            "feature_name": tool_feature_name(&tu.name),
            "error_code": "tool_validate_input_rejected"
        });
        if v293 {
            sad["tengu_quizzical_giraffe"] = json!("unset");
        }
        self.push_dd(ms(t_done, -1), "tengu_feature_sad", sad);
        let mut err = json!({
            "messageID": prev_msg,
            "toolUseID": &tu.id,
            "toolName": &tu.name,
            "error": "ValidateInputError",
            "error_message_hash": &sha256_hex(scrub_error_message(msg).as_bytes())[..12],
            "errorCode": code,
            "isMcp": false
        });
        if is_sub {
            err["subagent_type"] =
                json!(env.call.agent.agent_type.as_deref().unwrap_or("general-purpose"));
            err["is_built_in_agent"] = json!(builtin_agent.is_some());
        }
        if let Some(e) = &effort {
            err["effort_level"] = json!(e);
        }
        err["queryChainId"] = json!(chain_id);
        err["queryDepth"] = json!(prev_depth);
        err["requestId"] = json!(prev_req);
        self.push_dd_snake(t_done, "tengu_tool_use_error", err);
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
            if env.f.v300 {
                auto_mode_decision_v300(&mut decision);
            }
            if env.f.v307 {
                auto_mode_decision_v307(&mut decision);
            }
            self.push_dd_snake(td, "tengu_auto_mode_decision", decision);
        }
        // default 模式下要用户点头的：Edit / Write / NotebookEdit、非只读的 Bash（`cap/auto-2.1.285-20260930`
        // 08:00:45.839 的 Edit、08:02:04.556 的 Bash），以及非 auto 模式的 Skill。只读的 Bash
        // （`ls`、`git status`……）照配置放行。
        let prompted = tool_is_prompted(tu, shape.permission_mode, read_only);
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
            // 子代理里的弹框也报，挂在子代理身份上（`cap/auto-2.1.306-20261021-full/00186`，
            // 2.1.285 起各版抓包同样）。
            self.push_unary(shown, "response", (&tu.name, &tu.input), prev_msg);
            self.note_edit_decision((&tu.name, &tu.input), "accept", "user_temporary");
            // 用户点头落在这条请求发出前（两次请求之间就是在等他）。
            let tp = ms(t0, -40).max(ms(shown, 1));
            // 心跳：弹框把界面从干活切到等用户，上一条回复收尾后 11ms 上下；点头后 20ms 上下回到干活
            // （`cap/auto-2.1.305-20261021-full` B1 的 Edit：.862 → .873，点头 .744 → .765；A16 的
            // AskUserQuestion 是 `input needed`）。子代理里的弹框不动主线程这条。
            if !is_sub {
                self.beat(
                    ms(prev_end_dt, 11),
                    heartbeat::Trigger::Run(Some(heartbeat::awaiting_for(&tu.name))),
                );
                self.beat(ms(tp, 21), heartbeat::Trigger::Run(Some(heartbeat::TURN_RUNNING)));
            }
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
            self.push_unary(tp, "accept", (&tu.name, &tu.input), prev_msg);
            if !is_sub {
                self.push_file_suggestions_refresh(tp);
            }
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
            self.note_edit_decision((&tu.name, &tu.input), "accept", "config");
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
        let TurnFacts {
            is_main,
            ref chain_id,
            shell_snapshot_first,
            v277,
            v285,
            v291,
            v293,
            v299,
            v303,
            v305,
            v306,
            v307,
            v308,
            v309,
            v310,
            ..
        } = *env.f;
        let EventEnv { t0, builtin_agent, ref effort, ref effort_value, req_hash, .. } = *env;
        let ToolStep {
            is_sub,
            prev_end_dt,
            ref prev_req,
            ref prev_msg,
            prev_depth,
            n,
            prompted: batch_prompted,
            batch_approval,
            ..
        } = *step;
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
        // 每个工具跑了多久：2.1.285 起官方是一个接一个跑完就发下一条，上一条回复结束到下一条请求发出的
        // 间隔 ≈ 各工具耗时之和 + 一小段收尾（一个工具时中位 40～75ms，`cap/auto-2.1.285-20260930`、
        // `auto-2.1.293`、`auto-2.1.296`、`auto-2.1.299` 的 full 抓包逐轮核过；多一个工具多约 20ms），按调用数
        // 均分。此前按 `间隔 / (n + 1)` 分，一个工具时只报了半段间隔。更早的版本照旧。
        // `batch_approval` 是这一批盯弹框的总等待，先从间隔里扣掉再按调用数均分——同批别的工具就不会
        // 分到等待（问题 3）。它只进点头工具的 `permissionDurationMs`（见下）。
        let duration = if v285 {
            ((gap - batch_approval - 50 - 20 * (n - 1)) / n).max(1)
        } else {
            (gap / (n + 1) - 12).max(1)
        };
        let bash_failed = tu.name == "Bash" && tu.is_error;
        // 别的工具跑了、却以失败收尾（`tool.call` 抛错）。用户在弹框上拒绝的不算，那串另报。
        let call_failed = tu.name != "Bash"
            && tu.is_error
            && !tu.result_head.trim_start().starts_with(USER_REJECTED_TOOL_USE);
        let command = tu.input.get("command").and_then(|c| c.as_str()).unwrap_or("");
        let read_only = tu.name == "Bash"
            && bash_read_only(&bash_profile(command), env.f.session_cwd.as_deref());
        let prompted = tool_is_prompted(tu, shape.permission_mode, read_only);
        // 会话里第一条 Bash 先建 shell 快照，这段就在这次调用里、也就在上一条回复结束到这条请求发出的
        // 那段间隔里：2.1.291 起报快照耗时，它与这次调用的计时都从间隔推出的 `duration` 里分（见
        // [`bash_timing`]），时长不会撑过间隔。失败那条同样报 2.1.299 的计时。
        let first_bash = tu.name == "Bash" && shell_snapshot_first && is_main && i == 0;
        let bash_timing = (tu.name == "Bash").then(|| {
            let h = (i64::from(req_hash % 1_000) + i).rem_euclid(100);
            let snapshot = (first_bash && v291).then(|| 700 + i64::from(req_hash % 600));
            bash_timing(duration, h, snapshot)
        });
        let duration = bash_timing.map_or(duration, |t| t.duration);
        // 问题 3：要点头的工具，上一条回复到这条请求的间隔大头是盯弹框的等待。官方把这段记进
        // `permissionDurationMs`、**不**记进 `durationMs` / `externalWallMs`（`cap/auto-2.1.299-20261014-full`
        // 的 Edit 弹框：`durationMs:25`、`permissionDurationMs:881`）。`batch_approval` 已从上面的 `duration`
        // 里扣掉（所以这条的时长只是它那份合理执行），这里把等待平摊给这一批里点头的工具的
        // `permissionDurationMs`。重建拿不到真实等待，这是估值；genuinely 跑很久又要点头的命令会少报时长，
        // 比把等待全算成执行更贴近官方。
        let permission_ms =
            if prompted && batch_prompted > 0 { 11 + batch_approval / batch_prompted } else { 11 };
        if tu.name == "Bash" {
            if first_bash {
                let mut props = json!({});
                // 2.1.291 起多报这次快照花了多久（`cap/auto-2.1.291-20261006-full`：1237、722ms）。
                if let Some(snap_ms) = bash_timing.and_then(|t| t.snapshot) {
                    props["duration_ms"] = json!(snap_ms);
                }
                let snap = feature_with("shell_snapshot_create", props, v293);
                self.push_dd(ms(t_done, -40), "tengu_feature_ok", snap);
            }
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
            if v299 {
                bash_meta_v299(&mut bash, command, &tu.result_head, &shape.model, false);
            }
            // meta 带着这条请求的模型（2.1.299 起）：顶层 `betas` 按这个模型取，子代理跑 haiku 时是 haiku 那份
            // （`cap/auto-2.1.308-20261022-gaps` claude-code-guide 里那条 Bash，事件与 Datadog 都是 6 项）。
            self.own_betas_scope = bash.get("model").is_some();
            if bash_failed {
                // 非零退出（`cap/auto-2.1.285-20260930` 07:58:12.123）：同一份画像报
                // `command_failed`，退出码取结果开头的 `Exit code N`，没有 `was_backgrounded`；
                // 工具那头报 `feature_sad{tool_shell_error}` 与 `tool_use_error`，不报成功。
                if let Some(o) = bash.as_object_mut() {
                    o.shift_remove("was_backgrounded");
                    o.insert("exit_code".into(), json!(exit_code_of(&tu.result_head)));
                }
                self.push_dd_snake(t_done, "tengu_bash_tool_command_failed", bash);
                self.own_betas_scope = false;
                let mut sad =
                    json!({ "feature_name": "tool_bash", "error_code": "tool_shell_error" });
                // 2.1.293 末尾多一项实验标记（`tengu_started` 也多了同一项）。
                if v293 {
                    sad["tengu_quizzical_giraffe"] = json!("unset");
                }
                self.push_dd(t_done, "tengu_feature_sad", sad);
            } else {
                self.push_dd_snake(t_done, "tengu_bash_tool_command_executed", bash);
                self.own_betas_scope = false;
                self.emit_bash_mentions(command, t_done, prev_msg);
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
        self.emit_file_ops(tu, t_done, prev_msg);
        if tu.name != "Bash" && !call_failed {
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
        // 2.1.303 起 `messageID` 之后多一项 `toolUseID`（失败那条 `tool_use_error` 早就有）。
        if v303 {
            insert_after(&mut success, "messageID", vec![("toolUseID", json!(&tu.id))]);
        }
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
            "permissionDurationMs": permission_ms,
            "toolResultSizeBytes": tu.persisted_from.unwrap_or(tu.result_len),
            "toolInputSizeBytes": if tu.name == "ExitPlanMode" && tu.input_len <= 2 {
                self.file_sizes.get(PLAN_INPUT_KEY).copied().unwrap_or(tu.input_len)
            } else {
                tu.input_len
            }
        });
        // 2.1.299：`durationMs` 之后报这次调用的分段计时，取法见 [`tool_timing`]。
        if v299 && let Some(timing) = tool_timing(tu, duration, bash_timing, req_hash, i) {
            insert_after(&mut rest, "durationMs", timing);
        }
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
        tool_success_extras(&mut success, tu, self.file_sizes, v277, v285);
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
                "permissionDurationMs": permission_ms.max(5)
            });
            // 2.1.299：失败的 Bash 同样报计时（`cap/auto-2.1.299-20261014-full` 两条：33 → 25 + 7、
            // 35 → 19 + 14），取法同成功那条。
            if let Some(t) = bash_timing.filter(|_| v299) {
                insert_after(
                    &mut err,
                    "durationMs",
                    vec![("externalWallMs", json!(t.external)), ("preExternalMs", json!(t.pre))],
                );
            }
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
            // 又没有构造器名了。2.1.299（`cap/auto-2.1.299-20261014-full` 两条）、2.1.303
            // （`cap/auto-2.1.303-20261019-full` 两条）、2.1.305（`cap/auto-2.1.305-20261021-full` 两条）各换
            // 一次；2.1.300 ~ 2.1.302 没有样本，沿用 2.1.299，2.1.304 沿用 2.1.303。2.1.306
            // （`cap/auto-2.1.306-20261021-full` 三条）只换了 chunk 名与摘要，2.1.307
            // （`cap/auto-2.1.307-20261022-bash`，全量包里那次没有失败的 Bash）摘要、行列都换了，2.1.308
            // （`cap/auto-2.1.308-20261022-full` A0 那条）只换了 chunk 名，2.1.309
            // （`cap/auto-2.1.309-20261023-full` A0 那条）摘要、行列、块名都换了，2.1.310
            // （`cap/auto-2.1.310-20261024-bashfail`：全量包里那次没有失败的 Bash，另用 `-p` 跑
            // `ls` 不存在的目录补抓，同法在 2.1.309 上复现出 1a36b34122c0）摘要、行列、块名都换了。
            if v310 {
                err["error_stack_hash"] = json!("b26c68ea74b5");
                err["error_top_frame"] = json!("chunk-31gxg38t.js:3618:6047");
            } else if v309 {
                err["error_stack_hash"] = json!("1a36b34122c0");
                err["error_top_frame"] = json!("chunk-kang00hq.js:3682:5744");
            } else if v308 {
                err["error_stack_hash"] = json!("40e496cc06bc");
                err["error_top_frame"] = json!("chunk-a1cg2ptw.js:3661:6043");
            } else if v307 {
                err["error_stack_hash"] = json!("40e496cc06bc");
                err["error_top_frame"] = json!("chunk-wvsyb4fb.js:3661:6043");
            } else if v306 {
                err["error_stack_hash"] = json!("c19e7dcf6637");
                err["error_top_frame"] = json!("chunk-z4fyjkxt.js:3654:6164");
            } else if v305 {
                err["error_stack_hash"] = json!("a4913bdffc8a");
                err["error_top_frame"] = json!("chunk-h56h5ggw.js:3654:6164");
            } else if v303 {
                err["error_stack_hash"] = json!("99830f66ede0");
                err["error_top_frame"] = json!("chunk-jh3cvmqy.js:3614:6109");
            } else if v299 {
                err["error_stack_hash"] = json!("1dada4b9379c");
                err["error_top_frame"] = json!("chunk-4xx9trad.js:3588:6085");
            } else if v293 {
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
        } else if call_failed {
            // 执行抛错（官方 `o4r` 的 catch）：同一份调用字段，`error` / `errorCode` 是异常类名，再带
            // `Dp` 的文案哈希。没有样本能对出各工具的异常类与栈指纹，类名按通用的 `Error`、不报栈——
            // 宁可少几项，也不把一次失败报成成功。
            let mut err = json!({
                "messageID": &prev_msg,
                "toolName": &tu.name,
                "toolUseID": &tu.id,
                "isMcp": tu.name.starts_with("mcp__"),
                "toolInputSizeBytes": tu.input_len,
                "durationMs": duration,
                "preToolHookDurationMs": 0,
                "permissionDurationMs": permission_ms.max(5)
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
            err["error"] = json!("Error");
            let msg = tu.tool_error.as_deref().unwrap_or(&tu.result_head);
            err["error_message_hash"] =
                json!(&sha256_hex(scrub_error_message(msg).as_bytes())[..12]);
            err["errorCode"] = json!("Error");
            self.push_dd_snake(t_done, "tengu_tool_use_error", err);
        } else {
            self.push_dd_snake(t_done, "tengu_tool_use_success", success);
        }
    }
}

/// 模型调了没声明的工具时客户端回的结果开头。
const NO_SUCH_TOOL: &str = "<tool_use_error>Error: No such tool available:";

/// 一次 Bash 调用的计时：总时长、等外部（子进程）的时长、第一次等外部之前的准备时长（后两项 2.1.299
/// 起报），会话第一条还有同批报的快照耗时（2.1.291 起报）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::telemetry) struct BashTiming {
    pub(in crate::telemetry) duration: i64,
    pub(in crate::telemetry) external: i64,
    pub(in crate::telemetry) pre: i64,
    pub(in crate::telemetry) snapshot: Option<i64>,
}

/// [`BashTiming`] 的取法。`duration` 是从间隔推出的时长（上一条回复结束到这条请求发出之间按调用数
/// 均分），所有段都从它里面分，**不会超过它**；`h` 是 0..100 的扰动；`snapshot` 是会话第一条 Bash
/// 想报的快照耗时（700～1300ms），只有它才给。按 `cap/auto-2.1.299-20261014-full` 的 22 条 Bash：
///
/// - **会话里第一条**：准备 ≈ 快照再加几十毫秒，其余是命令本身（外部）。抓包里那几条都是快命令，外部
///   只有二三十到七八十毫秒、准备占九成以上（快照 799 → 1017 = 外部 60 + 准备 938；快照 842 → 879 =
///   外部 26 + 准备 850）。总时长就是推出来的那份：快照与准备先分，**余量全算外部**——命令跑得久（间隔
///   10s、跑 9s 的构建）外部就大，不会把这段时间压成一秒左右没了着落；间隔放不下想报的快照时，快照缩
///   到放得下为止（外部至少留 20～64ms、零头 0～14ms）。连 700ms 的快照都放不下时整体按比例分：准备
///   92%、快照取准备的 95%——快照会远低于官方从没低过的约 690ms，但间隔本身就塞不下一次真实的快照，
///   两头顾不全时优先不撑过间隔；
/// - **之后的**：准备占两到五成，其余基本是外部，总时长照旧。
pub(in crate::telemetry) fn bash_timing(
    duration: i64,
    h: i64,
    snapshot: Option<i64>,
) -> BashTiming {
    let Some(want) = snapshot else {
        let pre = (duration * (15 + h % 35) / 100).max(1);
        let external = (duration - pre - 1 - h % 4).max(0);
        return BashTiming { duration, external, pre, snapshot: None };
    };
    let external = 20 + h % 45;
    let slack = h % 15;
    let extra = 8 + (h * 7) % 130;
    let room = duration - external - slack - extra;
    if room >= 700 {
        let snap = want.min(room);
        let pre = snap + extra;
        // `room` 已扣掉外部的下限，余量不小于下限，全给外部：总时长恰好是推出来的那份。
        let external = external.max(duration - pre - slack);
        return BashTiming { duration, external, pre, snapshot: Some(snap) };
    }
    let duration = duration.max(1);
    let pre = (duration * 92 / 100).max(1);
    let external = ((duration - pre) * 3 / 4).max(0);
    BashTiming { duration, external, pre, snapshot: Some(pre * 95 / 100) }
}

/// 客户端声明「不做外部工作」的工具（可执行文件里的 `declaredNoOutsideWork` 名单，MCP 工具不算）：
/// 一次都没等外部也照报 `externalWallMs: 0`。不在名单里、又没等过外部的（`SubagentHandback`、
/// `CronCreate`、`SendMessage` 这些），三个计时键一个都不报。
///
/// 这个名单只管「一次都没等过外部」时报不报：客户端算计时（`fields(总时长, {declaredNoOutsideWork})`）
/// 先看有没有打过点，打过点就照实报、名单不起作用。Grep / Glob 在名单里，但调 ripgrep 那一步照样打
/// 「外部」的点（可执行文件里 `--files` 与搜索两处 `mark("external")`），所以照算外部时长，两者不矛盾——
/// 名单只让它们在没跑 ripgrep 就返回时（比如路径不存在）也报一个 `externalWallMs: 0`。
const TOOLS_NO_OUTSIDE_WORK: &[&str] = &[
    "Read",
    "Edit",
    "Write",
    "Glob",
    "Grep",
    "NotebookEdit",
    "TodoWrite",
    "TaskCreate",
    "TaskGet",
    "TaskList",
    "TaskUpdate",
    "Projects",
    "SendUserFile",
    "SendUserMessage",
    "ToolSearch",
    "Skill",
    "Agent",
    "AskUserQuestion",
    "Artifact",
    "ArtifactData",
    "ExitPlanMode",
    "Monitor",
    "ReadNotifications",
    "SearchMcpRegistry",
    "TaskStop",
];

/// 2.1.299 `tengu_tool_use_success` 在 `durationMs` 之后的分段计时，`None` 即一个键都不报。
///
/// 客户端按「谁在占着」把这次调用的时长切成等外部（子进程、网络、MCP 服务器）、等模型、等批准与自己
/// 几段，报 `externalWallMs`（恒报）、`approvalWaitMs` / `modelCallMs` / `preExternalMs`（为 0 不报，
/// 最后一项是第一次等外部或模型之前的准备）；一次都没等过、工具又不在 [`TOOLS_NO_OUTSIDE_WORK`]
/// 名单里就整组不报。各工具等什么照可执行文件，比例照 `cap/auto-2.1.299-20261014-full` 的 56 条：
///
/// - Bash：见 [`bash_timing`]；
/// - Glob / Grep：跑 ripgrep 那段算外部，开头一小段准备。**官方 2.1.299 不会报这两个**：决定把它们放进
///   工具列表的开关（可执行文件里的 `M$()`）写死返回 false，2.1.285～2.1.299 的抓包里一条声明都没有，
///   所以没有样本可抓。这里只照打点位置给形状，留给来访历史里带着这两个名字的情形；
/// - MCP 工具：调服务器那段算外部，之前只有一两毫秒（`cap/auto-2.1.299-20261014-mcp`：137 = 外部 135 +
///   准备 1）；
/// - Agent：同步派出去就返回，只有一段准备；
/// - WebSearch：几乎全是等模型；WebFetch：先取页面（外部）、再交模型处理；
/// - `Artifact` 的 `read` 动作不在名单里（要拉远端），没等外部就不报；名单里其余的只报 `externalWallMs: 0`。
fn tool_timing(
    tu: &ToolUse,
    duration: i64,
    bash: Option<BashTiming>,
    req_hash: u32,
    i: i64,
) -> Option<Vec<(&'static str, Value)>> {
    let h = (i64::from(req_hash % 1_000) + i).rem_euclid(100);
    // （外部, 模型, 准备）
    let (external, model, pre) = match tu.name.as_str() {
        "Bash" => {
            let t = bash?;
            (t.external, 0, t.pre)
        }
        "Glob" | "Grep" => {
            let pre = (duration * (5 + h % 15) / 100).max(1);
            ((duration - pre - 1 - h % 3).max(0), 0, pre)
        }
        n if n.starts_with("mcp__") => {
            let pre = (1 + h % 3).min(duration);
            ((duration - pre - h % 3).max(0), 0, pre)
        }
        "Agent" | "Task" => (0, 0, (duration / 3).max(1)),
        "WebSearch" => (0, (duration - 2).max(0), 1),
        "WebFetch" => {
            let ext = duration * 14 / 100;
            let model = duration * 66 / 100;
            (ext, model, (duration - ext - model - 6).max(0))
        }
        "Artifact" if tu.input.get("action").and_then(|a| a.as_str()) == Some("read") => {
            return None;
        }
        n if TOOLS_NO_OUTSIDE_WORK.contains(&n) => (0, 0, 0),
        _ => return None,
    };
    let mut out = vec![("externalWallMs", json!(external))];
    if model > 0 {
        out.push(("modelCallMs", json!(model)));
    }
    if pre > 0 {
        out.push(("preExternalMs", json!(pre)));
    }
    Some(out)
}

/// 这次工具调用要不要用户在弹框上点头（default 模式下的 Edit / Write / NotebookEdit 与非只读 Bash、
/// 非 auto 模式的 Skill，以及任何模式都要本人作答的 AskUserQuestion / ExitPlanMode）。弹框那段等待官方
/// 记进 `permissionDurationMs`、不进 `durationMs`，见 [`EventBuilder::emit_tool_result`]。
fn tool_is_prompted(tu: &ToolUse, permission_mode: &str, read_only: bool) -> bool {
    (tu.name == "Skill" && permission_mode != "auto")
        || (permission_mode == "default"
            && (matches!(tu.name.as_str(), "Edit" | "Write" | "NotebookEdit")
                || (tu.name == "Bash" && !read_only)))
        || tu.name == "AskUserQuestion"
        || tu.name == "ExitPlanMode"
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tu(name: &str, input: Value) -> ToolUse {
        ToolUse { name: name.into(), input, ..Default::default() }
    }

    fn keys(t: &Option<Vec<(&'static str, Value)>>) -> Vec<&'static str> {
        t.as_ref().map(|v| v.iter().map(|(k, _)| *k).collect()).unwrap_or_default()
    }

    /// 2.1.299 的分段计时照客户端的计法：名单里没等外部的只报 `externalWallMs: 0`；跑 ripgrep 的
    /// Grep / Glob、调服务器的 MCP 工具大头是外部；不在名单里又没等外部的整组不报；为 0 的段不报。
    #[test]
    fn tool_timing_follows_the_client_rules() {
        let none = Value::Null;
        for name in ["Read", "Edit", "Write", "ToolSearch", "Skill", "AskUserQuestion"] {
            let t = tool_timing(&tu(name, none.clone()), 30, None, 7, 0);
            assert_eq!(t, Some(vec![("externalWallMs", json!(0))]), "{name}");
        }
        for name in ["SubagentHandback", "CronCreate", "SendMessage", "ScheduleWakeup"] {
            assert_eq!(tool_timing(&tu(name, none.clone()), 30, None, 7, 0), None, "{name}");
        }
        assert_eq!(
            tool_timing(&tu("Artifact", json!({"action": "read"})), 30, None, 7, 0),
            None,
            "Artifact 的 read 要拉远端，不在名单里"
        );
        for name in ["Grep", "Glob", "mcp__opdash__search_logs"] {
            let t = tool_timing(&tu(name, none.clone()), 200, None, 7, 0);
            assert_eq!(keys(&t), ["externalWallMs", "preExternalMs"], "{name}");
            let v = t.unwrap();
            let (ext, pre) = (v[0].1.as_i64().unwrap(), v[1].1.as_i64().unwrap());
            assert!(ext > pre && ext + pre <= 200, "{name}: 大头是外部 {ext} / {pre}");
        }
        // 时长很短的 WebFetch：为 0 的模型 / 准备段不报。
        let short = tool_timing(&tu("WebFetch", none.clone()), 1, None, 7, 0);
        assert_eq!(keys(&short), ["externalWallMs"]);
        let long = tool_timing(&tu("WebFetch", none), 3_246, None, 7, 0);
        assert_eq!(keys(&long), ["externalWallMs", "modelCallMs", "preExternalMs"]);
    }
}
