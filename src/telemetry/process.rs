//! 逐请求处理：把一条调用推成 [`TurnFacts`]，交给 [`build_events`] 拼事件，再入队。

use super::*;

impl Telemetry {
    /// 把一条调用变成事件入队。`allow_defer` 为真时，侧查询会先扣住等同会话的下一条主线程
    /// 请求（拿新一轮的 prompt id）；`prev_end_override` 是扣住时记下的「上一条结束时刻」，
    /// 补发时算 `timeSinceLastApiCallMs` 用，否则会被后来的主线程请求顶掉。
    pub(super) fn process(
        st: &mut State,
        call: ApiCall,
        allow_defer: bool,
        prev_end_override: Option<SystemTime>,
    ) {
        let Some(mut shape) = parse_shape(&call.body) else { return };
        if shape.quota_probe {
            // 官方不为它报任何事件，但它标着一次进程启动，见 [`State::process_starts`]。
            if let (Some(dev), Some(sid)) = (
                shape.device_id.clone(),
                shape.session_id.clone().or_else(|| call.session_header.clone()),
            ) {
                st.process_starts.insert((call.cred_id, dev), (sid, call.started_at));
            }
            return;
        }
        // 速度档以上游回报为准（fast 被限流时会回落到标准档）。
        if let Some(speed) = call.speed.as_deref() {
            shape.fast_mode = speed == "fast";
        }
        // 身份三件缺一不发：没有 device_id/session_id/account_uuid 的请求在官方那边根本
        // 不是订阅客户端的形态，替它报遥测只会造出一份自相矛盾的记录。
        let Some(device_id) = shape.device_id.clone() else { return };
        let Some(session_id) = shape.session_id.clone().or_else(|| call.session_header.clone())
        else {
            return;
        };
        let Some(account_uuid) = shape
            .account_uuid
            .clone()
            .or_else(|| call.account_uuid.clone())
            .filter(|a| !a.trim().is_empty())
        else {
            return;
        };
        let version = version_from_ua(&call.ua_out);
        let now = Instant::now();

        if let Some(org) = call.organization_id.as_deref().filter(|o| !o.is_empty()) {
            st.org_uuid.insert(call.cred_id, org.to_string());
        }
        // 子代理：billing header 里 `cc_is_subagent=true`，2.1.277 起还有 `x-claude-code-agent-id`。
        // 每个子代理是会话里的一条支线，状态按支线号分开记（见 [`AgentState`]）。
        let is_agent = shape.is_subagent || call.agent.agent_id.is_some();
        let agent_key = call.agent.agent_id.clone().unwrap_or_default();
        let thread_key = if is_agent { format!("agent:{agent_key}") } else { "main".to_string() };
        // 线程增量请求先补成客户端视角的全量，再往下判类别（见 [`ThreadBase`]）。
        if shape.thread_type.as_deref() == Some("continue")
            && let Some(base) = st
                .sessions
                .get(&(call.cred_id, session_id.clone()))
                .and_then(|s| s.thread_bases.get(&thread_key))
        {
            let auto_in_delta = shape.auto_marker || shape.permission_declared;
            base.fill(&mut shape, auto_in_delta);
        }
        let Lineage { continued_from, cleared_from, cleared_prev, parent_session_id } =
            st.resolve_lineage(&call, &shape, &session_id, &device_id);
        let identity = Identity {
            parent_session_id: parent_session_id.clone(),
            sdk: shape.sdk,
            session_id: session_id.clone(),
            device_id: device_id.clone(),
            account_uuid: account_uuid.clone(),
            organization_uuid: st.org_uuid.get(&call.cred_id).cloned(),
            subscription_type: subscription_type(call.org_type.as_deref()).to_string(),
            version: version.clone(),
            agent_id: if is_agent { call.agent.agent_id.clone() } else { None },
            vcs: shape
                .git_repo
                .or_else(|| {
                    st.sessions.get(&(call.cred_id, session_id.clone())).and_then(|s| s.git_repo)
                })
                .filter(|r| *r)
                .map(|_| "git"),
        };
        // 会话级的那份（待发批次、退出收尾、启动模板用）不带支线号。
        let base_identity = Identity { agent_id: None, ..identity.clone() };

        let kind = classify_kind(&call, &shape, is_agent);
        if kind.runs_in_default_mode() {
            shape.permission_mode = "default";
        }
        // 展示名：出站体里 `[1m]` 已经被客户端剥成 `context-1m` beta，这里还原回去。
        let has_1m = call.betas.as_deref().is_some_and(|b| b.contains("context-1m-"));
        let display_model =
            if has_1m { format!("{}[1m]", shape.model) } else { shape.model.clone() };
        let resp_model = call.resp_model.clone().unwrap_or_else(|| shape.model.clone());

        let key = (call.cred_id, session_id.clone());
        let is_new_session = !st.sessions.contains_key(&key);
        // 侧查询（标题生成等）先扣住：它没有 cc_prompt_id，真实客户端给它打的是紧随其后那条
        // 主线程请求的新一轮 id。等到那条再补发，或者超时按现有 id 发（见 [`Self::gc`]）。
        if allow_defer
            && matches!(kind, Kind::Title | Kind::Helper)
            && let Some(sess) = st.sessions.get_mut(&key)
        {
            let prev_end = sess.last_call_end;
            // 它确实已经完成了：后到的主线程请求算 `timeSinceLastApiCallMs` 时要以它为准
            // （抓包里主线程那条的 2529ms 量的正是到标题完成的距离）。
            let this_end = call.started_at + Duration::from_millis(call.total_ms);
            sess.last_call_end = Some(prev_end.map_or(this_end, |e| e.max(this_end)));
            sess.recent_ends.push_back(this_end);
            sess.deferred.push((call, prev_end, now));
            return;
        }
        // 同一个 id 在按退出收尾之后再出现 = `--resume`：新进程从头计数，只在指标上标 resume。
        let resumed = is_new_session && st.ended.remove(&key).is_some();
        let continued = continued_from.is_some();
        let device_key = (call.cred_id, device_id.clone());
        let device_default = st.device_default_model.get(&device_key).map(|(m, _)| m.clone());
        // 新会话的 `parent_session_id` 就是 `cleared_from`（见上面 `identity` 的取法）。
        let sess = st.sessions.entry(key.clone()).or_insert_with(|| {
            Session::fresh(
                &call,
                &identity,
                now,
                cleared_prev.as_ref(),
                device_default.clone(),
                &display_model,
            )
        });
        sess.last_seen = now;
        let (prev_reply_calls, git_outcome) =
            sess.absorb_request(&call, &mut shape, kind, &thread_key, &agent_key);
        // 一轮结束（`end_turn`）才有 stop hook 与 turn_end；`tool_use` 是同一轮的中间步。
        let turn_over = call.stop_reason.as_deref().is_none_or(|s| s != "tool_use");
        // 这条请求客户端那头是失败的：收尾走 `tengu_api_error` 那一串，且没有任何用量。
        let failed = call.failure.is_some();
        // 客户端取消（见 [`ApiCall::aborted`]）：没有 success 也没有 error，收尾是取消那一串。
        let aborted = call.aborted && !failed;
        let is_main = kind == Kind::Main;
        let flags = CallFlags { kind, is_main, turn_over, failed, aborted };
        let TurnStart {
            emit_first_turn,
            new_prompt,
            turn_skill,
            post_compaction,
            turn_origin,
            prev_turn,
            rejected_calls,
            user_turn,
            notification_base,
            notifications,
            prev_main_end,
            prompt_id,
        } = sess.advance_turn(&call, &shape, flags, &version, &prev_reply_calls);
        // 主线程与猜下一句走 `previousRequestId` 链；标题那类没有。
        //
        // **以出站体里那份 `cc_prev_req` 为准**（同 `cc_prompt_id` 的取法）：那是这条请求
        // 已经发给上游的声明，而会话状态是回程时另算的一份。两者本该恒等（`cap/2.1.260-2`
        // 三条续轮逐字相同），但它们的更新路径不同——`cc_prev_req` 由
        // [`crate::proxy::CcSessionLink::record`] 在 `ReqLog::drop` 里**同步**写，
        // 这里的 `last_main_request_id` 走 [`Telemetry::record`] 那条队列。让遥测复述请求
        // 自己说过的话，两份就不可能对不上；体里没有（会话首轮、没有 billing header 的
        // 来访）才回落到会话状态。
        // 子代理支线：首条请求建档（链、起点、拉起它的那条主线程请求），之后每条读它。
        // 摘要请求只读不推进。
        let agent = kind.is_agent().then(|| sess.agent_view(&call, &shape, &agent_key));
        let previous_request_id = kind
            .has_chain()
            .then(|| {
                shape.cc_prev_req.clone().or_else(|| match &agent {
                    Some(a) => a.last_request_id.clone(),
                    None => sess.last_main_request_id.clone(),
                })
            })
            .flatten();
        let prev_main_message_id = sess.last_main_message_id.clone();
        let prev_main_request_id = sess.last_main_request_id.clone();
        let prev_main_depth = sess.last_main_depth;
        // `timeSinceLastApiCallMs` = **这条完成时刻 − 上一条完成时刻**（不分主线程/侧查询，
        // 按完成先后）。`cap/2.1.260-2`：标题 09.490−06.166=3323、主线程 12.019−09.490=2529、
        // 续轮 19.458−12.019=7439、猜下一句 21.385−19.458=1927，全部对上；用「这条开始」算
        // 的话并发的标题与主线程会出负数、续轮只剩工具执行那一秒。
        let (this_end, prev_end, time_since_last) = sess.note_end(&call, prev_end_override);
        let message_tokens = match &agent {
            Some(a) => a.prev_total,
            None if kind.has_chain() => sess.prev_total_input,
            None => 0,
        };
        if kind == Kind::Main && !sess.main_model_seen {
            sess.main_model_seen = true;
            sess.default_model = display_model.clone();
        }
        let default_model = sess.default_model.clone();
        let device_default_update = if kind == Kind::ModelValidation && !failed {
            Some(resp_model.clone())
        } else if kind == Kind::Main && device_default.is_none() {
            Some(default_model.clone())
        } else {
            None
        };
        // 事件顶层 `model` 与 Datadog 的 `model` 是**会话主模型**（用户设置的那个），侧查询
        // 自己用的 haiku 只出现在 api 事件的 meta 里。
        let session_model = sess.last_model.clone().unwrap_or_else(|| sess.default_model.clone());
        let model_changed =
            is_main && sess.last_model.as_deref().is_some_and(|m| m != display_model);
        // 同理：`diagnostics.previous_message_id` 是这条请求自己声明的那个，优先于会话状态。
        let previous_message_id =
            shape.diag_prev_message_id.clone().or_else(|| sess.last_message_id.clone());
        let tools_slot = match &agent {
            Some(a) => &a.tools_hash,
            None if kind.has_boundary() => &sess.tools_hash_main,
            None => &sess.tools_hash_side,
        };
        let tools_changed = tools_slot.as_deref() != Some(shape.tools_hash.as_str());
        let counted = sess.counted;
        let started_wall = sess.started_wall;
        // queryDepth：主线程本轮第几次请求；猜下一句 = 主线程最后一次 + 2（抓包：0→2、1→3）；
        // 子代理从 2 起每条 +1、整条支线一个链（`cap/2.1.277` 2…27、`cap/2.1.280` 2…7），
        // 它的摘要请求恒为 3、链另起（两份抓包 7 条都是）。
        // 主线程当前的链：猜下一句的 fork 统计里引用的是这条父链。
        let main_chain = sess.chain_id.clone();
        let (chain_id, query_depth) = sess.chain_for(kind, agent.as_ref());
        let turn_started: DateTime<Utc> =
            sess.turn_started.unwrap_or(call.started_at - Duration::from_millis(15)).into();
        let first_text_in_turn = is_main && call.text_chars > 0 && !sess.turn_text_seen;
        // 一个字都还没出就被打断：首字事件报 `interrupted`（`cap/auto-2.1.285-20260930` 08:01:42.635）。
        let first_text_interrupted =
            is_main && aborted && call.text_chars == 0 && !sess.turn_text_seen;
        // 首字之前跑过的工具数 = 本轮此前的 + 这条续轮带回来的。
        let tool_calls_before = sess.turn_tool_calls + shape.tool_uses.len() as u32;
        let user_secs = sess.user_secs(&call, &shape, new_prompt);
        let shell_snapshot_first = is_main
            && !sess.shell_snapshot_done
            && shape.tool_uses.iter().any(|t| t.name == "Bash");
        let prompt_index = sess.prompt_index.max(1);
        let prompt_seq = sess.prompts_seen.max(1);

        // 版本分档。2.1.270 起 `tengu_api_success` 多了 `firstContentMs` / `clientRequestId` /
        // `snapshotHash`，`systemPromptSource` 分成 `live_recorded`（会话首条）与
        // `from_snapshot`（之后）；2.1.277 起多 `turn_origin`，没有 effort 的输入（haiku）
        // 不再报 `effort_level`。tether 那三条、`declared_tool_set_held`、
        // `sleepy_snowflake_applied` 在 2.1.270–2.1.277 之间键集合还在变（`rh`、
        // `claimedCollapse`、`drop*` 几项进进出出），只按 `cap/2.1.280` 的布局给 2.1.280 起。
        let v270 = version_at_least(&version, "2.1.270");
        let v277 = version_at_least(&version, "2.1.277");
        let v280 = version_at_least(&version, "2.1.280");
        // 2.1.285：`tengu_api_success` 多十项（`dispatch`、`echoWireToolInputs`、未覆盖尾段那六项、
        // `queryOverheadMs`、`requestPrepareMs`），tether 两条多 `creditRetryStateless` /
        // `toolResultClearingHeldStateless`，auto 模式不再把请求钉成无状态（`cap/2.1.285`）。
        let v285 = version_at_least(&version, "2.1.285");
        // 客户端自己注入的一轮（后台任务通知 / 同伴会话）：没有提交、粘贴、渲染那几条，只有
        // 一条排队消息送达（`cap/2.1.280` 两条、`cap/2.1.285` 06:57:29.588）。
        let injected = new_prompt && version_at_least(&version, "2.1.277") && !user_turn;
        let first_prompt_tpl = new_prompt && !injected && !sess.first_prompt_tpl_done;
        if first_prompt_tpl {
            sess.first_prompt_tpl_done = true;
        }
        // 后台任务那段：进程启动后多久该出现的，这条请求发出时已经过了就补上（每条一次）。
        let background = sess.take_background(&version, shape.sdk, call.started_at);
        let first_main = v280 && is_main && !failed && !sess.first_main_done;
        if first_main {
            sess.first_main_done = true;
        }
        // 快照一条线一份：主线程（含猜下一句）一份，每个子代理（含它的摘要请求）各一份
        // （`cap/2.1.280` 主线程 `b06e…`/`d6bc…`、Explore 子代理 `7d8050a24c2c`，子代理首条
        // 同样报 `live_recorded`）。
        let snapshot =
            (v270 && kind.has_boundary()).then(|| sess.snapshot_for(kind, &agent_key, &shape));
        let sleepy = v280 && new_prompt && !sess.sleepy_models.contains(&display_model);
        if sleepy {
            sess.sleepy_models.push(display_model.clone());
        }
        // tether 只管主线程与子代理（摘要、猜下一句、标题这类辅助调用一条都没有），子代理
        // 每条支线一个线程（`cap/2.1.280` Explore 首条 `create/first_request`、之后 `continue/append`）。
        let tether = (v280 && matches!(kind, Kind::Main | Kind::Subagent))
            .then(|| sess.tether_for(&call, &shape, &display_model, is_main, &agent_key));
        // 这条线程的全量形态记下来，给下一条增量请求补（见 [`ThreadBase`]）。
        if matches!(kind, Kind::Main | Kind::Subagent) && shape.tools_count > 0 {
            sess.thread_bases.insert(
                thread_key.clone(),
                ThreadBase::of(&shape, call.reply_input_chars, call.tool_use_lens.clone()),
            );
        }

        let marks =
            TurnMarks { new_prompt, user_turn, first_text_in_turn, shell_snapshot_first, this_end };
        let CallRecord {
            usage_before,
            ignored_suggestion,
            interrupted_message_id,
            handback,
            agent_done,
        } = sess.record_call(&call, &shape, flags, marks, &display_model, &agent_key);
        sess.counted = true;
        let ctx_model = if is_main { display_model.clone() } else { session_model };
        let dd_model = ctx_model.trim_end_matches("[1m]").to_string();

        let mut file_sizes = std::mem::take(&mut sess.file_sizes);
        // 这一轮到这条为止跑过的工具数（`-p` 收尾那条 `tengu_sdk_result.tool_use_count`）。
        let sess_turn_tools = sess.turn_tool_calls;
        let (sess_turn_api_calls, sess_turn_api_ms) = (sess.turn_api_calls, sess.turn_api_ms);
        let session_cwd = sess.cwd.clone();
        let betas_full = call.betas.clone().unwrap_or_default();
        let betas_own = session_betas(&betas_full);
        // 子代理支线上的事件顶层 `betas` 报**主线程**那份，只有它自己的 `api_query` /
        // `api_success` / `agent_tool_completed` 报它这条请求的（`cap/2.1.285`：haiku 子代理 251
        // 条事件里 226 条是主线程那份 `claude-code…mid-conversation-system`，那三类 25 条是
        // haiku 的 `oauth,interleaved…`；`cap/2.1.280` 的 Explore 与主线程同模型，两份本来相同）。
        if is_main {
            sess.main_betas = betas_own.clone();
        }
        // 这条线程上第几条请求（0 起）：主线程数会话里的主线程请求，子代理数它那条支线的步数。
        let thread_step = if is_main {
            let n = sess.main_requests;
            sess.main_requests += 1;
            Some(n)
        } else if kind == Kind::Subagent {
            agent.as_ref().map(|a| a.steps)
        } else {
            None
        };
        let betas_session = if kind.is_agent() && !sess.main_betas.is_empty() {
            sess.main_betas.clone()
        } else {
            betas_own.clone()
        };

        let facts = TurnFacts {
            device_id,
            session_id,
            account_uuid,
            version,
            now,
            continued_from,
            cleared_from,
            cleared_prev,
            identity,
            base_identity,
            kind,
            has_1m,
            display_model,
            resp_model,
            key,
            is_new_session,
            resumed,
            continued,
            device_key,
            git_outcome,
            turn_over,
            failed,
            aborted,
            emit_first_turn,
            is_main,
            new_prompt,
            turn_skill,
            post_compaction,
            turn_origin,
            prev_turn,
            rejected_calls,
            user_turn,
            notification_base,
            notifications,
            prev_main_end,
            prompt_id,
            agent,
            previous_request_id,
            prev_main_message_id,
            prev_main_request_id,
            prev_main_depth,
            prev_end,
            time_since_last,
            message_tokens,
            default_model,
            device_default_update,
            model_changed,
            previous_message_id,
            tools_changed,
            counted,
            started_wall,
            main_chain,
            chain_id,
            query_depth,
            turn_started,
            first_text_in_turn,
            first_text_interrupted,
            tool_calls_before,
            user_secs,
            shell_snapshot_first,
            prompt_index,
            prompt_seq,
            v270,
            v277,
            v280,
            v285,
            injected,
            first_prompt_tpl,
            background,
            first_main,
            snapshot,
            sleepy,
            tether,
            usage_before,
            ignored_suggestion,
            interrupted_message_id,
            handback,
            agent_done,
            ctx_model,
            dd_model,
            sess_turn_tools,
            sess_turn_api_calls,
            sess_turn_api_ms,
            session_cwd,
            betas_full,
            betas_own,
            thread_step,
            betas_session,
        };
        let BuiltEvents { events, dd, probe_side } =
            build_events(&call, &shape, &facts, &mut file_sizes);
        let TurnFacts {
            device_id,
            session_id,
            account_uuid,
            version,
            now,
            identity,
            base_identity,
            kind,
            display_model,
            key,
            is_new_session,
            resumed,
            continued,
            device_key,
            turn_over,
            failed,
            aborted,
            is_main,
            prompt_id,
            device_default_update,
            counted,
            started_wall,
            user_secs,
            v285,
            betas_session,
            ..
        } = facts;

        // 启动握手**不在这里排队**。这里是回程（`ReqLog` 收尾之后），排在这儿等于让上游
        // 先看到一条 messages、几秒后才看到这个「会话」的启动流量——顺序整个反了。
        // 现在由转发路径在**首条请求发出之前**直接开跑，见
        // [`crate::proxy::spawn_session_handshake`]；那里还能分辨模拟与真实 CC，后者自己
        // 会打这一串，luban 不该重复。
        let _ = is_new_session;

        if let Some(m) = device_default_update {
            st.device_default_model.insert(device_key, (m, now));
        }
        if let Some((probe_identity, out)) = probe_side {
            let p =
                st.pending.entry((call.cred_id, probe_identity.session_id.clone())).or_default();
            p.version = version.clone();
            p.subscription_type = probe_identity.subscription_type.clone();
            p.model = display_model.clone();
            p.betas = betas_session.clone();
            p.identity = Some(probe_identity);
            p.push_batch(out, now);
        }
        if let Some(sess) = st.sessions.get_mut(&key) {
            sess.file_sizes = file_sizes;
        }

        // ---- 入队 ----
        let pending = st.pending.entry((call.cred_id, session_id.clone())).or_default();
        pending.version = version;
        pending.subscription_type = identity.subscription_type.clone();
        if let Some(vcs) = identity.vcs {
            pending.backfill_vcs(vcs);
        }
        pending.identity = Some(base_identity.clone());
        // **模型 / beta / prompt_id 只跟主线程走**（第一条就是侧查询时先占个位）。
        //
        // 这三项是导出指标时那条 `tengu_feature_ok{internal_metrics_export}` 的上下文，
        // 代表的是「这个会话」。被一条标题生成（haiku + structured-outputs、且没有
        // `cc_prompt_id`）覆盖之后，导出事件报的就成了 haiku 与标题那套 beta——而同一批
        // 指标里的 `model` 属性仍是会话主模型，自相矛盾。
        if is_main || pending.model.is_empty() {
            pending.model = display_model.clone();
            pending.betas = betas_session.clone();
            pending.prompt_id = prompt_id.clone();
        }
        pending.started_wall = Some(started_wall);
        pending.push_batch((events, dd), now);
        pending.metrics_since.get_or_insert(now);
        pending.metrics.push(CallMetric {
            session_id,
            device_id,
            account_uuid,
            model: display_model,
            category: kind.category(),
            effort: shape.effort.clone(),
            agent_name: (v285 && kind == Kind::Subagent)
                .then(|| call.agent.agent_type.clone())
                .flatten()
                .filter(|t| !t.is_empty()),
            cost: call.cost_usd.unwrap_or(0.0),
            input: call.input_tokens,
            output: call.output_tokens,
            cache_read: call.cache_read_tokens,
            cache_creation: call.cache_creation_tokens,
            cli_secs: if is_main && turn_over { call.total_ms as f64 / 1000.0 } else { 0.0 },
            user_secs,
            new_session: is_new_session && !counted,
            resumed: resumed || continued,
            continued,
            usage: !failed && !aborted,
        });

        // 主线程请求到了：新一轮的 prompt id 已经写进会话，把扣住的侧查询补发出去。
        if is_main {
            Self::replay_deferred(st, &key);
        }
    }

