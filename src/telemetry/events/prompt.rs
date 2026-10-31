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
            // 2.1.312 去掉了 `is_eap`（`cap/auto-2.1.312-20261028-full` `--continue` 那条只剩 `outcome`）。
            let restore = if env.f.v312 {
                json!({ "outcome": "restored" })
            } else {
                json!({ "outcome": "restored", "is_eap": false })
            };
            self.push(t_res, "tengu_resume_model_restore", restore);
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
            // 心跳：敲 `/clear`、清空时与逐出提示同一毫秒报 `conversation_clear` 并清场（官方 `aFn`；状态机
            // 是进程的，新会话接着旧会话那台，见 [`ClearedPrev::hb`]）。清场那一刻还在旧会话上，
            // 见 [`ClearIds`]。
            let (old_parent, old_prompt) = cleared_prev
                .as_ref()
                .map(|p| (p.parent_session_id.clone(), p.prompt_id.clone()))
                .unwrap_or_default();
            let ids = ClearIds {
                old: Identity {
                    session_id: prev.clone(),
                    parent_session_id: old_parent,
                    ..base_identity.clone()
                },
                old_prompt,
                clearing: Identity {
                    session_id: prev.clone(),
                    parent_session_id: Some(prev.clone()),
                    ..base_identity.clone()
                },
                clear_prompt: uuid_v4(),
            };
            self.beat_at(ms(tc, -980), heartbeat::Trigger::UserInput, BeatAt::ClearInput);
            self.beat_at(tc, heartbeat::Trigger::Reset("conversation_clear"), BeatAt::Clearing);
            let (clearing, clear_prompt) = (ids.clearing.clone(), ids.clear_prompt.clone());
            self.cleared = Some(ids);
            let prompt = MetaStage::Prompt;
            self.push_as(
                tc,
                "tengu_cache_eviction_hint",
                &clearing,
                prompt,
                &clear_prompt,
                false,
                json!({ "scope": "conversation_clear", "last_request_id": prev_req.as_deref().unwrap_or("") }),
            );
            self.push_as(
                tc,
                "tengu_shell_set_cwd",
                &clearing,
                prompt,
                &clear_prompt,
                false,
                json!({ "success": true }),
            );
            // 新会话起来到下一次提交之前没有 prompt。
            let fresh = MetaStage::Renderer;
            self.push_as(
                ms(tc, 1),
                "tengu_session_start",
                base_identity,
                fresh,
                "",
                false,
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
                self.push_as(
                    ms(tc, off),
                    "tengu_feature_ok",
                    base_identity,
                    fresh,
                    "",
                    true,
                    feature(name),
                );
            }
            self.push_as(
                ms(tc, 6),
                "tengu_input_command",
                base_identity,
                fresh,
                "",
                false,
                json!({ "input": "clear", "invocation_trigger": "user-slash" }),
            );
        } else if is_new_session {
            self.take_tpl(&tpl.startup, t0);
        }
    }

    /// 这一轮到期的后台模板事件，以及进程启动 30 秒后那条 IDE 探测（`Session::take_ide_detect`）。
    pub(super) fn emit_background(&mut self) {
        let env = self.env;
        let TurnFacts { started_wall, ref background, ide_detect, .. } = *env.f;
        let EventEnv { tpl, .. } = *env;
        if !background.is_empty() {
            self.take_tpl(&tpl.background[background.clone()], started_wall.into());
        }
        if ide_detect {
            // 找 IDE 那个循环从启动段某处起算 30 秒，落在首条事件后 30.4 ~ 30.8 秒。Datadog 那份同发
            // （`cap/auto-2.1.285-20260930` 起每份抓包两边条数相同）。
            let dt = ms(started_wall.into(), IDE_DETECT_MS as i64 + (env.req_hash % 400) as i64);
            let meta = feature_with("ide_detect", json!({ "found": false }), env.f.v293);
            self.push_dd(dt, "tengu_feature_ok", meta);
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
            v296,
            safety_events,
            ..
        } = *env.f;
        let EventEnv { t0, .. } = *env;
        // 上一轮最后一条回复里的工具在权限弹框上被拒（`cap/auto-2.1.285-20260930` 08:00:57.305 弹框、
        // 08:01:01.376 按 Esc）：弹框、拒绝那几条、turn、`turn_end{aborted_tools}`，没有 stop hook；
        // 客户端不再发请求，直到用户敲下一句——代理只能在这条新输入里看出来，补在它前面。
        if !rejected_calls.is_empty() {
            // 这一整段都是上一轮的事：弹框、拒绝、文件建议刷新、turn 与 `turn_end`，官方挂的都是上一轮的 prompt
            // （`cap/auto-2.1.305-20261021-full` B1 .484 ~ .412 全是 1572cea5，下一轮提交之后才换）。
            self.prev_prompt_scope = true;
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
                // 弹框与被拒的 `unary_event`（`cap/auto-2.1.305-20261021-full` B1 的 Bash：.484 / .360），编辑类
                // 工具另记一次拒绝。
                let tool = (c.name.as_str(), &c.input);
                self.push_unary(shown, "response", tool, &prev_msg);
                self.push_unary(t_rej, "reject", tool, &prev_msg);
                self.note_edit_decision(tool, "reject", "user_reject");
                self.push_file_suggestions_refresh(t_rej);
                self.push_dd(t_rej, "tengu_feature_ok", feature("permission_user_deny"));
                self.push(t_rej, "tengu_permission_request_escape", json!({}));
                let mut can_use = json!({
                    "messageID": &prev_msg,
                    "toolName": &c.name,
                    "deniedBy": "user_reject",
                    "decisionReasonType": "unknown"
                });
                // 2.1.296 起在 `decisionReasonType` 之后多 PreToolUse 钩子与权限判定两段耗时（`cap/
                // auto-2.1.296-20261013-full` B1：`preToolHookDurationMs: 1`、`permissionDurationMs: 3178`，
                // 后者就是弹框挂着的那段，与下面 `waiting_for_user_permission_ms` 同一个量）。
                if v296 {
                    can_use["preToolHookDurationMs"] = json!(1);
                    can_use["permissionDurationMs"] = json!(wait);
                }
                can_use["queryChainId"] = json!(&prev_turn.0);
                can_use["queryDepth"] = json!(prev_main_depth);
                can_use["requestId"] = json!(&prev_req);
                if env.f.v312 {
                    can_use_tool_v312(&mut can_use, &c.id);
                }
                self.push(t_rej, "tengu_tool_use_can_use_tool_rejected", can_use);
                // 2.1.307 紧跟着一条 `tengu_safety_event`（`cap/auto-2.1.307-20261022-full` B1，同一毫秒）。
                if safety_events {
                    let safety =
                        safety_event(Denial::UserReject, &c.name, &c.id, shape.permission_mode);
                    self.push(t_rej, "tengu_safety_event", safety);
                }
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
            // 心跳：弹框把界面切到等用户；被拒后这一轮停手，等用户那段在 `turn_end` 之后 27ms 收
            // （`cap/auto-2.1.305-20261021-full` B1：.364 → .391），不再回到干活。这都是上一轮的事，挂上一轮的
            // prompt（B1 那三条都是 1572cea5，下一轮提交后的开工才换）。
            if let Some((c, _)) = rejected_calls.first() {
                self.beat_before_prompt(
                    ms(prev_end_dt, 11),
                    heartbeat::Trigger::Run(Some(heartbeat::awaiting_for(&c.name))),
                );
                self.beat_before_prompt(ms(t2, 27), heartbeat::Trigger::Run(None));
            }
            self.push_dd(t2, "tengu_feature_ok", feature("turn"));
            let started: DateTime<Utc> = prev_turn.2.unwrap_or(call.started_at).into();
            self.push(
                t2,
                "tengu_turn_end",
                env.turn_end("aborted_tools", (t2 - started).num_milliseconds().max(0), None),
            );
            self.prev_prompt_scope = false;
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
                let queued = queued_delivered(0, true, 3_655, env.f.v293);
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
            v293,
            v299,
            v311,
            injected,
            first_prompt_tpl,
            sleepy,
            heron,
            ref capability_inline,
            ref ignored_suggestion,
            ref interrupted_message_id,
            ..
        } = *env.f;
        let EventEnv { t0, tpl, ref effort, ref effort_value, .. } = *env;
        if new_prompt {
            if injected {
                let queued = queued_delivered(u32::from(turn_origin == "peer"), v285, 6, v293);
                self.push_dd(ms(t0, -12), "tengu_feature_ok", queued);
                // 模板那段不套，工具搜索判定照有（`cap/2.1.285` 06:57:29.600）。
                self.push(
                    ms(t0, -1),
                    "tengu_tool_search_mode_decision",
                    tool_search_decision(shape, display_model, kind.is_agent()),
                );
            } else {
                let seg = if first_prompt_tpl { &tpl.prompt } else { &tpl.prompt_next };
                // 文件历史的快照在模板那条 `snapshot_success` 处：上次备份后改过的先各备份一版。
                if let Some(e) =
                    seg.iter().find(|e| e.name == "tengu_file_history_snapshot_success")
                {
                    self.emit_snapshot_backups(ms(t0, e.off));
                }
                if first_prompt_tpl {
                    self.take_tpl(&tpl.prompt, t0);
                    self.take_tpl(&tpl.first_prompt, t0);
                } else {
                    self.take_tpl(&tpl.prompt_next, t0);
                }
            }
            // `tengu_lucky_cerf` 曝光：首次输入那段模板里有它（`-p` 的在启动那段），位置照模板；
            // 别的输入（中途切到精简提示词的模型、注入的一轮）模板里没有，这里按模板那条的偏移
            // （提交前 7ms）补。这一轮模板已经发过就不补。
            let from_tpl = self.tpl_events.iter().any(|(_, e)| {
                e["event_data"]["experiment_metadata"]
                    .as_str()
                    .is_some_and(|m| m.contains(crate::proxy::LUCKY_CERF_FEATURE))
            });
            if !from_tpl {
                for x in &self.subst.lucky_cerf {
                    let t = ms(t0, -7);
                    let ev = env.f.base_identity.growth_event(
                        t,
                        &x.key,
                        x.var,
                        x.feature,
                        &env.f.version,
                    );
                    self.tpl_events.push((t, ev));
                }
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
                    let mut meta = bash_executed_meta(
                        &bash_profile(cmd),
                        *out + 1,
                        None,
                        false,
                        true,
                        shape.permission_mode,
                    );
                    if v293 {
                        bash_meta_v293(&mut meta, "");
                    }
                    if v299 {
                        bash_meta_v299(&mut meta, cmd, "", &shape.model, true);
                    }
                    if v311 {
                        let h = i64::from(env.req_hash % 100);
                        bash_meta_v311(&mut meta, cmd, Some(ShellUsage::typed(h)));
                    }
                    self.push_dd_snake(ms(t0, -6), "tengu_bash_tool_command_executed", meta);
                }
            } else {
                self.push(ms(t0, input_at), "tengu_input_prompt", input);
            }
            // 心跳（交互式）：用户敲字那一刻报 `user_input`——比提交早一秒上下（`cap/auto-2.1.305-20261021-full`
            // 提交前 863 ~ 988ms，驱动先敲字、停 0.8 秒再回车；代理看不到敲了多久，按这个算）；这一轮开工，
            // 界面转入 `turn_running`，落在这条请求发出后几毫秒。客户端自己注入的一轮没有敲字。
            // 敲字那条挂上一轮 prompt（官方那时还没换），提交后的开工才是这一轮的。
            if !shape.sdk {
                if typed {
                    self.beat_before_prompt(ms(t0, input_at - 980), heartbeat::Trigger::UserInput);
                }
                self.beat(ms(t0, 5), heartbeat::Trigger::Run(Some(heartbeat::TURN_RUNNING)));
            }
            if let Some((len, from_client_data)) = heron {
                self.push(
                    ms(t0, sdk_announce.map_or(-11, |off| off + 3)),
                    "tengu_heron_brook_applied",
                    json!({ "len": len, "fromClientData": from_client_data }),
                );
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
        } else if let Some((len, from_client_data)) = heron {
            // 压缩之后接着的续轮（不是新输入）：段缓存清了，发请求前组装 system 时报。
            self.push(
                ms(t0, -11),
                "tengu_heron_brook_applied",
                json!({ "len": len, "fromClientData": from_client_data }),
            );
        }
        // 模型能力的来源事件没赶上模板那一处（会话中途切到被改了默认的模型）：组装这条 system 时报，在请求发出前。
        for (name, m) in capability_inline {
            self.push(ms(t0, -9), name, m.clone());
        }
    }
}

/// `tengu_feature_ok{queued_message_delivered}`：一条命令在上一轮收尾时送达。2.1.285 起多
/// `relay_count` / `artifact_comment_count`，2.1.293 起再多 `monitor_event_count` / `monitor_event_chars`
/// （官方无条件带，`cap/auto-2.1.293-20261008-full` 起各版 18/18）且 `feature_name` 排最后。
fn queued_delivered(prompt_count: u32, v285: bool, wait_ms: u64, v293: bool) -> Value {
    let mut props =
        json!({ "delivery": "turn_end", "command_count": 1, "prompt_count": prompt_count });
    if v285 {
        props["relay_count"] = json!(0);
        props["artifact_comment_count"] = json!(0);
    }
    if v293 {
        props["monitor_event_count"] = json!(0);
        props["monitor_event_chars"] = json!(0);
    }
    props["wait_ms"] = json!(wait_ms);
    feature_with("queued_message_delivered", props, v293)
}
