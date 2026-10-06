//! 一条调用的事件链：[`EventEnv`] 是各段共用的只读输入，[`EventBuilder`] 攒输出，
//! 各段 `emit_*` 分在几个子模块里。

use super::*;

mod finish;
mod forks;
mod prompt;
mod request;
mod tools;

/// 拼这条调用的事件链：只读 [`TurnFacts`]，不碰会话状态。
pub(super) fn build_events(
    call: &ApiCall,
    shape: &RequestShape,
    f: &TurnFacts,
    file_sizes: &mut HashMap<String, usize>,
) -> BuiltEvents {
    let env = EventEnv::new(call, shape, f);
    let mut b = EventBuilder::new(&env, file_sizes);
    b.emit_startup();
    b.emit_background();
    b.emit_rejected_tools();
    b.emit_notifications();
    b.emit_new_prompt();
    b.emit_tool_steps();
    b.emit_pre_request();
    b.emit_request();
    b.emit_first_token();
    b.emit_success();
    b.emit_failure();
    b.emit_failed_turn_end();
    b.emit_suggestion();
    b.emit_side_question_or_compact();
    b.emit_away_summary();
    b.emit_agent_summary();
    b.emit_agent_steps();
    b.emit_agent_own_betas();
    b.emit_model_validation();
    b.finish()
}

/// [`Telemetry::build_events`] 的产出。
pub(super) struct BuiltEvents {
    pub(super) events: Vec<(DateTime<Utc>, Value)>,
    pub(super) dd: Vec<Value>,
    /// `--continue` 接上旧会话时，挂在启动探测那个临时会话上的那一串。
    pub(super) probe_side: Option<(Identity, TplOutput)>,
}

/// [`Telemetry::build_events`] 各段共用的只读输入：这条调用、前半段推出的 [`TurnFacts`]，以及
/// 由它们再算出来的几项。
pub(super) struct EventEnv<'a> {
    pub(super) call: &'a ApiCall,
    pub(super) shape: &'a RequestShape,
    pub(super) f: &'a TurnFacts,
    pub(super) t0: DateTime<Utc>,
    pub(super) ttft: i64,
    pub(super) total: i64,
    pub(super) t_first: DateTime<Utc>,
    pub(super) t_end: DateTime<Utc>,
    pub(super) build_age_mins: i64,
    pub(super) query_source: String,
    pub(super) builtin_agent: Option<&'a str>,
    pub(super) cache_ttl: &'static str,
    pub(super) effort: Option<String>,
    pub(super) effort_value: String,
    /// 2.1.260 起 `tengu_api_success` 多了 `systemPromptSource`。
    pub(super) modern: bool,
    pub(super) setting: String,
    /// 新会话：进程启动那串（120 多条），锚在首条 api_query 前 1.3s 起。
    pub(super) tpl: &'static Template,
    pub(super) pre_count: usize,
    pub(super) api_system: usize,
    pub(super) model_held: bool,
    pub(super) classifier_held: bool,
    pub(super) req_hash: u32,
    pub(super) retry_pad: u32,
    pub(super) body_chars: usize,
    pub(super) gzip_skip: &'static str,
    /// 失败那条的 `errorType`，`turn_end{api_error}` 的 `error_kind` 也用它；成功为空。
    pub(super) error_kind_name: String,
    pub(super) fork_messages: u32,
}

