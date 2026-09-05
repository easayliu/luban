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

/// 登录时申请的 OAuth scope 的默认值，与 Claude Code 保持一致（含顺序——scope 集合也是
/// 指纹的一部分）。`org:create_api_key` luban 自身用不到，但官方客户端就带着它；
/// `user:file_upload` 缺了会让经代理走 Files API 的上传被上游按 scope 拒掉。
///
/// 这只是**默认值**：实际申请哪几项由 settings 里的 [`crate::store::OAUTH_SCOPES`] 决定，
/// 没配就用这一串。要改的理由见 [`SCOPES_MINIMAL`]。
///
/// **`scope` 是必填的**：授权 URL 整个不带这个参数（想着由上游给默认范围）会被直接回
/// `Missing scope parameter`——2026-08-24 实测，所以没有「不传」这一档。少要权限只能是
/// **少几项**（[`SCOPES_MINIMAL`] 是现成的一档），要试别的组合就往设置里那个输入框填——
/// 那个框不做校验，写什么发什么，认不认由上游的同意页说，见 [`normalize_scopes`]。
pub const SCOPES: &str = "org:create_api_key user:profile user:inference \
                          user:sessions:claude_code user:mcp_servers user:file_upload";

/// 精简 scope：一次真实授权里观察到的最小集，只留 luban 自己用得上的三项。
///
/// - `user:inference` —— 转发 `/v1/*` 靠它，缺了这个号就只能登进来看额度；
/// - `user:profile` —— 交换后拉邮箱/等级/account_uuid，缺了只是标签与等级留白（登录不失败）；
/// - `user:file_upload` —— 经代理走 Files API 的上传。
///
/// 相比 [`SCOPES`] 少了 `org:create_api_key`（建 API key，luban 从不调）、
/// `user:sessions:claude_code`、`user:mcp_servers`（官方客户端自己的功能面）。
/// 少要权限的代价是**授权请求与官方客户端不再逐字一致**——scope 集合也是指纹的一部分，
/// 所以这不是默认值，是给「宁可少授权、不在意这点差异」的人留的一档。
///
/// 还有一条要知道：这一档只管**登录那一刻**。刷新 token 发的是固定的 [`REFRESH_SCOPES`]
/// （官方行为），后端允许刷新时扩展 scope，所以第一次刷新后这个号的 scope 就回到那五项了。
pub const SCOPES_MINIMAL: &str = "user:file_upload user:inference user:profile";

