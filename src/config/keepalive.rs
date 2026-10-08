//! 会话保活与启动握手：周期、端点、Axios 请求形态。

// ---------- session keepalive ----------

/// 基础保活间隔（秒）。`event_logging` 每 tick 都发；
/// `policy_limits` + `settings` 每 `KEEPALIVE_HOURLY_TICKS` 个 tick 发一次（≈1h）；
/// `metrics` 只在首 tick 发一次。
///
/// 抓包实测（`cap/2.1.145`，idle 段 09:05→09:35→10:05→10:35…）：安定后 event_logging
/// 每 ~30 min 一次，与版本检查和 Datadog 同节奏。此前设 5 min 是猜的，
/// 6 倍于真实频率反而是指纹。
pub const KEEPALIVE_INTERVAL_SECS: u64 = 30 * 60;

/// 多少个基础 tick 构成一个"小时级"周期（30min × 2 = 60min）。
pub const KEEPALIVE_HOURLY_TICKS: u64 = 2;

/// 保活请求的 User-Agent。抓包显示保活类端点都用 `claude-code/<版本>`，
/// 而非转发时的 `claude-cli/<版本>`。
pub const KEEPALIVE_USER_AGENT: &str = "claude-code/2.1.293";

/// 事件日志里的 `betas` 字段：会话级 beta 集合，不含每请求才带的模型级 beta
/// （`advanced-tool-use`/`effort`/`extended-cache-ttl` 等）。原取自 `cap/2.1.258/00032`
/// （opus-5 **1M** 会话那串，多一项 `context-1m`）；2.1.285 起模拟路径不再带 `context-1m`，
/// 保活默认的又是 sonnet-5，换成 `cap/2.1.285` 里非 1M 会话那串（opus-5-5 / sonnet-5 等
/// 七个 Claude 5 代模型的会话级 betas 都是它）。
pub const KEEPALIVE_EVENT_BETAS: &str = "claude-code-20250219,oauth-2025-04-20,\
    interleaved-thinking-2025-05-14,\
    redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
    context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
    mid-conversation-system-2026-04-07";

/// 保活端点路径。
pub const KEEPALIVE_EVENT_LOGGING: &str = "/api/event_logging/v2/batch";

pub const KEEPALIVE_METRICS: &str = "/api/claude_code/metrics";

pub const KEEPALIVE_POLICY_LIMITS: &str = "/api/claude_code/policy_limits";

pub const KEEPALIVE_SETTINGS: &str = "/api/claude_code/settings";

// ---------- startup bootstrap + 周期端点 ----------

/// 启动握手端点（全部发往 api.anthropic.com + OAuth token）。
pub const KEEPALIVE_BOOTSTRAP: &str = "/api/claude_cli/bootstrap";

pub const KEEPALIVE_PENGUIN_MODE: &str = "/api/claude_code_penguin_mode";

/// Statsig 特性标志评估端点：启动 + 每 6h（= 12 个 30-min tick）。
pub const KEEPALIVE_EVAL: &str = "/api/eval/sdk-zAZezfDKGoZuXXKe";

pub const KEEPALIVE_EVAL_TICKS: u64 = 12;

/// eval 端点的 User-Agent（真实客户端 Bun 运行时自报的 UA，与其他端点不同）。
/// `cap/2.1.280`、`cap/2.1.285` 的 eval 与早批 event_logging 都是 `Bun/1.4.3`。
pub const KEEPALIVE_UA_BUN: &str = "Bun/1.4.3";

/// 启动握手「领跑段」最多挡住首条 `/v1/messages` 多久
/// （见 [`crate::oauth::HandshakeRunner::lead`]）。
///
/// 抓包里这一段实测 1.65s（policy/settings 并发 → eval → 额度探测），主请求排在它后面。
/// luban 这边照着做，但**必须有上限**：那几个端点是 luban 替客户端补的，慢一点或挂了都
/// 不该让用户的第一条请求跟着卡住。超时就放行，剩下的在后台继续跑完。
///
/// 取 2.5s：够抓包那 1.65s 跑完，又不至于在端点无响应时把首条请求拖到用户能察觉。
pub const HANDSHAKE_LEAD_TIMEOUT_MS: u64 = 2_500;

/// `downloads.claude.ai/claude-code-releases/latest` 离会话起点多久
/// （见 [`crate::oauth::HandshakeRunner::downloads`]）。
///
/// `cap/2.1.260-2` 两个会话分别是 +9.6s（17:14:56.354 → 17:15:05.957）与 +9.8s
/// （17:43:01.139 → 17:43:10.900）。取 9.7s。
pub const DOWNLOAD_RELEASES_DELAY_MS: u64 = 9_700;