    /// 把某会话扣住的侧查询按顺序补发（主线程请求到了，或扣得太久）。
    pub(super) fn replay_deferred(st: &mut State, key: &(i64, String)) {
        let deferred =
            st.sessions.get_mut(key).map(|s| std::mem::take(&mut s.deferred)).unwrap_or_default();
        for (c, prev_end, _) in deferred {
            Self::process(st, c, false, prev_end);
        }
    }
}

/// 这条请求的角色与收尾方式，[`Session::advance_turn`] / [`Session::record_call`] 共用。
#[derive(Clone, Copy)]
struct CallFlags {
    kind: Kind,
    is_main: bool,
    /// 一轮结束（`end_turn`）；`tool_use` 是同一轮的中间步。
    turn_over: bool,
    failed: bool,
    aborted: bool,
}

/// [`Session::advance_turn`] 推进这一轮之后得到的量。
struct TurnStart {
    emit_first_turn: bool,
    new_prompt: bool,
    turn_skill: Option<String>,
    post_compaction: bool,
    turn_origin: String,
    prev_turn: (String, u32, Option<SystemTime>),
    rejected_calls: Vec<(ToolCall, ToolResultInfo)>,
    user_turn: bool,
    notification_base: u32,
    notifications: Vec<usize>,
    prev_main_end: Option<SystemTime>,
    prompt_id: String,
}