/// 刷新 token 时随请求发送的 `scope`。
///
/// **是固定常量，不是登录时申请的那组**：官方客户端（`services/oauth/client.ts` 的
/// `refreshOAuthToken`）对 claude.ai 订阅号刻意不传已存的 scopes，让缺省值
/// `CLAUDE_AI_OAUTH_SCOPES` 生效——后端允许刷新时**扩展** scope，这样老 token 不必重新登录
/// 就能拿到后来加进来的 `user:file_upload`。里面没有 `org:create_api_key`（那一项只在
/// 授权 URL 上出现）。
///
/// 副作用要知道：登录时选了 [`SCOPES_MINIMAL`] 的号，第一次刷新后 scope 会被扩回这五项。
/// 那正是官方客户端的行为，刻意不做「按 settings 发」——那会造出一条官方从不产生的请求体。
pub const REFRESH_SCOPES: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// 把填进来的 scope 串规整成「单空格分隔、按输入顺序去重」的形态。
///
/// 顺序按输入保留而不排序：scope 集合是指纹的一部分，照抄一份抓包的顺序就该原样发出去。
///
/// **只规整，不校验**：写什么都收，原样发给上游。这里曾拦过「必须含 `user:inference`」和
/// 一套字符集，结果把这个输入框唯一的用途——试上游到底认哪些 scope——给拦掉了：连
/// `user:inference-1` 这种明摆着是拿来探边界的值都存不进去。合不合法由上游的同意页判，
/// 它的报错（如 `Missing scope parameter`）比我们猜的白名单准。
pub fn normalize_scopes(raw: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for item in raw.split_whitespace() {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out.join(" ")
}

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

/// `advisor-tool-2026-03-01`：haiku 没有 `effort` 时 [`CC_BETA_ADVANCED_TOOL_USE`] 的落位参照物
/// （2.1.251 起四族都带，haiku 官方串里 `advanced-tool-use` 紧跟其后，`cap/2.1.258/00031`）。
pub const CC_BETA_ADVISOR_TOOL: &str = "advisor-tool-2026-03-01";

/// `advanced-tool-use-2025-11-20`：对齐订阅端工具能力。
/// 官方排在 [`CC_BETA_EFFORT`] 之前；没有 effort 时排在 [`CC_BETA_ADVISOR_TOOL`] 之后；
/// 两个都没有才排在客户端自有串之后（2.1.220 的 haiku）。
pub const CC_BETA_ADVANCED_TOOL_USE: &str = "advanced-tool-use-2025-11-20";

/// `cache-diagnosis-2026-04-07`：2.1.251 起官方串的**最后一项**，
/// [`CC_BETA_EXTENDED_CACHE_TTL`] 的落位参照物（排在它前面）。API-key 客户端不发，
/// [`crate::proxy::merge_beta`] 补。
pub const CC_BETA_CACHE_DIAGNOSIS: &str = "cache-diagnosis-2026-04-07";

/// `server-side-fallback-2026-07-01`：2.1.258 订阅端四族都发，API-key 端都不发
/// （`cap/2.1.258-api` 原始请求头）。官方位置：`effort` 之后；haiku 没有 `effort`，在
/// `advanced-tool-use` 之后。
///
/// 2.1.260 起日期回到 [`CC_BETA_SERVER_SIDE_FALLBACK_JUN`]，且 opus 族整项不发了。
/// [`crate::proxy::merge_beta`] 对这项按**前缀**判在不在，免得给一个已经带 06-01 的
/// 2.1.260 来访再插一条 07-01，拼出「两条 server-side-fallback」这种官方不产生的形态。
pub const CC_BETA_SERVER_SIDE_FALLBACK: &str = "server-side-fallback-2026-07-01";

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
/// API-key 端的 fable 发。故 [`crate::proxy::merge_beta`] 对 fable 族把它剥掉——fable 上原始
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

/// 官方客户端的 `User-Agent`。用于 luban 自身发起的账号级请求（token 刷新、profile），
/// 这些请求原先不带任何 UA——一个持有订阅 refresh_token 却没有 UA 的客户端非常显眼。
/// 转发 `/v1/*` 时以来访客户端自己的 UA 为准（转发头覆盖此默认值）。
///
/// 取最近一次抓到的官方版本（cap/2.1.260）。落后不致命——真实用户升级也有先后——
/// 但落得太多就成了「一个几个月没升级过的客户端在不停刷 token」。
///
/// 动这里必须同时动 [`CC_VERSION_BASE`]（billing header 里的 `cc_version`）、
/// [`KEEPALIVE_USER_AGENT`] 与 [`CC_BUILD_TIMES`]（遥测里的构建时间），还有
/// [`CC_PROFILES`] 里那几串 beta：同一个客户端不会一边自称 2.1.260、一边报另一个版本的
/// cc_version、构建时间或上一版的 beta 集合。几处对不上是官方从不产生的组合。
pub const CC_USER_AGENT: &str = "claude-cli/2.1.260 (external, cli)";

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

/// `x-anthropic-billing-header` 里的 `cch`：**取值不在这里**，见 [`crate::proxy::cch_value`]。
///
/// 官方客户端仅在**订阅(OAuth)模式**下发送 `cch=<5 位小写 hex>`；API-key 模式（即接入
/// luban 的形态）不发。于是「OAuth token + 无 cch」是一个确定性判据，得补。
///
/// 曾经补的是常量 `00000`。那是个**跨账号恒定**的值：所有经由 luban 的请求都带同一个真实
/// 客户端从不产生的 `cch`，上游一按此聚类就把所有账号串成一串。现在改成每请求随机的 5 位
/// 小写 hex——形状与抓包一致，语义仍未知，别当成已经对齐。
pub mod billing_cch_is_a_shape_not_a_value {}

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
/// 以及 claude-cli/2.1.258（订阅端直连 `cap/2.1.258` ↔ API-key 端经 luban 入站原文
/// `cap/2.1.258-api`，同机同版本）：
///
/// | 模型 | 直连 / 经 luban | 官方基座 | 命中的锚点 | 在合并块里的偏移 |
/// |---|---|---|---|---|
/// | opus-5    | 00012 / 00006 | 1214B  | `Write code that…` 或 `Before you start…` | 1216 |
/// | fable-5-1 | 00013 / 00013 | 1214B  | 同上 | 1216 |
/// | sonnet-5  | 00026 / 00017 | 10520B | `# Text output…`   | 10522 |
/// | haiku-4.5 | 00031 / 00025 | 10622B | `# Text output…`   | 10624 |
///
/// 全部满足：合并块 = `基座 ‖ "\n\n" ‖ 其余`，锚点前紧跟 `\n\n`，切开后前缀与官方基座
/// **逐字节相同**。基座本身按模型族复用：haiku 与 sonnet-5 同一份、fable-5 与 opus-5 同一份。
/// opus/fable 的其余部分开头随会话而变（订阅端那次是 `Before you start…`，API-key 端那次是
/// `Write code that…`），两条锚点都在表里，取最早命中的即可。fable 的 API-key 形态是四块
/// （reporting 单独成块），见 [`crate::proxy::align_system_shape`]。
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
    // opus-5 / fable-5-1 @ claude-cli/2.1.258（cap/2.1.258/00012、00013）。2.1.251 的三句在这份
    // body 里**一句都不出现**，少了它整形直接退回三块、`ttl:"1h"`/`scope:"global"` 全都不写
    // ——这正是「fable-5-1 没走 1h 缓存」的根因。基座正文也跟着变了（1156B → 1214B，
    // `<system-reminder>` 那行换成了 mid-conversation system turns 的说法），但拆块只认锚点
    // 不认基座，不受影响。sonnet-5 / haiku 在 2.1.258 仍以 `# Text output…` 开头（00026、00031）。
    "Before you start, say in a line what you're about to do",
];

/// 官方 `system[1]` 那句身份声明，四个模型族逐字节相同（57 字节）。
///
/// 它同时是两件事：**上游对 OAuth 凭证唯一强制的正文**（缺了它订阅额度不给用），以及
/// 「这是不是一条 Claude Code 请求」的判据——[`crate::proxy::is_cc_shaped`] 认的就是它。
pub const CC_SYSTEM_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";

