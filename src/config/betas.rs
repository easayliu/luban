//! 官方客户端发的 `anthropic-beta` 各项。

/// **没有一张全局 beta 顺序表**——这一点是 haiku 那对抓包（`cap/raw/00026` 经 luban ↔
/// `00031` 直连）证伪出来的，记在这里免得再走一遍回头路。
///
/// 三个模型族的客户端自有串（去掉注入项后）：
///
/// | 模型 | 客户端自己发的顺序 |
/// |---|---|
/// | opus-5   | `claude-code, context-1m, interleaved, redact, ttc, cm, pcs, mid-conv, effort, fallback-credit` |
/// | sonnet-5 | `claude-code, interleaved, redact, ttc, cm, pcs, mid-conv, effort` |
/// | haiku-4.5 | `interleaved, redact, ttc, cm, pcs, claude-code` ← `claude-code` 跑到了队尾 |
///
/// opus/sonnet 里 `claude-code` 在最前、`oauth` 紧随其后；haiku 里 `claude-code` 在第 6 位、
/// `oauth` 反而在最前。**任何单一总序都无法同时满足这两条**（前者要求 claude-code < oauth，
/// 后者要求 oauth < claude-code），所以原来那张 `CC_BETA_ORDER` 只能碰巧对上 opus/sonnet。
///
/// 真正的不变量是：**客户端自有串的相对顺序，在订阅模式里逐字不变**（四对抓包全部满足）。
/// 故正确做法是不排序、只按经验规则把缺的插进去——注入哪几项、各自落在哪，见
/// [`crate::proxy::merge_beta_for`]，那里是唯一的真源，别再另起一张表。
pub mod cc_beta_order_is_not_a_table {}

/// `claude-code-20250219`：[`OAUTH_BETA_HEADER`](super::OAUTH_BETA_HEADER) 的落位参照物。
pub const CC_BETA_CLAUDE_CODE: &str = "claude-code-20250219";

/// `context-1m-2025-08-07`：1M 上下文会话才带。2.1.285 的 opus 默认 200K、profile 串里没有它；
/// 来访自己带了时落在官方位置——紧跟开头的 `claude-code` / `oauth` 之后、`interleaved-thinking`
/// 之前（`cap/auto-2.1.285-20260930/00235`、`00238`），见 [`crate::proxy::simulated_beta`]。
pub const CC_BETA_CONTEXT_1M: &str = "context-1m-2025-08-07";

/// `effort-2025-11-24`：[`CC_BETA_ADVANCED_TOOL_USE`] 的落位参照物（haiku 不发这一项）。
pub const CC_BETA_EFFORT: &str = "effort-2025-11-24";

/// `advisor-tool-2026-03-01`：haiku 没有 `effort` 时 [`CC_BETA_ADVANCED_TOOL_USE`] 的落位参照物
/// （2.1.251 起四族都带，haiku 官方串里 `advanced-tool-use` 紧跟其后，`cap/2.1.258/00031`）。
pub const CC_BETA_ADVISOR_TOOL: &str = "advisor-tool-2026-03-01";

/// `advanced-tool-use-2025-11-20`：对齐订阅端工具能力。
/// 官方排在 [`CC_BETA_EFFORT`] 之前；没有 effort 时排在 [`CC_BETA_ADVISOR_TOOL`] 之后；
/// 两个都没有才排在客户端自有串之后（2.1.220 的 haiku）。
pub const CC_BETA_ADVANCED_TOOL_USE: &str = "advanced-tool-use-2025-11-20";

/// `cache-diagnosis-2026-04-07`：2.1.251 起官方串的**最后一项**，
/// [`CC_BETA_EXTENDED_CACHE_TTL`] 的落位参照物（排在它前面）。API-key 客户端不发，
/// [`crate::proxy::merge_beta_for`] 补。
pub const CC_BETA_CACHE_DIAGNOSIS: &str = "cache-diagnosis-2026-04-07";

/// `server-side-fallback-2026-07-01`：2.1.258 订阅端四族都发，API-key 端都不发
/// （`cap/2.1.258-api` 原始请求头）。官方位置：`effort` 之后；haiku 没有 `effort`，在
/// `advanced-tool-use` 之后。
///
/// 2.1.260 起日期回到 [`CC_BETA_SERVER_SIDE_FALLBACK_JUN`]，且 opus 族整项不发了。
/// [`crate::proxy::merge_beta_for`] 对这项按**前缀**判在不在，免得给一个已经带 06-01 的
/// 2.1.260 来访再插一条 07-01，拼出「两条 server-side-fallback」这种官方不产生的形态。
pub const CC_BETA_SERVER_SIDE_FALLBACK: &str = "server-side-fallback-2026-07-01";