/// [`Session::record_call`] 要的这一轮里的几样标记。
#[derive(Clone, Copy)]
struct TurnMarks {
    new_prompt: bool,
    user_turn: bool,
    first_text_in_turn: bool,
    shell_snapshot_first: bool,
    this_end: SystemTime,
}

/// [`Session::record_call`] 更新会话之前取下的量。
struct CallRecord {
    usage_before: [i64; 4],
    ignored_suggestion: Option<(String, SystemTime, usize)>,
    interrupted_message_id: Option<String>,
    handback: Option<ToolCall>,
    agent_done: Option<AgentState>,
}

impl Session {
    /// 把这条请求里与会话相关的形态记下、并按会话补全：给续轮带回的工具结果配上上一条回复的
    /// 调用、记下工作目录与 git 状态、对齐权限模式。返回上一条回复的工具调用与 git 探测结果。
    fn absorb_request(
        &mut self,
        call: &ApiCall,
        shape: &mut RequestShape,
        kind: Kind,
        thread_key: &str,
        agent_key: &str,
    ) -> (Vec<ToolCall>, &'static str) {
        // 续轮带回的工具结果配上一条回复里记下的调用：thread 续轮全靠它，全量请求也拿它补上判决。
        if let Some(calls) = self.reply_calls.get(thread_key) {
            for tu in shape.tool_uses.iter_mut() {
                if let Some(c) = calls.iter().find(|c| c.id == tu.id) {
                    tu.apply_call(&c.name, &c.input, c.verdict.as_ref());
                }
            }
        }
        // 上一条回复的工具调用：这条新输入若带着被拒的工具结果，要用它认出是哪个工具。
        let prev_reply_calls = self.reply_calls.get(thread_key).cloned().unwrap_or_default();
        if matches!(kind, Kind::Main | Kind::Subagent) && !call.aborted {
            self.reply_calls.insert(thread_key.to_string(), call.tool_calls.clone());
        }
        if shape.cwd.is_some() {
            self.cwd = shape.cwd.clone();
        }
        if shape.git_repo.is_some() {
            self.git_repo = shape.git_repo;
        }
        if shape.git_dirty.is_some() {
            self.git_dirty = shape.git_dirty;
        }
        // auto 模式那条 git 状态探测报的结果：仓库里有改动 `dirty`、没有 `clean`，不是仓库才
        // `not_a_repo`（`cap/auto-2.1.285-20260930` 75 条全是 dirty）。
        let git_outcome = match (self.git_repo, self.git_dirty) {
            (Some(true), Some(false)) => "clean",
            (Some(true), _) => "dirty",
            _ => "not_a_repo",
        };
        // 客户端跑 Bash 之前把多余的 `cd <工作目录> && ` 去掉（`cap/auto-2.1.285-20260930`：
        // 官方的 `bashCommandLen` / `toolInputSizeBytes` 恰好短这 117 字，`has_chain` 也因此为 false）。
        if let Some(cwd) = self.cwd.as_deref() {
            let prefix = format!("cd {cwd} && ");
            for tu in shape.tool_uses.iter_mut().filter(|t| t.name == "Bash") {
                let stripped = tu
                    .input
                    .get("command")
                    .and_then(|c| c.as_str())
                    .and_then(|c| c.strip_prefix(prefix.as_str()))
                    .map(str::to_string);
                if let Some(cmd) = stripped {
                    tu.input["command"] = json!(cmd);
                    tu.input_len = tool_input_len("Bash", &tu.input);
                    tu.command_len = js_len(&cmd);
                }
            }
        }
        if kind == Kind::Main {
            self.main_permission = shape.permission_mode;
        } else if kind == Kind::WebSearchTool
            || (kind.is_agent() && !shape.auto_marker && !shape.permission_declared)
        {
            // WebSearch 那条与没自带模式的子代理跟主线程走。
            shape.permission_mode = self.main_permission;
        }
        // 内置的 claude-code-guide 子代理自带 `dontAsk` 权限模式，它与它的摘要请求都这么报
        // （`cap/2.1.285` 八条，同一会话主线程是 default）；Explore 这类跟着主线程走
        // （`cap/2.1.280` 七条 auto）。
        let agent_type = call
            .agent
            .agent_type
            .clone()
            .filter(|t| !t.is_empty())
            .or_else(|| self.agents.get(agent_key).and_then(|a| a.agent_type.clone()));
        if kind.is_agent() && agent_type.as_deref() == Some("claude-code-guide") {
            shape.permission_mode = "dontAsk";
        }
        (prev_reply_calls, git_outcome)
    }