/// [`CC_SYSTEM_IDENTITY`] 去掉句号的前缀——用于 [`crate::proxy::is_cc_shaped`] 的匹配。
///
/// agent-sdk 的身份句是 `"…for Claude, running within the Claude Agent SDK."`，句号变逗号，
/// `contains(CC_SYSTEM_IDENTITY)` 匹配不到。用这个无句号前缀就能同时命中两种写法。
pub const CC_SYSTEM_IDENTITY_PREFIX: &str =
    "You are Claude Code, Anthropic's official CLI for Claude";

/// `system[0]` 那条 billing header 里的 `cc_version` 的**主版本**，形如 `2.1.260`。
///
/// 完整 `cc_version`（如 `2.1.260.222`）的第四段：模拟路径按 profile 取
/// [`CcProfile::billing_suffix`]；给真实 CC 客户端补 billing header 时仍由
/// [`crate::proxy::cc_version_suffix`] 从请求 body 派生。
/// 主版本号要和 [`CC_USER_AGENT`] 对得上——同一个客户端不会一边自称 2.1.260
/// 一边报另一个 cc_version。
///
/// **这只是模拟路径的版本。** 真实 CC 来访自己带着版本（UA 里那串），给它补 billing
/// header 时用的是**它自报的那个**（见 [`crate::proxy::billing_header_text`]）——给一个
/// 2.1.258 的来访写 2.1.260 的 cc_version，就是把两个版本混进了同一条请求。
pub const CC_VERSION_BASE: &str = "2.1.260";

/// 模拟模式注入的 `# Reporting outcomes` 块（911 字节），2.1.251 起出现。
///
/// 逐字节取自 `cap/2.1.251/00019`（opus-4-6 直连）的 `system[2]`，`cap/2.1.258/00013`
/// （fable-5-1）sha256 相同。它夹在身份声明与基座之间，无 `cache_control`。
///
/// **2.1.258 起只有 fable 族带它**：`cap/2.1.258` 里 fable-5-1（00013）是 5 块
/// `[billing, 身份, reporting, 基座, 其余]`，opus-5（00012/00025）、sonnet-5（00026）、
/// haiku-4.5（00031）都是 4 块 `[billing, 身份, 基座, 其余]`。2.1.251 时四族都带。
/// 按 profile 注入，判据是 [`CcSystemShape::IdentityReporting`]。
pub const CC_SYSTEM_REPORTING: &str = include_str!("assets/cc_system_reporting.txt");

/// 模拟模式注入的官方系统提示词**基座**（opus-5 / fable-5-1，1214 字节）。
///
/// 逐字节取自 `cap/2.1.258/00012`（opus-5 直连），与 `00013`（fable-5-1）、`00025`（opus-5）
/// sha256 相同。与 2.1.251 那份（1156B）只差一行：`<system-reminder>` 那句换成了
/// mid-conversation system turns 的说法。
pub const CC_SYSTEM_BASE_OPUS: &str = include_str!("assets/cc_system_base_opus.txt");

/// 模拟模式注入的官方系统提示词基座（sonnet-5，10520 字节）。
///
/// 逐字节取自 `cap/2.1.258/00026`（sonnet-5 直连）。与 2.1.251 那份（10580B）同样只差
/// `<system-reminder>` 那一行。
pub const CC_SYSTEM_BASE_SONNET: &str = include_str!("assets/cc_system_base_sonnet.txt");

/// 模拟模式注入的官方系统提示词基座（haiku-4.5 / opus-4-6[1m]，10622 字节）。
///
/// 逐字节取自 `cap/2.1.258/00031`（haiku-4.5 直连）。2.1.251 时它与 opus-4-6 那份 sha256
/// 相同，2.1.258 没有 opus-4-6 的样本，沿用这个映射。
pub const CC_SYSTEM_BASE_HAIKU: &str = include_str!("assets/cc_system_base_haiku.txt");

// ---------- 2.1.260 请求 profile ----------

/// 一条官方 2.1.260 请求属于哪一类。
///
/// **只按模型族分不够。** 同为 haiku-4.5，SDK 子代理（`cap/2.1.260/00020`）、无工具
/// helper（`00024`）、会话标题生成（`cap/2.1.260-2/00058`）与额度探测（`00004`）四者的
/// beta 串、`thinking`、`system` 块数、顶层键序和 billing 后缀**没有一项相同**。故
/// profile 的键是「模型族 + 请求用途」，不是模型族。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcProfileKind {
    /// 主线程 opus-5（`cap/2.1.260-2/00013`、`00025`、`00057`）。
    MainOpus,
    /// 主线程 fable-5-1（`cap/2.1.260/00018`、`00021`、`00031`）。
    MainFable,
    /// 主线程 sonnet-5：**2.1.260 没有样本**，由 2.1.258 那份按已证规则外推，见
    /// [`CC_PROFILES`] 的说明。
    MainSonnet,
    /// 主线程 haiku-4.5：同样**没有 2.1.260 样本**，外推而来。
    MainHaiku,
    /// agent-sdk 子代理 haiku（`cap/2.1.260/00020`、`00025`）：带工具、`cc_is_subagent`。
    SdkSubagentHaiku,
    /// 无工具的 haiku 辅助请求（`cap/2.1.260/00024`、`00027`）：`thinking:disabled`。
    HelperSubagentHaiku,
    /// 会话标题生成 haiku（`cap/2.1.260-2/00058`）：`output_config.format=json_schema`。
    SessionTitleHaiku,
    /// 安全分类 sonnet（`cap/2.1.260/00019`、`00030`）：`max_tokens:64`、非流式、键序独一份。
    SecurityClassifierSonnet,
    /// 额度探测（`cap/2.1.260-2/00004`）：`max_tokens:1`、无 `system`、无 billing header。
    QuotaProbe,
}