impl<'a> EventEnv<'a> {
    pub(super) fn new(call: &'a ApiCall, shape: &'a RequestShape, f: &'a TurnFacts) -> Self {
        let TurnFacts {
            kind,
            is_main,
            query_depth,
            prev_main_depth,
            prompt_seq,
            v285,
            ref agent,
            ref identity,
            ref display_model,
            ref version,
            ..
        } = *f;
        let t0: DateTime<Utc> = call.started_at.into();
        let ttft = call.ttft_ms.unwrap_or(call.total_ms.min(1_500)) as i64;
        let total = call.total_ms as i64;
        let build_age_mins = {
            let bt = DateTime::parse_from_rfc3339(identity.build_time())
                .map(|d| d.with_timezone(&Utc))
                .unwrap_or(t0);
            (t0 - bt).num_minutes().max(0)
        };
        let query_source = match kind {
            Kind::Subagent => agent_query_source(call.agent.agent_type.as_deref()),
            // `-p` 打印模式的主线程（billing header `cc_entrypoint=sdk-cli`）报 `sdk`
            // （`cap/auto-2.1.285-20260930/00441` 等十条）。
            Kind::Main if shape.sdk => "sdk".to_string(),
            _ => kind.query_source().to_string(),
        };
        // 客户端内部的消息条数比 API 那份多（harness 注入的 system-reminder 等）：
        // `cap/2.1.260-2`：8→2、12→5、16→8、18→10，即 post + 5 + 第几次输入 + 本轮已有的
        // 续轮次数；`apiSystemMessageCount` = 第几次输入 + 本轮已有的续轮次数（1、2、3、3）。
        // 「本轮已有的续轮次数」主线程就是这条自己的 depth，猜下一句则是主线程最后一条的
        // depth（它自己的 depth 是 +2 过的，不能拿来算）。侧查询没有这些，pre == post、0。
        let turn_extra = if is_main { query_depth } else { prev_main_depth } as usize;
        let (pre_count, api_system) = if kind.is_agent() {
            // 子代理：`apiSystemMessageCount` 就是体里 `role: system` 的条数，规范化前比规范化后
            // 多出 这些 + 8（首条 + 6、摘要请求 + 10）——`cap/2.1.280` Explore 七条里六条逐条相等。
            let extra = match (kind, agent.as_ref().map_or(0, |a| a.steps)) {
                (Kind::AgentSummary, _) => 10,
                (_, 0) => 6,
                _ => 8,
            };
            (shape.messages_len + shape.api_system_messages + extra, shape.api_system_messages)
        } else if kind.has_boundary() {
            (
                shape.messages_len + 5 + prompt_seq as usize + turn_extra,
                prompt_seq as usize + turn_extra,
            )
        } else {
            (shape.messages_len, 0)
        };
        // 三条 tether 事件共用的「无状态」判定：模型被固定成无状态发送，或 auto 模式的分类器
        // 在跑（`cap/2.1.280`：auto 的三条 true，切回 default 的四条 false）。两者任一为真，
        // 这条实际就不走线程（`sentThreadType: "none"`）。
        let model_held = model_held_stateless(display_model);
        // 2.1.285 起 auto 模式不再钉成无状态：前三轮 auto 的 `classifierHeldStateless` 全是
        // false，请求照带 `message-threads` 并建线程（`cap/2.1.285/00030`、`00040` 批次）。
        let classifier_held = !v285 && shape.permission_mode == "auto";
        // 两条收尾事件都要的量。
        //
        // `durationMsIncludingRetries` 比 `durationMs` 多出的那 1–5ms 是客户端重试包装层的
        // 开销，官方五条是 +3/+5/+1/+1/+4；按 request-id 取个稳定的零头，别恒等。
        let req_hash = call
            .request_id
            .as_deref()
            .map(|r| r.bytes().fold(0u32, |a, b| a.wrapping_mul(31).wrapping_add(u32::from(b))))
            .unwrap_or(0);
        let body_chars = std::str::from_utf8(&call.body).map(js_len).unwrap_or(call.body.len());
        // 请求体不到 4 KiB 不压缩，报 `below_min_size`；够大的才报「走代理不压缩」的 `proxy`
        // （`cap/auto-2.1.285-20260930` 128 条：22 条 573–3821 字是前者，106 条 4107 起是后者）。
        let gzip_skip = if body_chars < 4096 { "below_min_size" } else { "proxy" };
        // fork 统计里的 `messageCount` 是分叉查询回来的内容块数：思考块 + 正文，只有正文就是 1
        // （`cap/2.1.285`：haiku 摘要带思考两条都是 2，离开回顾一条纯文本 1、一条带思考 2；
        // `cap/2.1.280` 的猜下一句同样 1 与 2 都有）。
        let fork_messages = 1 + u32::from(call.saw_thinking);
        EventEnv {
            call,
            shape,
            f,
            t0,
            ttft,
            total,
            t_first: ms(t0, ttft),
            t_end: ms(t0, total),
            build_age_mins,
            query_source,
            builtin_agent: call.agent.agent_type.as_deref().filter(|t| *t != "custom"),
            cache_ttl: if shape.cache_ttl_1h { "1h" } else { "5m" },
            effort: shape.effort.clone(),
            effort_value: shape.effort.clone().unwrap_or_else(|| "high".to_string()),
            modern: version_at_least(version, "2.1.260"),
            setting: model_setting(display_model),
            tpl: template_for(version, shape.sdk),
            pre_count,
            api_system,
            model_held,
            classifier_held,
            req_hash,
            retry_pad: req_hash % 5,
            body_chars,
            gzip_skip,
            error_kind_name: call.failure.as_ref().map(error_kind).unwrap_or_default(),
            fork_messages,
        }
    }