    /// 推进这一轮：首轮标记、会话版本与 beta、新输入的计数与链、本轮发起方、被拒的工具与通知、
    /// prompt id。
    fn advance_turn(
        &mut self,
        call: &ApiCall,
        shape: &RequestShape,
        flags: CallFlags,
        version: &str,
        prev_reply_calls: &[ToolCall],
    ) -> TurnStart {
        let CallFlags { kind, is_main, turn_over, failed, aborted } = flags;
        // 首轮那串版本检查是「一轮跑完了」才发的，失败的那轮不算。
        let emit_first_turn = kind == Kind::Main && turn_over && !failed && !self.first_turn_done;
        if emit_first_turn {
            self.first_turn_done = true;
        }
        // 版本跟着每条走没问题（同一会话所有请求同一个 UA），但 **`betas` 只跟主线程**。
        //
        // 会话级 beta 是「这个会话」的属性，而侧查询（标题生成用 haiku + structured-outputs、
        // 安全分类用 auto-mode-classifier）各有一套完全不同的 beta。无条件覆盖之后，从侧
        // 查询结束到下一条主请求之间，[`Telemetry::latest_session`]（保活挂身份用）与指标
        // 导出那条 `tengu_feature_ok{internal_metrics_export}` 报的就是标题生成的 beta ——
        // 一个「会话主模型是 opus、会话 beta 却是标题生成那套」的组合，官方不产生。
        //
        // 会话第一条就是侧查询时还是要写一次，否则整个会话的 beta 都是空的。
        self.version = version.to_string();
        if is_main || self.betas.is_empty() {
            self.betas = session_betas(call.betas.as_deref().unwrap_or(""));
        }
        // 新一轮用户输入（只有主线程算）：prompt 计数 +1、换 prompt_id（优先用 billing header
        // 里客户端自己的）与 queryChainId、depth 归零。tool_result 续轮沿用上一轮的，depth +1。
        // 侧查询（标题、猜下一句）不动这些计数。
        let new_prompt = is_main && (shape.new_prompt || self.prompts_seen == 0);
        // 本轮发起方：请求自己声明的优先（新输入与续轮都带），没带就沿用本轮的。
        if let Some(o) = shape.turn_origin.as_deref() {
            if is_main {
                self.turn_origin = turn_origin_of(o);
            }
        } else if new_prompt {
            self.turn_origin = "human".to_string();
        }
        // `!` 跑的命令那一轮与 `-p` 的输入官方都不打标：billing header 写 `human` / `sdk`，事件报
        // `unstamped`（`cap/auto-2.1.285-20260930/00191`、`00441` 等十条）。
        if is_main && (shape.sdk || (new_prompt && shape.bash_input)) {
            self.turn_origin = "unstamped".to_string();
        }
        if new_prompt {
            self.turn_skill = shape.command_skill.clone();
        }
        let turn_skill = if is_main { self.turn_skill.clone() } else { None };
        // 压缩之后的第一条主线程请求：快照重录、对话 token 从零算、报 `isPostCompaction`
        // （`00178`）。
        let post_compaction = is_main && std::mem::take(&mut self.post_compact);
        if post_compaction {
            self.snapshot_hash = None;
            self.prev_total_input = 0;
        }
        if kind == Kind::Compact && !failed {
            self.post_compact = true;
        }
        let turn_origin = self.turn_origin.clone();
        // 上一轮的链、深度与起点：这次新输入前若有工具被用户拒了，拒绝那串与 `aborted_tools`
        // 的收尾挂在上一轮上。
        let prev_turn = (self.chain_id.clone(), self.turn_depth, self.turn_started);
        let rejected_calls: Vec<(ToolCall, ToolResultInfo)> = if new_prompt && is_main {
            shape
                .rejected
                .iter()
                .filter_map(|r| {
                    let c = prev_reply_calls.iter().find(|c| c.id == r.id)?;
                    Some((c.clone(), r.clone()))
                })
                .collect()
        } else {
            Vec::new()
        };
        // 用户自己起的一轮：敲的（`human`），或官方不打标的 `!` 命令与 `-p` 输入（`unstamped`）。
        // 同伴会话、后台任务通知这类客户端注入的一轮不算。
        let user_turn = matches!(turn_origin.as_str(), "human" | "unstamped");
        // 夹在这次输入里的后台任务通知各占一个输入号，排在这次输入前面（见
        // [`RequestShape::merged_notifications`]）。
        let notification_base = self.prompt_index;
        let notifications = if new_prompt && user_turn && is_main {
            shape.merged_notifications.clone()
        } else {
            Vec::new()
        };
        self.prompt_index += notifications.len() as u32;
        let prev_main_end = self.last_main_end;
        if is_main && !aborted {
            self.last_main_end = Some(this_end_wall(call));
        }
        if new_prompt {
            // 同伴会话发来的一轮（`peer`）不算用户的第几次输入：官方那条 input_prompt 不带
            // `prompt_index`，下一次输入接着原来的数（`cap/2.1.280`：…2、peer、3）。
            // `!` 跑的命令那一轮报 `input_bash`，同样不占输入号（`cap/auto-2.1.285-20260930` 07:58:47
            // 那次输入是 22，跳过了前面那条 `!git log`）。
            if turn_origin != "peer" && !shape.bash_input {
                self.prompt_index += 1;
            }
            self.prompts_seen += 1;
            self.chain_id = uuid_v4();
            self.turn_depth = 0;
            // 换 `turn_started` 之前先把上一轮的提交时刻挪走：`user_secs` 的窗口下界要它。
            self.prev_prompt_submit = self.turn_started;
            self.turn_started = Some(call.started_at - Duration::from_millis(15));
            self.turn_text_seen = false;
            self.turn_tool_calls = 0;
            self.turn_api_calls = 0;
            self.turn_api_ms = 0;
        }
        if let Some(pid) = shape.cc_prompt_id.clone() {
            self.prompt_id = pid;
        } else if self.prompt_id.is_empty() {
            self.prompt_id = uuid_v4();
        }
        let prompt_id = self.prompt_id.clone();
        TurnStart {
            emit_first_turn,
            new_prompt,
            turn_skill,
            post_compaction,
            turn_origin,
            prev_turn,
            rejected_calls,
            user_turn,
            notification_base,
            notifications,
            prev_main_end,
            prompt_id,
        }
    }