/// 主线程 opus-5 的 refusal fallback 链：cyber 类拒答按官方推荐先落 4.8，4.8 也拒再落 4.6。
/// 官方 2.1.260 的 opus 客户端**不发** `fallbacks`，这条是 luban 自定的，只在
/// `opus_refusal_fallback` 实验开关（**默认关**）开着时补——补了就是官方从不产生的请求形态，
/// 见 [`crate::store::OPUS_REFUSAL_FALLBACK`]。上游按模型公布允许的 fallback 目标
/// （`/v1/models` 的 `allowed_fallback_models`），不在名单里的会 400，那条 400 由
/// `crate::proxy::remember_fallback_rejection` 学下来、剥掉重发。
pub const OPUS_REFUSAL_FALLBACKS: &str =
    r#"[{"model":"claude-opus-4-8"},{"model":"claude-opus-4-6"}]"#;

/// `server-side-fallback-2026-06-01`：2.1.260 的取值（`cap/2.1.260/00018` fable 主线程、
/// `00024` haiku 无工具 helper）。2.1.258 那份是 `2026-07-01`——同一项换了日期，不是新增项。
pub const CC_BETA_SERVER_SIDE_FALLBACK_JUN: &str = "server-side-fallback-2026-06-01";

/// `per-turn-control-2026-07-01`：2.1.260 的 fable 族新增（`cap/2.1.260/00018`），占的正是
/// 2.1.258 里 [`CC_BETA_ADVISOR_TOOL`] 那个位置（`mid-conversation-system` 之后、
/// `advanced-tool-use` 之前）；同版本的 opus 族仍发 `advisor-tool`，两项没有同时出现过。
pub const CC_BETA_PER_TURN_CONTROL: &str = "per-turn-control-2026-07-01";

/// `structured-outputs-2025-12-15`：会话标题生成那条请求才发（`cap/2.1.260-2/00058`），
/// 配的是 body 里的 `output_config.format.type = "json_schema"`。
pub const CC_BETA_STRUCTURED_OUTPUTS: &str = "structured-outputs-2025-12-15";

/// `auto-mode-classifier-2026-07-16`：安全分类那条辅助请求才发
/// （`cap/2.1.260/00019`、`00030`）。
pub const CC_BETA_AUTO_MODE_CLASSIFIER: &str = "auto-mode-classifier-2026-07-16";

/// `fallback-credit-2026-06-01`：订阅端与 API-key 端四族都发（`cap/2.1.258-api` 原始请求头；
/// telemetry 事件里的 `betas` 字段漏记了它，别拿那个字段当头）。官方位置：紧跟
/// [`CC_BETA_SERVER_SIDE_FALLBACK`]，缺时补在此处。
pub const CC_BETA_FALLBACK_CREDIT: &str = "fallback-credit-2026-06-01";

/// `thinking-display-updates-2026-08-18`：订阅端**只有 fable 族**发（`cap/2.1.258/00013`），
/// 配 body 里的 `thinking.display:"updates"`；API-key 端的 fable 不发。官方位置：
/// [`CC_BETA_FALLBACK_CREDIT`] 之后。
pub const CC_BETA_THINKING_DISPLAY_UPDATES: &str = "thinking-display-updates-2026-08-18";

/// `redact-thinking-2026-02-12`：订阅端 fable 族**不发**（opus / sonnet / haiku 发），而
/// API-key 端的 fable 发。故 [`crate::proxy::merge_beta_for`] 对 fable 族把它剥掉——fable 上原始
/// 思维链本来就不返回，这项对它没有语义。
pub const CC_BETA_REDACT_THINKING: &str = "redact-thinking-2026-02-12";

/// `extended-cache-ttl-2025-04-11`：2.1.220 的四份直连抓包里是**最后一项**；2.1.251 起排在
/// [`CC_BETA_CACHE_DIAGNOSIS`] 之前（`cap/2.1.258` 四族一致）。
///
/// 它同时是 `cache_control.ttl` 的准入条件：断点上那个 `ttl:"1h"`（默认写，由
/// [`crate::store::ForwardFlags::cache_ttl_1h`] 拨）没有这个 beta 就是无源之水，
/// 故那一项还连着 `merge_beta` 一起开着（耦合点在 [`crate::proxy::rewrite_body`]）。
/// 拆块本身不依赖它——裸的 `{"type":"ephemeral"}` 是 GA 能力。
pub const CC_BETA_EXTENDED_CACHE_TTL: &str = "extended-cache-ttl-2025-04-11";