/// 插件市场那条离会话起点多久。
///
/// 同一份抓包里是 +2min5s（17:14:56 → 17:17:01.380），而且**不是每个会话都有**
/// （第三个会话整段窗口里没有）。取 125s——晚一点、少一点都比跟启动风暴挤在一起像。
pub const DOWNLOAD_PLUGINS_DELAY_MS: u64 = 125_000;

// ---------- Axios 形态的辅助端点 ----------

/// 辅助端点共用的 `Accept`（axios 的默认值）。
pub const AXIOS_ACCEPT: &str = "application/json, text/plain, */*";

/// 没显式传 `User-Agent` 的 axios 调用发出去的 UA（axios 的 http 适配器自己补的）。
///
/// 抓包里两处可见：`cap/2.1.260-2/00005`（penguin_mode）与 `00006`（mcp_servers）都是
/// `axios/1.15.2`。OAuth 的 token 与 profile 两条在源码里同样没传 UA
/// （`services/oauth/client.ts` / `getOauthProfile.ts`），故也是这一个。
pub const AXIOS_DEFAULT_USER_AGENT: &str = "axios/1.15.2";

/// 辅助端点的 `Accept-Encoding`：**与 Messages API 那份不是同一个串**。
///
/// axios 走 Node 的 http 客户端，默认发 `gzip, compress, deflate, br`（多一个 `compress`、
/// 没有 `zstd`）；Messages API 那条走的是 Bun 自己的客户端，发的是
/// [`CC_ACCEPT_ENCODING`](super::CC_ACCEPT_ENCODING)（`gzip, deflate, br, zstd`）。两处混用就是把两个运行时的形态
/// 拼在同一个进程上——真实客户端不会这样。
///
/// `compress`（LZW）实际不会被上游选中（Cloudflare 只回 gzip/br），声明它没有解码风险。
pub const AXIOS_ACCEPT_ENCODING: &str = "gzip, compress, deflate, br";

/// 辅助端点的 `Connection`：axios 那套**每条都显式 `close`**（`cap/2.1.260-2` 全部
/// 11 类辅助请求一致），而 Messages API 与 eval 走的是 `keep-alive`。
pub const AXIOS_CONNECTION: &str = "close";

/// 一个辅助端点的线上头序与拼写。
///
/// **每个端点一份，不能合并成一张总表**：axios 把「实例默认头」与「本次调用传的头」按
/// 各自的插入序拼起来，于是同一套值在不同调用点排列不同。举两例（`cap/2.1.260-2`）：
///
/// ```text
/// policy_limits: Accept, Authorization, anthropic-beta, User-Agent, …
/// bootstrap:     Accept, Content-Type, User-Agent, Authorization, anthropic-beta, …
/// ```
///
/// `Authorization` 与 `User-Agent` 的先后正好相反。任何单一总序都满足不了两者——同
/// [`cc_beta_order_is_not_a_table`](super::cc_beta_order_is_not_a_table) 记的是一类问题。
///
/// 表里列的是**全部**头（含 `Host`/`Content-Length` 这些由 HTTP 客户端自己追加的），
/// 交给 `wreq` 的 `OrigHeaderMap`：表里有、本次没带的不会凭空发出；表外的照发但排在队尾。
pub struct AxiosShape {
    /// 端点名，只用于日志与测试断言。
    pub name: &'static str,
    pub order: &'static [&'static str],
}

/// 尾部三件套：`Accept-Encoding` → `Host` → `Connection`，11 类辅助请求全部一致。
/// 带 body 的那几个在它之前还有 `Content-Length`。
const AXIOS_TAIL: &[&str] = &["Accept-Encoding", "Host", "Connection"];

