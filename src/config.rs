//! Claude Code OAuth 常量与配置。
//!
//! 这些是 Claude Code 官方客户端使用的公开 OAuth 参数，luban 复用它们
//! 以完成「用 Claude 订阅账号登录」的授权流程。

/// Claude Code 公开 OAuth Client ID。
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// 授权页地址（用户在浏览器打开、登录并同意授权）。
pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";

/// Token 交换 / 刷新端点。
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

/// 账号 profile 端点（用 access_token 获取邮箱/姓名/订阅等级）。
pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";

/// 手动粘贴模式使用的 redirect_uri，token 端点会据此校验。
pub const REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";

/// 申请的 OAuth scope，与 Claude Code 保持一致。
pub const SCOPES: &str = "user:profile user:inference user:sessions:claude_code user:mcp_servers";

/// 用 OAuth access token 调用 Anthropic API 时必须携带的 beta 头。
pub const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";

/// 转发时确保携带的 beta 组：对齐官方订阅客户端的 `anthropic-beta`。
/// API 模式的 Claude Code 不会自带这些，缺失会导致缓存 TTL 退化与部分工具能力关闭。
/// - `oauth-2025-04-20`：OAuth 鉴权必需。
/// - `extended-cache-ttl-2025-04-11`：官方订阅客户端固定携带。我们**不**改写缓存 TTL
///   （客户端声明 5m 就按 5m 发），保留它只为让 `anthropic-beta` 与官方客户端逐字节一致。
/// - `advanced-tool-use-2025-11-20`：对齐订阅端工具能力。
/// - `prompt-caching-scope-2026-01-05`：允许 `cache_control.scope: "global"`（body 改写依赖）。
pub const INJECT_BETAS: &[&str] = &[
    OAUTH_BETA_HEADER,
    "extended-cache-ttl-2025-04-11",
    "advanced-tool-use-2025-11-20",
    "prompt-caching-scope-2026-01-05",
];

/// 官方订阅客户端 `anthropic-beta` 的**排列顺序**（抓包 claude-cli/2.1.218 直连 API 得到，
/// 两次抓包顺序一致，仅 `context-1m` 视会话有无）。
///
/// 只补齐 [`INJECT_BETAS`] 会把缺失项追加到末尾，得到 `…,effort,oauth,extended-cache-ttl,
/// advanced-tool-use` 这种官方客户端不会产生的排列——集合对了顺序错，一次精确匹配即可判定
/// 中间有代理。故转发前按本表重排；表外的未知 beta 保持相对顺序放在末尾。
pub const CC_BETA_ORDER: &[&str] = &[
    "claude-code-20250219",
    OAUTH_BETA_HEADER,
    "context-1m-2025-08-07",
    "interleaved-thinking-2025-05-14",
    "redact-thinking-2026-02-12",
    "thinking-token-count-2026-05-13",
    "context-management-2025-06-27",
    "prompt-caching-scope-2026-01-05",
    "mid-conversation-system-2026-04-07",
    "advanced-tool-use-2025-11-20",
    "effort-2025-11-24",
    "extended-cache-ttl-2025-04-11",
];

/// 官方客户端的 `User-Agent`。用于 luban 自身发起的账号级请求（token 刷新、profile），
/// 这些请求原先不带任何 UA——一个持有订阅 refresh_token 却没有 UA 的客户端非常显眼。
/// 转发 `/v1/*` 时以来访客户端自己的 UA 为准（转发头覆盖此默认值）。
pub const CC_USER_AGENT: &str = "claude-cli/2.1.218 (external, cli)";

/// `Accept-Encoding`：与官方客户端逐字节一致。
///
/// 原先该头被剥离且 reqwest 未开压缩 feature，上游收到的是「自称 claude-cli 却完全不声明
/// 压缩支持」的请求。上游对 `text/event-stream` 实测不压缩，故声明后流式响应仍是 identity，
/// 用量嗅探不受影响；非流式响应可能带 `content-encoding`，原样透传给客户端解码。
pub const CC_ACCEPT_ENCODING: &str = "gzip, deflate, br, zstd";

/// 注入到 `x-anthropic-billing-header` 的 `cch` 值。
///
/// 官方客户端仅在**订阅(OAuth)模式**下发送 `cch=<5 位小写 hex>`；API-key 模式（即接入
/// luban 的形态）不发。于是「OAuth token + 无 cch」成为一个确定性判据。真实算法无法从抓包
/// 反推（同账号内逐请求变化，18 组候选输入 × 6 种摘要均未命中），故只能填一个占位值。
///
/// 注意这是个**跨账号恒定**的值：所有经由 luban 的请求都带同一个真实客户端从不产生的
/// `cch`，一旦上游按此聚类，等于把所有账号串成一串。要改成每账号不同又不打爆 prompt cache，
/// 把 [`crate::proxy::cch_value`] 换成从「已在缓存前缀内的内容」派生即可（见该函数注释）。
pub const BILLING_CCH: &str = "00000";

/// **已知无法对齐的形态差异**（记录在案，别再重复排查）。
///
/// 1. **header 名大小写**。hyper 把 `HeaderName` 一律存成并发出小写；官方客户端（node/undici）
///    发的是分裂形态——undici 托管的标准头首字母大写、SDK 自定义头全小写：
///    ```text
///    Accept / Accept-Encoding / Authorization / Connection / Content-Type / Host /
///    User-Agent / X-Claude-Code-Session-Id / X-Stainless-*        ← 首字母大写
///    anthropic-beta / anthropic-version / x-app / x-client-request-id /
///    anthropic-dangerous-direct-browser-access                    ← 全小写
///    ```
///    reqwest 有 `http1_title_case_headers()`，但那是**全部**首字母大写，会把 `anthropic-beta`
///    写成 `Anthropic-Beta`，同样对不上，只是换了个错法。逐头指定大小写 hyper 没有 API，
///    要修得换掉整个 HTTP 栈。
///
/// 2. **`user-agent` / `host` / `content-length` 的位置**。这三个由 hyper 自己追加在头列表
///    末尾，无法插到来访客户端原本的位置。其余转发头的顺序是保住的，见
///    [`crate::proxy::build_forward_headers`]。
///
/// 3. **TLS ClientHello 指纹**。rustls 的扩展顺序/密码套件与 node 的 BoringSSL 不同。
///
/// 4. **`cc_version` 的构建后缀**。抓包显示订阅模式是 `2.1.218.2d7`、API-key 模式是
///    `2.1.218.0b9`（同机同版本同时段）。这个后缀随鉴权模式变化，luban 原样转发，
///    等于补了 [`BILLING_CCH`] 却留着另一个更直接的判据。成因未知，待查。
///
/// 比对基准只能用**HTTPS CONNECT 隧道**里抓到的 flow（保留原始字节）；明文 HTTP 到 luban
/// 那几个 flow 的头名会被 mitmproxy 机械 title-case（21 个头无一例外），大小写与顺序都不可信。
pub mod known_fingerprint_gaps {}

/// 官方上游 API base（代理转发目标）。
pub const UPSTREAM_BASE_URL: &str = "https://api.anthropic.com";

/// 距离过期不足该秒数时视为需要刷新。
pub const REFRESH_LEEWAY_SECS: u64 = 300;
