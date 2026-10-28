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
        let shape = parse_shape(&call.body);
        Self::process_parsed(st, call, shape, allow_defer, prev_end_override);
    }

    /// [`Self::process`] 的本体，请求体已经解析好（[`parse_shape`]）。转发来的调用在拿状态锁
    /// **之前**解析（[`Telemetry::drain`]）：100KB 以上的 JSON 要几毫秒，放在锁里就是让转发
    /// 路径上同样要这把锁的那几处（`seed_org_uuid` 之类）跟着等。
    pub(super) fn process_parsed(
        st: &mut State,
        call: ApiCall,
        shape: Option<RequestShape>,
        allow_defer: bool,
        prev_end_override: Option<SystemTime>,
    ) {
        let Some(mut shape) = shape else { return };
        // 这条调用的事件里解析 shell 命令的总额度：建事件时拿着状态锁，命令是租户给的，见 [`super::bash_ast::ParseBudget`]。
        let _parse_budget = super::bash_ast::ParseBudget::start(Duration::from_millis(
            config::TELEMETRY_BASH_PARSE_BUDGET_MS,
        ));
        // 体里没写 `cc_prompt_id`（billing-only 不往 header 里追加）就用会话链上那份，见 [`ApiCall::link_prompt_id`]。
        if shape.cc_prompt_id.is_none() {
            shape.cc_prompt_id = call.link_prompt_id.clone();
        }
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
        let is_delta = shape.thread_type.as_deref() == Some("continue");
        let mut filled_from_base = false;
        if is_delta
            && let Some(base) = st
                .sessions
                .get(&(call.cred_id, session_id.clone()))
                .and_then(|s| s.thread_bases.get(&thread_key))
        {
            let auto_in_delta = shape.auto_marker || shape.permission_declared;
            base.fill(&mut shape, auto_in_delta);
            filled_from_base = true;
        }
        // 会话还没有：标题生成先扣住，等这个会话的主线程请求来建会话（见 [`State::presession`]）。
        // 必须在判会话来历（`resolve_lineage`）之前扣：那一步会把 `/clear` 的旧会话标成已清、把
        // `--continue` 的旧会话与启动标记删掉，标题拿走了，主线程那条就认不出来历了。只扣标题：
        // 它是新会话首次输入时与主线程那条同时发的，主线程请求紧随其后必到；无工具 helper 没有
        // 这个保证。
        // `--continue` 的新进程也算「会话还没有」：旧会话在会话表里，但主线程那条一到就会被
        // `resolve_lineage` 删掉重建（这里只判、不改）。
        if allow_defer
            && classify_kind(&call, &shape, is_agent) == Kind::Title
            && (!st.sessions.contains_key(&(call.cred_id, session_id.clone()))
                || st.continue_marker(&call, &shape, &session_id, &device_id).is_some())
        {
            st.presession.entry((call.cred_id, session_id)).or_default().push((call, now));
            return;
        }
        // 主线程与进度摘要的重试链（[`HeldRetry`]）。官方遇到可重试的失败自己重发，重试途中只报重发事件，最终成功或
        // 失败才收尾；luban 每次尝试都看得到，单看一次失败分不出是不是最后一次，所以先扣住：
        // - 扣着一串、这条还是同一请求：接上（这条也是可重试的失败就继续扣，否则带着前面几次往下处理）；
        // - 扣着一串、这条换了请求：前面那串最后一次就是最终失败，先把它补报，再处理这条；
        // - 没扣着、这条是可重试的失败：开一串扣住。
        // 必须在判会话来历（`resolve_lineage`）之前扣：那一步会改会话表（`/clear` 的旧会话标成已清、`--continue` 的
        // 旧会话删掉重建），扣住的这条之后补处理时还要靠它认来历。扣着的时候会话照样算活跃，免得等重发那会儿
        // 被当成闲置退出（[`Telemetry::gc`] 也跳过有扣着的会话）。
        let key = (call.cred_id, session_id.clone());
        let mut call = call;
        let kind = classify_kind(&call, &shape, is_agent);
        // 子代理这条一收尾，摘要器跟着停：它那条线上扣着的失败等不来重发了，先当最终失败补处理，收尾那条摘要统计
        // 才算得进去（官方 `calls` 在发出时就记了，补处理晚于收尾就改不回已经报出去的那条）。
        if kind == Kind::Subagent && ends_agent(&call) {
            let lane = RetryLane::AgentSummary(agent_key.clone());
            if let Some(held) = st.held_retries.remove(&(call.cred_id, session_id.clone(), lane)) {
                Self::finish_retry_chain(st, held);
            }
        }
        // 进度摘要每个子代理一条线（与主线程、子代理本身同时在跑，[`RetryLane`]），是后台请求：过载不重发。同一次
        // 调用按模型与消息内容认（[`RequestShape::summary_digest`]）：两次调用之间笔记可能等长、模型也可能换。
        let lane = match kind {
            Kind::Main => Some((
                RetryLane::Main,
                (shape.cc_prompt_id.clone(), shape.messages_len, shape.tools_count),
                false,
            )),
            Kind::AgentSummary | Kind::AgentSummaryShort => Some((
                RetryLane::AgentSummary(agent_key.clone()),
                (
                    shape.summary_digest.clone().or_else(|| shape.cc_prompt_id.clone()),
                    shape.messages_len,
                    shape.input_text_chars,
                ),
                true,
            )),
            _ => None,
        };
        if allow_defer && let Some((lane, sig, background)) = lane {
            let hold_key = (call.cred_id, session_id.clone(), lane);
            let mut prior: Vec<PriorAttempt> = Vec::new();
            if let Some(held) = st.held_retries.remove(&hold_key) {
                if held.sig == sig && held.prior.len() < config::TELEMETRY_RETRY_CHAIN_MAX {
                    prior = held.prior;
                    prior.push(PriorAttempt::of(&held.last));
                } else {
                    Self::finish_retry_chain(st, held);
                }
            }
            let retryable = !call.aborted
                && call.failure.as_ref().is_some_and(|f| retried_by_client(f, background));
            if retryable {
                if let Some(sess) = st.sessions.get_mut(&key) {
                    sess.last_seen = now;
                }
                st.held_retries.insert(
                    hold_key,
                    HeldRetry { prior, last: call, sig, at: now, device_id: device_id.clone() },
                );
                return;
            }
            call.prior_attempts = prior;
        }
        let call = call;
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

        if kind.runs_in_default_mode() {
            shape.permission_mode = "default";
        }
        // 展示名：出站体里 `[1m]` 已经被客户端剥成 `context-1m` beta，这里还原回去。
        let has_1m = call.betas.as_deref().is_some_and(|b| b.contains("context-1m-"));
        let display_model =
            if has_1m { format!("{}[1m]", shape.model) } else { shape.model.clone() };
        let resp_model = call.resp_model.clone().unwrap_or_else(|| shape.model.clone());

        // 放在重试链之后判：补报前面那串最终失败时可能刚把会话建起来。
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
            if kind == Kind::Title {
                sess.waiting_titles.push((call.logical_start(), sleepy_key(&display_model)));
            }
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
        // 会话建起来之前扣住的标题（[`State::presession`]）：主线程请求到了，并进会话自己的侧查询
        // 队列，照那条路补发（这条处理完就补）。**并之前先记下它们的完成时刻**：标题比主线程那条
        // 早完成，主线程那条的 `timeSinceLastApiCallMs` 量的正是到标题完成的距离（2.1.291 四族首条
        // 主线程请求报 2825 / 6138 / 2469 / 2305ms）。
        if kind == Kind::Main
            && let Some(parked) = st.presession.remove(&key)
        {
            for (c, at) in parked {
                let prev_end = sess.last_call_end;
                let this_end = c.started_at + Duration::from_millis(c.total_ms);
                sess.last_call_end = Some(prev_end.map_or(this_end, |e| e.max(this_end)));
                sess.recent_ends.push_back(this_end);
                if let Some(m) = body_model(&c.body) {
                    sess.waiting_titles.push((c.logical_start(), sleepy_key(&m)));
                }
                sess.deferred.push((c, prev_end, at));
            }
        }
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
            prev_main_anchor_end,
            prev_main_anchor_tool_call,
            prompt_id,
            prev_prompt_id,
            delegated_running,
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
        let agent = kind.is_agent().then(|| sess.agent_view(&call, &shape, &agent_key, kind));
        // 「猜下一句」分叉里接着发的那条：链、深度、上一条都接着分叉自己的（[`SuggestionFork`]）。分叉头一条
        // 另起；接着发的那条要是没赶上前一条（状态丢了），也只能另起。
        let suggestion_fork = (kind == Kind::Suggestion && shape.suggestion_step > 0)
            .then(|| sess.suggestion_fork.clone())
            .flatten();
        let previous_request_id = kind
            .has_chain()
            .then(|| {
                shape.cc_prev_req.clone().or_else(|| match (&agent, &suggestion_fork) {
                    (Some(a), _) => a.last_request_id.clone(),
                    (None, Some(f)) => f.request_id.clone(),
                    (None, None) => sess.last_main_request_id.clone(),
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
        let message_tokens = match (&agent, &suggestion_fork) {
            (Some(a), _) => a.prev_total,
            (None, Some(f)) => f.total_tokens,
            (None, None) if kind.has_chain() => sess.prev_total_input,
            (None, None) => 0,
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
        let (chain_id, query_depth) = match &suggestion_fork {
            Some(f) => (f.chain_id.clone(), f.depth + 1),
            None => sess.chain_for(kind, agent.as_ref()),
        };
        // 分叉的回复调了工具：客户端会带着拒绝结果接着发，记下到这条为止的分叉。不再调工具（收尾）、失败、
        // 被取消，分叉就到此为止——会被重发的失败除外：客户端原样重发这一条，分叉还接着（重发那条照旧接这份）。
        // 猜下一句是后台请求，过载不重发。
        let retrying = failed && call.failure.as_ref().is_some_and(|f| retried_by_client(f, true));
        if kind == Kind::Suggestion && !retrying {
            sess.suggestion_fork = (!turn_over && !failed && !aborted).then(|| {
                let prev = suggestion_fork.as_ref();
                let mut usage = prev.map_or([0; 4], |f| f.usage);
                for (t, v) in usage.iter_mut().zip([
                    call.input_tokens,
                    call.output_tokens,
                    call.cache_read_tokens,
                    call.cache_creation_tokens,
                ]) {
                    *t += v;
                }
                SuggestionFork {
                    chain_id: chain_id.clone(),
                    depth: query_depth,
                    request_id: call.request_id.clone(),
                    total_tokens: call.input_tokens
                        + call.cache_read_tokens
                        + call.cache_creation_tokens
                        + call.output_tokens,
                    started: prev.map_or(call.logical_start(), |f| f.started),
                    usage,
                    messages: prev.map_or(0, |f| f.messages)
                        + suggestion_resumed_messages(&shape)
                        + reply_messages(&call),
                    first_request_id: prev
                        .map_or_else(|| call.request_id.clone(), |f| f.first_request_id.clone()),
                    candidate_chars: prev
                        .map(|f| f.candidate_chars)
                        .filter(|c| *c > 0)
                        .unwrap_or(call.text_chars),
                }
            });
        }
        // 出了建议：分叉收尾时挑整个分叉里头一个非空正文（[`SuggestionFork::candidate_chars`]），挂着等用户下一次
        // 输入报 `ignored`；`generationRequestId` 是分叉头一条（官方 `M` 取分叉里头一条回复的）。
        if kind == Kind::Suggestion && turn_over && !failed && !aborted {
            let (request_id, chars) = match &suggestion_fork {
                Some(f) if f.candidate_chars > 0 => (f.first_request_id.clone(), f.candidate_chars),
                Some(f) => (f.first_request_id.clone(), call.text_chars),
                None => (call.request_id.clone(), call.text_chars),
            };
            if chars > 0 {
                sess.shown_suggestion = Some((request_id.unwrap_or_default(), this_end, chars));
            }
        }
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
        // 2.1.291：`tengu_api_success` 多 `cache_creation_1h/5m_input_tokens`，`tether_live_outcome` 多
        // 四项线程闲置 / 存活计时，每轮收尾多一条 `tengu_message_display_hooks`，会话首条请求多一条
        // `session_transcript_write`，`tool_use_error` 多 `error_constructor`
        // （`cap/auto-2.1.291-20261006-full`）。
        let v291 = version_at_least(&version, "2.1.291");
        // 2.1.293：标题生成换成 haiku-5-5，它头一次用也报 `sleepy_snowflake_applied`；haiku-5-5 的
        // 会话多一条 `heron_brook_applied`（`cap/auto-2.1.293-20261008-full`）。
        let v293 = version_at_least(&version, "2.1.293");
        // 2.1.296：权限弹框上被拒的工具，拒绝那条多两个时长键（`cap/auto-2.1.296-20261013-full`）。
        let v296 = version_at_least(&version, "2.1.296");
        // 2.1.299：工具成功多两个计时键、Bash 命令多几项判定、tether 多 `searchResultHistory`
        // （`cap/auto-2.1.299-20261014-full`）。
        let v299 = version_at_least(&version, "2.1.299");
        // 2.1.300：auto 模式判决多 `coworkWebAllowOverride`、`-p` 收尾多两个 debug 丢弃计数
        // （`cap/auto-2.1.300-20261015-full`）。
        let v300 = version_at_least(&version, "2.1.300");
        // 2.1.303：`tengu_tool_use_success` 在 `messageID` 之后多 `toolUseID`
        // （`cap/auto-2.1.303-20261019-full` 55 条全有）。
        let v303 = version_at_least(&version, "2.1.303");
        // 2.1.305：`tengu_api_success` 在 `firstContentMs` 之后多 `streamEventCount`
        // （`cap/auto-2.1.305-20261021-full` 165 条全有）。
        let v305 = version_at_least(&version, "2.1.305");
        // 2.1.306：失败 Bash 的错误栈指纹换了（`cap/auto-2.1.306-20261021-full` 三条）。
        let v306 = version_at_least(&version, "2.1.306");
        // 2.1.307：进度摘要器收尾多 writer 三项、auto 判决多 `skippedOnAllowRule`、压缩尝试多 `keepNoRound`，
        // 工具被拒多一条 `tengu_safety_event`（`cap/auto-2.1.307-20261022-full`），失败 Bash 的错误栈指纹又换了
        // （`cap/auto-2.1.307-20261022-bash`）。
        let v307 = version_at_least(&version, "2.1.307");
        // 2.1.308：失败 Bash 的错误栈只换了打包块名（`cap/auto-2.1.308-20261022-full` A0 那条）。
        let v308 = version_at_least(&version, "2.1.308");
        // 2.1.309：失败 Bash 的错误栈摘要、行列、块名都换了（`cap/auto-2.1.309-20261023-full` A0 那条）。
        let v309 = version_at_least(&version, "2.1.309");
        // 2.1.310：失败 Bash 的错误栈摘要、行列、块名都换了（`cap/auto-2.1.310-20261024-bashfail`）。
        let v310 = version_at_least(&version, "2.1.310");
        // 2.1.311：失败 Bash 的错误栈又换了（`cap/auto-2.1.311-20261027-bashfail`），Bash 事件多命令类别、耗时与
        // CPU 那几项（[`super::tools::bash_meta_v311`]）。
        let v311 = version_at_least(&version, "2.1.311");
        let safety_events = v307 && crate::proxy::snoopy_creek::account_enabled(call.cred_id);
        // 客户端自己注入的一轮（后台任务通知 / 同伴会话）：没有提交、粘贴、渲染那几条，只有
        // 一条排队消息送达（`cap/2.1.280` 两条、`cap/2.1.285` 06:57:29.588）。
        let injected = new_prompt && version_at_least(&version, "2.1.277") && !user_turn;
        let first_prompt_tpl = new_prompt && !injected && !sess.first_prompt_tpl_done;
        // 2.1.296 模板才有那条曝光；分组取这个会话钉住的那份，与模拟出站的 `Agent` 说明同源。
        let lucky_cerf: Vec<crate::proxy::LuckyCerfExposure> = if v296 && new_prompt {
            crate::proxy::lucky_cerf_exposures(call.cred_id, &session_id, &shape.model, &version)
                .into_iter()
                .filter(|e| !sess.lucky_cerf_exposed.contains(&e.feature))
                .collect()
        } else {
            Vec::new()
        };
        sess.lucky_cerf_exposed.extend(lucky_cerf.iter().map(|e| e.feature));
        if first_prompt_tpl {
            sess.first_prompt_tpl_done = true;
        }
        // 后台任务那段：进程启动后多久该出现的，这条请求发出时已经过了就补上（每条一次）。
        let background = sess.take_background(&version, shape.sdk, call.started_at);
        let ide_detect = sess.take_ide_detect(&version, shape.sdk, call.started_at);
        let attachment_counts = sess.attachment_counts.clone();
        let first_main = v280 && is_main && !failed && !sess.first_main_done;
        if first_main {
            sess.first_main_done = true;
        }
        // 快照一条线一份：主线程（含猜下一句）一份，每个子代理（含它的摘要请求）各一份
        // （`cap/2.1.280` 主线程 `b06e…`/`d6bc…`、Explore 子代理 `7d8050a24c2c`，子代理首条
        // 同样报 `live_recorded`）。
        let snapshot =
            (v270 && kind.has_boundary()).then(|| sess.snapshot_for(kind, &agent_key, &shape));
        // 2.1.293 的标题生成那条也报（`cap/auto-2.1.293-20261008-full` 每个会话的标题请求前都有一条
        // `model: claude-haiku-5-5`；主线程也是 haiku-5-5 的会话里，主线程那次就不再报）。
        // 官方按 `Mt(model).toLowerCase()` 去重（剥 `[1m]`），meta 报头一次实际调用的展示名：haiku-5-5\[1m] 主线程的
        // 会话里，标题那条 `claude-haiku-5-5` 报了，主线程就不再报（`cap/auto-2.1.308-20261022-gaps` G2 只有一条）。
        // 官方在请求**发起前**记首次应用（`Zmr`），归属按发起顺序定，不按 luban 收到回包、处理的顺序。主线程判的时候，
        // 同模型、发起得更早（都按逻辑起点：重试链取首发那次）的标题还没处理，就让给它：
        // - 还在路上（[`State::dispatched_titles`]，发出时登记，见 [`Telemetry::note_dispatch`]）——标题先发、主线程先
        //   返回；
        // - 已经返回、扣在队里（[`Session::waiting_titles`]）——标题被扣住晚处理。
        let sleepy_k = sleepy_key(&display_model);
        let logical_start = call.logical_start();
        let earlier_title = |titles: &[(SystemTime, String)]| {
            titles.iter().any(|(at, k)| *at <= logical_start && *k == sleepy_k)
        };
        let title_first = v293
            && is_main
            && (earlier_title(&sess.waiting_titles)
                || st.dispatched_titles.get(&key).is_some_and(|t| earlier_title(t)));
        let sleepy = v280
            && (new_prompt || (v293 && kind == Kind::Title))
            && !title_first
            && !sess.sleepy_models.contains(&sleepy_k);
        if sleepy {
            sess.sleepy_models.push(sleepy_k.clone());
        }
        // 标题处理了就不再在路上：它这条（含重试的各次）发出时的登记去掉。
        if kind == Kind::Title
            && let Some(t) = st.dispatched_titles.get_mut(&key)
        {
            t.retain(|(at, k)| !(*k == sleepy_k && *at >= logical_start && *at <= call.started_at));
            if t.is_empty() {
                st.dispatched_titles.remove(&key);
            }
        }
        // 服务端下发了 `tengu_heron_brook` 那段 system 文本才报（[`crate::proxy::heron_brook`]：2.1.293 / 296 时
        // haiku-5-5 的 bootstrap 带 2790 字，`fromClientData: true`；2.1.299 起不再下发、也没有这条），官方落在会话头
        // 一条请求之前——有标题请求时是标题那条，紧挨它的 `sleepy_snowflake_applied`。标题请求里看不出主线程是哪个
        // 模型（opus 会话的标题同样是 haiku-5-5，却没有这一条），这里挂在主线程的首次输入上。
        // 段缓存清掉（压缩成功）之后的下一条主线程请求也重新组装，不论是不是新输入（自动压缩后接着的是续轮）。
        // `<total_tokens>` 那一族（档位、用户轮、预算）由 bootstrap 的 `client_data` 给出时，官方算那一项的那一刻各报
        // 一条 `tengu_lapis_anchor_from_client_data`（一个进程一次；2.1.309 起的可执行文件里都有）。会话钉住的那一档
        // 与模拟路径同一份（[`crate::proxy::lapis_anchor::session_mode`]，只让主线程那条来钉），位置跟启动模板走
        // （[`Subst::lapis`]）。system 与 messages 是客户端自己的两种（真实 CC 透传 [`ApiCall::client_tools`]、
        // billing-only [`ApiCall::bare`]）另说，报不报看实际出站：体里（system、首轮环境说明、每轮跟的那条都算——
        // 换了 system 的 Agent SDK / `--system-prompt` 提醒只在 messages 里）有没有 `<total_tokens>` 与号的这一档对得上
        // 才报。第三方应用本来就不带提醒，号下发 off 时照报（官方拿到 off 就报这一条）；经网关的真实 CC 拿不到号的
        // client_data、照默认发提醒，这时报 off 就与它的请求体自相矛盾，不报。
        let own_system = call.client_tools || call.bare;
        let lapis = if v309 {
            let m = if is_main {
                crate::proxy::lapis_anchor::session_mode(call.cred_id, &session_id, &display_model)
            } else {
                crate::proxy::lapis_anchor::peek_session_mode(
                    call.cred_id,
                    &session_id,
                    &display_model,
                )
            };
            m.client_data_events(own_system.then_some(crate::proxy::lapis_anchor::Observed {
                reminders: shape.total_tokens_reminder,
                max: shape.total_tokens_max,
            }))
        } else {
            Vec::new()
        };
        let heron = (v293 && is_main && (new_prompt || post_compaction) && !sess.heron_done)
            .then(|| crate::proxy::heron_brook::applied(call.cred_id, &display_model))
            .flatten();
        if heron.is_some() {
            sess.heron_done = true;
        }
        // tether 只管主线程与子代理（摘要、猜下一句、标题这类辅助调用一条都没有），子代理
        // 每条支线一个线程（`cap/2.1.280` Explore 首条 `create/first_request`、之后 `continue/append`）。
        // 没有线程可言就不报：官方不用线程时 tether 一条都没有（`cap/auto-2.1.309-20261023-api` 客户端自报那几批）。
        // billing-only 模拟体里不写 `thread`、头上也去掉了那项 beta（[`ApiCall::bare`]）；真实 CC 透传
        // （[`ApiCall::client_tools`]）看出站头上有没有 message-threads——经网关的客户端自己就不带。
        let threads = if call.client_tools {
            call.betas.as_deref().is_some_and(|b| {
                b.split(',').any(|p| p.trim() == crate::config::CC_BETA_MESSAGE_THREADS)
            })
        } else {
            !call.bare
        };
        let tether = (v280 && matches!(kind, Kind::Main | Kind::Subagent) && threads)
            .then(|| sess.tether_for(&call, &shape, &display_model, is_main, &agent_key));
        // 这条线程的全量形态记下来，给下一条增量请求补（见 [`ThreadBase`]）。
        // 只记全量视角：不是增量（`create` 或没开线程），或增量已经按底本补全了。没补上的增量（底本
        // 丢了）不能拿来当底本。此前用「工具数 > 0」近似「是全量」，无工具的主线程对话（模拟路径不补
        // 工具时的第三方多轮）首轮就记不下，续轮报出的 system 长度、消息数都是增量那份。
        if matches!(kind, Kind::Main | Kind::Subagent) && (!is_delta || filled_from_base) {
            sess.thread_bases.insert(
                thread_key.clone(),
                ThreadBase::of(&shape, call.reply_input_chars, call.tool_use_lens.clone()),
            );
        }

        let marks =
            TurnMarks { new_prompt, user_turn, first_text_in_turn, shell_snapshot_first, this_end };
        // 这条请求带回的工具结果是这条线上一条回复调的工具：改动行数记在那条回复的 `message.model` 名下
        // （官方跨进程执行工具时传的是发起调用那条 assistant 消息的 `message.model`，`R?.message.model`），
        // 不是这条请求的模型——请求 opus、回落到 sonnet 写的文件，记在 sonnet 上。
        let loc_reply_model = if is_main {
            sess.last_main_reply_model.clone()
        } else if kind == Kind::Subagent {
            sess.agents.get(&agent_key).and_then(|a| a.last_reply_model.clone())
        } else {
            None
        };
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
        let mut hb = std::mem::take(&mut sess.hb);
        hb.refresh(call.cred_id);
        let mut files = std::mem::take(&mut sess.files);
        // 这一轮到这条为止跑过的工具数（`-p` 收尾那条 `tengu_sdk_result.tool_use_count`）。
        let sess_turn_tools = sess.turn_tool_calls;
        let (sess_turn_api_calls, sess_turn_api_ms) = (sess.turn_api_calls, sess.turn_api_ms);
        let (sess_turn_mcp, sess_turn_toolsearch) =
            (sess.turn_mcp_calls, sess.turn_toolsearch_calls);
        let turn_first_reply_end = sess.turn_first_reply_end.map(DateTime::<Utc>::from);
        let sess_turn_retry = sess.turn_retry_status;
        let session_cwd = sess.cwd.clone();
        let betas_full = call.betas.clone().unwrap_or_default();
        let betas_own = session_betas(&betas_full, &version);
        // 子代理支线上的事件顶层 `betas` 报**主线程**那份；它自己的 `api_query` / `api_success` 报这条请求的
        // 完整串（与 meta 同值），`agent_tool_selected` / `agent_tool_completed` 与它里面的 Bash 报它这条请求过滤出
        // 的那份（`cap/2.1.285`：claude-code-guide 跑 haiku-4.5，API 事件是 11 项的完整串、收尾那条是
        // `oauth,interleaved…` 6 项；`cap/auto-2.1.308-20261022-gaps` 跑 haiku-5-5 是 16 项与 6 项；Explore 与主线程
        // 同模型，几份本来相同）。
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
        // 非主线程的请求（子代理、标题、页面处理、猜下一句……）处理时报的事件，顶层同样是**主线程**那份：官方取会话主模型
        // 的会话级 beta，只有 meta 里带了别的模型的那几条报那个模型的（`EventBuilder::own_betas_scope`；
        // `cap/auto-2.1.308-20261022-full` / `-gaps`：标题那条处理时的 `session_title_generated` / `tool_schema_sizes` /
        // `tool_search_mode_decision` 都是主线程那 7 项，只有 `sleepy_snowflake_applied` 是 haiku 那 6 项）。新会话里的标题
        // 先扣住、等主线程那条处理完才补处理（[`State::presession`]），这时主线程那份已经有了；扣不住（主线程那条迟迟
        // 不来）时按会话主模型（与顶层 `model` 同源）照这一版的 profile 推；会话主模型也只知道是这条自己的，就用它自己那份。
        let betas_session = if is_main {
            betas_own.clone()
        } else if !sess.main_betas.is_empty() {
            sess.main_betas.clone()
        } else if ctx_model != display_model {
            let v = crate::proxy::parse_version(&version);
            session_betas(&crate::proxy::cc_main_beta_at(&ctx_model, v), &version)
        } else {
            betas_own.clone()
        };

        // tether 的时间锚点：主线程含被取消的那条，子代理看它自己那条支线，其余看会话上一条。
        let anchor_end = if is_main {
            prev_main_anchor_end
        } else if kind == Kind::Subagent {
            agent.as_ref().and_then(|a| a.last_end)
        } else {
            prev_end
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
            prev_prompt_id,
            delegated_running,
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
            v291,
            v293,
            v296,
            v299,
            v300,
            v303,
            v305,
            v306,
            v307,
            v308,
            v309,
            v310,
            v311,
            safety_events,
            suggestion_fork,
            lucky_cerf,
            main_effort: sess.main_effort.clone(),
            // 主线程的工具锚点单独记（只在本会话里算），子代理看它自己那条支线上一条回复。
            anchor_tool_call: if is_main {
                prev_main_anchor_tool_call
            } else {
                !prev_reply_calls.is_empty()
            },
            anchor_end,
            injected,
            first_prompt_tpl,
            background,
            ide_detect,
            attachment_counts,
            first_main,
            snapshot,
            sleepy,
            heron,
            lapis,
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
            sess_turn_mcp,
            sess_turn_toolsearch,
            turn_first_reply_end,
            sess_turn_retry,
            session_cwd,
            betas_full,
            betas_own,
            thread_step,
            betas_session,
        };
        let BuiltEvents { events, dd, probe_side, loc, decisions, post_compact_attachments } =
            build_events(&call, &shape, &facts, &mut file_sizes, &mut hb, &mut files);
        sess.note_attachments(&events);
        // 压缩成功：官方统计的是压缩那一刻内部消息表里的附件。压完之后表里只剩保留那组带的与压缩时放回去的
        // （[`BuiltEvents::post_compact_attachments`]），账本换成它，之后各轮再接着记（这一条自己的统计上面
        // 已经用旧账本算过）。
        if kind == Kind::Compact && !failed && !aborted {
            sess.attachment_counts = post_compact_attachments.unwrap_or_default();
        }
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
            ctx_model,
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
            sess.hb = hb;
            sess.files = files;
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
        // 指标里的 `model` 属性仍是会话主模型，自相矛盾。第一条就是侧查询时，`model` 也取会话主模型
        // （设备记着的默认模型，与 `betas_session` 推 beta 的那个同源），不拿侧查询自己的。
        if is_main || pending.model.is_empty() {
            pending.model = ctx_model;
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
            loc_model: loc_reply_model
                .unwrap_or_else(|| display_model.trim_end_matches("[1m]").to_string()),
            loc,
            decisions,
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

        // 主线程请求到了：新一轮的 prompt id 已经写进会话，把扣住的侧查询补发出去（会话建起来
        // 之前就扣住的标题已经并进这个队列）。
        if is_main {
            Self::replay_deferred(st, &key);
        }
    }

    /// 扣太久（[`Telemetry::gc`]）还没等到主线程请求的标题（[`State::presession`]）：照旧由第一条
    /// 自己建会话、按它自己的样子发。
    pub(super) fn replay_presession(st: &mut State, key: &(i64, String)) {
        for (c, _) in st.presession.remove(key).unwrap_or_default() {
            Self::process(st, c, false, None);
        }
    }

    /// 一串扣住的失败到头了（换了请求或扣太久）：最后一次就是最终失败，带着前面几次照常处理（不再扣）。
    pub(super) fn finish_retry_chain(st: &mut State, held: HeldRetry) {
        let mut last = held.last;
        last.prior_attempts = held.prior;
        Self::process(st, last, false, None);
    }

    /// 把某会话扣住的侧查询按顺序补发（主线程请求到了，或扣得太久）。
    pub(super) fn replay_deferred(st: &mut State, key: &(i64, String)) {
        let deferred = st
            .sessions
            .get_mut(key)
            .map(|s| {
                s.waiting_titles.clear();
                std::mem::take(&mut s.deferred)
            })
            .unwrap_or_default();
        for (c, prev_end, _) in deferred {
            Self::process(st, c, false, prev_end);
        }
    }
}

/// `tengu_sleepy_snowflake_applied` 的去重键：官方 `Mt(model).toLowerCase()`，即剥掉 `[1m]` 再小写。
pub(super) fn sleepy_key(model: &str) -> String {
    let m = model.to_ascii_lowercase();
    m.strip_suffix("[1m]").unwrap_or(&m).to_string()
}

/// 只取请求体的 `model`（扣住的标题建会话前没解析过形态）。
fn body_model(body: &[u8]) -> Option<String> {
    #[derive(serde::Deserialize)]
    struct M {
        model: String,
    }
    serde_json::from_slice::<M>(body).ok().map(|m| m.model)
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
    prev_main_anchor_end: Option<SystemTime>,
    prev_main_anchor_tool_call: bool,
    prompt_id: String,
    prev_prompt_id: String,
    /// 见 [`TurnFacts::delegated_running`]。
    delegated_running: bool,
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
            self.main_effort = shape.effort.clone();
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
            self.betas = session_betas(call.betas.as_deref().unwrap_or(""), version);
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
            // 官方压缩成功调 `XEn("compaction")` 清掉系统提示词各段的缓存，下一次组装重新跑 heron 那段、再报一次
            // `tengu_heron_brook_applied`。
            self.heron_done = false;
        }
        // 用户取消的压缩不算：官方只在压缩成功时清系统段缓存、换快照。
        if kind == Kind::Compact && !failed && !aborted {
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
        // tether 的时间锚点：被取消的主线程请求也算它的收尾时刻（见 [`Session::last_main_anchor_end`]）。
        let prev_main_anchor_end = self.last_main_anchor_end;
        let prev_main_anchor_tool_call = self.main_anchor_tool_call;
        if is_main {
            self.last_main_anchor_end = Some(this_end_wall(call));
            if !aborted {
                self.main_anchor_tool_call = !call.tool_calls.is_empty();
            }
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
            // 本轮从用户提交算起，在客户端的重试循环之外：首条请求重发过的，从头一次尝试发出算
            // （SDK `duration_ms` / `ttft_ms` 都要含此前的尝试与退避）。
            self.turn_started = Some(call.logical_start() - Duration::from_millis(15));
            self.turn_text_seen = false;
            self.turn_tool_calls = 0;
            self.turn_api_calls = 0;
            self.turn_api_ms = 0;
            self.turn_mcp_calls = 0;
            self.turn_toolsearch_calls = 0;
            self.turn_first_reply_end = None;
            self.turn_retry_status = None;
        }
        // 会话的 prompt 只跟主线程这一侧走：子代理那一侧的请求带的是派它那一轮的 id，后台代理晚到时主线程
        // 早换了几轮，拿它覆盖会把会话拨回去（下一次提交前的 `user_input` 就挂错了 prompt）。归属按请求实际
        // 带的 `x-claude-code-agent-id` 认，不只看类别：子代理里的 WebFetch 应用、进度摘要这类辅助请求也带它
        // （`cap/2.1.285` 六条 auxiliary，计费头写着 `cc_is_subagent=true`）。这一侧自己的事件照旧报它带的那个。
        let own_prompt = shape.cc_prompt_id.clone();
        let agent_side = kind.is_agent()
            || kind == Kind::AgentSummaryShort
            || call.agent.agent_id.as_deref().is_some_and(|a| !a.is_empty());
        if !agent_side && let Some(pid) = own_prompt.clone() {
            if pid != self.prompt_id {
                self.prev_prompt_id = std::mem::replace(&mut self.prompt_id, pid);
            }
        } else if self.prompt_id.is_empty() {
            self.prompt_id = uuid_v4();
        } else if new_prompt && !agent_side {
            // 哪儿都没有 prompt id 时自己造，也得逐轮换：官方每次新输入一个新 id（`cap/2.1.258-api/00022`
            // 四轮四个值）。
            self.prev_prompt_id = std::mem::replace(&mut self.prompt_id, uuid_v4());
        }
        let prompt_id = match own_prompt {
            Some(pid) if agent_side => pid,
            _ => self.prompt_id.clone(),
        };
        let prev_prompt_id = self.prev_prompt_id.clone();
        // 新一轮用户输入：上一轮派出去没跑完的不再占着（子代理失败或被打断不会收尾，别让它一直挂着）。
        if new_prompt {
            self.delegated_pending.clear();
            self.nested_pending = 0;
            for a in self.agents.values_mut() {
                a.delegated = false;
            }
        }
        let delegated_running = is_main
            && (!self.delegated_pending.is_empty() || self.agents.values().any(|a| a.delegated));
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
            prev_main_anchor_end,
            prev_main_anchor_tool_call,
            prompt_id,
            prev_prompt_id,
            delegated_running,
        }
    }

    /// 新露面的子代理认领一次派遣：首条用户消息的一个文本块就是派它那次 `Agent` 调用的 `prompt`，但不一定
    /// 原样——客户端常在末尾补一个换行（`cap/` 2.1.280 ~ 2.1.306 全部 61 个子代理，原样相同的只有 14 个，
    /// 其余去掉首尾空白后都相同），所以两边都去掉首尾空白再比。对上主线程派的那份就是主线程直接派的；
    /// 对不上、而子代理那边有没认领的派遣，就是孙代理。只有缺了可比的信息才退回：派遣那头 `prompt` 取不到
    /// （空串那项），或这条请求体读不出首条用户消息。两头都有文本却对不上（上一轮的后台代理晚到）不认领，
    /// 免得占掉这一轮别人的那份。返回它是不是占了主线程那份。
    fn claim_delegation(&mut self, call: &ApiCall) -> bool {
        if self.delegated_pending.is_empty() && self.nested_pending == 0 {
            return false;
        }
        let texts = first_user_texts(&call.body);
        if let Some(i) = self
            .delegated_pending
            .iter()
            .position(|p| !p.is_empty() && texts.iter().any(|t| t.trim() == p))
        {
            self.delegated_pending.remove(i);
            return true;
        }
        if self.nested_pending > 0 {
            self.nested_pending -= 1;
            return false;
        }
        let i = if texts.is_empty() {
            (!self.delegated_pending.is_empty()).then_some(0)
        } else {
            self.delegated_pending.iter().position(String::is_empty)
        };
        let Some(i) = i else { return false };
        self.delegated_pending.remove(i);
        true
    }

    /// 子代理支线：首条请求建档（链、起点、拉起它的那条主线程请求），之后每条读它。
    /// 摘要请求只读不推进。
    fn agent_view(
        &mut self,
        call: &ApiCall,
        shape: &RequestShape,
        agent_key: &str,
        kind: Kind,
    ) -> AgentView {
        let spawn = self.last_spawn_request_id.clone();
        // 摘要器在子代理起跑时读当前主模型：按它首条请求的发出时刻查，处理到这条时主线程可能已经换了。
        let session_model = self.main_model_at(call.started_at);
        let first =
            kind == Kind::Subagent && self.agents.get(agent_key).is_none_or(|a| !a.claim_checked);
        let delegated = first && self.claim_delegation(call);
        let a = self.agents.entry(agent_key.to_string()).or_default();
        if a.agent_type.is_none() {
            a.agent_type = call.agent.agent_type.clone().filter(|t| !t.is_empty());
        }
        if kind == Kind::Subagent && a.model.is_none() {
            a.model = Some(shape.model.clone());
        }
        if a.chain_id.is_empty() {
            a.chain_id = uuid_v4();
            a.started = Some(call.started_at);
            a.session_model = session_model;
            a.prompt_chars = shape.prompt_len;
            a.invoking_request_id = spawn;
        }
        if first {
            a.delegated = delegated;
            a.claim_checked = true;
        }
        AgentView {
            chain_id: a.chain_id.clone(),
            steps: a.steps,
            last_request_id: a.last_request_id.clone(),
            last_message_id: a.last_message_id.clone(),
            prev_total: a.prev_total,
            tools_hash: a.tools_hash.clone(),
            invoking_request_id: (a.steps == 0).then(|| a.invoking_request_id.clone()).flatten(),
            last_end: a.last_end,
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
            | Kind::AgentSummaryShort
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
            // 提交在客户端的重试循环之外：重发过的从头一次尝试算。
            let submit = call.logical_start() - Duration::from_millis(15);
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

    /// 进程启动后约 30 秒那条 IDE 探测（官方 `TCr`：每秒找一次装了扩展的 IDE，30 秒找不到报
    /// `ide_detect{found:false}`；`cap/auto-2.1.291-20261006-full` 起每份全量抓包的交互式会话都在首条事件后
    /// 30.4 ~ 30.8 秒，不到 30 秒就退出的没有，`-p` 没有）：这条请求发出时已经过了那一刻就补上，一个进程
    /// 一次。
    fn take_ide_detect(&mut self, version: &str, sdk: bool, started_at: SystemTime) -> bool {
        if self.ide_detect_done || sdk || !version_at_least(version, "2.1.285") {
            return false;
        }
        let due = self.started_wall + Duration::from_millis(IDE_DETECT_MS) <= started_at;
        if due {
            self.ide_detect_done = true;
        }
        due
    }

    /// 记下这条调用报过的主线程附件（见 [`Session::attachment_counts`]）。子代理那条线上的不算。
    fn note_attachments(&mut self, events: &[(DateTime<Utc>, Value)]) {
        for (_, e) in events {
            if e["event_data"]["event_name"].as_str() != Some("tengu_attachments") {
                continue;
            }
            let Some(meta) = e["event_data"]["additional_metadata"]
                .as_str()
                .and_then(|b| STANDARD.decode(b).ok())
                .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
            else {
                continue;
            };
            // 子代理与「猜下一句」分叉的附件在它们自己的消息表里，不进主线程那份。
            if meta
                .get("query_source")
                .and_then(|q| q.as_str())
                .is_some_and(|q| q.starts_with("agent:") || q == "prompt_suggestion")
            {
                continue;
            }
            for t in meta.get("attachment_types").and_then(|t| t.as_array()).into_iter().flatten() {
                let Some(t) = t.as_str() else { continue };
                match self.attachment_counts.iter_mut().find(|(k, _)| k == t) {
                    Some((_, n)) => *n += 1,
                    None => self.attachment_counts.push((t.to_string(), 1)),
                }
            }
        }
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
        // 补发的侧查询（扣住的标题）比后来的主线程请求结束得早：只有比现有锚点晚结束的才换，
        // 不然下一条没带 `diagnostics.previous_message_id` 的请求会回头引用那条标题。
        if !aborted
            && call.message_id.is_some()
            && self.last_message_end.is_none_or(|e| this_end >= e)
        {
            self.last_message_id = call.message_id.clone();
            self.last_message_end = Some(this_end);
        }
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
        // 「猜下一句」出的建议（分叉收尾时挂上，见 [`Session::shown_suggestion`]）：用户下一次输入（或 `/compact`、
        // `/btw` 这类斜杠命令）时报 ignored，这条输入把它消费掉。
        let ignored_suggestion =
            if (new_prompt && user_turn) || matches!(kind, Kind::Compact | Kind::SideQuestion) {
                self.shown_suggestion.take()
            } else {
                None
            };
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
            if let Some(m) = call.resp_model.clone().filter(|m| !m.is_empty()) {
                self.last_main_reply_model = Some(m);
            }
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
            self.note_main_model(call.started_at, display_model);
            self.turn_tool_calls += shape.tool_uses.len() as u32;
            for tu in &shape.tool_uses {
                if tu.name.starts_with("mcp__") {
                    self.turn_mcp_calls += 1;
                } else if tu.name == "ToolSearch" {
                    self.turn_toolsearch_calls += 1;
                }
            }
            self.turn_api_calls += 1;
            let retry = RetryView::of(&call.prior_attempts, call.dispatch.as_deref());
            if retry.retries > 0 {
                self.turn_retry_status =
                    Some(self.turn_retry_status.unwrap_or(0).max(retry.max_retry_status));
            }
            // 失败的不算进 API 耗时（`sdk_result.duration_api_ms`，`cap/auto-2.1.306-20261021-retry` R2 报 0）；重发过才
            // 成功的从头一次发出算起（R1 报 1947，就是那条的 `durationMsIncludingRetries`）。
            if !failed {
                self.turn_api_ms += match call.prior_attempts.first() {
                    Some(first) => this_end_wall(call)
                        .duration_since(first.started_at)
                        .map_or(call.total_ms as i64, |d| d.as_millis() as i64),
                    None => call.total_ms as i64,
                };
            }
            if self.turn_first_reply_end.is_none() && !failed && !aborted {
                self.turn_first_reply_end =
                    Some(call.started_at + Duration::from_millis(call.total_ms));
            }
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
        // 按实际调用数，不按工具长度表：那张表按工具名聚合，一条回复并行派两个 Agent 只有一项。
        // 调用表缺了（老抓包回放只给长度表）时退回长度表，至少算一个。
        let is_agent = |name: &str| name == "Agent" || name == "Task";
        let spawned = call
            .tool_calls
            .iter()
            .filter(|c| is_agent(&c.name))
            .count()
            .max(usize::from(call.tool_use_lens.iter().any(|(name, _)| is_agent(name))));
        if is_main && spawned > 0 {
            self.last_spawn_request_id = call.request_id.clone();
            if !failed && !aborted {
                let mut prompts: Vec<String> = call
                    .tool_calls
                    .iter()
                    .filter(|c| is_agent(&c.name))
                    .map(|c| {
                        c.input
                            .get("prompt")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim()
                            .to_string()
                    })
                    .collect();
                prompts.resize(spawned, String::new());
                self.delegated_pending.extend(prompts);
            }
        }
        // 子代理再派的孙代理：只记个数，孙代理首条按它认领，不占主线程那份。
        if kind == Kind::Subagent && spawned > 0 && !failed && !aborted {
            self.nested_pending += spawned as u32;
        }
        // 子代理以 SubagentHandback 收尾（`cap/auto-2.1.285-20260930` 07:51:29.419–.455）：那个工具
        // 客户端当场执行，子代理不再发请求，这一条就是它的最后一条。
        let handback = (kind == Kind::Subagent && !turn_over && !failed && !aborted)
            .then(|| call.tool_calls.iter().find(|c| c.name == "SubagentHandback").cloned())
            .flatten();
        // 摘要请求不推进支线，只记数（收尾那条摘要器统计要它）。收没收下照客户端的过滤（`ad`）：
        // 一行、去掉首尾空白后不超过 100 个字、不是标题或代码块才收。代理看不到正文的行数与开头，
        // 只按字数判——多行或以 `#`、``` 开头的短回复会被多算成收下。
        if matches!(kind, Kind::AgentSummary | Kind::AgentSummaryShort)
            && !aborted
            && let Some(a) = self.agents.get_mut(agent_key)
        {
            a.summaries += 1;
            // 计时器触发那一刻读配置、发头一次；中途重发过的（[`RetryLane::AgentSummary`]，前面几次已经并进
            // 这一条）按头一次算。
            let sent = call.logical_start();
            // 摘要器每次按当时的模式发：short 走侧查询，full 从子代理分叉。官方 `calls_kept_on_full`
            // 数的是「发这一条时配置是 short、却按 full 发」的次数（每次计时器触发都重读配置），所以逐条
            // 按**发出那一刻**号的 eval（[`crate::proxy::agent_summary_config::config_at`]）判——遥测在
            // 回复完成后才处理，那时的配置可能已经换过；也不在收尾时拿最后的配置回算。这一条本身是
            // short 就说明那一刻配置必是 short，不必再看。
            let short = kind == Kind::AgentSummaryShort;
            // 2.1.307：short 摘要的模型与子代理不同，就是换了 writer。换上了，客户端的小快模型就是它；没换、
            // writer 也没失败过，那一刻配着的 writer（有的话）就不是小快模型（收尾那条按这两样推 `fe()`）。
            let writer = a.model.as_deref().is_some_and(|m| m != shape.model);
            if short {
                a.summaries_short += 1;
                if writer {
                    a.small_fast = Some(shape.model.clone());
                } else if !a.summary_writer_failed {
                    let cfg = crate::proxy::agent_summary_config::config_at(
                        call.cred_id,
                        model_family(&a.session_model),
                        sent,
                    );
                    let family = model_family(a.model.as_deref().unwrap_or_default());
                    if let Some(w) = cfg.writer_for(family)
                        && !a.not_small_fast.iter().any(|x| x == w)
                    {
                        a.not_small_fast.push(w.to_string());
                    }
                }
            } else if crate::proxy::agent_summary_config::config_at(
                call.cred_id,
                model_family(&a.session_model),
                sent,
            )
            .short
            {
                a.summaries_kept_on_full += 1;
            }
            a.summary_last_short = Some(short);
            a.summary_last_sent = Some(sent);
            a.summary_last_end = Some(this_end_wall(call));
            if failed {
                // 走到这里的是整次调用的最终失败：可重试的失败先扣着，客户端重发成功就并进成功那条。
                a.summaries_failed += 1;
                // 换了 writer 的 short 摘要失败了：摘要器记下，之后不再换。
                if short && writer {
                    a.summary_writer_failed = true;
                }
            } else if (1..=100).contains(&call.text_chars) {
                a.summaries_accepted += 1;
            }
        }
        // 子代理支线推进一步（摘要请求不算）。
        let mut agent_done: Option<AgentState> = None;
        if kind == Kind::Subagent
            && let Some(a) = self.agents.get_mut(agent_key)
        {
            a.steps += 1;
            a.last_end = Some(this_end_wall(call));
            a.tool_uses += shape.tool_uses.len() as u32;
            a.last_request_id = call.request_id.clone().or(a.last_request_id.take());
            a.last_message_id = call.message_id.clone().or(a.last_message_id.take());
            if let Some(m) = call.resp_model.clone().filter(|m| !m.is_empty()) {
                a.last_reply_model = Some(m);
            }
            if !failed {
                a.prev_total = call.input_tokens
                    + call.cache_read_tokens
                    + call.cache_creation_tokens
                    + call.output_tokens;
                a.tools_hash = Some(shape.tools_hash.clone());
            }
            // `end_turn` 收尾就是子代理跑完了：收尾事件要它的全程统计，档案随之删掉。以
            // SubagentHandback 工具收尾的也是（客户端就地执行、不再发请求）。
            if ends_agent(call) {
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

/// 请求体里首条用户消息的各个文本块（字符串内容算一块）。
fn first_user_texts(body: &[u8]) -> Vec<String> {
    let Ok(v) = serde_json::from_slice::<Value>(body) else { return Vec::new() };
    let Some(m) = v["messages"].as_array().and_then(|ms| ms.iter().find(|m| m["role"] == "user"))
    else {
        return Vec::new();
    };
    match &m["content"] {
        Value::String(s) => vec![s.clone()],
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str).map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}