    pub(super) fn uptime(&self, dt: DateTime<Utc>) -> f64 {
        let start: DateTime<Utc> = self.f.started_wall.into();
        ((dt - start).num_milliseconds().max(0) as f64) / 1000.0
    }

    pub(super) fn ctx(&self, dt: DateTime<Utc>) -> EventCtx<'a> {
        EventCtx {
            model: &self.f.ctx_model,
            betas: &self.f.betas_session,
            prompt_id: &self.f.prompt_id,
            uptime_secs: self.uptime(dt),
        }
    }

    /// 一轮结束（`end_turn`）才有 stop hook 与 turn_end；`tool_use` 是同一轮的中间步。
    ///
    /// `error_kind` 只有 `terminal_reason: "api_error"` 那种才带（官方是
    /// `error_kind: Te(reason==="api_error" ? errorKind ?? "unknown" : undefined)`，
    /// 其余情形整个键不出现）。
    pub(super) fn turn_end(
        &self,
        terminal: &str,
        duration: i64,
        error_kind: Option<&str>,
    ) -> Value {
        let kind = self.f.kind;
        let mut o = Map::new();
        o.insert("terminal_reason".into(), json!(terminal));
        if let Some(k) = error_kind {
            o.insert("error_kind".into(), json!(k));
        }
        o.insert("is_error".into(), json!(error_kind.is_some()));
        o.insert("is_subagent".into(), json!(kind.is_agent()));
        o.insert("goal_active".into(), json!(false));
        o.insert("duration_ms".into(), json!(duration));
        o.insert("query_source".into(), json!(self.query_source));
        o.insert("query_source_category".into(), json!(kind.category()));
        Value::Object(o)
    }

    /// 链路字段的写法：有链的带 chain/depth，没链的一律不带。
    pub(super) fn chain_fields(&self, obj: &mut Map<String, Value>) {
        if self.f.kind.has_chain() {
            obj.insert("queryChainId".into(), json!(&self.f.chain_id));
            obj.insert("queryDepth".into(), json!(self.f.query_depth));
        }
    }

    /// tether 收尾那条的写法，成功与被取消的两条路共用（取消报 `finalOutcome: aborted`，
    /// `cap/auto-2.1.285-20260930` 08:01:42.632）。
    pub(super) fn live_outcome(&self, final_outcome: &str) -> Option<Value> {
        let EventEnv { call, shape, model_held, classifier_held, t0, .. } = *self;
        let TurnFacts {
            kind,
            v285,
            v291,
            anchor_tool_call,
            anchor_end,
            ref tether,
            ref betas_full,
            ..
        } = *self.f;
        let t = tether.as_ref()?;
        let sent = match shape.thread_type.as_deref() {
            Some("create") => "create",
            Some("continue") => "continue",
            _ => "none",
        };
        let stateless = sent == "none";
        let omitted = sent == "continue";
        let live = json!({
            "requestId": call.request_id.as_deref().unwrap_or(""),
            "sentThreadType": sent,
            "sourceCategory": kind.category(),
            "planReason": if stateless { "none" } else { t.reason },
            "engineDecision": t.decision,
            "engineReason": t.reason,
            "firstThreadError": "none",
            "finalOutcome": final_outcome,
            "replayed": false,
            "droppedFrom": "none",
            "threadUnsupported": false,
            "modelHeldStateless": model_held,
            "relayHeldStateless": false,
            "classifierHeldStateless": classifier_held,
            "serverToolHistory": false,
            // 就是「这条请求的 beta 里有 `mid-conversation-tool-changes`」：`cap/2.1.280`、
            // `cap/2.1.285` 逐条相等（opus-5 / fable-5 / opus-4-8 这几个
            // `modelHeldStateless` 为 false 的也是 true）。此前按 `modelHeldStateless`
            // 报，2.1.277 那条 opus-5 的「例外」正是这个缘故。
            "toolAdditionHistory": betas_full.contains("mid-conversation-tool-changes-"),
            "toolRemovalHistory": false,
            "toolChangeHistory": false,
            "requestScopedStateless": false,
            "keptReminderClearAt": false,
            "keptReminderScope": "all",
            "deltaMessageCount": shape.after_last_assistant,
            "messageCount": shape.messages_len,
            "turnsInThread": t.turns,
            "omittedSystem": omitted,
            "omittedTools": omitted,
            "omittedBytes": if omitted { shape.omitted_bytes } else { 0 },
            "inheritBreakerTripped": false,
            "dropArmed": false,
            "dropHeldStateless": false,
            "continuesSinceDropCreate": -1,
            "dropReportUnmatched": false
        });
        let mut live = live;
        if v285 {
            insert_after(
                &mut live,
                "classifierHeldStateless",
                vec![
                    ("creditRetryStateless", json!(false)),
                    ("toolResultClearingHeldStateless", json!(false)),
                ],
            );
        }
        // 2.1.291：队尾多四项线程计时（`cap/auto-2.1.291-20261006-full` 每条都有）。时间锚点是同一条线
        // 上一条请求的收尾（[`TurnFacts::anchor_end`]：主线程含被 Esc 取消的那条，子代理看它自己那条
        // 支线）：`threadIdleMs` 是它收尾到这条发出隔了多久，与这条续用还是另起线程无关——
        // `create/config_changed` 那条也报 44369；没有锚点（会话第一条、子代理首条）报 -1。
        // `anchorHasToolCall` 是上一条**有效回复**里有没有工具调用（被用户回答的 AskUserQuestion 之后
        // 的新输入也是 true），不是「这条是工具续轮」。`threadLifetimeMs` 抓包里恒 -1。
        if v291 {
            let idle = anchor_end.map_or(-1, |end| {
                let end: DateTime<Utc> = end.into();
                (t0 - end).num_milliseconds().max(0)
            });
            if let Some(o) = live.as_object_mut() {
                o.insert("threadIdleMs".into(), json!(idle));
                o.insert("threadIdleMonotonicMs".into(), json!(idle));
                o.insert("threadLifetimeMs".into(), json!(-1));
                o.insert(
                    "anchorHasToolCall".into(),
                    json!(anchor_end.is_some() && anchor_tool_call),
                );
            }
        }
        Some(live)
    }

    pub(super) fn subst(&self) -> Subst<'_> {
        let f = self.f;
        Subst {
            version: &f.version,
            model: &f.display_model,
            model_setting: &self.setting,
            permission_mode: self.shape.permission_mode,
            resumed: f.resumed,
            prompt_index: f.prompt_seq,
            deferred: self.shape.deferred_tools > 0,
            tool_search: tool_search_decision(self.shape, &f.display_model, f.kind.is_agent()),
            sdk: self.shape.sdk,
            haiku: self.shape.model.contains("haiku"),
            tools_off: self.shape.tools_off,
        }
    }
}