/// profile 的 `thinking` 形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcThinking {
    /// `{"type":"adaptive"}`——2.1.258 的 opus / sonnet 主线程（`cap/2.1.258/00012`、`00026`）。
    Adaptive,
    /// `{"type":"adaptive","display":"updates"}`——2.1.258 的 fable，2.1.260 起主线程四族都是。
    AdaptiveUpdates,
    /// `{"budget_tokens":N,"type":"enabled"}`——2.1.258 的 haiku（`cap/2.1.258/00031`）。
    Enabled,
    /// `{"budget_tokens":N,"type":"enabled","display":"updates"}`——2.1.260 的 SDK 子代理。
    EnabledUpdates,
    /// `{"type":"disabled"}`——helper / 标题 / 安全分类。**这是官方形态**，不是多余字段，
    /// 别当成第三方客户端塞的东西剥掉。
    Disabled,
    /// 整个字段都不发——额度探测。
    Absent,
}

/// `system` 的固定前缀形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CcSystemShape {
    /// `[billing, 身份句, 基座, 客户端]`——opus / sonnet / haiku 主线程。
    Identity,
    /// `[billing, 身份句, # Reporting outcomes, 基座, 客户端]`——fable 主线程。
    IdentityReporting,
    /// 没有 `system`——额度探测。
    None,
}

/// 一条官方请求形态的**全部**派生量，一处定义、各处引用。
///
/// 原先这些量散在 `cc_system_base(model)` / `cc_beta_seed(model)` /
/// `cc_system_reporting(model)` 三个按模型族分派的函数里，于是「同一族不同用途要有不同
/// 形态」根本表达不出来。合成一张表之后，加一个 profile 就是加一行。
#[derive(Debug, Clone, Copy)]
pub struct CcProfile {
    pub kind: CcProfileKind,
    /// 这份形态取自哪个客户端版本。写进 `cc_version` 与出站 UA 的就是它。
    pub version: &'static str,
    /// `anthropic-beta` 的**完整**官方串，去掉 `oauth`（由落位规则补回官方位置）与动态的
    /// `afk-mode`（同模型两次请求有/无交替出现，不进固定种子）。
    pub beta: &'static str,
    /// `cc_version` 的第四段。同一版本里每个 profile 一个固定值，见 [`CC_PROFILES`]。
    pub billing_suffix: &'static str,
    /// billing header 里带 `cc_is_subagent=true`。
    pub subagent: bool,
    pub system: CcSystemShape,
    pub thinking: CcThinking,
    /// 顶层 `fallbacks` 的 JSON 字面量；`None` 即不发这个字段。
    pub fallbacks: Option<&'static str>,
    /// 顶层键序，见 [`CC_BODY_ORDER_MAIN`]。
    pub body_key_order: &'static [&'static str],
}

impl CcProfile {
    /// 这个 profile 的请求带不带 `system[0]` 那条 billing header。
    pub fn has_billing_header(&self) -> bool {
        !matches!(self.system, CcSystemShape::None)
    }
}

/// 主线程与带 `system` 的辅助请求共用的顶层键序。
///
/// 主线程四族（`cap/2.1.260-2/00013`、`cap/2.1.260/00018`）、SDK 子代理（`00020`）、
/// 无工具 helper（`00024`）与标题生成（`cap/2.1.260-2/00058`）六份抓包的键序都是这一串的
/// 子序列——各自缺的键直接跳过，共有键一个都没挪位。`temperature` 只在后两者出现，
/// 排在 `thinking` 之后；主线程从不发它，故它相对 `context_management` 的位置无从观测，
/// 取「紧跟 thinking」这个唯一有证据的落点。
pub const CC_BODY_ORDER_MAIN: &[&str] = &[
    "model",
    "messages",
    "system",
    "tools",
    "metadata",
    "max_tokens",
    "thinking",
    "temperature",
    "context_management",
    "fallbacks",
    "output_config",
    "diagnostics",
];

/// 安全分类请求的顶层键序（`cap/2.1.260/00019`、`00030`）：**和主线程完全不同**，
/// `max_tokens` 排在第二、`system` 在 `messages` 前面。不能拿主线程那串硬套。
pub const CC_BODY_ORDER_CLASSIFIER: &[&str] =
    &["model", "max_tokens", "system", "messages", "stop_sequences", "thinking", "metadata"];

/// 额度探测的顶层键序（`cap/2.1.260-2/00004`、`00021`、`00047`）。
pub const CC_BODY_ORDER_QUOTA: &[&str] = &["model", "max_tokens", "messages", "metadata"];

