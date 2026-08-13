//! Claude Code OAuth 常量与配置。
//!
//! 这些是 Claude Code 官方客户端使用的公开 OAuth 参数，luban 复用它们
//! 以完成「用 Claude 订阅账号登录」的授权流程。

/// Claude Code 公开 OAuth Client ID。
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// 授权页地址（用户在浏览器打开、登录并同意授权）。
/// 官方客户端已从 `claude.ai/oauth/authorize` 迁到 claude.com 的新路径，跟着走。
pub const AUTHORIZE_URL: &str = "https://claude.com/cai/oauth/authorize";

/// Token 交换 / 刷新端点。
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

/// 账号 profile 端点（用 access_token 获取邮箱/姓名/订阅等级）。
pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";

/// 手动粘贴模式使用的 redirect_uri，token 端点会据此校验。
pub const REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";

/// 申请的 OAuth scope，与 Claude Code 保持一致（含顺序——scope 集合也是指纹的一部分）。
/// `org:create_api_key` luban 自身用不到，但官方客户端就带着它；`user:file_upload` 缺了
/// 会让经代理走 Files API 的上传被上游按 scope 拒掉。
pub const SCOPES: &str = "org:create_api_key user:profile user:inference \
                          user:sessions:claude_code user:mcp_servers user:file_upload";

/// 用 OAuth access token 调用 Anthropic API 时必须携带的 beta 头。
pub const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";

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
/// [`crate::proxy::merge_beta`]，那里是唯一的真源，别再另起一张表。
pub mod cc_beta_order_is_not_a_table {}

/// `claude-code-20250219`：[`OAUTH_BETA_HEADER`] 的落位参照物。
pub const CC_BETA_CLAUDE_CODE: &str = "claude-code-20250219";

/// `effort-2025-11-24`：[`CC_BETA_ADVANCED_TOOL_USE`] 的落位参照物（haiku 不发这一项）。
pub const CC_BETA_EFFORT: &str = "effort-2025-11-24";

/// `advanced-tool-use-2025-11-20`：对齐订阅端工具能力。
/// 官方排在 [`CC_BETA_EFFORT`] 之前；没有 effort 时排在客户端自有串之后。
pub const CC_BETA_ADVANCED_TOOL_USE: &str = "advanced-tool-use-2025-11-20";

/// `extended-cache-ttl-2025-04-11`：官方四份直连抓包里都是**最后一项**。
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

/// 官方客户端的 `User-Agent`。用于 luban 自身发起的账号级请求（token 刷新、profile），
/// 这些请求原先不带任何 UA——一个持有订阅 refresh_token 却没有 UA 的客户端非常显眼。
/// 转发 `/v1/*` 时以来访客户端自己的 UA 为准（转发头覆盖此默认值）。
///
/// 取最近一次抓到的官方版本（cap/raw 是 2.1.220）。落后不致命——真实用户升级也有先后——
/// 但落得太多就成了「一个几个月没升级过的客户端在不停刷 token」。
pub const CC_USER_AGENT: &str = "claude-cli/2.1.220 (external, cli)";

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
/// 该头也被钉进 [`crate::clients::upstream_client`] 的 `default_headers`，
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

