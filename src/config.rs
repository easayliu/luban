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
/// 原先该头被剥离，上游收到的是「自称 claude-cli 却完全不声明压缩支持」的请求。
///
/// **声明了就一定会被压。** 上游（Cloudflare）连 140 字节的 401 错误体都压，`text/event-stream`
/// 也不例外——v0.2.12 只恢复了这个头却没开 reqwest 的解压 feature，导致所有响应体都是我们
/// 读不懂的字节，用量统计、计价、账号级错误判定整片失效。教训：**这个常量和 reqwest 的
/// gzip/brotli/zstd/deflate feature 是一套的，动其一必须动其二。**
///
/// 当时误判的根源是拿抓包当证据——那份抓包的 SSE 响应体是空的（导出没存流式 body），
/// 只凭「没看到 `content-encoding`」就断定上游不压 SSE，属于把证据缺失当证据。
///
/// 该头也被钉进 [`crate::web::upstream_client`] 的 `default_headers`，
/// 免得 luban 自身的刷新/profile 请求被解压中间件补上一个非官方取值。
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

/// 官方客户端请求头的**拼写与顺序**，逐字节取自抓包 040（HTTPS 隧道内的原始字节，是唯一
/// 可信的基准；明文到 luban 的那几个 flow 会被 mitmproxy 机械 title-case）。
///
/// 一张表兼两用，喂给 `wreq` 的 `OrigHeaderMap`：
/// - **拼写**：注意这不是「全部首字母大写」——`anthropic-*`/`x-app`/`x-client-request-id`
///   本来就是全小写（Stainless SDK 自己拼的），而 `X-Stainless-OS` 的 `OS` 是全大写，
///   机械 title-case 会写成 `X-Stainless-Os`。所以只能逐头列表，没有规则可套。
/// - **顺序**：`OrigHeaderMap` 同时决定线上头序，故 `Content-Length`/`Host`/`User-Agent`
///   （由 HTTP 客户端自己追加、原先只能待在队尾）也列在此处的官方位置上。
///
/// 实测语义（预检验证，见 [`known_fingerprint_gaps`]）：表里有、本次请求没带的头**不会**
/// 凭空发出；反之表外的头照发，但一律小写并排在所有表内头之后。
pub const CC_HEADER_ORDER: &[&str] = &[
    "Accept",
    "Accept-Encoding",
    "Authorization",
    "Connection",
    "Content-Length",
    "Content-Type",
    "Host",
    "User-Agent",
    "X-Claude-Code-Session-Id",
    "X-Stainless-Arch",
    "X-Stainless-Lang",
    "X-Stainless-OS",
    "X-Stainless-Package-Version",
    "X-Stainless-Retry-Count",
    "X-Stainless-Runtime",
    "X-Stainless-Runtime-Version",
    "X-Stainless-Timeout",
    "anthropic-beta",
    "anthropic-dangerous-direct-browser-access",
    "anthropic-version",
    "x-app",
    "x-client-request-id",
];

/// **已知无法对齐的形态差异**（记录在案，别再重复排查）。
///
/// **官方客户端的运行时是 Bun，不是 node。** 2.1.218 与 2.1.220 的可执行文件都是 Bun v1.4.0
/// 打出的单文件（255 MB Mach-O，`strings` 里有 `Bun v1.4.0`/`BoringSSL`/`versions.bun`，
/// 且**没有任何 `OpenSSL x.y.z` 版本串**）。抓包里的 `X-Stainless-Runtime: node` /
/// `X-Stainless-Runtime-Version: v26.3.0` 是误报——Bun 的 node 兼容层设了
/// `process.versions.node`，Stainless SDK 照着认。据此：头的形态出自 Bun 自己的 HTTP
/// 客户端（不是 undici），TLS 出自 BoringSSL（不是 OpenSSL）。
///
/// ~~1. header 名大小写~~ / ~~2. `user-agent`/`host`/`content-length` 的位置~~ —— **已解决**，
///    换到 `wreq` 的 `OrigHeaderMap`（见 [`CC_HEADER_ORDER`] 与
///    [`crate::proxy::orig_header_case`]）。留在这里是为了记住此路不通的那些尝试：
///    `HeaderName` 构造即归一化成小写，来访侧的原始拼写在进到
///    [`crate::proxy::build_forward_headers`] 之前就没了（也不需要——要装的是官方客户端，
///    照固定表在出站侧重建即可）；reqwest 的 `http1_title_case_headers()` 是**全部**首字母
///    大写，会把 `anthropic-beta` 写成 `Anthropic-Beta`、`X-Stainless-OS` 写成
///    `X-Stainless-Os`，22 个头里错 6 个，只是换了个错法；hyper 1.x 的 `ext::HeaderCaseMap`
///    与 reqwest 的 `Request::extensions_mut` 都是 `pub(crate)`，两半都够不着。
///
/// 3. **TLS ClientHello 指纹**。换到 wreq 后 TLS 从 rustls(aws-lc-rs) 变成 BoringSSL，与
///    Bun 的 BoringSSL **同族**（rustls 才是那个异类），且 wreq 把 cipher/curves/sigalgs/
///    扩展顺序/GREASE 都做成了公开旋钮（`wreq::tls::TlsOptions`）——但**同族不等于同指纹**，
///    Bun 的 BoringSSL 版本、编译选项与它那个 Zig HTTP 客户端设的参数都得对上。
///    **在有基准之前不要调**：cap/ 里只有 HTTP 层，没有 ClientHello 字节，得先抓一次真客户端
///    的 JA3/JA4，否则就是又一次拿证据缺失当证据（见 [`CC_ACCEPT_ENCODING`]）。
///    注意 `native-tls` 不是解法：它按平台分裂（macOS 走 Security.framework、Windows 走
///    SChannel、Linux 才是 OpenSSL），而官方客户端三个平台统一是 BoringSSL。
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