/// 2.1.260 的 profile 全表。beta 串逐字取自抓包，去掉 `oauth` 与 `afk-mode`。
///
/// | profile | 抓包 | 后缀 | system | tools | thinking |
/// |---|---|---|---|---:|---|
/// | `MainOpus` | `2.1.260-2/00025` | `222` | 4 块 | 16 | adaptive+updates |
/// | `MainFable` | `2.1.260/00018` | `bcd` | 5 块 | 13 | adaptive+updates |
/// | `MainSonnet` | *无*（外推） | `1e2` | 4 块 | — | adaptive+updates |
/// | `MainHaiku` | *无*（外推） | `1e2` | 4 块 | — | enabled+updates |
/// | `SdkSubagentHaiku` | `2.1.260/00020` | `660` | 3 块 | 4 | enabled+updates |
/// | `HelperSubagentHaiku` | `2.1.260/00024` | `d95` | 2 块 | 0 | disabled |
/// | `SessionTitleHaiku` | `2.1.260-2/00058` | `ced` | 3 块 | 0 | disabled |
/// | `SecurityClassifierSonnet` | `2.1.260/00019` | `3de` | 3 块 | — | disabled |
/// | `QuotaProbe` | `2.1.260-2/00004` | — | 无 | — | 无 |
///
/// **`MainSonnet` / `MainHaiku` 是外推的，不是抓包。** 2.1.260 只抓到了 opus 与 fable 的
/// 主线程（[`crate::config::cc_2_1_260_missing_samples`]）。外推只用了两条在**全部六份**
/// 2.1.260 抓包上都成立的规则：
///
/// 1. `thinking-display-updates` 与 `redact-thinking` 互斥——前者在（主线程 opus/fable、
///    SDK 子代理）则后者必不在，反之亦然（helper、标题、分类）。主线程要显示思考过程，
///    故 2.1.260 的 sonnet/haiku 主线程按「有 display-updates、无 redact-thinking」推。
/// 2. `server-side-fallback` 出现时日期一律是 `2026-06-01`（fable 主线程、helper）。
///
/// 剩下那一处**没有证据**：opus 主线程在 2.1.260 整项不发 `server-side-fallback`（2.1.258
/// 时发）。sonnet/haiku 是跟着 opus 一起不发了，还是像 fable 那样留着换了日期，抓包答不
/// 上。这里取后者——它们在 2.1.258 自己就发这一项，「留着换日期」比「整项消失」离各自的
/// 上一版更近。抓到样本前这两行都不能算已证。
pub const CC_PROFILES: &[CcProfile] = &[
    CcProfile {
        kind: CcProfileKind::MainOpus,
        version: "2.1.260",
        // `cap/2.1.260-2/00025`：相对 2.1.258 删了 `redact-thinking` 与
        // `server-side-fallback`，加了 `thinking-display-updates`。
        beta: "claude-code-20250219,context-1m-2025-08-07,\
               interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
               context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
               mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,\
               advanced-tool-use-2025-11-20,effort-2025-11-24,fallback-credit-2026-06-01,\
               thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
               cache-diagnosis-2026-04-07",
        billing_suffix: "222",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::AdaptiveUpdates,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
    },
    CcProfile {
        kind: CcProfileKind::MainFable,
        version: "2.1.260",
        // `cap/2.1.260/00018`：相对 2.1.258 把 `advisor-tool` 换成了 `per-turn-control`，
        // `server-side-fallback` 的日期从 07-01 回到 06-01。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               per-turn-control-2026-07-01,advanced-tool-use-2025-11-20,effort-2025-11-24,\
               server-side-fallback-2026-06-01,fallback-credit-2026-06-01,\
               thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
               cache-diagnosis-2026-04-07",
        billing_suffix: "bcd",
        subagent: false,
        system: CcSystemShape::IdentityReporting,
        thinking: CcThinking::AdaptiveUpdates,
        // 2.1.258 时是字符串 `"default"`，2.1.260 换成了数组。**语义随之变了**：这是在替
        // 用户声明「本模型不可用时服务端改用 opus-5 跑」，模型换了计价也跟着换。故模拟
        // 路径默认**不发**它，见 [`crate::proxy::ensure_fallbacks`]——表里留着是因为它是
        // 官方形态的一部分，形态与要不要替用户拨这个开关是两件事。
        fallbacks: Some(r#"[{"model":"claude-opus-5"}]"#),
        body_key_order: CC_BODY_ORDER_MAIN,
    },
    CcProfile {
        kind: CcProfileKind::MainSonnet,
        version: "2.1.260",
        // 外推：`cap/2.1.258/00026` 去掉 `redact-thinking`、把 `server-side-fallback` 换成
        // 06-01、在 `fallback-credit` 之后补 `thinking-display-updates`。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,effort-2025-11-24,\
               server-side-fallback-2026-06-01,fallback-credit-2026-06-01,\
               thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
               cache-diagnosis-2026-04-07",
        // 没有 2.1.260 的 sonnet 主线程样本，后缀沿用 2.1.258 那个四族通用值。它一定不对，
        // 但比抄 opus 的 `222`（那是「opus 主线程」的标记）更少造出错误关联。
        billing_suffix: "1e2",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::AdaptiveUpdates,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
    },
    CcProfile {
        kind: CcProfileKind::MainHaiku,
        version: "2.1.260",
        // 外推：`cap/2.1.258/00031` 同上三处改动。haiku 不发 `effort`/`mid-conversation-system`，
        // 且 `claude-code` 排在第 6 位而非队首——这个位置在 2.1.260 的三份 haiku 抓包里没变。
        beta: "interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
               context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
               claude-code-20250219,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
               server-side-fallback-2026-06-01,fallback-credit-2026-06-01,\
               thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
               cache-diagnosis-2026-04-07",
        billing_suffix: "1e2",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::EnabledUpdates,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
    },
    CcProfile {
        kind: CcProfileKind::SdkSubagentHaiku,
        version: "2.1.260",
        // `cap/2.1.260/00020`：比主线程 haiku 短得多——没有 advisor-tool / advanced-tool-use /
        // server-side-fallback / fallback-credit / extended-cache-ttl。
        beta: "interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
               context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
               claude-code-20250219,thinking-display-updates-2026-08-18,\
               cache-diagnosis-2026-04-07",
        billing_suffix: "660",
        subagent: true,
        system: CcSystemShape::Identity,
        thinking: CcThinking::EnabledUpdates,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
    },
    CcProfile {
        kind: CcProfileKind::HelperSubagentHaiku,
        version: "2.1.260",
        // `cap/2.1.260/00024`。没有 `claude-code` beta，却是官方 2.1.260 请求。
        beta: "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,server-side-fallback-2026-06-01,\
               fallback-credit-2026-06-01,cache-diagnosis-2026-04-07",
        billing_suffix: "d95",
        subagent: true,
        system: CcSystemShape::Identity,
        thinking: CcThinking::Disabled,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
    },
    CcProfile {
        kind: CcProfileKind::SessionTitleHaiku,
        version: "2.1.260",
        // `cap/2.1.260-2/00058`。同样没有 `claude-code` beta；`structured-outputs` 配的是
        // body 里的 `output_config.format=json_schema`。
        beta: "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,\
               structured-outputs-2025-12-15,fallback-credit-2026-06-01,\
               cache-diagnosis-2026-04-07",
        billing_suffix: "ced",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::Disabled,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
    },
    CcProfile {
        kind: CcProfileKind::SecurityClassifierSonnet,
        version: "2.1.260",
        // `cap/2.1.260/00019`。注意它**没有** `thinking-token-count` 与 `cache-diagnosis`，
        // 是六个 profile 里唯一少这两项的。
        beta: "claude-code-20250219,context-1m-2025-08-07,\
               interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
               mid-conversation-system-2026-04-07,auto-mode-classifier-2026-07-16,\
               extended-cache-ttl-2025-04-11",
        billing_suffix: "3de",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::Disabled,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_CLASSIFIER,
    },
    CcProfile {
        kind: CcProfileKind::QuotaProbe,
        version: "2.1.260",
        // `cap/2.1.260-2/00004`。整条请求只有四个顶层键，`system` 与 billing header 都没有。
        beta: "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05",
        // 没有 billing header，这个值用不上；留空串免得被误当成真后缀写出去。
        billing_suffix: "",
        subagent: false,
        system: CcSystemShape::None,
        thinking: CcThinking::Absent,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_QUOTA,
    },
];

/// 2.1.258 的主线程四族 profile，**给真实 2.1.258 来访用**。
///
/// 留着它不是为了怀旧：[`crate::proxy::merge_beta`] 要按来访**自报的版本**决定补哪几项。
/// 拿 2.1.260 那张表去处理一个自报 2.1.258 的客户端，会给它补上 `thinking-display-updates`
/// 并剥掉 `redact-thinking`——那是 2.1.260 才有的形态，拼在一条 2.1.258 的请求上就是
/// 「同一条请求里混了两个版本」，比不补更容易被认出来。
///
/// 相对 2.1.260 的三处差异（就是这一版升级改的那三样）：
/// - opus / sonnet / haiku 发 `redact-thinking`、不发 `thinking-display-updates`；
/// - `server-side-fallback` 是 `2026-07-01`，且 opus 也发；
/// - fable 发的是 `advisor-tool` 而不是 `per-turn-control`。
///
/// 后缀四族统一 `1e2`（`cap/2.1.258` 五份对话抓包全是这个值）。只列主线程四族：2.1.258
/// 没有抓到辅助请求的样本，编不出来的行就不编。
pub const CC_PROFILES_2_1_258: &[CcProfile] = &[
    CcProfile {
        kind: CcProfileKind::MainOpus,
        version: "2.1.258",
        // `cap/2.1.258/00025`（opus-5 直连，无 afk-mode 的那次）。
        beta: "claude-code-20250219,context-1m-2025-08-07,\
               interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,effort-2025-11-24,\
               server-side-fallback-2026-07-01,fallback-credit-2026-06-01,\
               extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07",
        billing_suffix: "1e2",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::Adaptive,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
    },
    CcProfile {
        kind: CcProfileKind::MainFable,
        version: "2.1.258",
        // `cap/2.1.258/00013`（fable-5-1 直连）。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
               advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,effort-2025-11-24,\
               server-side-fallback-2026-07-01,fallback-credit-2026-06-01,\
               thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
               cache-diagnosis-2026-04-07",
        billing_suffix: "1e2",
        subagent: false,
        system: CcSystemShape::IdentityReporting,
        thinking: CcThinking::AdaptiveUpdates,
        // 2.1.258 发的是字符串 `"default"`（`cap/2.1.258/00013`）。
        fallbacks: Some(r#""default""#),
        body_key_order: CC_BODY_ORDER_MAIN,
    },
    CcProfile {
        kind: CcProfileKind::MainSonnet,
        version: "2.1.258",
        // `cap/2.1.258/00026`（sonnet-5 直连）。
        beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
               redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
               context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
               mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,\
               advanced-tool-use-2025-11-20,effort-2025-11-24,\
               server-side-fallback-2026-07-01,fallback-credit-2026-06-01,\
               extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07",
        billing_suffix: "1e2",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::Adaptive,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
    },
    CcProfile {
        kind: CcProfileKind::MainHaiku,
        version: "2.1.258",
        // `cap/2.1.258/00031`（haiku-4.5 直连）。
        beta: "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
               thinking-token-count-2026-05-13,context-management-2025-06-27,\
               prompt-caching-scope-2026-01-05,claude-code-20250219,\
               advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
               server-side-fallback-2026-07-01,fallback-credit-2026-06-01,\
               extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07",
        billing_suffix: "1e2",
        subagent: false,
        system: CcSystemShape::Identity,
        thinking: CcThinking::Enabled,
        fallbacks: None,
        body_key_order: CC_BODY_ORDER_MAIN,
    },
];

/// 按 kind 取 **2.1.260** 的 profile。表是常量，查不到即编译期就漏写了一行，故直接兜底到
/// `MainOpus` 而不是返回 `Option`——调用点没有「没有 profile」这种状态可处理。
pub fn cc_profile(kind: CcProfileKind) -> &'static CcProfile {
    CC_PROFILES.iter().find(|p| p.kind == kind).unwrap_or(&CC_PROFILES[0])
}

/// 按 kind **与来访自报的版本**取 profile。
///
/// `version` 是 `(major, minor, patch)`，来自客户端 UA（`claude-cli/x.y.z`）。低于 2.1.260
/// 时取 [`CC_PROFILES_2_1_258`]；**读不出版本时也取旧那份**——绝大多数在跑的客户端还不是
/// 2.1.260，猜新的一版等于给它们集体换一套形态。
///
/// 只有主线程四族有旧版行，其余 kind 一律落回 2.1.260 那张表。
pub fn cc_profile_at(kind: CcProfileKind, version: Option<(u64, u64, u64)>) -> &'static CcProfile {
    let is_260 = version.is_some_and(|v| v >= (2, 1, 260));
    if !is_260 && let Some(p) = CC_PROFILES_2_1_258.iter().find(|p| p.kind == kind) {
        return p;
    }
    cc_profile(kind)
}