/// `prompt-caching-scope-2026-01-05`：`cache_control.scope: "global"` 的准入条件，
/// [`crate::proxy::align_system_shape`] 给基座标 global 时依赖它。
/// 四份 raw 抓包里客户端自己都带，实际很少真的需要补。
pub const CC_BETA_PROMPT_CACHING_SCOPE: &str = "prompt-caching-scope-2026-01-05";

/// `message-threads-2026-08-12`：2.1.270 新增（`cap/2.1.270/00017`、`00025` sonnet 主线程，
/// `00024` 标题生成），配 body 顶层的 `thread`（首轮 `{"type":"create"}`，续轮
/// `{"type":"continue","previous_message_id":…}`）。官方位置：**队尾**，在
/// [`CC_BETA_CACHE_DIAGNOSIS`] 之后——后者从此不再是最后一项，[`crate::proxy::merge_beta_for`]
/// 补 `cache-diagnosis` 时有它就插它前面。
///
/// **beta 与 `thread` 字段不成对**：2.1.280 非 auto 模式的 opus / fable 主线程（`cap/2.1.280/00065`、
/// `00068`）带这项 beta，体里却没有 `thread`；只有 sonnet / haiku 的首轮写 `create`（`00073`、
/// `00038`）。模拟路径照发 beta、不写 `thread`，与 opus / fable 的官方形态相同。
///
/// `merge_beta` **不补**这一项：没有 API-key 端的抓包，不知道那一侧发不发。
pub const CC_BETA_MESSAGE_THREADS: &str = "message-threads-2026-08-12";

/// 会话中途工具集变了（ToolSearch 载入延迟工具、MCP 工具上线）时，message-threads 续轮照带
/// 完整 `tools`（`cap/auto-2.1.285-20260930/00054`、`00795`），见
/// [`crate::proxy::is_official_thread_continuation`]。
pub const CC_BETA_MID_CONVERSATION_TOOL_CHANGES: &str = "mid-conversation-tool-changes-2026-07-01";

/// `inline-tools-2026-09-15`：2.1.293 新增，opus / sonnet / fable / haiku-5-5 主线程、子代理、标题生成与
/// helper 都发（`cap/auto-2.1.293-20261008-full/00419` 等），紧跟
/// [`CC_BETA_MID_CONVERSATION_TOOL_CHANGES`]。配套的变化是 MCP 工具不再进 `tools`，改写成首轮
/// 那条 `role: system` 消息里的 `tool_addition` 块（`00419` 的 `messages[1]`）。haiku-4.5 主线程与
/// 额度探测不发。
pub const CC_BETA_INLINE_TOOLS: &str = "inline-tools-2026-09-15";

/// `messages` 中途允许 `role: system` 消息。2.1.285 的 opus / sonnet / fable 主线程带它，把
/// `<total_tokens>` 提醒这类附件写成独立的 system 消息（`cap/auto-2.1.285-20260930/00036`）；
/// haiku 不带，同样的附件落成 user 消息里的 `<system-reminder>`（`00411`、`00412`）。
pub const CC_BETA_MID_CONVERSATION_SYSTEM: &str = "mid-conversation-system-2026-04-07";

/// `count_tokens` 的 beta（`cap/auto-2.1.285-20260930/00104` 起 35 条）：官方 ToolSearch 给延迟
/// 工具与 MCP 说明数 token 时发，体只有 `model` / `messages` / `tools`。
pub const CC_BETA_TOKEN_COUNTING: &str = "token-counting-2024-11-01";

/// `dangerous-tool-use-2026-09-03`：2.1.280 新增，四族主线程都发（`cap/2.1.280/00021` opus、
/// `00029` fable、`00033` sonnet、`00038` haiku），占的正是 2.1.277 里 [`CC_BETA_FALLBACK_CREDIT`]
/// 那一格（`effort` 之后、`thinking-binding-controls` 之前；haiku 没有 `effort`，在
/// `advanced-tool-use` 之后）。配 body 顶层的 `safeguards`（`[{type: dangerous_tool_use,
/// classifier_context: {…}}]`，auto 权限模式下服务端的危险工具分类器要的本机上下文：cwd、
/// home、规则根目录、git 状态、auto 模式的环境描述……）。**beta 与字段不成对**：haiku 主线程
/// 带这项 beta 却不发 `safeguards`；非 auto 模式的四族主线程（`00065`、`00068`、`00073`）同样
/// 带 beta、不发 `safeguards` 与 `afk-mode`。故模拟路径（一个非 auto 会话）照发 beta、不造
/// `safeguards`。透传路径不动来访那份。
///
/// 只记在案、不设常量：它由 profile 的 beta 串整串带出，[`crate::proxy::merge_beta_for`] 不补它
/// （没有 API-key 端样本，同 [`CC_BETA_MESSAGE_THREADS`] 的理由）。
pub mod cc_beta_dangerous_tool_use {}
