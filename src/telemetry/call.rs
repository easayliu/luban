//! 逐请求：转发路径交过来的一条 API 调用

use super::*;

/// 出站请求上 2.1.277 起的三个 `x-claude-code-*` 头（`cap/2.1.280/00165`）：子代理带
/// `agent-id`（支线号，17 位 hex）与 `agent-type`（内置的写类型名如 `Explore`，自定义的写
/// `custom`），每条都带 `request-class`（`main` / `subagent` / `auxiliary`）。
///
/// 遥测靠它们认出子代理与它的摘要请求（`request-class: auxiliary` 且带 `agent-id`），
/// **只给这两类**的事件顶层写 `agent_id` / `agent_type`——主线程与其余侧查询的事件没有
/// 这两项。模拟路径只写 `request-class`，另两个只有真 CC 来访才有。
#[derive(Debug, Clone, Default)]
pub struct AgentHeaders {
    pub agent_id: Option<String>,
    pub agent_type: Option<String>,
    pub request_class: Option<String>,
}

/// 转发路径在响应流结束时交过来的一条已完成的 `/v1/messages`。
///
/// 请求侧的量都从 `body`（**实际发往上游的那份**）里解析，响应侧的量由
/// [`crate::proxy`] 的用量嗅探给出。
pub struct ApiCall {
    pub cred_id: i64,
    pub account_uuid: Option<String>,
    pub org_type: Option<String>,
    /// 实际发往上游的请求体。
    pub body: Bytes,
    /// 实际发出的 `anthropic-beta`。
    pub betas: Option<String>,
    /// 实际发出的 `X-Claude-Code-Session-Id`（body 里没有时的兜底）。
    pub session_header: Option<String>,
    /// 实际发出的 UA，取版本号用。
    pub ua_out: String,
    /// 上游响应头 `anthropic-organization-id`。
    pub organization_id: Option<String>,
    /// 请求发出的时刻。
    pub started_at: SystemTime,
    pub ttft_ms: Option<u64>,
    pub total_ms: u64,
    /// 上游响应头 `request-id`。
    pub request_id: Option<String>,
    /// 出站的 `x-client-request-id`（官方客户端每请求一个 uuid v4）。失败事件要报它。
    pub client_request_id: Option<String>,
    /// 出站的 `x-claude-code-*` 头，见 [`AgentHeaders`]。
    pub agent: AgentHeaders,
    /// 响应里的 `message.id`（`msg_…`）。
    pub message_id: Option<String>,
    pub stop_reason: Option<String>,
    /// 响应回报的模型名（规范名）。
    pub resp_model: Option<String>,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_tokens: i64,
    pub cache_creation_tokens: i64,
    /// 缓存写入按 ttl 拆开的两份（usage 里的 `cache_creation.ephemeral_5m_input_tokens` /
    /// `ephemeral_1h_input_tokens`）。上游没给拆分时是 `None`，见 2.1.291 的 `tengu_api_success`。
    pub cache_creation_5m_tokens: Option<i64>,
    pub cache_creation_1h_tokens: Option<i64>,
    /// `usage.iterations` 的轮数（服务端压缩这类一次请求跑了多轮）；没有这个数组是 0。
    pub iterations: usize,
    /// 响应正文里 text / thinking 的字符数。
    pub text_chars: usize,
    /// 这条回复按 `inputTextCharLength` 口径的字数，见 [`ThreadBase::reply_chars`]。
    pub reply_input_chars: usize,
    pub thinking_chars: usize,
    /// 响应里出现过思考块（`redacted_thinking` 与空思考块都算），决定要不要报
    /// `thinkingContentLength`。
    pub saw_thinking: bool,
    /// 流式回复里除 `ping` 外的 SSE 事件数（2.1.305 起 `tengu_api_success` 的 `streamEventCount`：客户端
    /// 逐事件累加、跳过 ping，`cap/auto-2.1.305-20261021-full` 165 条都是事件总数减一个 ping）；
    /// 非流式为 `None`，官方回退到非流式时也不报。
    pub stream_events: Option<u32>,
    /// 响应里每个工具的入参 JSON 字符数（工具名 → 之和，按首次出现排序），
    /// 即 `toolUseContentLengths`。
    pub tool_use_lens: Vec<(String, usize)>,
    /// 回复里每个要客户端执行的工具调用（id、名字、入参、auto 模式的服务端判决），见
    /// [`ToolCall`]。
    pub tool_calls: Vec<ToolCall>,
    pub cost_usd: Option<f64>,
    pub speed: Option<String>,
    /// 这条请求**失败**了：报 `tengu_api_error` 而不是 `tengu_api_success`。
    pub failure: Option<CallFailure>,
    /// 流式回复还没收尾，客户端就把连接掐了（按 Esc 打断、猜下一句被新输入顶掉）：官方那头
    /// 既不报 success 也不报 error，报的是取消那一串（`aborted_streaming`）。
    pub aborted: bool,
    /// 这条主线程回复接替了一条续命成功过的缓存保活记录：收尾时补
    /// `tengu_feature_ok{api_cache_keepalive}`（见 [`crate::proxy`] 的 `cache_keepalive`）。
    pub cache_keepalive_ok: bool,
    /// 出站的工具声明是来访客户端自带的（真实 CC 经 `ANTHROPIC_BASE_URL` 透传，网关 / API-key 那套形态），
    /// 不是模拟路径注入的固定资产：`tengu_context_size` 的工具计数按网关基线报，见
    /// [`super::template`] 的 `gateway_tool_tokens`。
    pub client_tools: bool,
    /// 实际出站的 `anthropic-dispatch-id`（真实客户端透传它自己的，模拟路径恒为 `v2d`，额度探测不带）。
    pub dispatch: Option<String>,
    /// 同一请求在这一条之前失败过的那几次（客户端自己重发的），按先后。转发路径交过来时总是空的，
    /// 遥测把扣住的失败接到最终那一次上时才填（见 `Telemetry::process` 的重试链）。
    pub prior_attempts: Vec<PriorAttempt>,
}