    /// 子代理支线：首条请求建档（链、起点、拉起它的那条主线程请求），之后每条读它。
    /// 摘要请求只读不推进。
    fn agent_view(&mut self, call: &ApiCall, shape: &RequestShape, agent_key: &str) -> AgentView {
        let spawn = self.last_spawn_request_id.clone();
        let a = self.agents.entry(agent_key.to_string()).or_default();
        if a.agent_type.is_none() {
            a.agent_type = call.agent.agent_type.clone().filter(|t| !t.is_empty());
        }
        if a.chain_id.is_empty() {
            a.chain_id = uuid_v4();
            a.started = Some(call.started_at);
            a.prompt_chars = shape.prompt_len;
            a.invoking_request_id = spawn;
        }
        AgentView {
            chain_id: a.chain_id.clone(),
            steps: a.steps,
            last_request_id: a.last_request_id.clone(),
            last_message_id: a.last_message_id.clone(),
            prev_total: a.prev_total,
            tools_hash: a.tools_hash.clone(),
            invoking_request_id: (a.steps == 0).then(|| a.invoking_request_id.clone()).flatten(),
        }
    }

    /// 记下这条的完成时刻，返回 `(这条完成时刻, 上一条完成时刻, timeSinceLastApiCallMs)`。
    fn note_end(
        &mut self,
        call: &ApiCall,
        prev_end_override: Option<SystemTime>,
    ) -> (SystemTime, Option<SystemTime>, Option<u64>) {
        let this_end = call.started_at + Duration::from_millis(call.total_ms);
        let prev_end: Option<SystemTime> = prev_end_override.or(self.last_call_end);
        // 并发的请求（子代理摘要与子代理本身、主线程与后台的回顾）谁先结束说不准：取在这条之前
        // 结束的最近一条（`cap/2.1.285` 的 agent_summary 两条、主线程续轮都报了这一项）。
        let prev_done = prev_end_override
            .or_else(|| self.recent_ends.iter().filter(|t| **t < this_end).max().copied())
            .or(prev_end);
        let time_since_last =
            prev_done.and_then(|t| this_end.duration_since(t).ok()).map(|d| d.as_millis() as u64);
        self.recent_ends.push_back(this_end);
        while self.recent_ends.len() > 16 {
            self.recent_ends.pop_front();
        }
        (this_end, prev_end, time_since_last)
    }