/// 官方订阅客户端把系统提示词切成 4 块，第二刀落在**基座结束处**。本表是切点之后那一段的
/// 开头，用来在 API-key 模式的合并块里定位这一刀——**每个模型族的基座不同，各有各的锚点**。
///
/// 全部依据 `cap/raw` 里的原始字节（claude-cli/2.1.220，同机同版本、直连与经 luban 成对）：
///
/// | 模型 | 直连 / 经 luban | 官方基座 | 命中的锚点 | 在合并块里的偏移 |
/// |---|---|---|---|---|
/// | opus-5    | 00006 / 00002 | 1210B  | `Write code that…` | 1212  |
/// | sonnet-5  | 00009 / 00012 | 10676B | `# Text output…`   | 10678 |
/// | haiku-4.5 | 00031 / 00026 | 10676B | `# Text output…`   | 10678 |
/// | fable-5   | 00035 / 00037 | 1210B  | `# Communicating…` | 1212  |
///
/// 四例都满足：合并块 = `基座 ‖ "\n\n" ‖ 其余`，锚点前紧跟 `\n\n`，切开后前缀与官方基座
/// **逐字节相同**。基座本身按模型族复用：haiku 与 sonnet-5 同一份、fable-5 与 opus-5 同一份。
///
/// **别把「锚点互斥」当通例**：opus 与 sonnet 那两句确实互不出现在对方的 body 里，但那只是这
/// 两个模型族的实情，换个模型就未必。fable-5 就同时含两条——它自己的锚点在偏移 1212，opus 那句
/// 也在正文里（偏移 3284）。所以取的必须是**最早命中**的那个，绝不能按表序先到先得：那样
/// fable 会被切在 3282，基座凭空多出 2072 字节。新增模型族时按这个前提校验，别假设互斥。
///
/// **认锚点不认长度**：基座长度随模型变（1210B vs 10676B），写死长度必错。锚点本身也会随
/// CC 版本/模型族漂——一个都匹配不到就不拆（见 [`crate::proxy::align_system_shape`]），
/// 宁可退回三块原样转发，也不切在错误的位置上。要补新模型族，**只能拿原始字节抓包**，
/// 别拿 `cap/*.json` 顶（见 [`CC_HEADER_ORDER`] 的教训）。
pub const CC_SYSTEM_BASE_ANCHORS: &[&str] = &[
    // opus-4-8 / opus-5
    "Write code that reads like the surrounding code: match its comment density, naming, and idiom.",
    // sonnet-5 / haiku-4.5
    "# Text output (does not apply to tool calls)",
    // fable-5（与 opus-5 共用基座，但其余部分的开头不同）
    "# Communicating with the user",
];

/// 官方 `system[1]` 那句身份声明，四个模型族逐字节相同（57 字节）。
///
/// 它同时是两件事：**上游对 OAuth 凭证唯一强制的正文**（缺了它订阅额度不给用），以及
/// 「这是不是一条 Claude Code 请求」的判据——[`crate::proxy::is_cc_shaped`] 认的就是它。
pub const CC_SYSTEM_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// `system[0]` 那条 billing header 里的 `cc_version`，形如 `2.1.220.04c`。
///
/// 后缀（`.04c`/`.564`）随构建变，与鉴权模式无关（见 [`known_fingerprint_gaps`] 第 4 条），
/// 故取一个即可。主版本号要和 [`CC_USER_AGENT`] 对得上——同一个客户端不会一边自称 2.1.220
/// 一边报另一个 cc_version。
pub const CC_VERSION: &str = "2.1.220.04c";

/// 模拟模式注入的官方系统提示词**基座**（opus-5 / fable-5 那一族，1214 字节）。
///
/// 逐字节取自 `cap/raw/00006`（opus-5 直连）的 `system[2]`，与 `00035`（fable-5）
/// sha256 相同。开头那个 `\n` 是官方就有的，别 trim。
///
/// 这是**基座**，不含 `# Environment`、工具清单、技能列表那些本机内容——那些属于官方的
/// `system[3]`（「其余」段），逐客户端不同，模拟时那一格留给来访客户端自己的 system。
pub const CC_SYSTEM_BASE_OPUS: &str = include_str!("assets/cc_system_base_opus.txt");

/// 模拟模式注入的官方系统提示词基座（sonnet-5 / haiku-4.5 那一族，10682 字节）。
///
/// 逐字节取自 `cap/raw/00009`（sonnet-5 直连）的 `system[2]`，与 `00031`（haiku-4.5）
/// sha256 相同。比 opus 那份大一个数量级——这一族的基座本来就长，不是抄错了。
pub const CC_SYSTEM_BASE_SONNET: &str = include_str!("assets/cc_system_base_sonnet.txt");

/// 模拟模式下**代客户端发出**的 `anthropic-beta` 自有串（不含 luban 自己会补的那几项），
/// opus-5 / sonnet-5 / fable-5 及认不出的模型共用这份。haiku 另有一份，见
/// [`CC_BETA_SIMULATED_HAIKU`]——**这两份不能合并**，理由与
/// [`cc_beta_order_is_not_a_table`] 记的是同一件事。
///
/// 逐字取自 `cap/raw/00009`（sonnet-5 直连）那串，去掉 [`crate::proxy::merge_beta`] 负责
/// 插入的三项（`oauth`/`advanced-tool-use`/`extended-cache-ttl`）。于是交给 `merge_beta`
/// 之后能**逐字节还原**官方那串，回归测试见 `tests::simulated_beta_matches_official`。
///
/// **刻意不取 opus-5 那串**：它比这份多 `context-1m-2025-08-07` 与
/// `fallback-credit-2026-06-01`（fable-5 那串则多 `server-side-fallback-2026-06-01` 与
/// `fallback-credit`），这些都不是纯形态——`context-1m` 是 1M 上下文的准入（超过 200k
/// 输入按另一档计价），另两项关联额度回补与服务端换模型。替用户声明这类东西超出了「装成
/// 官方客户端」的范围，与 [`known_fingerprint_gaps`] 第 7 条不补 `fallbacks` 是同一条口径。
/// sonnet 那串是官方真实发过的完整串，本身就自洽，不存在「集合对了顺序错」的问题。
///
/// 来访客户端自己带的 beta 不会被这串顶掉，见 [`crate::proxy::simulated_beta`]。
pub const CC_BETA_SIMULATED: &str = "claude-code-20250219,interleaved-thinking-2025-05-14,\
    redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,\
    prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,effort-2025-11-24";