/// 一条请求带回的那批工具结果共用的几项：工具是上一条回复产生的，事件里的 requestId /
/// messageID / 深度都指上一条。
pub(super) struct ToolStep {
    pub(super) is_sub: bool,
    pub(super) prev_end_dt: DateTime<Utc>,
    pub(super) prev_req: String,
    pub(super) prev_msg: String,
    pub(super) prev_depth: u32,
    /// 这条请求带回的工具结果数。
    pub(super) n: i64,
}

/// 攒这条调用的几路事件。各段 `emit_*` 照官方的先后依次往里写，[`Self::finish`] 再按原来的
/// 顺序并起来。
pub(super) struct EventBuilder<'a> {
    pub(super) env: &'a EventEnv<'a>,
    pub(super) subst: Subst<'a>,
    pub(super) file_sizes: &'a mut HashMap<String, usize>,
    pub(super) events: Vec<(DateTime<Utc>, Value)>,
    pub(super) dd: Vec<Value>,
    /// 这条支线请求里挂在**主线程**身份上的事件（子代理首步前的 `agent_tool_selected`），
    /// 最后与模板事件一起并进 `events`。
    pub(super) main_side: Vec<(DateTime<Utc>, Value)>,
    /// 静态模板那几串攒在这里，最后再并进 `events`/`dd`。
    pub(super) tpl_events: Vec<(DateTime<Utc>, Value)>,
    pub(super) tpl_dd: Vec<Value>,
    pub(super) probe_side: Option<(Identity, TplOutput)>,
}