/// **2.1.260 还缺的抓包**（记在案，别把外推当成已证）。
///
/// 1. 2.1.260 的 API-key 端四族成对抓包——没有它就无法证明「API-key → OAuth」的差分在
///    2.1.260 上仍是 2.1.258 那套（[`crate::proxy::merge_beta`] 的落位规则依赖这一点）。
/// 2. 2.1.260 的普通主线程 **sonnet-5** 请求。
/// 3. 2.1.260 的普通主线程 **haiku-4.5** 请求。
/// 4. TLS ClientHello / JA3 / JA4 原始字节，见 [`known_fingerprint_gaps`] 第 3 条。
///
/// 前三项缺着时，[`CC_PROFILES`] 里 `MainSonnet` / `MainHaiku` 两行是外推值，四模型族的
/// 差分矩阵不能宣称完整。
pub mod cc_2_1_260_missing_samples {}

/// 模拟模式下整套重建的固定请求头，取值逐字节取自 `cap/2.1.258/00012`（opus-5 直连），
/// 与 2.1.251 的 `00019` 逐字相同（Stainless SDK 0.112.1、node v26.3.0 都没变）。
///
/// 表里**只有固定值**；随请求变的几个不在此列，由 [`crate::proxy::official_headers`] 另外
/// 塞：`Authorization`（凭证）、`X-Claude-Code-Session-Id`（每设备派生）、
/// `x-client-request-id`（每请求 uuid），以及 `anthropic-beta`（见 [`CcProfile::beta`]）。
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
    ("x-stainless-package-version", "0.112.1"),
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
/// 5. **`cch` 的算法仍未知**（形状已对齐）。官方每次请求都不同（`0848d`、`5cb85`…），
///    luban 现在也每请求发一个随机的 5 位小写 hex，见 [`crate::proxy::cch_value`]。
///    形状对上了，语义没有——别把它当成已经对齐的一项。原先那个跨账号恒定的 `00000`
///    更糟：上游一按它聚类就把所有账号串成一串。
///
/// ~~6. `system` 块的切分与缓存 TTL~~ —— **已对齐**，见 [`crate::proxy::align_system_shape`]
///    与 [`CC_SYSTEM_BASE_ANCHORS`]。四个模型族的 raw 抓包逐字节验过。剩余风险只有锚点会随
///    CC 版本/新模型漂，漂了就退回三块原样转发（不会切错）。
///
/// 7. **`fallbacks` 与 `server-side-fallback`**。2.1.258 的订阅端直连抓包里四族都带
///    `server-side-fallback-2026-07-01` beta，但顶层 `fallbacks` 字段**只有 fable-5-1 发**
///    （`"fallbacks":"default"`，`cap/2.1.258/00013`）；opus-5 / sonnet-5 / haiku 有 beta、
///    没字段（00012/00025/00026/00031）。故「有 beta 没字段」本身就是官方形态，不再算不自洽。
///    API-key 端四族都**不发**这项 beta（`cap/2.1.258-api` 原始请求头），由
///    [`crate::proxy::merge_beta`] 补。
///
///    **fable 族仍刻意不补 `fallbacks`**：它声明的是「本模型拒答/不可用时由服务端改用别的
///    模型」，补上等于替用户决定换模型跑，模型换了计价也跟着换。这是用户该自己拨的语义，
///    不是形态。代价：模拟出的 fable-5-1 请求比官方少这一个顶层字段。
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
pub const KEEPALIVE_USER_AGENT: &str = "claude-code/2.1.260";