/// 辅助端点的头序表，逐条取自 `cap/2.1.260-2`（括号里是抓包编号）。
///
/// 只列 axios 那套；eval 走 Bun 客户端，形态完全不同，见 [`AXIOS_SHAPE_EVAL`]。
pub const AXIOS_SHAPES: &[AxiosShape] = &[
    // 00001
    AxiosShape {
        name: "policy_limits",
        order: &[
            "Accept",
            "Authorization",
            "anthropic-beta",
            "User-Agent",
            "If-None-Match",
            "Accept-Encoding",
            "Host",
            "Connection",
        ],
    },
    // 00002：只有这一个带 `Cache-Control`/`Pragma`。
    AxiosShape {
        name: "settings",
        order: &[
            "Accept",
            "Authorization",
            "anthropic-beta",
            "User-Agent",
            "Cache-Control",
            "Pragma",
            "If-None-Match",
            "Accept-Encoding",
            "Host",
            "Connection",
        ],
    },
    // 00005
    AxiosShape {
        name: "penguin_mode",
        order: &[
            "Accept",
            "Authorization",
            "anthropic-beta",
            "User-Agent",
            "Accept-Encoding",
            "Host",
            "Connection",
        ],
    },
    // 00006
    AxiosShape {
        name: "mcp_servers",
        order: &[
            "Accept",
            "Content-Type",
            "Authorization",
            "anthropic-beta",
            "anthropic-version",
            "anthropic-mcp-client-capabilities",
            "MCP-Protocol-Version",
            "User-Agent",
            "Accept-Encoding",
            "Host",
            "Connection",
        ],
    },
    // 00007：无鉴权，UA 是 SDK 那份。
    AxiosShape {
        name: "mcp_registry",
        order: &["Accept", "User-Agent", "Accept-Encoding", "Host", "Connection"],
    },
    // 00008
    AxiosShape {
        name: "bootstrap",
        order: &[
            "Accept",
            "Content-Type",
            "User-Agent",
            "Authorization",
            "anthropic-beta",
            "Accept-Encoding",
            "Host",
            "Connection",
        ],
    },
    // 00009
    AxiosShape {
        name: "code_triggers",
        order: &[
            "Accept",
            "Content-Type",
            "User-Agent",
            "Authorization",
            "anthropic-version",
            "anthropic-client-platform",
            "x-organization-uuid",
            "anthropic-beta",
            "Accept-Encoding",
            "Host",
            "Connection",
        ],
    },
    // 00015
    AxiosShape {
        name: "metrics",
        order: &[
            "Accept",
            "Content-Type",
            "User-Agent",
            "Authorization",
            "anthropic-beta",
            "Content-Length",
            "Accept-Encoding",
            "Host",
            "Connection",
        ],
    },
    // 00016
    AxiosShape {
        name: "event_logging",
        order: &[
            "Accept",
            "Content-Type",
            "User-Agent",
            "x-service-name",
            "Authorization",
            "anthropic-beta",
            "Content-Length",
            "Accept-Encoding",
            "Host",
            "Connection",
        ],
    },
    // 00017：Datadog 那台主机，无 Authorization。
    AxiosShape {
        name: "datadog",
        order: &[
            "Accept",
            "Content-Type",
            "DD-API-KEY",
            "User-Agent",
            "Content-Length",
            "Accept-Encoding",
            "Host",
            "Connection",
        ],
    },
    // 00032 / 00037：downloads.claude.ai，无鉴权。
    AxiosShape {
        name: "download",
        order: &["Accept", "User-Agent", "Accept-Encoding", "Host", "Connection"],
    },
    // **无抓包，按 axios 规律推断**（`cap/` 里没有 token/profile 端点的样本）。规律取自
    // 00005/00006/00017 三条没显式 UA 的调用：`Accept` 打头，随后是调用点 `headers` 里
    // 的键按书写序，axios 自补的 `User-Agent` 排在它们之后，再接尾部。
    //
    // token 端点：源码只传了 `Content-Type`（`services/oauth/client.ts`）。
    AxiosShape {
        name: "oauth_token",
        order: &[
            "Accept",
            "Content-Type",
            "User-Agent",
            "Content-Length",
            "Accept-Encoding",
            "Host",
            "Connection",
        ],
    },
    // profile 端点：源码传的是 `Authorization` 再 `Content-Type`（`getOauthProfile.ts`），
    // GET 上带 `Content-Type` 是 axios 原样发出的（00006 那条 GET 就带着）。
    AxiosShape {
        name: "oauth_profile",
        order: &[
            "Accept",
            "Authorization",
            "Content-Type",
            "User-Agent",
            "Accept-Encoding",
            "Host",
            "Connection",
        ],
    },
];

/// eval（`/api/eval/sdk-…`）**不是 axios**：它走 Bun 自带的 fetch，UA 是
/// [`KEEPALIVE_UA_BUN`]、`Connection: keep-alive`、`Accept: */*`，`Accept-Encoding` 也是
/// Messages API 那份 [`CC_ACCEPT_ENCODING`](super::CC_ACCEPT_ENCODING)。整条与 axios 那套没有一处相同
/// （`cap/2.1.260-2/00003`），故单列一份。
pub const AXIOS_SHAPE_EVAL: AxiosShape = AxiosShape {
    name: "eval",
    order: &[
        "Authorization",
        "Content-Type",
        "anthropic-beta",
        "Connection",
        "User-Agent",
        "Accept",
        "Host",
        "Accept-Encoding",
        "Content-Length",
    ],
};

/// 按端点名取头序表。查不到即漏写了一行——退回只有尾部三件套的最小形态，比发一个
/// 随机顺序强。
pub fn axios_shape(name: &str) -> &'static [&'static str] {
    match AXIOS_SHAPES.iter().find(|s| s.name == name) {
        Some(s) => s.order,
        None => AXIOS_TAIL,
    }
}