    /// 这条请求的 `queryChainId` 与 `queryDepth`。
    fn chain_for(&self, kind: Kind, agent: Option<&AgentView>) -> (String, u32) {
        match kind {
            Kind::Main => (self.chain_id.clone(), self.turn_depth),
            Kind::Suggestion | Kind::AwaySummary => (uuid_v4(), self.last_main_depth + 2),
            Kind::Subagent => {
                let a = agent.expect("subagent calls carry agent state");
                (a.chain_id.clone(), 2 + a.steps)
            }
            Kind::AgentSummary => (uuid_v4(), 3),
            Kind::SideQuestion | Kind::Compact => (uuid_v4(), 1),
            Kind::Title
            | Kind::Helper
            | Kind::WebFetchApply
            | Kind::WebSearchTool
            | Kind::RenameName
            | Kind::ModelValidation => (String::new(), 0),
        }
    }

    /// `active_time.total{type:user}`：用户敲这条输入花掉的时间。
    ///
    /// 官方的口径不是「上一条请求结束到这次提交」，而是**输入框里每次改动之间的间隔之和**
    /// （`ActivityTracker.recordUserActivity`：每个按键/粘贴/提交各记一次，只累加
    /// 间隔小于 `USER_ACTIVITY_TIMEOUT_MS` = 5s 的那些，且 CLI 忙着的时候不算）。
    /// 也就是说它量的是「打字时长」，与那一轮 API 花了多久无关——旧口径取 CLI 时长的
    /// 一个比例是量错了对象。
    ///
    /// 代理这一侧看不见按键，但看得见输入的**字数**，而打字时长就是它的线性函数。
    /// 三份抓包（一次输入 2 字 → 0.878s / 2 字 → 1.118s / 3 字 + 20 字 → 3.988s 合计）
    /// 拟合出 `0.8 + 0.1 × 字数`：2 字 → 1.0（实测均值 0.998）、3 字 + 20 字 → 1.1 + 2.8
    /// = 3.9（实测 3.988）。0.8s 是「上一次活动到第一个按键」加「最后一个按键到提交」
    /// 那两段，0.1s/字 ≈ 10 字/秒。
    ///
    /// 再按可用窗口截断：窗口是**上一次提交到这次提交**（会话第一条则从进程起点算），
    /// 不能是「上一条请求结束到这次提交」——用户会边看回复边打字，抓包里第二次输入
    /// 贡献的 2.9s 就大于上一轮结束之后剩下的那 2.2s。粘贴一大段时估算值会顶到窗口上限，
    /// 方向也是对的（官方那边粘贴只记一次改动，只有两段间隔）。
    fn user_secs(&self, call: &ApiCall, shape: &RequestShape, new_prompt: bool) -> f64 {
        if !new_prompt {
            0.0
        } else {
            let submit = call.started_at - Duration::from_millis(15);
            let typed = 0.8 + 0.1 * shape.prompt_len as f64;
            let base = self.prev_prompt_submit.unwrap_or(self.started_wall);
            let window = submit.duration_since(base).map_or(typed, |d| d.as_secs_f64());
            typed.min(window).max(0.0)
        }
    }