/// 同一请求先前失败的一次尝试：重发事件（`tengu_dispatch_header_fallback` / `tengu_api_retry`）与最终那条的
/// `attempt`、含重试的总时长都从它们算。
#[derive(Debug, Clone)]
pub struct PriorAttempt {
    pub started_at: SystemTime,
    /// 那一次出站的 `anthropic-dispatch-id`，见 [`ApiCall::dispatch`]。
    pub dispatch: Option<String>,
    pub total_ms: u64,
    pub status: Option<u16>,
    pub error_type: Option<String>,
    pub message: String,
    pub in_band: bool,
    pub request_id: Option<String>,
}

impl ApiCall {
    /// 这个逻辑请求最早那次尝试发出的时刻：客户端自己重发过的（[`Self::prior_attempts`]）取头一次，否则就是这条。
    /// 提交、`api_query` 这些请求前缀的事件挂在它上面，首字与收尾仍按最终这次算。
    pub fn logical_start(&self) -> SystemTime {
        self.prior_attempts.first().map_or(self.started_at, |a| a.started_at)
    }
}

impl PriorAttempt {
    /// 从一次失败的尝试取下要用的几样（没有失败信息的当连接层失败）。
    pub(crate) fn of(c: &ApiCall) -> Self {
        let f = c.failure.as_ref();
        PriorAttempt {
            started_at: c.started_at,
            dispatch: c.dispatch.clone(),
            total_ms: c.total_ms,
            status: f.and_then(|f| f.status),
            error_type: f.and_then(|f| f.error_type.clone()),
            message: f.map(|f| f.message.clone()).unwrap_or_default(),
            in_band: f.is_some_and(|f| f.in_band),
            request_id: c.request_id.clone(),
        }
    }
}

/// 回复里的一个工具调用。下一条请求把它的结果带回来时，那串工具事件（入参字节数、Bash 的
/// 命令分类、auto 模式的判决）照它报。
#[derive(Debug, Clone, Default)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
    /// 响应 `safeguard_results` 里给它的判决：`not_flagged` / `flagged` / `skipped`；没有判决
    /// （default 模式、只读工具）为 `None`。
    pub verdict: Option<String>,
}

