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
        let shape = call.body.facts().shape();
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
        // 没补全形态、只从只留身份的底本沿用了工具表（[`ThreadBase::tools_known`]）。
        let mut tools_from_base = false;
        // 确认接上的那份底本里记的回复：它的上游 request-id 与工具调用（同一条回复后面接了兄弟分支时，会话里「最近一条
        // 回复」是别的分支，工具详情与 requestId 都得从这条所接的回复取）。
        let mut parent_reply: Option<ParentReply> = None;
        // 先按续轮指针挑底本（[`ThreadLine::select`]：按回复 id 留着的那几份里找），挑不出（指向没见过的回复）就不补——
        // 拿对不上的底本补出来的消息数、工具、system 都是错的。
        let line = st
            .sessions
            .get(&(call.cred_id, session_id.clone()))
            .and_then(|s| s.thread_bases.get(&thread_key));
        let parent_of = |base: &ThreadBase| ParentReply {
            request_id: base.request_id.clone(),
            calls: base.reply_calls.clone(),
            depth: base.depth,
            chain_id: base.chain_id.clone(),
            prompt_id: base.prompt_id.clone(),
            turn_started: base.turn_started,
            turn: base.turn.clone(),
            turn_unknown: base.turn_unknown,
            total_tokens: base.total_tokens,
            message_id: base.reply_id.clone(),
            tether: base.tether.clone(),
            model: base.model.clone(),
            tools_hash: base.tools_hash.clone(),
            tools_known: base.tools_known,
        };
        if is_delta
            && let Some(line) = line
            && let Some((base, confirmed)) = line.select(shape.thread_prev_id.as_deref())
        {
            // 接的是一份只留身份的底本（它自己就是没接上的半截增量）：形态补不了，这条同样是半截的；身份照认。
            if !base.shape_incomplete {
                let auto_in_delta = shape.auto_marker || shape.permission_declared;
                base.fill(&mut shape, auto_in_delta, confirmed);
                filled_from_base = true;
            } else if confirmed {
                // 形态补不了，但带回的工具结果照样是那条回复调的工具：配上工具名，续轮那串工具事件才发得出来。
                base.pair_orphan_results(&mut shape);
                // 那条的工具表知道（它体里明说过，或它接的那条知道）：这条没带 `tools` 就沿用它的（321 `00088` → `00091`
                // 冷回放 toolsCount 18）。
                if base.tools_known && !shape.tools_declared {
                    base.fill_tools(&mut shape);
                    tools_from_base = true;
                }
            }
            if confirmed {
                parent_reply = Some(parent_of(&base));
            }
        } else if !is_delta
            && let Some(prev) = shape.diag_prev_message_id.as_deref()
            && let Some(line) = line
            && let Some((base, true)) = line.select(Some(prev))
        {
            // 全量请求不用底本补形态（体里就是全量），但它声明接的那条回复（`diagnostics.previous_message_id`）照样决定
            // 工具事件的身份、深度、查询链与本轮累计：回接过旧的一轮之后，会话里「最近一条回复」可能是旧轮那条。
            parent_reply = Some(parent_of(&base));
        } else if !is_delta
            && shape.diag_prev_message_id.is_none()
            && !shape.tool_uses.is_empty()
            && let Some(line) = line
            && let Some(base) =
                line.find_by_tool_call(|id| shape.tool_uses.iter().any(|t| t.id == id))
        {
            // 没带父指针的全量续轮：它回传的工具结果是哪条回复调的，那条就是它接的回复（工具调用 id 只此一家）。
            parent_reply = Some(parent_of(&base));
        }
        // 没带父指针的全量续轮回传的工具，哪份底本的回复都没调过、这条线程最近那条回复记下的调用里也没有：它接的不是最近那条，
        // 所接的回复认不出（同明说的指针指向没见过的回复）。最近那条没调工具（正常的文本回复）同样如此——工具结果只能接调了它的
        // 回复。只认成功回复记下的调用（[`Session::absorb_request`] 失败、取消的不记）；这条线程一条回复都没记过才看不出。
        let implicit_parent_unknown = !is_delta
            && parent_reply.is_none()
            && shape.diag_prev_message_id.is_none()
            && !shape.tool_uses.is_empty()
            && st
                .sessions
                .get(&(call.cred_id, session_id.clone()))
                .and_then(|s| s.reply_calls.get(&thread_key))
                .is_some_and(|calls| {
                    !calls.iter().any(|c| shape.tool_uses.iter().any(|t| t.id == c.id))
                });
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
                if let Some(m) = c.body.facts().model.clone() {
                    sess.waiting_titles.push((c.logical_start(), sleepy_key(&m)));
                }
                sess.deferred.push((c, prev_end, at));
            }
        }
        // 这条主线程续轮不属于会话里最近这一轮：回接更早的一轮（所接回复的那一轮不是此刻这一轮），或接一条认不出的回复
        // （不在底本里、也不是最近那条），或接的那条自己那一轮就说不准。会话状态被它改动之前整份存下，处理完放回
        // （[`super::session::LatestState`]）。新输入照判（同 [`Session::advance_turn`]，冷启动的工具结果续轮不算新输入）。
        let early_main = kind == Kind::Main;
        let early_new_prompt = early_main
            && (shape.new_prompt || (sess.main_requests == 0 && !cold_continuation(&shape)));
        let declared_prev =
            shape.thread_prev_id.as_deref().or(shape.diag_prev_message_id.as_deref());
        let isolate = early_main
            && !early_new_prompt
            && match &parent_reply {
                None => {
                    implicit_parent_unknown
                        || declared_prev
                            .is_some_and(|d| sess.last_main_message_id.as_deref() != Some(d))
                }
                Some(p) => p.turn_unknown || p.turn.as_ref().is_some_and(|t| !sess.same_turn(t)),
            };
        let saved_latest = isolate.then(|| sess.snapshot_latest(&thread_key));
        let (prev_reply_calls, git_outcome) = sess.absorb_request(
            &call,
            &mut shape,
            kind,
            &thread_key,
            &agent_key,
            parent_reply.as_ref().map(|p| p.calls.as_slice()),
        );
        // 一轮结束（`end_turn`）才有 stop hook 与 turn_end；`tool_use` 是同一轮的中间步。
        let turn_over = call.stop_reason.as_deref().is_none_or(|s| s != "tool_use");
        // 这条请求客户端那头是失败的：收尾走 `tengu_api_error` 那一串，且没有任何用量。
        let failed = call.failure.is_some();
        // 客户端取消（见 [`ApiCall::aborted`]）：没有 success 也没有 error，收尾是取消那一串。
        let aborted = call.aborted && !failed;
        let is_main = kind == Kind::Main;
        // 没补全形态的线程增量（底本没接上，体里只有新增那几条）：工具表、消息数都是半截的，不拿来更新线程配置（tether 的
        // 上一条、工具长度表报过的 hash）——之后明说接回完整底本的续轮要跟的仍是此前那份。
        let shape_partial = is_delta && !filled_from_base;
        // 半截的增量里工具表知道：体里明说了，或从所接的那条沿用了。
        let partial_tools_known = shape.tools_declared || tools_from_base;
        // 半截的增量里没有助手消息：增量那几条就是末条助手之后的条数（同 [`ThreadBase::fill`]），这一项照样知道。
        if shape_partial && shape.assistant_messages == 0 {
            shape.after_last_assistant = shape.messages_len;
        }
        let flags = CallFlags {
            kind,
            is_main,
            turn_over,
            failed,
            aborted,
            shape_partial,
            partial_tools_known,
            post_compaction: false,
        };
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
        // 这条新输入拒绝的是确认接上的那条回复里的工具（回接旧的一轮、中间夹过别的新输入时尤其如此）：拒绝那一串是那一轮
        // 的事，挂它的 prompt、查询链与深度，不是会话里最近那一轮的。
        let (prev_turn, prev_prompt_id) = match &parent_reply {
            Some(p) if !rejected_calls.is_empty() => (
                (
                    if p.chain_id.is_empty() { prev_turn.0 } else { p.chain_id.clone() },
                    p.depth,
                    p.turn_started.or(prev_turn.2),
                ),
                if p.prompt_id.is_empty() { prev_prompt_id } else { p.prompt_id.clone() },
            ),
            _ => (prev_turn, prev_prompt_id),
        };
        // 续轮（不是新输入）确认接上了某条回复：它是那条回复所在那一轮的事——体里没带 `cc_prompt_id` 时 prompt 用那一轮的，
        // 本轮起点也是那一轮的（回接旧的一轮时，会话里的 prompt 与起点已是后来那次新输入的）。
        let parent_turn = parent_reply.as_ref().filter(|_| !new_prompt);
        let prompt_id = match parent_turn {
            Some(p) if shape.cc_prompt_id.is_none() && !p.prompt_id.is_empty() => {
                p.prompt_id.clone()
            }
            _ => prompt_id,
        };
        // 主线程回接那一轮：首字出没出过、API 次数与耗时、首条回复的结束时刻这些本轮累计也换回那一轮的（夹在中间的别的
        // 请求不算进去）。线性往下接的续轮，换回来的就是此刻的，没有变化。
        // 回接的是更早的一轮（不是会话此刻这一轮）时只在这一次调用里借用：处理完把会话里较新那一轮的累计放回去
        // （见下面 `record_call` 之后），不然之后接着那一轮的全量请求会读到这里换进来的旧累计。
        if is_main && let Some(t) = parent_turn.and_then(|p| p.turn.as_ref()) {
            sess.restore_turn(t);
        }
        let turn_started_at: Option<SystemTime> =
            parent_turn.and_then(|p| p.turn_started).or(sess.turn_started);
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
        let prev_main_message_id = sess.last_main_message_id.clone();
        // 这条所接的那条回复认不出：声明接的（续轮指针 / `diagnostics.previous_message_id`）既不在底本里、也不是最近那条，或没带
        // 指针、回传的工具不是任何认得的回复调的。新输入也算——它声明接的那条回复的用量、request-id 同样无从得知（底本被
        // 挤掉之后不能拿最近那条顶）。**凡是取「所接那条回复」的地方都看它**：`messageTokens`、`previousRequestId`、诊断里的
        // `previousMessageId`、工具事件的 `messageID`，认不出就不报，不回落到会话里最近那条。
        let parent_ref_unknown = parent_reply.is_none()
            && matches!(kind, Kind::Main | Kind::Subagent)
            && if new_prompt {
                // 新输入声明接的回复（`diagnostics.previous_message_id`）不在底本里、也不是最近那条主线程回复：认不出。不靠
                // 「被挤掉的记录」判（那份记录总有上限，挤得够多又会回落）。两种照旧回落、不算认不出：这个进程还没见过这个会话
                // 的主线程回复（冷启动、`--resume`），或声明的正是刚被打断（Esc）的那条——它不进最近回复的记录。
                // 线程增量里的新输入以续轮指针声明（`thread.previous_message_id`），全量的以 diagnostics 声明。
                shape
                    .thread_prev_id
                    .as_deref()
                    .or(shape.diag_prev_message_id.as_deref())
                    .is_some_and(|d| {
                        prev_main_message_id.as_deref().is_some_and(|latest| latest != d)
                            && sess.interrupted_message_id.as_deref() != Some(d)
                    })
            } else {
                implicit_parent_unknown
                    || shape
                        .thread_prev_id
                        .as_deref()
                        .or(shape.diag_prev_message_id.as_deref())
                        .is_some_and(|declared| {
                            let latest = if is_main {
                                prev_main_message_id.as_deref()
                            } else {
                                agent.as_ref().and_then(|a| a.last_message_id.as_deref())
                            };
                            latest != Some(declared)
                        })
            };
        // 续轮（不是新输入）所接的回复认不出：这一轮的来龙去脉（查询链、深度、轮累计）也说不准。
        let parent_unknown = !new_prompt && parent_ref_unknown;
        // 这条所在那一轮（主线程的一轮、子代理的那条支线）的来龙去脉说不准：所接的回复认不出，或接的那条自己那一轮就说不准
        // （它底本上记着 [`ThreadBase::turn_unknown`]）。查询链、深度、轮时长都不报，底本也照记「说不准」传给接着它的续轮
        // ——主线程、子代理一样。
        // 子代理照支线状态回落、接的正是支线最近那条，而那条自己就说不准（[`AgentState::lineage_unknown`]）：同样说不准。
        let follows_unknown_agent_reply = kind == Kind::Subagent
            && parent_reply.is_none()
            && sess.agents.get(&agent_key).is_some_and(|a| a.lineage_unknown);
        // 主线程这个会话里一次新输入都没见过（冷启动后先接到的都是半截的续轮）：照会话状态回落的续轮，接的那一轮从没见过。
        let session_turn_unseen = is_main && parent_reply.is_none() && sess.prompts_seen == 0;
        let lineage_unknown = matches!(kind, Kind::Main | Kind::Subagent)
            && !new_prompt
            && (parent_unknown
                || follows_unknown_agent_reply
                || session_turn_unseen
                || parent_reply.as_ref().is_some_and(|p| p.turn_unknown));
        // 主线程这一轮的累计说不准：这条只在这一次调用里借用会话里的累计（照常累计、照常读出，但事件里不报），处理完原样放回
        // （[`super::session::LatestState`]），也不记进它的底本——不然会话里那一轮与之后接着它的续轮都会带上这条的数。
        let turn_stats_unknown = is_main && lineage_unknown;
        // 所接回复认不出时，上一条请求是谁也不知道：只认体里自己声明的 `cc_prev_req`，不回落到会话里最近那条。
        let previous_request_id = (kind.has_chain()
            && (!parent_ref_unknown || shape.cc_prev_req.is_some()))
        .then(|| {
            shape
                .cc_prev_req
                .clone()
                .or_else(|| parent_reply.as_ref().and_then(|p| p.request_id.clone()))
                .or_else(|| match (&agent, &suggestion_fork) {
                    (Some(a), _) => a.last_request_id.clone(),
                    (None, Some(f)) => f.request_id.clone(),
                    (None, None) => sess.last_main_request_id.clone(),
                })
        })
        .flatten();
        let prev_main_request_id = sess.last_main_request_id.clone();
        let prev_main_depth = sess.last_main_depth;
        // `timeSinceLastApiCallMs` = **这条完成时刻 − 上一条完成时刻**（不分主线程/侧查询，
        // 按完成先后）。`cap/2.1.260-2`：标题 09.490−06.166=3323、主线程 12.019−09.490=2529、
        // 续轮 19.458−12.019=7439、猜下一句 21.385−19.458=1927，全部对上；用「这条开始」算
        // 的话并发的标题与主线程会出负数、续轮只剩工具执行那一秒。
        let (this_end, prev_end, time_since_last) = sess.note_end(&call, prev_end_override);
        // `messageTokens` 是所接那条回复的总用量：确认接上的父回复按它自己的（回接旧的一轮、兄弟分支、接一条说不准的回复都
        // 一样，用量是那条回复本身的）；所接的回复认不出就不报；其余照会话 / 支线里最近那条。
        let parent_tokens = parent_reply.as_ref().and_then(|p| p.total_tokens);
        // 压缩之后的第一条主线程请求从零算（`isPostCompaction`，321 `00204` 报 0）：压缩前那条回复的用量不算——哪怕它正是所接的那条。
        let message_tokens = match (&agent, &suggestion_fork) {
            _ if post_compaction => Some(0),
            _ if parent_ref_unknown => None,
            _ if parent_tokens.is_some() => parent_tokens,
            (Some(a), _) => Some(a.prev_total),
            (None, Some(f)) => Some(f.total_tokens),
            (None, None) if kind.has_chain() => Some(sess.prev_total_input),
            (None, None) => Some(0),
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
        // 模型换没换：确认接上了某条回复就跟那条比（中间夹着别的分支、半截增量时，会话里最近那条不是它所接的）。
        let model_changed = is_main
            && match parent_reply.as_ref().filter(|p| !p.model.is_empty()) {
                Some(p) => p.model != display_model,
                // 所接的回复认不出：上一条是什么模型不知道，不报（会话里最近那条不是它所接的）。
                None if parent_ref_unknown => false,
                None => sess.last_model.as_deref().is_some_and(|m| m != display_model),
            };
        // 同理：`diagnostics.previous_message_id` 是这条请求自己声明的那个，优先于会话状态；没声明时用确认接上的那条回复（没带
        // 指针、按回传的工具认出的），所接的认不出就不报，都不是才回落到会话里最近那条回复。
        let previous_message_id = shape
            .diag_prev_message_id
            .clone()
            .or_else(|| parent_reply.as_ref().and_then(|p| p.message_id.clone()))
            .or_else(|| (!parent_ref_unknown).then(|| sess.last_message_id.clone()).flatten());
        let tools_slot = match &agent {
            Some(a) => &a.tools_hash,
            None if kind.has_boundary() => &sess.tools_hash_main,
            None => &sess.tools_hash_side,
        };
        // 半截的增量没带 `tools`：它的工具表不知道，不算变了（不重发长度表）。
        // 确认接上了某条回复就跟那条的工具表比（同上，不跟别的分支刚报过的比）。
        let reported = match parent_reply.as_ref().filter(|p| !p.tools_hash.is_empty()) {
            Some(p) => Some(p.tools_hash.as_str()),
            None => tools_slot.as_deref(),
        };
        // 所接的底本工具表不知道（只留身份、体里又没带 `tools`，[`ThreadBase::tools_known`]）：变没变说不准，不重发长度表。
        let parent_tools_unknown = parent_reply.as_ref().is_some_and(|p| !p.tools_known);
        let tools_changed = !(shape_partial && !partial_tools_known)
            && !parent_tools_unknown
            && reported != Some(shape.tools_hash.as_str());
        let counted = sess.counted;
        let started_wall = sess.started_wall;
        // queryDepth：主线程本轮第几次请求；猜下一句 = 主线程最后一次 + 2（抓包：0→2、1→3）；
        // 子代理从 2 起每条 +1、整条支线一个链（`cap/2.1.277` 2…27、`cap/2.1.280` 2…7），
        // 它的摘要请求恒为 3、链另起（两份抓包 7 条都是）。
        // 主线程当前的链：猜下一句的 fork 统计里引用的是这条父链。
        let main_chain = sess.chain_id.clone();
        let (mut chain_id, mut query_depth) = match &suggestion_fork {
            Some(f) => (f.chain_id.clone(), f.depth + 1),
            None => sess.chain_for(kind, agent.as_ref()),
        };
        // 主线程或子代理的工具结果续轮确认接上了某条回复：深度是那条回复的加一、查询链是那条回复的，不是按处理先后累加
        // ——同一条回复后面接了两条兄弟分支时，后到的那条在会话里的深度已被先到的那条推过一格；回接旧的一轮时，会话的
        // 查询链已是后来那次新输入的。之后的累加也从这里接着（主线程的 `turn_depth`、子代理的步数）。
        // 接的那条自己那一轮就说不准（[`ThreadBase::turn_unknown`]）：它记的深度、链本是猜的，不拿来接着推（深度与链这一条
        // 也不报，见 `lineage_unknown`），也不让它改动子代理的步数。
        if matches!(kind, Kind::Main | Kind::Subagent)
            && !new_prompt
            && let Some(p) = parent_reply.as_ref().filter(|p| !p.turn_unknown)
        {
            query_depth = p.depth + 1;
            if !p.chain_id.is_empty() {
                chain_id = p.chain_id.clone();
            }
            if kind == Kind::Main {
                sess.turn_depth = query_depth;
            } else if let Some(a) = sess.agents.get_mut(&agent_key) {
                a.steps = query_depth.saturating_sub(2);
            }
        }
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
            turn_started_at.unwrap_or(call.started_at - Duration::from_millis(15)).into();
        // 这一轮说不准（[`TurnFacts::turn_stats_unknown`]）：这一轮此前出没出过字、从哪起都不知道，首字事件（含被打断那种）
        // 不报——否则同一轮里接着它的几条会各报一次「首字」，时长也借自别的轮。
        let first_text_in_turn =
            is_main && call.text_chars > 0 && !sess.turn_text_seen && !turn_stats_unknown;
        // 一个字都还没出就被打断：首字事件报 `interrupted`（`cap/auto-2.1.285-20260930` 08:01:42.635）。
        let first_text_interrupted = is_main
            && aborted
            && call.text_chars == 0
            && !sess.turn_text_seen
            && !turn_stats_unknown;
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
        // 2.1.312：权限与 auto 判定事件带 tool_use 的 id、`context_announcement` 多 `assumed_utc` 等，见
        // [`TurnFacts::v312`]；`tengu_snoopy_creek` 读不到时也改按关（[`crate::proxy::snoopy_creek`]）。
        let v312 = version_at_least(&version, "2.1.312");
        // 2.1.314：`api_success` 多新旧图片与线程类型那五项、auto 判定多连接方式、压缩两条多 `leaveOutPasses`，错误栈
        // 又换了块，见 [`TurnFacts::v314`]（2.1.313 没有发布）。
        let v314 = version_at_least(&version, "2.1.314");
        // 2.1.315 起：Bash 事件多程序名、测试范围 / 结果与重复命令统计，错误栈换块，见 [`TurnFacts::v316`]（只抓了
        // 2.1.316，2.1.315 的 JS 层与它只差一个 plist 串）。
        let v316 = version_at_least(&version, "2.1.315");
        // 2.1.317：Bash 事件多「输出交给 tail」那组、`bash_test_outcome` 的 `head` 放宽、重复命令统计按会话抽样并换布局，
        // 错误栈换块、5xx 带构造名，见 [`TurnFacts::v317`]。
        let v317 = version_at_least(&version, "2.1.317");
        // 2.1.318：错误栈换块、5xx 不再带构造名，`/compact` 那条去掉 `summaryPromptArm`，见 [`TurnFacts::v318`]。
        let v318 = version_at_least(&version, "2.1.318");
        // 2.1.319：错误栈换块，`tool_use_success` 多 `tool_input_stream_ms`、`tether_decision` 多 `toolsHashKept`，见
        // [`TurnFacts::v319`]。
        let v319 = version_at_least(&version, "2.1.319");
        // 2.1.321：错误栈换块，见 [`TurnFacts::v321`]（2.1.320 没抓，按这一版）。
        let v321 = version_at_least(&version, "2.1.320");
        // 2.1.322：错误栈换块，见 [`TurnFacts::v322`]。
        let v322 = version_at_least(&version, "2.1.322");
        // 这个会话的 Bash 调用记不记重复命令统计（2.1.317 起按会话 id 抽样，比例跟号的 `tengu_vivid_peach`）。
        let bash_metered = v317
            && super::bash_repeat::metered(
                &session_id,
                crate::proxy::vivid_peach::rate(call.cred_id),
            );
        let safety_events =
            v307 && crate::proxy::snoopy_creek::account_enabled(call.cred_id, &version);
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
        // 头一回带上 `x-claude-code-sandy-plover` 的那条报首次使用，按进程一次（`/clear` 接着算）。重发过的看整串；这个
        // 会话的额度探测带过也算（[`State::tools_used_header_sent`]）——真实 CC 经网关透传时主线程不带这个头，只有探测带。
        let probe_header = st.tools_used_header_sent.remove(&key).is_some();
        let used_header = call.chain_tools_used_header() || probe_header;
        let tools_used_header_first = used_header && !sess.tools_used_header_reported;
        sess.tools_used_header_reported |= used_header;
        // 2.1.318 起拼头时记档位（[`super::template::tools_used_size_events`]）：额度探测那条带的是 `[]`，排在这条之前。
        let tools_used_size_events = if v318 {
            let mut sizes = Vec::new();
            if probe_header {
                sizes.push(super::ToolsUsedSize::of("[]"));
            }
            sizes.extend(call.chain_tools_used_sizes());
            super::template::tools_used_size_events(&mut sess.tools_used_steps, &sizes)
        } else {
            Vec::new()
        };
        let tools_used_truncation = call.chain_tools_used_truncation();
        // 2.1.319：拼出的头里带了连接器字段 / MCP 标记，头一回各报一次（连接器字段按会话、MCP 标记按进程）。
        let sizes = call.chain_tools_used_sizes();
        let connector_fields = v319 && sizes.iter().any(|s| s.connector_fields);
        let mcp_mark = v319 && sizes.iter().any(|s| s.mcp_mark);
        let tools_used_connector_fields_first =
            connector_fields && !sess.tools_used_connector_fields_reported;
        sess.tools_used_connector_fields_reported |= connector_fields;
        let tools_used_mcp_mark_first = mcp_mark && !sess.tools_used_mcp_mark_reported;
        let batching_reminder_first =
            shape.send_time_nudge && !sess.batching_reminder_models.contains(&shape.model);
        if batching_reminder_first {
            sess.batching_reminder_models.push(shape.model.clone());
        }
        sess.tools_used_mcp_mark_reported |= mcp_mark;
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
                crate::proxy::lapis_anchor::session_mode(
                    call.cred_id,
                    &session_id,
                    &display_model,
                    &version,
                )
            } else {
                crate::proxy::lapis_anchor::peek_session_mode(
                    call.cred_id,
                    &session_id,
                    &display_model,
                    &version,
                )
            };
            m.client_data_events(own_system.then_some(crate::proxy::lapis_anchor::Observed {
                reminders: shape.total_tokens_reminder,
                max: shape.total_tokens_max,
            }))
        } else {
            Vec::new()
        };
        // 模型能力被号的 `client_data` 改了默认（2.1.314 起的原创游戏那段，[`crate::proxy::original_games`]）：官方组装
        // 主线程 system 时头一回用到就报一条来源事件，按会话、按「能力 + 模型」去重（`y()` 的 `-cap:model` / `cap:model`），
        // 所以会话中途切到一个被改了默认的模型，切过去那条也报。体归客户端自己的同样看实际出站：那段在不在体里得与
        // 下发的一致，经网关、拿不到号 client_data 的客户端不报。这条要发启动 / 首次输入模板、模板里有那一处（交互式
        // 首次输入那段的 `mcp_instructions_pool_change` 之后、`-p` 启动段的 `fork_subagent_enabled` 之前）就跟
        // `<total_tokens>` 那一族一起走模板（[`Subst::capability_events`]），否则在这条请求前补
        // （[`TurnFacts::capability_inline`]）。
        let capability = is_main
            .then(|| {
                crate::proxy::original_games::client_data_event(
                    call.cred_id,
                    &display_model,
                    &version,
                )
            })
            .flatten()
            .filter(|(name, _)| {
                // 体里那段在不在，得与下发一致：能力打开（`from`）时在；关掉（`off`）时只有 `heron_brook` 下发的正文里本就
                // 含这一段才在（2.1.318 抓包那天 opus 就是这样，[`crate::proxy::original_games::heron_covers`]）。看的是
                // 实际出站的 system（体归客户端的是客户端组装的，模拟的是 luban 组装的）；线程续轮只剩 billing 那块时看不出，
                // 这一轮不报也不记已报。
                let facts = call.body.facts();
                let has = (facts.prompt_sent() && !call.sim_no_rest).then(|| {
                    facts.has_original_games_text(crate::proxy::original_games::text_for(&version))
                });
                let expect = *name == "tengu_model_capability_from_client_data"
                    || crate::proxy::original_games::heron_covers(call.cred_id, &display_model);
                has == Some(expect)
            })
            .filter(|(name, m)| {
                let off = if *name == "tengu_model_capability_from_client_data" { "" } else { "-" };
                let key = format!("{off}{}:{}", m["capability"], m["model"]);
                sess.capability_logged.insert(key)
            });
        // 2.1.321 的 `# Finishing work` / `# Memory, notes and feedback` 两节同一套判定、同一种事件
        // （[`crate::proxy::finishing_notes`]），组装 system 时排在原创游戏那段后面算，事件也跟在它后面。体归客户端自己的
        // 同样看实际出站：打开（`from`）时那一节的标题得在体里，关掉（`off`）时得不在。
        let sections: Vec<(&'static str, Value)> = if is_main {
            crate::proxy::finishing_notes::client_data_events(
                call.cred_id,
                &display_model,
                &version,
            )
            .into_iter()
            .filter(|(section, name, _)| {
                // 同上看实际出站的 system；只剩 billing 那块（线程续轮）时看不出，这一轮不报也不记已报，等看得见的那一轮。
                let has = call.body.facts().has_section(*section).filter(|_| !call.sim_no_rest);
                has == Some(*name == "tengu_model_capability_from_client_data")
            })
            .filter(|(_, name, m)| {
                let off = if *name == "tengu_model_capability_from_client_data" { "" } else { "-" };
                let key = format!("{off}{}:{}", m["capability"], m["model"]);
                sess.capability_logged.insert(key)
            })
            .map(|(_, name, m)| (name, m))
            .collect()
        } else {
            Vec::new()
        };
        let capability: Vec<(&'static str, Value)> =
            capability.into_iter().chain(sections).collect();
        let via_template = if shape.sdk {
            continued_from.is_some() || (cleared_from.is_none() && is_new_session)
        } else {
            first_prompt_tpl
        };
        let (capability_events, capability_inline): (Vec<_>, Vec<_>) =
            if via_template { (capability, Vec::new()) } else { (Vec::new(), capability) };
        // 组织配置那道闸：2.1.310 起、见过这个号的 eval 才按它改启动模板那条影子判定、在闸内再补真判定
        // （[`Subst::org_gate`]）。分桶按这台机器（`device_id`）算。
        let org_gate = if v310 {
            crate::proxy::org_config_gate::flag(call.cred_id).map(|flag| super::template::OrgGate {
                flag,
                tier: crate::proxy::org_config_gate::tier_of(call.org_type.as_deref()),
                bucket: crate::proxy::org_config_gate::bucket(&identity.device_id),
            })
        } else {
            None
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
        let tether_and_state = (v280 && matches!(kind, Kind::Main | Kind::Subagent) && threads)
            .then(|| {
                let flags = CallFlags { post_compaction, ..flags };
                let parent = parent_reply.as_ref().and_then(|p| p.tether.as_ref());
                sess.tether_for(&call, &shape, &display_model, flags, &agent_key, parent)
            });
        let (tether, tether_state) = match tether_and_state {
            Some((t, s)) => (Some(t), Some(s)),
            None => (None, None),
        };
        // 这条线程的全量形态记下来，给下一条增量请求补（见 [`ThreadBase`]）。
        // 只记全量视角：不是增量（`create` 或没开线程），或增量已经按底本补全了。没补上的增量（底本
        // 丢了）不能拿来当底本。此前用「工具数 > 0」近似「是全量」，无工具的主线程对话（模拟路径不补
        // 工具时的第三方多轮）首轮就记不下，续轮报出的 system 长度、消息数都是增量那份。
        // 指向没见过的回复的增量没补（`filled_from_base` 为 false），也就不存：留着已确认的那份，等真正接着它的那条来推进。
        // 没接上底本的增量（`filled_from_base` 为 false）不当最新那份——留着已确认的那份，等真正接着它的那条来推进；但只要
        // 有回复 id，就按 id 留一份只有身份的（[`ThreadBase::shape_incomplete`]）：之后明说接它、或回传它调的工具的续轮，
        // 照样认得出接的是它（用量、调的工具、「那一轮说不准」都从这里来）。
        if matches!(kind, Kind::Main | Kind::Subagent)
            && (!is_delta || filled_from_base || call.message_id.is_some())
        {
            let mut base = ThreadBase::of(
                &shape,
                call.reply_input_chars,
                call.tool_use_lens.clone(),
                call.message_id.clone(),
            );
            base.request_id = call.request_id.clone();
            base.depth = query_depth;
            base.chain_id = chain_id.clone();
            base.prompt_id = prompt_id.clone();
            base.turn_started = turn_started_at;
            base.turn_unknown = lineage_unknown;
            // 失败、取消的没有用量：接着它的续轮报的就是它自己报的那个（它接的那条的）。
            base.total_tokens = if !failed && !aborted {
                Some(
                    call.input_tokens
                        + call.cache_read_tokens
                        + call.cache_creation_tokens
                        + call.output_tokens,
                )
            } else {
                message_tokens
            };
            base.reply_calls = Arc::new(call.tool_calls.clone());
            base.model = display_model.clone();
            // 这条处理完的线程配置（半截的增量没写进去，它的底本只留身份，不记）。
            // 半截的增量记一份只知道一半的（[`TetherThread::partial`]），接着它的请求按它比。
            base.tether = tether_state;
            let line = sess.thread_bases.entry(thread_key.clone()).or_default();
            if !is_delta || filled_from_base {
                line.store(base);
            } else {
                base.shape_incomplete = true;
                base.tools_known = partial_tools_known;
                line.store_detached(base);
            }
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
        if kind == Kind::Subagent
            && let Some(a) = sess.agents.get_mut(&agent_key)
        {
            a.lineage_unknown = lineage_unknown;
        }
        // 这条请求累计进这一轮之后，把那一轮此刻的累计记到它的底本上（回接它的续轮换回这份）。
        if is_main
            && !turn_stats_unknown
            && let Some(reply) = call.message_id.as_deref()
        {
            let turn = sess.turn_stats();
            if let Some(line) = sess.thread_bases.get_mut(&thread_key) {
                line.set_turn(reply, turn);
            }
        }
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
        // 这条（回接旧的一轮）的本轮累计都已取出、也记回了那一轮的底本：会话放回较新那一轮的累计与深度。必须在取完之后
        // 放，不然这条自己的事件读到的是较新那一轮的。
        if let Some(latest) = saved_latest {
            sess.restore_latest(&thread_key, latest);
        }
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
            parent_reply_request_id: parent_reply.as_ref().and_then(|p| p.request_id.clone()),
            parent_reply_message_id: parent_reply.as_ref().and_then(|p| p.message_id.clone()),
            parent_reply_depth: parent_reply.as_ref().map(|p| p.depth),
            parent_unknown,
            lineage_unknown,
            tools_unknown: shape_partial && !partial_tools_known,
            turn_stats_unknown,
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
            v312,
            v314,
            v316,
            v317,
            v318,
            v319,
            v321,
            v322,
            bash_metered,
            tools_used_header_first,
            tools_used_connector_fields_first,
            tools_used_mcp_mark_first,
            batching_reminder_first,
            tools_used_size_events,
            tools_used_truncation,
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
            capability_events,
            capability_inline,
            org_gate,
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
        // 建事件时拿出来用的文件大小表、心跳与文件账本，建到一半 panic（没料到的请求体）也得放回会话：外层只丢这一条的
        // 事件（[`Telemetry::drain`]），会话之后的请求还要接着用它们。
        let built = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            build_events(&call, &shape, &facts, &mut file_sizes, &mut hb, &mut files)
        }));
        let built = match built {
            Ok(b) => b,
            Err(p) => {
                sess.file_sizes = file_sizes;
                sess.hb = hb;
                sess.files = files;
                std::panic::resume_unwind(p);
            }
        };
        let BuiltEvents { events, dd, probe_side, loc, decisions, post_compact_attachments } =
            built;
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