/// 事件日志里的 `betas` 字段：会话级 beta 集合，不含每请求才带的模型级 beta
/// （`advanced-tool-use`/`effort`/`extended-cache-ttl` 等）。取自
/// `cap/2.1.258/00032`（event_logging 批次，opus-5 会话那串）。比 2.1.251 多了队尾的
/// `mid-conversation-system-2026-04-07`。
pub const KEEPALIVE_EVENT_BETAS: &str = "claude-code-20250219,oauth-2025-04-20,\
    context-1m-2025-08-07,interleaved-thinking-2025-05-14,\
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
pub const KEEPALIVE_UA_BUN: &str = "Bun/1.4.1";

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
/// [`CC_ACCEPT_ENCODING`]（`gzip, deflate, br, zstd`）。两处混用就是把两个运行时的形态
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
/// [`cc_beta_order_is_not_a_table`] 记的是一类问题。
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
/// Messages API 那份 [`CC_ACCEPT_ENCODING`]。整条与 axios 那套没有一处相同
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

// ---------- 官方 CC 工具名白名单 ----------

/// 官方 Claude Code 客户端声明的全部工具名（含 deferred 展开后的名字）。
///
/// 来源：`cap/raw` 八份直连抓包 + `cap/2.1.145` 订阅直连。**只有这些名字在上游白名单内**，
/// 其余 custom tool 名即使功能正常也会被上游判为第三方应用（扣超额池或 400），故
/// [`crate::proxy::should_mimic_tool`] 对不在此集合内的 custom tool 统一加 `mcp__` 前缀。
///
/// 新版 CC 如果加了工具名，在这里补一条即可——漏补的代价只是多混淆一个官方名
/// （功能不受影响，回程会还原），发现后补上即恢复。
pub const CC_TOOL_NAMES: &[&str] = &[
    "Agent",
    "Artifact",
    "AskUserQuestion",
    "Bash",
    "CronCreate",
    "CronDelete",
    "CronList",
    "DeferredToolPlaceholder",
    "DesignSync",
    "Edit",
    "EnterPlanMode",
    "EnterWorktree",
    "ExitPlanMode",
    "ExitWorktree",
    "LSP",
    "ListAgents",
    "Monitor",
    "NotebookEdit",
    "PushNotification",
    "Read",
    "RemoteTrigger",
    "ReportFindings",
    "ScheduleWakeup",
    "SendFeedback",
    "SendMessage",
    "ShareOnboardingGuide",
    "Skill",
    "TaskCreate",
    "TaskGet",
    "TaskList",
    "TaskOutput",
    "TaskStop",
    "TaskUpdate",
    "ToolSearch",
    "WaitForMcpServers",
    "WebFetch",
    "WebSearch",
    "Workflow",
    "Write",
];