/// 一条失败请求客户端那头看到的东西。
///
/// 官方客户端对失败请求发的是 `tengu_api_error`（外加一条
/// `tengu_feature_bad{api_request}` 与一条 `terminal_reason: "api_error"` 的
/// `tengu_turn_end`），字段与 `tengu_api_success` 大半重合但不含任何用量。
pub struct CallFailure {
    /// HTTP 状态码；中途 `event: error` 那种客户端拿到的是 200 + 错误负载，SDK 那边
    /// 状态码为空，故这里也留空（见 `in_band`）。
    pub status: Option<u16>,
    /// 上游 `error.type`（`rate_limit_error`、`overloaded_error`…）。
    pub error_type: Option<String>,
    /// 上游 `error.message`，或本地对断流的描述。
    pub message: String,
    /// 错误是裹在 200 里的流内事件（`event: error`）而不是 HTTP 状态码。官方对这类的
    /// `errorType` 报 `in_band_<上游 type>`。
    pub in_band: bool,
}

/// 转发路径在建 `ReqLog` 时先攒好的那部分（响应侧的量在流结束时才有）。
pub struct Capture {
    pub sink: Telemetry,
    pub account_uuid: Option<String>,
    pub org_type: Option<String>,
    pub body: Bytes,
    pub betas: Option<String>,
    pub session_header: Option<String>,
    /// 实际发出的 `x-client-request-id`。
    pub client_request_id: Option<String>,
    /// 实际发出的 `anthropic-dispatch-id`。
    pub dispatch: Option<String>,
    pub agent: AgentHeaders,
    pub organization_id: Option<String>,
    pub started_at: SystemTime,
    /// 这条主线程回复接替了一条续命成功过的缓存保活记录，见 [`ApiCall::cache_keepalive_ok`]。
    pub cache_keepalive_ok: bool,
    /// 见 [`ApiCall::client_tools`]。
    pub client_tools: bool,
}

impl Capture {
    /// 一条**没走到正常收尾**的请求：连接层就失败了（`ReqLog` 压根没建起来），或者在
    /// `ReqLog` 建起来之前就早退了（401 换号那条路）。响应侧的量一概没有，收尾走
    /// [`Telemetry::process`] 的失败分支，与正常路径上那条 `tengu_api_error` 同一套。
    ///
    /// 放在这里而不是在 [`crate::proxy`] 里现拼一个 [`ApiCall`]：那结构二十多个字段，
    /// 在调用点各写一份迟早会漂开——「哪些字段在失败时该留空」是这一侧的知识。
    pub fn record_failure(
        self,
        cred_id: i64,
        ua_out: String,
        total_ms: u64,
        request_id: Option<String>,
        failure: CallFailure,
    ) {
        let sink = self.sink.clone();
        sink.record(ApiCall {
            cred_id,
            account_uuid: self.account_uuid,
            org_type: self.org_type,
            body: self.body,
            betas: self.betas,
            session_header: self.session_header,
            ua_out,
            organization_id: self.organization_id,
            started_at: self.started_at,
            // 首字节从未到达（连接层失败），或响应体没读成（401 那条）。
            ttft_ms: None,
            total_ms,
            request_id,
            client_request_id: self.client_request_id,
            agent: self.agent,
            message_id: None,
            stop_reason: None,
            resp_model: None,
            input_tokens: 0,
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_creation_tokens: 0,
            cache_creation_5m_tokens: None,
            cache_creation_1h_tokens: None,
            iterations: 0,
            text_chars: 0,
            reply_input_chars: 0,
            thinking_chars: 0,
            saw_thinking: false,
            stream_events: None,
            tool_use_lens: Vec::new(),
            tool_calls: Vec::new(),
            cost_usd: None,
            speed: None,
            failure: Some(failure),
            aborted: false,
            cache_keepalive_ok: false,
            client_tools: self.client_tools,
            dispatch: self.dispatch,
            prior_attempts: Vec::new(),
        });
    }
}