/// haiku 族的自有串，逐字取自 `cap/raw/00031`（haiku-4.5 直连），同样去掉 `merge_beta`
/// 负责插入的三项。
///
/// **和另外三族的差别不只是少两项**：haiku 不发 `mid-conversation-system` 与 `effort`，
/// 而且把 `claude-code-20250219` 排在**队尾**（另外三族在队首）。拿 sonnet 那份去发 haiku，
/// 得到的是一个真实客户端不产生的排列——正是 [`cc_beta_order_is_not_a_table`] 记的那件事，
/// 只不过这次落在模拟路径上。所以模型族与串是绑定的，别再想着合成一张总表。
///
/// 基座那边则相反：haiku 与 sonnet-5 的 `system[2]` sha256 相同，共用
/// [`CC_SYSTEM_BASE_SONNET`]。同一族在一处相同、在另一处不同，两边各自按证据来。
pub const CC_BETA_SIMULATED_HAIKU: &str = "interleaved-thinking-2025-05-14,\
    redact-thinking-2026-02-12,thinking-token-count-2026-05-13,context-management-2025-06-27,\
    prompt-caching-scope-2026-01-05,claude-code-20250219";

/// 模拟模式下整套重建的固定请求头，取值逐字节取自 `cap/raw/00006`（opus-5 直连）。
///
/// 表里**只有固定值**；随请求变的几个不在此列，由 [`crate::proxy::official_headers`] 另外
/// 塞：`Authorization`（凭证）、`X-Claude-Code-Session-Id`（每设备派生）、
/// `x-client-request-id`（每请求 uuid），以及 `anthropic-beta`（见 [`CC_BETA_SIMULATED`]）。
///
/// **头名全小写是有意的**：`HeaderName::from_static` 只收小写，大写会 panic；线上的拼写与
/// 顺序另由 [`CC_HEADER_ORDER`] 经 `OrigHeaderMap` 决定，跟这里写成什么样无关。
///
/// `X-Stainless-Arch`/`OS` 这类本机信息只能填一个定值（抓包那台是 arm64 mac）——模拟路径
/// 上来访客户端根本不提供这些，凭空造一个「每设备不同」的组合反而可能拼出 arm64+Windows
/// 这种真实客户端不产生的搭配。代价记在这儿：所有经模拟路径的请求平台头完全一致。
pub const CC_SIM_HEADERS: &[(&str, &str)] = &[
    ("accept", "application/json"),
    ("content-type", "application/json"),
    ("user-agent", CC_USER_AGENT),
    ("x-stainless-arch", "arm64"),
    ("x-stainless-lang", "js"),
    ("x-stainless-os", "MacOS"),
    ("x-stainless-package-version", "0.94.0"),
    ("x-stainless-retry-count", "0"),
    ("x-stainless-runtime", "node"),
    ("x-stainless-runtime-version", "v26.3.0"),
    ("x-stainless-timeout", "600"),
    ("anthropic-dangerous-direct-browser-access", "true"),
    ("anthropic-version", "2023-06-01"),
    ("x-app", "cli"),
    ("connection", "keep-alive"),
    ("accept-encoding", CC_ACCEPT_ENCODING),
];