// ---------- 逐请求遥测（tengu_api_* 事件链） ----------

/// 官方各版本的 `build_time`（遥测事件 `env.build_time` / Datadog `build_time`，以及
/// `tengu_api_success.buildAgeMins` 的基准）。取自对应版本抓包的 event_logging 批次。
///
/// 出站 UA 是哪个版本就报哪个版本的构建时间——版本与构建时间对不上是官方从不产生的组合。
/// 表里没有的版本退回最后一项（最新已知版本）的值：宁可差几天，也不能缺字段。
pub const CC_BUILD_TIMES: &[(&str, &str)] = &[
    ("2.1.246", "2026-08-25T18:33:51Z"),
    ("2.1.258", "2026-09-01T21:54:40Z"),
    // `cap/2.1.260-2/00016` 的 event_logging 批次（`env.build_time`）。
    ("2.1.260", "2026-09-03T19:41:35Z"),
];

/// 按版本取 `build_time`，见 [`CC_BUILD_TIMES`]。
pub fn cc_build_time(version: &str) -> &'static str {
    CC_BUILD_TIMES
        .iter()
        .find(|(v, _)| *v == version)
        .or(CC_BUILD_TIMES.last())
        .map(|(_, t)| *t)
        .unwrap_or("2026-09-01T21:54:40Z")
}

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

#[cfg(test)]
mod tests {
    use super::*;

    /// 规整只做两件事：压空白、按输入顺序去重。**不排序**——scope 集合是指纹的一部分，
    /// 用户照抄一份抓包的顺序就该原样发出去。
    #[test]
    fn normalize_keeps_the_order_it_was_given() {
        assert_eq!(
            normalize_scopes("  user:profile\n\tuser:inference  "),
            "user:profile user:inference"
        );
        assert_eq!(
            normalize_scopes("user:inference user:profile user:inference"),
            "user:inference user:profile"
        );
        assert_eq!(normalize_scopes("   "), "");
        // 默认那两串本身已经是规整形态（写常量时手抖多个空格也能被这条测出来）。
        assert_eq!(normalize_scopes(SCOPES), SCOPES);
        assert_eq!(normalize_scopes(SCOPES_MINIMAL), SCOPES_MINIMAL);
    }

    /// 不校验：探边界用的怪值、写错的分隔符、没见过的 scope 一律照收——拦下来就没法用这个
    /// 输入框去问上游「你认不认这个」了。只有空白会被压掉（空串 = 用默认值）。
    #[test]
    fn anything_goes_in_it_comes_out_normalized() {
        assert_eq!(normalize_scopes("user:inference-1"), "user:inference-1");
        assert_eq!(
            normalize_scopes("user:inference, user:profile"),
            "user:inference, user:profile"
        );
        assert_eq!(normalize_scopes("\"user:inference\""), "\"user:inference\"");
        assert_eq!(
            normalize_scopes("user:some_scope_invented_next_month"),
            "user:some_scope_invented_next_month"
        );
    }
}