    /// 后台任务那段：进程启动后多久该出现的，这条请求发出时已经过了就补上（每条一次）。
    fn take_background(
        &mut self,
        version: &str,
        sdk: bool,
        started_at: SystemTime,
    ) -> std::ops::Range<usize> {
        let bg = &template_for(version, sdk).background;
        let from = self.background_done;
        let mut to = from;
        while to < bg.len()
            && self.started_wall + Duration::from_millis(bg[to].off.max(0) as u64) <= started_at
        {
            to += 1;
        }
        self.background_done = to;
        from..to
    }

    /// 这条线（主线程或某个子代理）的快照 hash：头一次录下（`live_recorded`），之后沿用。
    fn snapshot_for(
        &mut self,
        kind: Kind,
        agent_key: &str,
        shape: &RequestShape,
    ) -> (&'static str, String) {
        let slot = if kind.is_agent() {
            &mut self.agents.get_mut(agent_key).expect("agent state exists").snapshot_hash
        } else {
            &mut self.snapshot_hash
        };
        let recorded = slot.is_none();
        let hash = slot.get_or_insert_with(|| snapshot_hash_of(shape)).clone();
        (if recorded { "live_recorded" } else { "from_snapshot" }, hash)
    }

    /// 这条线的 tether 判定，并把线程状态记成这条的。
    fn tether_for(
        &mut self,
        call: &ApiCall,
        shape: &RequestShape,
        display_model: &str,
        is_main: bool,
        agent_key: &str,
    ) -> Tether {
        let slot = if is_main {
            &mut self.tether_main
        } else {
            &mut self.agents.get_mut(agent_key).expect("agent state exists").tether
        };
        let betas = call.betas.clone().unwrap_or_default();
        let t = tether_decide(slot.as_ref(), shape, display_model, &betas, is_main);
        *slot = Some(TetherThread {
            model: display_model.to_string(),
            betas,
            effort: shape.effort.clone(),
            tools_hash: shape.tools_hash.clone(),
            thinking_type: shape.thinking_type.clone(),
            messages: shape.messages_len,
            turns: t.turns,
        });
        t
    }

