//! 遥测上报：事件 / 指标 / Datadog 的节奏、上限与端点。

use super::*;

/// 遥测事件顶层 `betas` 是**会话级** beta 集合（不含逐请求才带的模型级 beta）。从出站
/// `anthropic-beta` 里按这些前缀筛出来，顺序照出站头。取自 `cap/2.1.258/00020` 的
/// event_logging 批次：opus 会话多一项 `context-1m`，fable 会话少一项 `redact-thinking`，
/// 也就是说它就是出站头的一个子集，而不是一份固定串。
pub const TELEMETRY_SESSION_BETA_PREFIXES: &[&str] = &[
    "claude-code-",
    "oauth-",
    "context-1m-",
    "interleaved-thinking-",
    "redact-thinking-",
    "thinking-token-count-",
    "context-management-",
    "prompt-caching-scope-",
    "mid-conversation-system-",
];

/// event_logging 批次的攒批时长：真实客户端每 ~30s 把攒下的事件一次发出
/// （`cap/2.1.258`：09:24:59 起，批次落在 09:25:29 / 09:26:39 / 09:30:11）。
pub const TELEMETRY_EVENT_FLUSH_SECS: u64 = 30;

/// Datadog 日志的攒批时长：**首条待发日志入队后 15s** 发出。两份抓包八个批次全部落在
/// 15.0–15.8s（`cap/2.1.258`：uptime 0→09:25:14、69→09:26:23、282→09:29:56、303→09:30:17；
/// `cap/2.1.260-1`：0→17:15:11、63→17:16:15、126→17:17:17、301→17:20:13）。event_logging
/// 那路同样量法是 30s（29.8–31.6s），两路各自计时、互不同步。
pub const TELEMETRY_DATADOG_FLUSH_SECS: u64 = 15;

/// OTel 指标（`/api/claude_code/metrics`）的导出间隔：进程启动 5 分钟后第一发
/// （`cap/2.1.260-1`：17:14:56 起、17:19:57 发；`cap/2.1.258`：09:24:59 起、09:30:00 发），
/// 之后每 5 分钟；客户端退出时若有未导出的也立刻发。每次导出还伴随一条
/// `tengu_feature_ok{internal_metrics_export}`（event_logging 与 Datadog 各一条），没有导出
/// 时（退出前刚导过、没有新用量）就没有这条——`cap/2.1.260-1` 第二个会话的退出批次里正是缺它。
pub const TELEMETRY_METRICS_FLUSH_SECS: u64 = 300;

/// 单个批次最多装多少条事件。官方只按时间攒批、不按条数（`cap/2.1.260-2` 一批 271 条 /
/// 552KB），这里只是防失控的兜底，正常永远碰不到。
pub const TELEMETRY_BATCH_MAX: usize = 1000;

/// 一个遥测会话多久没有请求就按「客户端退出」收尾（补退出事件、立刻导出指标）并忘掉它。
///
/// luban 看不见客户端退出，只能拿闲置时长推：太长，短会话的退出批次和指标会拖很久才发
/// （官方是退出当下就发）；太短，用户看会儿文档再回来就被当成退出 + resume。取 30 分钟：
/// 与官方空闲版本检查的周期同长，保活在这个窗口内还能把空闲事件挂到真实会话上。
pub const TELEMETRY_SESSION_IDLE_SECS: u64 = 30 * 60;

/// 侧查询（会话标题生成等）最多扣多久等同会话的下一条主线程请求：真实客户端给它打的是
/// **新一轮**的 prompt id，而那个 id 只在主线程请求的 billing header 里（`cap/2.1.260-2`：标题
/// 请求比主线程那条早 4ms 发出，cc_prompt_id 却是新一轮的）。等不到就按会话现有的 id 发。
pub const TELEMETRY_SIDE_QUERY_HOLD_SECS: u64 = 10;

/// 「已按退出收尾」的会话 id 记多久：期间同一个 id 再来按 `--resume` 处理（指标
/// `start_type: resume`），过了就当全新会话。真实用户 resume 几天前的对话很常见，取 7 天。
pub const TELEMETRY_ENDED_SESSION_MEMORY_SECS: u64 = 7 * 24 * 60 * 60;

/// 逐请求遥测「待处理」队列的上限（条）。见 `telemetry::IngestQueue`。
///
/// 处理一条是几毫秒的 JSON 解析加事件构造，单个消费者每秒能吃掉几百条，正常流量下这个
/// 队列恒为空。设上限只为堵住病态情形：每条排队的调用都拎着一份出站体（100KB+ 是常态），
/// 1024 条约合 100MB 量级，再多就该丢而不是撑爆内存。
pub const TELEMETRY_INGEST_QUEUE_MAX: usize = 1024;

// ---------- Datadog 遥测 ----------

/// Datadog 日志摄入 URL（与 api.anthropic.com 是不同的主机）。
pub const DATADOG_INTAKE_URL: &str = "https://http-intake.logs.us5.datadoghq.com/api/v2/logs";

/// Datadog 公钥（公开的 client token，非 secret）。
pub const DATADOG_API_KEY: &str = "pubea5604404508cdd34afb69e6f42a05bc";

/// Datadog 请求的 User-Agent（真实客户端通过 axios 发送，没传 UA，即 axios 缺省值）。
pub const DATADOG_USER_AGENT: &str = AXIOS_DEFAULT_USER_AGENT;