/// 官方客户端请求头的**拼写与顺序**，逐字节取自 `cap/raw/00006`（claude-cli/2.1.220 直连
/// api.anthropic.com，CONNECT 隧道里的原始报文头）。
///
/// **别再拿 `cap/*.json` 当顺序基准**：那些文件的 `headers`/body 都被抓包工具按字母序重排过
/// （大写头一段、小写头一段，`text` 会排在 `type` 前）。本表最初就是照抄 `cap/040` 的
/// `headers` 字典，于是拼写抄对了、顺序抄的却是 JSON 的排序结果——`Accept-Encoding`/
/// `Connection`/`Host`/`Content-Length` 官方全在队尾，被字母序拎到了前段。顺序信息只有
/// `cap/raw/*.req.raw` 这种原始字节留得住。
///
/// 一张表兼两用，喂给 `wreq` 的 `OrigHeaderMap`：
/// - **拼写**：注意这不是「全部首字母大写」——`anthropic-*`/`x-app`/`x-client-request-id`
///   本来就是全小写（Stainless SDK 自己拼的），而 `X-Stainless-OS` 的 `OS` 是全大写，
///   机械 title-case 会写成 `X-Stainless-Os`。所以只能逐头列表，没有规则可套。
/// - **顺序**：`OrigHeaderMap` 同时决定线上头序，故 `Connection`/`Host`/`Accept-Encoding`/
///   `Content-Length`（由 HTTP 客户端自己追加）也列在此处的官方位置——恰好也是队尾四个。
///
/// 实测语义（预检验证，见 [`known_fingerprint_gaps`]）：表里有、本次请求没带的头**不会**
/// 凭空发出；反之表外的头照发，但一律小写并排在所有表内头之后。
pub const CC_HEADER_ORDER: &[&str] = &[
    "Accept",
    "Authorization",
    "Content-Type",
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
    // 以下四个由 HTTP 客户端自己追加，官方线序里它们在队尾，不是字母序里的位置。
    "Connection",
    "Host",
    "Accept-Encoding",
    "Content-Length",
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
/// ~~4. `cc_version` 的构建后缀~~ —— **已排除，不是判据**。原记录说它随鉴权模式变化（依据是
///    040=`2.1.218.2d7` / 041=`2.1.218.0b9`）。后续抓包否掉了这个相关性：cap/raw 的
///    00002（经 luban）与 00006（直连）同为 `2.1.220.04c`，003/004 那对也同为 `2.1.218.d82`。
///    后缀确实会变，但与鉴权模式无关，luban 原样转发即可。
///
/// 5. **`cch` 是恒定占位值**。官方每次请求都不同（`0848d`、`5cb85`…），luban 固定发
///    [`BILLING_CCH`]。详见该常量注释——上游一按此聚类就把所有账号串成一串。
///
/// ~~6. `system` 块的切分与缓存 TTL~~ —— **已对齐**，见 [`crate::proxy::align_system_shape`]
///    与 [`CC_SYSTEM_BASE_ANCHORS`]。四个模型族的 raw 抓包逐字节验过。剩余风险只有锚点会随
///    CC 版本/新模型漂，漂了就退回三块原样转发（不会切错）。
///
/// 7. **`fallbacks` 与 `server-side-fallback-2026-06-01`**。fable-5 那对抓包
///    （`cap/raw/00035` 直连 ↔ `00037` 经 luban）里，直连侧多一个顶层字段
///    `"fallbacks":[{"model":"claude-opus-5"}]`，`anthropic-beta` 里也多一项
///    `server-side-fallback-2026-06-01`（排在 `effort` 与 `fallback-credit` 之间）；
///    经 luban 那侧两者都没有。
///
///    **刻意不补**：这不是纯形态差异——`fallbacks` 声明的是「本模型不可用时改用哪个模型」，
///    补上等于替用户决定被限流时换模型跑，模型换了计价也跟着换。凭空塞一个 beta 却不带对应
///    字段则是另一种不自洽。只有一对抓包，还分不清它是订阅模式独有还是该会话自己开的，
///    在拿到「同一客户端两种模式下都发/都不发」的证据之前不动它。
///
/// 比对基准只能用**原始字节**——`cap/raw/*.raw` 那种（HTTPS 隧道内的报文，头名大小写、头序、
/// body 的 key 顺序都留得住）。`cap/*.json` 是抓包工具重新序列化过的：headers 与 body 的 key
/// 全被按字母序重排，只有数组元素的顺序还作数。[`CC_HEADER_ORDER`] 曾照着它抄，抄出一份
/// 官方客户端不会产生的头序。
pub mod known_fingerprint_gaps {}

/// 官方上游 API base（代理转发目标）。
pub const UPSTREAM_BASE_URL: &str = "https://api.anthropic.com";

/// 距离过期不足该秒数时视为需要刷新。
pub const REFRESH_LEEWAY_SECS: u64 = 300;
