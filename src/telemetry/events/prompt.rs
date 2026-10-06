//! 发请求之前：会话启动、后台模板、被拒的工具、通知与新输入。

use super::*;

impl EventBuilder<'_> {
    /// 新会话的启动那串；`--continue` 接上旧会话、或同一进程 `/clear` 之后的那几条。
    pub(super) fn emit_startup(&mut self) {
        let env = self.env;
        let shape = env.shape;
        let TurnFacts {
            ref continued_from,
            ref cleared_from,
            ref cleared_prev,
            ref base_identity,
            is_new_session,
            ref dd_model,
            ..
        } = *env.f;
        let EventEnv { t0, tpl, .. } = *env;
        // 新进程 `--continue` 接上旧会话：启动那串报在进程启动时那个临时会话 id 上，随后在这个
        // 会话上报接续那三条（`cap/auto-2.1.285-20260930` 08:03:54.947，比启动探测早 250ms 上下）。
        if let Some((probe_sid, at)) = &continued_from {
            let at: DateTime<Utc> = (*at).into();
            let t_res = ms(at, -257).min(ms(t0, -1_000));
            let probe_identity = Identity {
                session_id: probe_sid.clone(),
                parent_session_id: None,
                ..base_identity.clone()
            };
            let (ev, d) = emit_template(
                &tpl.startup,
                t_res,
                &probe_identity,
                |dt| env.ctx(dt),
                dd_model,
                &self.subst,
            );
            self.probe_side = Some((probe_identity, (ev, d)));
            self.push(
                t_res,
                "tengu_session_start",
                json!({
                    "previous_session_id": probe_sid,
                    "source": "resume",
                    "permissionMode": shape.permission_mode,
                    "dangerouslySkipPermissionsPassed": false,
                    "modeIsBypass": false,
                    "print": false
                }),
            );
            self.push(
                t_res,
                "tengu_resume_model_restore",
                json!({ "outcome": "restored", "is_eap": false }),
            );
            self.push(
                t_res,
                "tengu_continue",
                json!({ "success": true, "resume_duration_ms": 49 }),
            );
        } else if let Some(prev) = &cleared_from {
            // 同一进程里 `/clear`：没有启动那一串，只有清空那几条（08:02:27.765–.771，第一句输入前
            // 5 秒上下），之后这个会话的每条事件都带 `parent_session_id`。
            let (prev_req, prev_end) = cleared_prev
                .clone()
                .map(|p| (p.last_main_request_id, p.last_call_end))
                .unwrap_or_default();
            let floor: DateTime<Utc> = prev_end.map_or(ms(t0, -5_400), |e| {
                DateTime::<Utc>::from(e) + chrono::Duration::milliseconds(500)
            });
            let tc = ms(t0, -5_400).max(floor).min(ms(t0, -50));
            self.push(
                tc,
                "tengu_cache_eviction_hint",
                json!({ "scope": "conversation_clear", "last_request_id": prev_req.as_deref().unwrap_or("") }),
            );
            self.push(tc, "tengu_shell_set_cwd", json!({ "success": true }));
            self.push(
                ms(tc, 1),
                "tengu_session_start",
                json!({
                    "previous_session_id": prev,
                    "source": "clear",
                    "permissionMode": shape.permission_mode,
                    "dangerouslySkipPermissionsPassed": false,
                    "modeIsBypass": false,
                    "print": false
                }),
            );
            for (off, name) in [(3, "cmd_clear"), (6, "cmd_dispatch")] {
                self.push_dd(ms(tc, off), "tengu_feature_ok", feature(name));
            }
            self.push(
                ms(tc, 6),
                "tengu_input_command",
                json!({ "input": "clear", "invocation_trigger": "user-slash" }),
            );
        } else if is_new_session {
            self.take_tpl(&tpl.startup, t0);
        }
    }

    /// 这一轮到期的后台模板事件。
    pub(super) fn emit_background(&mut self) {
        let env = self.env;
        let TurnFacts { started_wall, ref background, .. } = *env.f;
        let EventEnv { tpl, .. } = *env;
        if !background.is_empty() {
            self.take_tpl(&tpl.background[background.clone()], started_wall.into());
        }
    }

    /// 上一轮在权限弹框上被拒的工具：弹框、拒绝、turn 与 `turn_end{aborted_tools}`。
    pub(super) fn emit_rejected_tools(&mut self) {
        let env = self.env;
        let (call, shape) = (env.call, env.shape);
        let TurnFacts {
            ref prev_turn,
            ref rejected_calls,
            ref prev_main_message_id,
            ref prev_main_request_id,
            prev_main_depth,
            prev_end,
            ..
        } = *env.f;
        let EventEnv { t0, .. } = *env;
        // 上一轮最后一条回复里的工具在权限弹框上被拒（`cap/auto-2.1.285-20260930` 08:00:57.305 弹框、
        // 08:01:01.376 按 Esc）：弹框、拒绝那几条、turn、`turn_end{aborted_tools}`，没有 stop hook；
        // 客户端不再发请求，直到用户敲下一句——代理只能在这条新输入里看出来，补在它前面。
        if !rejected_calls.is_empty() {
            let prev_end_dt: DateTime<Utc> =
                prev_end.unwrap_or(call.started_at - Duration::from_secs(6)).into();
            let shown = ms(prev_end_dt, 8);
            let wait = ((t0 - shown).num_milliseconds() / 2).clamp(300, 4_000);
            let t_rej = ms(shown, wait);
            let prev_req = prev_main_request_id.clone().unwrap_or_default();
            let prev_msg = prev_main_message_id.clone().unwrap_or_default();
            for (c, _) in rejected_calls {
                let bash = c.name == "Bash";
                self.push(
                    shown,
                    "tengu_tool_use_show_permission_request",
                    json!({
                        "messageID": &prev_msg,
                        "toolName": &c.name,
                        "isMcp": false,
                        "sandboxEnabled": false,
                        "permissionMode": shape.permission_mode,
                        "originAgentType": "main"
                    }),
                );
                self.push_dd(t_rej, "tengu_feature_ok", feature("permission_user_deny"));
                self.push(t_rej, "tengu_permission_request_escape", json!({}));
                self.push(
                    t_rej,
                    "tengu_tool_use_can_use_tool_rejected",
                    json!({
                        "messageID": &prev_msg,
                        "toolName": &c.name,
                        "deniedBy": "user_reject",
                        "decisionReasonType": "unknown",
                        "queryChainId": &prev_turn.0,
                        "queryDepth": prev_main_depth,
                        "requestId": &prev_req
                    }),
                );
                let mut rejected = json!({
                    "messageID": &prev_msg,
                    "isMcp": false,
                    "toolName": &c.name,
                    "sandboxEnabled": false,
                    "waiting_for_user_permission_ms": wait
                });
                if bash {
                    rejected["destructive_category"] = json!("none");
                    rejected["destructive_target_scope"] = json!("none");
                    rejected["git_destructive_target"] = json!("none");
                    rejected["permission_mode"] = json!(shape.permission_mode);
                }
                rejected["hasFeedback"] = json!(false);
                self.push_dd_snake(t_rej, "tengu_tool_use_rejected_in_prompt", rejected);
            }
            let t2 = ms(t_rej, 2);
            self.push_dd(t2, "tengu_feature_ok", feature("turn"));
            let started: DateTime<Utc> = prev_turn.2.unwrap_or(call.started_at).into();
            self.push(
                t2,
                "tengu_turn_end",
                env.turn_end("aborted_tools", (t2 - started).num_milliseconds().max(0), None),
            );
        }
    }

    /// 夹进这次输入的后台任务通知。
    pub(super) fn emit_notifications(&mut self) {
        let env = self.env;
        let TurnFacts { notification_base, ref notifications, prev_main_end, .. } = *env.f;
        let EventEnv { t0, ref effort, .. } = *env;
        // 上一轮收尾时送到、夹进这次输入的后台任务通知：送达一条、输入一条（`turn_origin:
        // task-notification`），各占一个输入号。
        if !notifications.is_empty() {
            let base: DateTime<Utc> = prev_main_end.map_or(ms(t0, -2_000), DateTime::<Utc>::from);
            for (i, len) in notifications.iter().enumerate() {
                let tn = ms(base, 17 + i as i64).min(ms(t0, -60));
                let queued = json!({
                    "feature_name": "queued_message_delivered",
                    "delivery": "turn_end",
                    "command_count": 1,
                    "prompt_count": 0,
                    "relay_count": 0,
                    "artifact_comment_count": 0,
                    "wait_ms": 3_655
                });
                self.push_dd(tn, "tengu_feature_ok", queued);
                let mut input = json!({
                    "is_negative": false,
                    "is_keep_going": false,
                    "is_wakeup": false,
                    "prompt_index": notification_base + 1 + i as u32,
                    "prompt_length": len,
                    "prompt_source": "system"
                });
                if let Some(e) = &effort {
                    input["effort_level"] = json!(e);
                }
                input["turn_origin"] = json!("task-notification");
                self.push(tn, "tengu_input_prompt", input);
            }
        }
    }

    /// 新输入那一串。
    pub(super) fn emit_new_prompt(&mut self) {
        let env = self.env;
        let shape = env.shape;
        let TurnFacts {
            kind,
            ref display_model,
            git_outcome,
            new_prompt,
            ref turn_origin,
            user_turn,
            prompt_index,
            v277,
            v280,
            v285,
            v291,
            injected,
            first_prompt_tpl,
            sleepy,
            ref ignored_suggestion,
            ref interrupted_message_id,
            ..
        } = *env.f;
        let EventEnv { t0, tpl, ref effort, ref effort_value, .. } = *env;
        if new_prompt {
            if injected {
                let mut queued = json!({
                    "feature_name": "queued_message_delivered",
                    "delivery": "turn_end",
                    "command_count": 1,
                    "prompt_count": u32::from(turn_origin == "peer")
                });
                if v285 {
                    queued["relay_count"] = json!(0);
                    queued["artifact_comment_count"] = json!(0);
                }
                queued["wait_ms"] = json!(6);
                self.push_dd(ms(t0, -12), "tengu_feature_ok", queued);
                // 模板那段不套，工具搜索判定照有（`cap/2.1.285` 06:57:29.600）。
                self.push(
                    ms(t0, -1),
                    "tengu_tool_search_mode_decision",
                    tool_search_decision(shape, display_model, kind.is_agent()),
                );
            } else if first_prompt_tpl {
                self.take_tpl(&tpl.prompt, t0);
                self.take_tpl(&tpl.first_prompt, t0);
            } else {
                self.take_tpl(&tpl.prompt_next, t0);
            }
            // 用户敲的报 `typed`，同伴会话发来的（peer）、后台任务完成的通知
            // （task-notification）这类由客户端自己注入的报 `system`（`cap/2.1.277` 1 条、
            // `cap/2.1.280` 2 条）。
            let typed = user_turn;
            let mut input = json!({
                "is_negative": false,
                "is_keep_going": false,
                // 2.1.260 那两份抓包首次输入是 true（把进程从等待里叫醒的那一次）；
                // 2.1.277 / 2.1.280 的 17 次输入全是 false，含每个会话的第一次。
                "is_wakeup": prompt_index == 1 && !v277
            });
            if !(v277 && turn_origin == "peer") {
                input["prompt_index"] = json!(prompt_index);
            }
            input["prompt_length"] = json!(shape.prompt_len);
            // `-p` 的输入报 `sdk`（`cap/auto-2.1.285-20260930` 十条）。
            input["prompt_source"] = json!(if shape.sdk {
                "sdk"
            } else if typed {
                "typed"
            } else {
                "system"
            });
            // 2.1.277 起没有 effort 的模型（haiku）整个键不出现（`cap/2.1.277`、`cap/2.1.280`
            // 各两条），之前的版本照旧报默认的 high。
            if !v277 || effort.is_some() {
                input["effort_level"] = json!(&effort_value);
            }
            if v277 {
                input["turn_origin"] = json!(&turn_origin);
            }
            if let Some(id) = &interrupted_message_id {
                input["interrupted_message_id"] = json!(id);
            }
            // 2.1.285 首次输入那条落在首条 api_query 前 35ms，排在模板里的附件、上下文宣告与
            // `artifact_*` 之前（`cap/2.1.285/00040` 06:34:16.504 / .539）；之后的输入是 -13ms 上下。
            //
            // `-p` 的首次输入跟着 SDK 模板走：启动那串一路排到 API 前 3.8s 上下（附件、记忆、上下文
            // 宣告都在模板里），输入落在上下文宣告前 8ms、`sleepy_snowflake_applied` 在它后 4ms
            // （`cap/auto-2.1.285-20260930/00448`：-3807 / -3799 / -3795，与模板同一批）。按 -35
            // 报会排到宣告之后 3.7s，顺序整个反了。
            let sdk_announce = (v285 && shape.sdk && first_prompt_tpl)
                .then(|| tpl.startup.iter().find(|e| e.name == "tengu_context_announcement"))
                .flatten()
                .map(|e| e.off);
            let input_at = match sdk_announce {
                Some(off) => off - 8,
                None if v285 && first_prompt_tpl => -35,
                None => -15,
            };
            if let Some((rid, shown_end, chars)) = &ignored_suggestion {
                let submit = ms(t0, if shape.bash_input { -57 } else { input_at - 3 });
                self.push(
                    submit,
                    "tengu_prompt_suggestion",
                    ignored_suggestion_meta(rid, *shown_end, *chars, submit, shape.prompt_len),
                );
            }
            if shape.bash_input {
                self.push(
                    ms(t0, -56),
                    "tengu_input_bash",
                    json!({ "powershell": false, "respond": true }),
                );
                // 那条命令当场在本机跑完（`07:58:39.375`，比输入晚 50ms）。
                if let Some((cmd, out)) = &shape.bash_typed {
                    let meta = bash_executed_meta(
                        &bash_profile(cmd),
                        *out + 1,
                        None,
                        false,
                        true,
                        shape.permission_mode,
                    );
                    self.push_dd_snake(ms(t0, -6), "tengu_bash_tool_command_executed", meta);
                }
            } else {
                self.push(ms(t0, input_at), "tengu_input_prompt", input);
            }
            // 每个模型头一次用于新输入时报一次（值恒为 growthbook / all）。
            if sleepy {
                self.push(
                    ms(t0, sdk_announce.map_or(-10, |off| off + 4)),
                    "tengu_sleepy_snowflake_applied",
                    json!({ "model": &display_model, "source": "growthbook", "value": "all" }),
                );
            }
            // auto 模式下每次输入先探一次工作区的 git 状态（`cap/2.1.280` 前三次输入是 auto，
            // 各一条；之后切回 default 就没有了），续轮等其余请求见下面。代理看不见客户端的
            // 工作目录，结果照抓包报。
            if v280 && shape.permission_mode == "auto" {
                self.push(ms(t0, -10), "tengu_auto_mode_git_state_probe", {
                    let mut probe = json!({
                        "duration_ms": u32::from(!prompt_index.is_multiple_of(3)),
                        "wait_ms": 0,
                        "outcome": git_outcome,
                        "truncated": false
                    });
                    // 2.1.291 多两项：会话第一次输入那条是进程里头一回探（`cap/auto-2.1.291-
                    // 20261006-full` 8 个会话各一条 true，其余 67 条 false）。
                    if v291 {
                        probe["first_in_process"] = json!(prompt_index == 1);
                        probe["repo_visibility_lookup"] = json!("none");
                    }
                    probe
                });
            }
        }
    }
}