/// 这条请求的角色与收尾方式，[`Session::advance_turn`] / [`Session::record_call`] / [`Session::tether_for`] 共用。
#[derive(Clone, Copy)]
struct CallFlags {
    kind: Kind,
    is_main: bool,
    /// 一轮结束（`end_turn`）；`tool_use` 是同一轮的中间步。
    turn_over: bool,
    failed: bool,
    aborted: bool,
    /// 形态是半截的（没补全的线程增量）：不更新线程配置（见 [`Telemetry::process`] 里 `shape_partial`）。
    shape_partial: bool,
    /// 半截的增量里工具表知道（体里明说了，或从所接的只留身份的底本沿用了）。
    partial_tools_known: bool,
    /// 压缩之后的头一条主线程请求（[`Session::advance_turn`] 判出来之后才填，之前恒 false）。
    post_compaction: bool,
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
        parent_calls: Option<&[ToolCall]>,
    ) -> (Vec<ToolCall>, &'static str) {
        // 续轮带回的工具结果配上一条回复里记下的调用：thread 续轮全靠它，全量请求也拿它补上判决。确认接上了某份底本的
        // 线程续轮用那条回复自己的调用（[`ThreadBase::reply_calls`]），否则用这条线程最近一条回复的。
        let latest = self.reply_calls.get(thread_key).map(Vec::as_slice);
        let reply = parent_calls.or(latest);
        if let Some(calls) = reply {
            for tu in shape.tool_uses.iter_mut() {
                if let Some(c) = calls.iter().find(|c| c.id == tu.id) {
                    tu.apply_call(&c.name, &c.input, c.verdict.as_ref());
                    tu.input_stream_ms = c.input_stream_ms;
                }
            }
        }
        // 上一条回复的工具调用：这条新输入若带着被拒的工具结果，要用它认出是哪个工具。
        let prev_reply_calls = reply.map(<[ToolCall]>::to_vec).unwrap_or_default();
        // 失败的没有回复：客户端接着发的（重发、下一条）接的仍是此前那条，不拿空调用盖掉它。回复以调工具收尾、调用却一条没记下
        // 的，记成「不知道」（删掉），不记成「没调工具」：后者是定论，没带指针的全量续轮回传工具时拿它判所接的不是最近那条。
        if matches!(kind, Kind::Main | Kind::Subagent) && !call.aborted && call.failure.is_none() {
            if call.tool_calls.is_empty() && call.stop_reason.as_deref() == Some("tool_use") {
                self.reply_calls.remove(thread_key);
            } else {
                self.reply_calls.insert(thread_key.to_string(), call.tool_calls.clone());
            }
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
        let CallFlags { kind, is_main, turn_over, failed, aborted, .. } = flags;
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
        // 会话头一条主线程请求（这个进程里头一回见到这个会话；按主线程请求数判，不按新输入数——冷启动先接到几条半截的续轮，
        // 之后那条没带父指针的也不是头一条）照算新输入，除非它分明是半截的一轮（[`cold_continuation`]）：那时它接的回复认
        // 不出，本轮累计按说不准处理。
        let new_prompt =
            is_main && (shape.new_prompt || (self.main_requests == 0 && !cold_continuation(shape)));
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
        // 2.1.317 重复命令统计判别的代理那次结果在不在上下文里：每条主线程 / 子代理请求都记下这条支线看得见的结果
        // （完整体整份换、线程续轮的增量体追加），不只在跑 Bash 的那条上记——子代理首条完整请求带着的父线程结果，
        // 之后的增量续轮里不会再出现。
        if matches!(kind, Kind::Main | Kind::Subagent) {
            let agent = if kind == Kind::Subagent {
                call.agent.agent_id.as_deref().unwrap_or("")
            } else {
                ""
            };
            let delta = shape.thread_type.as_deref() == Some("continue");
            self.files.bash_meter.note_visible(agent, &shape.result_ids, delta);
        }
        // 用户取消的压缩不算：官方只在压缩成功时清系统段缓存、换快照。
        if kind == Kind::Compact && !failed && !aborted {
            self.post_compact = true;
            // 压缩后这条支线先前 Bash 调用的结果都不在它的上下文里了（[`super::bash_repeat::BashMeter::compacted`]）；
            // 子代理自己压缩时请求同样带着它的 agent id，主线程压缩不动别的子代理。
            self.files.bash_meter.compacted(call.agent.agent_id.as_deref().unwrap_or(""));
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
        let facts = call.body.facts();
        let texts = &facts.first_user_texts;
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

    /// 这条线的 tether 判定，并把线程状态记成这条的。另返回这条处理完的线程配置，记进它的底本（[`ThreadBase::tether`]）：
    /// 半截的增量也给一份只知道一半的（[`TetherThread::partial`]），接着它的请求按它比，不回落到别的分支写的那份。
    fn tether_for(
        &mut self,
        call: &ApiCall,
        shape: &RequestShape,
        display_model: &str,
        flags: CallFlags,
        agent_key: &str,
        parent: Option<&TetherThread>,
    ) -> (Tether, TetherThread) {
        let CallFlags { is_main, shape_partial, partial_tools_known, post_compaction, .. } = flags;
        // tether 比 beta 时不算逐请求变的那两项（auto 模式的 `afk-mode`、`dangerous-tool-use`）：压缩前那条带、压缩后那条
        // 不带，官方照样报 `changedBetas: false`（`cap/auto-2.1.319-20261106-full/00209` → `00214`）。官方比之前还排过序，
        // 同一组换了先后不算变；只用于比较，出站头照原样。
        let betas = call
            .betas
            .as_deref()
            .unwrap_or_default()
            .split(',')
            .map(str::trim)
            .filter(|b| !b.is_empty())
            .filter(|b| !b.starts_with("afk-mode-") && !b.starts_with("dangerous-tool-use-"))
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>()
            .join(",");
        let cleared = if is_main && self.tether_main.is_none() {
            self.cleared_tether_messages.take()
        } else {
            None
        };
        let slot = if is_main {
            &mut self.tether_main
        } else {
            &mut self.agents.get_mut(agent_key).expect("agent state exists").tether
        };
        // 「上一条」：确认接上的那条回复处理完时的线程配置（[`ThreadBase::tether`]）；没确认的照这条线程共享的那份。同一条线程
        // 上交错着两条分支（D 接 X、E 接 A）时，各自跟自己所接的那条比，不跟另一条分支刚写进去的比。
        let prev = parent.or(slot.as_ref());
        // 半截的增量（[`CallFlags::shape_partial`]）：体里的 `thread: continue` 就是客户端判的接着用，配置没变；条数都是半截的，
        // 不报。不拿半截的形态去比。
        let partial_state = || TetherThread {
            model: display_model.to_string(),
            betas: betas.clone(),
            effort: shape.effort.clone(),
            tools_content: if partial_tools_known {
                shape.tools_content_hash.clone()
            } else {
                String::new()
            },
            tools_known: partial_tools_known,
            thinking_type: shape.thinking_type.clone(),
            messages: 0,
            turns: 0,
            hashes: Vec::new(),
            aborted: call.aborted,
            partial: true,
        };
        if shape_partial && shape.thread_type.as_deref() == Some("continue") {
            let t = Tether {
                decision: "continue",
                reason: "append",
                changed_model: false,
                changed_tools: false,
                changed_betas: false,
                changed_latched: false,
                changed_thinking: false,
                changed_effort: false,
                tools_kept: true,
                claimed_clear: false,
                claimed_rewind: false,
                claimed_compact: false,
                unclaimed: false,
                first_changed: -1,
                turns: 0,
                prev_messages: 0,
                // 比上一条多出来的：服务端持有的那条回复加体里这几条（同补全了的续轮，`cap/auto-2.1.321` 00063 冷回放 3）。
                delta: shape.messages_len + 1,
                counts_unknown: true,
                prev_unknown: false,
                tools_unknown: false,
                echo: true,
            };
            return (t, partial_state());
        }
        let t =
            tether_decide(prev, cleared, post_compaction, shape, display_model, &betas, is_main);
        // 客户端视角的整段：完整体就是体里那些；线程续轮是上一条的、加服务端持有的那条回复（不知道内容）、加增量，
        // 再对齐到补全后的条数。
        let hashes: Vec<Option<u64>> = if shape.thread_type.as_deref() == Some("continue") {
            let mut h = prev.map(|p| p.hashes.clone()).unwrap_or_default();
            h.push(None);
            h.extend(shape.message_hashes.iter().copied().map(Some));
            h.resize(shape.messages_len, None);
            h
        } else {
            shape.message_hashes.iter().copied().map(Some).collect()
        };
        // 半截的增量（[`CallFlags::shape_partial`]）照常判这一条，但不当成下一条要比的「上一条」。
        if shape_partial {
            return (t, partial_state());
        }
        let slot = if is_main {
            &mut self.tether_main
        } else {
            &mut self.agents.get_mut(agent_key).expect("agent state exists").tether
        };
        let state = TetherThread {
            model: display_model.to_string(),
            betas,
            effort: shape.effort.clone(),
            tools_content: shape.tools_content_hash.clone(),
            thinking_type: shape.thinking_type.clone(),
            messages: shape.messages_len,
            turns: t.turns,
            hashes,
            aborted: call.aborted,
            partial: false,
            tools_known: true,
        };
        *slot = Some(state.clone());
        (t, state)
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
        let CallFlags { kind, is_main, turn_over, failed, aborted, shape_partial, .. } = flags;
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
            // 2.1.315 起 short 摘要一律由客户端的小快模型写（官方 `Yss()`），子代理本身就是它（haiku 子代理）也照样
            // 算换了 writer：失败就记下、之后不再 short（官方 `K ||= ce === "short"`，不看模型同不同）。
            let writer = (version_at_least(&self.version, "2.1.315") && short)
                || a.model.as_deref().is_some_and(|m| m != shape.model);
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
                // 2.1.315 起摘要器不分模型族，一律读 `tengu_groovy_ember`（`config_at` 里 opus 族对应它）。
                if version_at_least(&self.version, "2.1.315") {
                    "opus"
                } else {
                    model_family(&a.session_model)
                },
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
                    if !a.summary_writer_failed {
                        a.summary_writer_failed_at = Some(this_end_wall(call));
                    }
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
                if !shape_partial {
                    a.tools_hash = Some(shape.tools_hash.clone());
                }
            }
            // `end_turn` 收尾就是子代理跑完了：收尾事件要它的全程统计，档案随之删掉。以
            // SubagentHandback 工具收尾的也是（客户端就地执行、不再发请求）。
            if ends_agent(call) {
                agent_done = self.agents.remove(agent_key);
            }
        }
        // 长度表报过一次就不再重发——但失败那条压根没报（`tengu_tool_schema_sizes` 在官方
        // 那边就长在 `tengu_api_success` 里），别让它把「已报过」的标记占掉。
        if !failed && !shape_partial && !kind.is_agent() && kind != Kind::ModelValidation {
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

/// 线程增量确认接上的那条回复（[`super::session::ThreadLine::select`]）里记的东西：它的上游 request-id、回复里的工具
/// 调用、queryDepth、queryChainId 与所在那一轮的 prompt id。这条增量带回的工具结果、拒绝都是那条回复的事。
struct ParentReply {
    request_id: Option<String>,
    calls: Arc<Vec<ToolCall>>,
    depth: u32,
    chain_id: String,
    prompt_id: String,
    turn_started: Option<SystemTime>,
    turn: Option<super::session::TurnStats>,
    turn_unknown: bool,
    total_tokens: Option<i64>,
    message_id: Option<String>,
    /// 那条请求处理完时的线程配置、模型与工具表 hash（[`ThreadBase::tether`]）：这条跟它比。
    tether: Option<TetherThread>,
    model: String,
    tools_hash: String,
    /// 那条的工具表知道（[`ThreadBase::tools_known`]）。
    tools_known: bool,
}

/// 会话头一条却分明是半截的一轮（不是新输入）：线程续轮（`thread.type: continue`），或带着工具结果、又明说了接哪条回复
/// （`diagnostics.previous_message_id`）的全量请求。客户端新进程（含 `--resume`）总以新输入、新建线程开头，不会这样——只有
/// luban 自己冷启动、没见过这一轮的开头时才会接到（`cap/auto-2.1.321-20261110-bashfail/2.1.321/00063` 单独回放）。体里以工具
/// 结果收尾、却没说接哪条回复的全量请求不算：新进程的首条照旧按第一次输入计。
fn cold_continuation(shape: &RequestShape) -> bool {
    !shape.new_prompt
        && (shape.thread_type.as_deref() == Some("continue")
            || (shape.diag_prev_message_id.is_some()
                && (!shape.tool_uses.is_empty() || !shape.orphan_results.is_empty())))
}