    /// 这条处理完，更新会话状态给下一条用。返回处理这条要的那几样「更新之前」的量。
    fn record_call(
        &mut self,
        call: &ApiCall,
        shape: &RequestShape,
        flags: CallFlags,
        marks: TurnMarks,
        display_model: &str,
        agent_key: &str,
    ) -> CallRecord {
        let CallFlags { kind, is_main, turn_over, failed, aborted } = flags;
        let TurnMarks { new_prompt, user_turn, first_text_in_turn, shell_snapshot_first, this_end } =
            marks;
        // 更新会话状态给下一条用。
        // 补发的侧查询比后来的主线程请求结束得早，别把「最近一次结束」往回拨。
        self.last_call_end = Some(self.last_call_end.map_or(this_end, |e| e.max(this_end)));
        // 被取消的那条不进链：官方下一条的 `previousRequestId` / `previousMessageId` 仍指它之前
        // 那条（`cap/auto-2.1.285-20260930/00264`）。
        if !aborted {
            self.last_message_id = call.message_id.clone().or(self.last_message_id.take());
        }
        // 「猜下一句」出的建议：有正文就挂着，等用户下一次输入（或 `/compact`、`/btw` 这类斜杠
        // 命令）时报 ignored；这条输入把它消费掉。
        let usage_before = self.usage_totals;
        if !failed && !aborted {
            for (t, v) in self.usage_totals.iter_mut().zip([
                call.input_tokens,
                call.output_tokens,
                call.cache_read_tokens,
                call.cache_creation_tokens,
            ]) {
                *t += v;
            }
        }
        let ignored_suggestion =
            if (new_prompt && user_turn) || matches!(kind, Kind::Compact | Kind::SideQuestion) {
                self.shown_suggestion.take()
            } else {
                None
            };
        if kind == Kind::Suggestion && !failed && !aborted && call.text_chars > 0 {
            self.shown_suggestion =
                Some((call.request_id.clone().unwrap_or_default(), this_end, call.text_chars));
        }
        let interrupted_message_id =
            if new_prompt { self.interrupted_message_id.take() } else { None };
        if is_main && aborted {
            self.interrupted_message_id = call.message_id.clone();
        }
        if is_main && !aborted {
            self.last_main_request_id =
                call.request_id.clone().or(self.last_main_request_id.take());
            self.last_main_message_id =
                call.message_id.clone().or(self.last_main_message_id.take());
        }
        if is_main {
            self.last_main_depth = self.turn_depth;
            // 失败那条没有用量，`messageTokens`（「对话此刻的 token 数」）不该被它清零。
            if !failed && !aborted {
                self.prev_total_input = call.input_tokens
                    + call.cache_read_tokens
                    + call.cache_creation_tokens
                    + call.output_tokens;
            }
            self.last_model = Some(display_model.to_string());
            self.turn_tool_calls += shape.tool_uses.len() as u32;
            self.turn_api_calls += 1;
            self.turn_api_ms += call.total_ms as i64;
            if first_text_in_turn {
                self.turn_text_seen = true;
            }
            if shell_snapshot_first {
                self.shell_snapshot_done = true;
            }
            if !turn_over {
                self.turn_depth += 1;
            }
        }
        // 拉起子代理的那条主线程请求（回复里调了 `Agent`），见 [`Session::last_spawn_request_id`]。
        if is_main && call.tool_use_lens.iter().any(|(name, _)| name == "Agent" || name == "Task") {
            self.last_spawn_request_id = call.request_id.clone();
        }
        // 子代理以 SubagentHandback 收尾（`cap/auto-2.1.285-20260930` 07:51:29.419–.455）：那个工具
        // 客户端当场执行，子代理不再发请求，这一条就是它的最后一条。
        let handback = (kind == Kind::Subagent && !turn_over && !failed && !aborted)
            .then(|| call.tool_calls.iter().find(|c| c.name == "SubagentHandback").cloned())
            .flatten();
        // 子代理支线推进一步（摘要请求不算）。
        let mut agent_done: Option<AgentState> = None;
        if kind == Kind::Subagent
            && let Some(a) = self.agents.get_mut(agent_key)
        {
            a.steps += 1;
            a.tool_uses += shape.tool_uses.len() as u32;
            a.last_request_id = call.request_id.clone().or(a.last_request_id.take());
            a.last_message_id = call.message_id.clone().or(a.last_message_id.take());
            if !failed {
                a.prev_total = call.input_tokens
                    + call.cache_read_tokens
                    + call.cache_creation_tokens
                    + call.output_tokens;
                a.tools_hash = Some(shape.tools_hash.clone());
            }
            // `end_turn` 收尾就是子代理跑完了：收尾事件要它的全程统计，档案随之删掉。以
            // SubagentHandback 工具收尾的也是（客户端就地执行、不再发请求）。
            if (turn_over || handback.is_some()) && !failed && !aborted {
                agent_done = self.agents.remove(agent_key);
            }
        }
        // 长度表报过一次就不再重发——但失败那条压根没报（`tengu_tool_schema_sizes` 在官方
        // 那边就长在 `tengu_api_success` 里），别让它把「已报过」的标记占掉。
        if !failed && !kind.is_agent() && kind != Kind::ModelValidation {
            if kind.has_boundary() {
                self.tools_hash_main = Some(shape.tools_hash.clone());
            } else {
                self.tools_hash_side = Some(shape.tools_hash.clone());
            }
        }
        CallRecord {
            usage_before,
            ignored_suggestion,
            interrupted_message_id,
            handback,
            agent_done,
        }
    }
}