impl<'a> EventBuilder<'a> {
    pub(super) fn new(env: &'a EventEnv<'a>, file_sizes: &'a mut HashMap<String, usize>) -> Self {
        EventBuilder {
            env,
            subst: env.subst(),
            file_sizes,
            events: Vec::with_capacity(16),
            dd: Vec::with_capacity(6),
            main_side: Vec::new(),
            tpl_events: Vec::new(),
            tpl_dd: Vec::new(),
            probe_side: None,
        }
    }

    /// 发一条事件。服务端下发的抽样配置在这里统一过一遍（[`sample_event`]），抽掉的不发。
    pub(super) fn push(&mut self, dt: DateTime<Utc>, name: &str, mut extra: Value) {
        let env = self.env;
        if !sample_event(name, &env.f.version, &mut extra) {
            return;
        }
        self.events.push((dt, env.f.identity.event(name, dt, &env.ctx(dt), extra)));
    }

    /// 同一条事件同时进事件链与 Datadog，两边是同一份 meta。
    pub(super) fn push_dd(&mut self, dt: DateTime<Utc>, name: &str, extra: Value) {
        let env = self.env;
        self.push(dt, name, extra.clone());
        self.dd.push(env.f.identity.dd_entry(name, &env.ctx(dt), &env.f.dd_model, extra));
    }

    /// 同上，Datadog 那份是 [`snake_flat`] 扁平化过的字段。
    pub(super) fn push_dd_snake(&mut self, dt: DateTime<Utc>, name: &str, extra: Value) {
        let env = self.env;
        let flat = snake_flat(&extra);
        self.push(dt, name, extra);
        self.dd.push(env.f.identity.dd_entry(name, &env.ctx(dt), &env.f.dd_model, flat));
    }

    pub(super) fn take_tpl(&mut self, tpl: &[TplEvent], anchor: DateTime<Utc>) {
        let env = self.env;
        let (ev, d) = emit_template(
            tpl,
            anchor,
            &env.f.base_identity,
            |dt| env.ctx(dt),
            &env.f.dd_model,
            &self.subst,
        );
        self.tpl_events.extend(ev);
        self.tpl_dd.extend(d);
    }

    pub(super) fn finish(mut self) -> BuiltEvents {
        self.events.extend(self.tpl_events);
        self.events.extend(self.main_side);
        self.dd.extend(self.tpl_dd);
        BuiltEvents { events: self.events, dd: self.dd, probe_side: self.probe_side }
    }
}

/// `dt` 往后挪 `d` 毫秒（负数往前）。
pub(super) fn ms(dt: DateTime<Utc>, d: i64) -> DateTime<Utc> {
    dt + chrono::Duration::milliseconds(d)
}

pub(super) fn feature(name: &str) -> Value {
    json!({ "feature_name": name })
}
