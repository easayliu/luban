//! 官方客户端本身：UA、system 提示词、版本号、工具名与构建时间。

/// 官方客户端的 `User-Agent`。用于 luban 自身发起的账号级请求（token 刷新、profile），
/// 这些请求原先不带任何 UA——一个持有订阅 refresh_token 却没有 UA 的客户端非常显眼。
/// 转发 `/v1/*` 时以来访客户端自己的 UA 为准（转发头覆盖此默认值）。
///
/// 取最近一次抓到的官方版本（`cap/auto-2.1.291-20261006-full`）。落后不致命——真实用户升级也有先后——
/// 但落得太多就成了「一个几个月没升级过的客户端在不停刷 token」。
///
/// 动这里必须同时动 [`CC_VERSION_BASE`]（billing header 里的 `cc_version`）、
/// [`KEEPALIVE_USER_AGENT`](super::KEEPALIVE_USER_AGENT) 与 [`CC_BUILD_TIMES`]（遥测里的构建时间），还有
/// [`CC_PROFILES`](super::CC_PROFILES) 里那几串 beta 与 [`CC_SYSTEM_BASE`] / [`CC_SYSTEM_REST`] 等提示词 / 工具资产：
/// 同一个客户端不会一边自称 2.1.291、一边报另一个版本的 cc_version、构建时间、上一版的
/// beta 集合或上一版的提示词。几处对不上是官方从不产生的组合。
pub const CC_USER_AGENT: &str = "claude-cli/2.1.291 (external, cli)";

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

/// `x-anthropic-billing-header` 里的 `cch`：**取值不在这里**，算法见 `proxy::body::compute_cch`、
/// 回填见 `proxy::body::apply_cch`。
///
/// 官方客户端仅在**订阅(OAuth)模式**下发送 `cch=<5 位小写 hex>`；API-key 模式（即接入
/// luban 的形态）不发。于是「OAuth token + 无 cch」是一个确定性判据，得补。
///
/// 曾经补的是常量 `00000`（跨账号恒定，上游一按它聚类就把所有账号串成一串），后来退一步
/// 改成每请求随机值（形状对、语义不对）。现在**语义也对齐了**：`cch` 是官方 Bun HTTP 出口层
/// 对最终出站 body 做的 xxHash64 取低 20 位（种子 `proxy::body::CCH_SEED`），
/// luban 按同一算法算真值——`cap/` 九组抓包 263 条 `/v1/messages` 全部命中。真实来访与模拟
/// 请求各有一项独立策略（`cch_real_recompute` / `cch_sim_compute`，关掉即退回旧做法）。
pub mod billing_cch_is_computed_not_random {}

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
/// 别拿 `cap/*.json` 顶（见 [`CC_HEADER_ORDER`](super::CC_HEADER_ORDER) 的教训）。
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

/// 官方**子代理**（SDK 子代理、Helper）`system[1]` 那句身份声明，逐字取自
/// `cap/2.1.260/00020`、`00024`、`00027`——三份完全相同。子代理不写 [`CC_SYSTEM_IDENTITY`]，
/// 写的是这句。供 `proxy::is_official_helper_request` 认官方 Helper 用。
pub const CC_SDK_AGENT_IDENTITY: &str =
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.";

/// `system[0]` 那条 billing header 里的 `cc_version` 的**主版本**，形如 `2.1.260`。
///
/// 完整 `cc_version`（如 `2.1.280.bc5`）的第四段两条路径都由
/// [`crate::proxy::cc_version_suffix`] 从请求 body 派生（会话首条非 meta 用户文本 + 这个版本号）。
/// 主版本号要和 [`CC_USER_AGENT`] 对得上——同一个客户端不会一边自称 2.1.260
/// 一边报另一个 cc_version。
///
/// **这只是模拟路径的版本。** 真实 CC 来访自己带着版本（UA 里那串），给它补 billing
/// header 时用的是**它自报的那个**（见 [`crate::proxy::billing_header_text`]）——给一个
/// 2.1.258 的来访写 2.1.260 的 cc_version，就是把两个版本混进了同一条请求。
pub const CC_VERSION_BASE: &str = "2.1.291";

/// 已**抓包证实存在**的官方 Claude Code 最新版本，形如 `2.1.270`。它是「来访自报的版本说不
/// 说得通」那道闸（[`crate::proxy::known_latest_release`]）的写死下限：网上学来的
/// `claude-code-releases/latest` 与它取大者。
///
/// **与 [`CC_VERSION_BASE`] 是两件事。** 后者是模拟路径发出去的版本，和 [`CC_USER_AGENT`]、
/// [`CC_BUILD_TIMES`]、[`CC_PROFILES`](super::CC_PROFILES) 那几串 beta 绑在一起，只能随重新抓包整套换；这一个只
/// 回答「官方发到哪了」，抓到新版的请求就能抬，不牵动模拟形态。原先两者共用一个常量，于是
/// luban 刚起、还没拉到 `latest` 的那段时间里，一条真实 2.1.270 的来访会被判成「自报版本高于
/// 最新版」——读不出版本，落进 2.1.258 那张表，完整的订阅端请求被塞回上一版才有的
/// `server-side-fallback` / `fallback-credit`。
///
/// 依据：`cap/auto-2.1.291-20261006-full`（四族主线程、子代理、标题生成、额度探测、`-p`，UA
/// `claude-cli/2.1.291`）。
/// 不能低于任何一张 profile 表的版本（否则那张表永远选不中），有测试钉着。
pub const CC_LATEST_KNOWN_RELEASE: &str = "2.1.291";

/// 模拟模式注入的 `# Reporting outcomes` 块（911 字节），2.1.251 起出现。
///
/// 逐字节取自 `cap/2.1.251/00019`（opus-4-6 直连）的 `system[2]`，`cap/2.1.258/00013`
/// （fable-5-1）sha256 相同。它夹在身份声明与基座之间，无 `cache_control`。
///
/// **2.1.258 起只有 fable 族带它**：`cap/2.1.258` 里 fable-5-1（00013）是 5 块
/// `[billing, 身份, reporting, 基座, 其余]`，opus-5（00012/00025）、sonnet-5（00026）、
/// haiku-4.5（00031）都是 4 块 `[billing, 身份, 基座, 其余]`。2.1.251 时四族都带。
/// 按 profile 注入，判据是 [`CcSystemShape::IdentityReporting`](super::CcSystemShape::IdentityReporting)。
pub const CC_SYSTEM_REPORTING: &str = include_str!("../assets/cc_system_reporting.txt");

/// 模拟模式注入的官方系统提示词**基座**（2.1.277 起，1588 字节；2.1.291 起只剩 opus / sonnet /
/// fable 三族用它，haiku 换成了 [`CC_SYSTEM_BASE_HAIKU`]）。
///
/// 逐字节取自 `cap/2.1.277/00023`（fable-5-1 直连）的 `system[2]`，同目录 sonnet-5（`00031`）、
/// haiku-4.5（`00046`）、opus-5（`00357`）的主线程 sha256 全部相同。2.1.258 / 2.1.260 时三族
/// 各有各的基座（opus / fable 1214 字节，sonnet 10520，haiku 10622），2.1.277 收成了一份：
/// 就是 2.1.258 那份 opus 短基座多了 `<pasted_content>` 那一行 `# Harness` 条目。2.1.280 四族
/// 主线程（`cap/2.1.280/00021`、`00029`、`00033`、`00038`）、2.1.285 的 11 个模型（`cap/2.1.285/00030`
/// ~ `00088`）与 2.1.291 的 opus / sonnet / fable（`cap/auto-2.1.291-20261006-full/00340`、`00253`、
/// `00464`）都与它逐字节相同。
///
/// 只给模拟路径用；透传路径拆块认的是 [`CC_SYSTEM_BASE_ANCHORS`]，不认基座正文。
pub const CC_SYSTEM_BASE: &str = include_str!("../assets/cc_system_base.txt");

/// 2.1.291 haiku 主线程的**基座**（11050 字节）：`cap/auto-2.1.291-20261006-full/00303`（默认模式）
/// 与 `00553` 的 `system[2]` 逐字节相同，`cap/auto-2.1.291-20261006` 的 `00242` 也是。
///
/// 2.1.291 起 haiku 不再走 opus / sonnet / fable 那套「短基座 + 工具说明挪进第四块」的写法，
/// 而是回到长基座（`# Doing tasks`、`# Using your tools` 等整节都在这里）配长版工具描述
/// （[`crate::proxy::cc_tools_core`]：Agent 8451、Bash 11913 字节）——与 2.1.285 的 `-p` 打印模式
/// 同一路（`cap/auto-2.1.285-20260930/00441`）。可执行文件按模型挑这一套（`AV(model)` 不成立的
/// 模型），不是账号开关。逐字照抄，没有随机器变的内容。
pub const CC_SYSTEM_BASE_HAIKU: &str = include_str!("../assets/cc_system_base_haiku.txt");

/// 模拟模式注入的官方 `system` **第四块**（基座之后的「其余」段）模板——2.1.291 opus / sonnet
/// 主线程，4700 字节。
///
/// 取自 `cap/auto-2.1.291-20261006-full/00340` 的 `system[3]`（5443 字节），opus 默认模式（`00216`）、
/// auto 模式（`00032`）与 sonnet-5-5（`00253`）sha256 全部相同。末尾那行
/// `<total_tokens>15000000 tokens left</total_tokens>` 与 2.1.277 同一个数。fable 与 haiku 2.1.291
/// 起各有一份（[`CC_SYSTEM_REST_FABLE`]、[`CC_SYSTEM_REST_HAIKU`]）。
///
/// 相对 2.1.285 那份：记忆一节末尾去掉了「引用记忆时整句包进 `<cc-memory filenames=…>`」那句，
/// 其余逐字未变；末尾 `<total_tokens>` 之后多了一段 `WebSearch takes a mode…`（见下）。
///
/// 相对 2.1.280 那份（6738 字节）：记忆一节整段换了写法——`# auto memory` 改回 `# Memory`，
/// applicable / durable / legible 那套与 `## Citing memories` 没了，换成「每条记忆一个文件、
/// frontmatter 带 `metadata.type`（user / feedback / project / reference）、`MEMORY.md` 当索引」
/// 的说明，引用记忆的 `<cc-memory>` 写法并进同一段；`# Environment` 的模型列表里
/// `Sonnet 5: 'claude-sonnet-5'` 换成 `Sonnet 5.5: 'claude-sonnet-5-5'`。其余段落逐字未变。
/// 2.1.280 相对 2.1.277 的改动（开头换成 `Write code that reads like…`、Fable 自我介绍与
/// `# Delivering work` / `# Writing for the user` 两节删掉）照旧。随机器变的仍只有记忆目录
/// 一处，换成两个占位由 [`crate::proxy::render_system_rest`] 填：
///
/// | 占位 | 官方取值 | 填法 |
/// |---|---|---|
/// | `{{home}}` | 记忆目录的 `/Users/<user>` | 来访自己写了工作目录就用它的（`crate::proxy::client_env`），没写才按账号 + 设备派生，见 `SimEnv` |
/// | `{{cwd_slug}}` | 记忆目录的项目段，cwd 里 `/` 与 `_` 换成 `-`（`-private-tmp-proxy-captures-20260930-143352`） | 同上，由那份环境算出 |
///
/// 另去掉了末尾 `EndConversation (deferred tool): … Load the full guidance via ToolSearch(…)`
/// 那一段（233 字节，2.1.291 逐字未变）与 2.1.291 新增在 `<total_tokens>` 之后的 `WebSearch takes
/// a mode. Use "standard" by default…` 那一段（2.1.291 的 WebSearch 多了 `mode` 参数）：模拟路径
/// **不注** `ToolSearch`、`DeferredToolPlaceholder` 与延迟池里的 `WebSearch`
/// （[`crate::proxy::cc_tools_core`] 说明了为什么），留着这两段就是提示词让模型去调一个工具集里
/// 没有的工具，提示词与工具集不成套。其余每个字节照抓包。
///
/// **为什么要这一块**：官方主线程的 `system` 是 `[billing, 身份, 基座, 其余]`，「其余」这块
/// 几千到一万字节、带末尾断点，上游对末块有内容级检测（见 `proxy::MAX_CLIENT_SYSTEM_CHARS`）。
/// 此前模拟路径末块放的是客户端自己那段提示词，与官方形态差得最远的正是这一块。
pub const CC_SYSTEM_REST: &str = include_str!("../assets/cc_system_rest.txt");

/// 2.1.291 fable 主线程的第四块模板（10794 字节）：`cap/auto-2.1.291-20261006-full/00464`
/// （auto 模式）与 `cap/auto-2.1.291-20261006/00100`、`00136` 的 `system[3]`（11515 字节）逐字节
/// 相同。2.1.285 时 fable 与 opus 同一份；2.1.291 起 fable 换回 2.1.277 那种写法——开头是「先用
/// 一句话说要做什么……」，多了 Fable 5.1 的自我介绍、`# Delivering work` 与 `# Writing for the
/// user` 两节（可执行文件按模型挑，不是账号开关）。占位与去掉的两段同 [`CC_SYSTEM_REST`]。
pub const CC_SYSTEM_REST_FABLE: &str = include_str!("../assets/cc_system_rest_fable.txt");

/// [`CC_SYSTEM_REST_FABLE`] 里 fable-5-1 的自我介绍段（逐字取自那份模板）。
pub const CC_FABLE_5_1_IDENTITY: &str = "This iteration of Claude is Claude Fable 5.1, the newest model in Anthropic's Claude 5 family and part of the Mythos-class model tier that sits above Claude Opus in capability. Claude Fable 5.1 and Claude Mythos 5.1 share the same underlying model. Claude Fable 5.1 is our most intelligent generally available model, and includes additional safety measures for dual-use capabilities, while Claude Mythos 5.1 is available without those measures to only approved organizations. Fable 5.1 is the most advanced generally available Claude model. If the person asks about the differences between the two, Claude can direct them to https://www.anthropic.com/claude/fable for more information.";

/// 其余 fable 模型（fable-5）的自我介绍段：2.1.291 可执行文件按模型挑这一段（`function qIo`：
/// 规范名恰为 `claude-fable-5-1` 用上面那段，其余 fable 用这段），逐字取自同一处字符串常量。
/// 第四块的其余部分两者相同，见 [`crate::proxy::cc_system_rest`]。fable-5 这一版没有抓包。
pub const CC_FABLE_5_IDENTITY: &str = "This iteration of Claude is Claude Fable 5, the first model in Anthropic's new Claude 5 family and part of a new Mythos-class model tier that sits above Claude Opus in capability. Claude Fable 5 and Claude Mythos 5 share the same underlying model. Claude Fable 5 includes additional safety measures for dual-use capabilities, while Claude Mythos 5 is available without those measures to only approved organizations. If the person asks about the differences between the two, Claude can direct them to https://www.anthropic.com/news/claude-fable-5-mythos-5 for more information.";

/// 2.1.291 haiku 主线程的第四块模板（16720 字节）：`cap/auto-2.1.291-20261006-full/00303`、`00553`
/// 的 `system[3]`（17172 字节）逐字节相同。配 [`CC_SYSTEM_BASE_HAIKU`] 那套长基座：开头是
/// `# Text output (does not apply to tool calls)` 一节，记忆一节是 2.1.280 那种 `# auto memory`
/// 长写法。没有 EndConversation 那段；`WebSearch takes a mode` 那段同样去掉。占位同 [`CC_SYSTEM_REST`]。
pub const CC_SYSTEM_REST_HAIKU: &str = include_str!("../assets/cc_system_rest_haiku.txt");

// ---------- 首轮环境说明（`# Environment` 那条） ----------

/// 模拟的那台机器的 Darwin 内核版本：环境说明的 `OS Version: Darwin …` 与遥测 OTel 资源属性
/// `os.version` 同一个值（抓包机 `cap/auto-2.1.291-20261006-full` 两处都是 `27.0.0`）。
pub const CC_OS_RELEASE: &str = "27.0.0";

/// 2.1.291 首轮环境说明的**开头一段**模板（`# Environment` 到「Downloaded files…」那条）。
///
/// 官方首轮把环境、模型、Agent 类型、技能清单、`<total_tokens>` 与日期拼成一份：opus / sonnet /
/// fable（带 `mid-conversation-system`）是首条用户消息之后一条 `role: system` 消息、各段空一行
/// （`cap/auto-2.1.291-20261006-full/00340`、`00253`、`00464`）；haiku 与老一代模型每段各裹一个
/// `<system-reminder>`，排在首条用户消息最前面（`00303`、`00553`）。拼法见
/// `proxy::body::insert_env_note`。
///
/// 占位：`{{cwd}}` 工作目录、`{{os_release}}`（[`CC_OS_RELEASE`]）、`{{scratchpad}}`——不精简工具时
/// 是整行 `Scratchpad directory`（含行尾换行），精简（`sim_trim_tools`）时为空：同一台机器加那三个
/// 环境变量后官方这一行也没了（`cap/auto-2.1.291-20261006/00031` 对 `00066`）。`Is a git
/// repository` 恒写 `true`，`Platform` / `Shell` 与请求头的 MacOS 成套。
pub const CC_ENV_HEAD: &str = include_str!("../assets/cc_env_head.txt");

/// 环境说明里的 Agent 类型一段（opus / sonnet / fable，2703 字节），逐字节取自
/// `cap/auto-2.1.291-20261006-full/00340`；精简与否、三族之间都相同（`00066`、`00136`、`00205`）。
/// 六个都是内建 Agent，抓包机没有自定义 Agent。
pub const CC_ENV_AGENTS: &str = include_str!("../assets/cc_env_agents.txt");

/// haiku 那份 Agent 类型（2910 字节，`00303`、`cap/auto-2.1.291-20261006/00276`）：只有 Explore 的
/// 描述换了一版，其余逐字相同。
pub const CC_ENV_AGENTS_HAIKU: &str = include_str!("../assets/cc_env_agents_haiku.txt");

/// 环境说明里的技能清单，**只留内建技能**（17 个，7473 字节）：抓包机那份里带 `:` 的
/// （`commit-commands:*` 等插件、`anthropic-skills:*` 账号同步的）是那台机器自己装的，去掉。
/// 内建那部分在四族之间逐字相同，haiku 预算不够时砍的也只是非内建那几条的描述（`00303`）。
/// 精简工具时官方少了 [`CC_ENV_TRIMMED_SKILLS`] 三条（`cap/auto-2.1.291-20261006/00066`）。
pub const CC_ENV_SKILLS: &str = include_str!("../assets/cc_env_skills.txt");

/// 精简工具（关掉 Artifact）时技能清单里随之消失的三条。
pub const CC_ENV_TRIMMED_SKILLS: &[&str] =
    &["artifact-design", "artifact-diagramming", "artifact-capabilities"];

/// 环境说明里「You are powered by the model named …」那行的模型名与知识截止，逐条取自抓包
/// （`cap/` 各版本的环境说明）。键是去掉日期与 `[1m]` 的规范名；表里没有的模型按官方的兜底写法
/// 只写模型 id，见 `proxy::body::insert_env_note`。
pub const CC_MODEL_IDENTITIES: &[(&str, &str, &str)] = &[
    ("claude-opus-5-5", "Opus 5.5", "June 2026"),
    ("claude-fable-5-1", "Fable 5.1", "June 2026"),
    ("claude-sonnet-5-5", "Sonnet 5.5", "June 2026"),
    ("claude-haiku-4-5", "Haiku 4.5", "February 2025"),
    ("claude-opus-5", "Opus 5", "May 2026"),
    ("claude-fable-5", "Fable 5", "January 2026"),
    ("claude-sonnet-5", "Sonnet 5", "January 2026"),
    ("claude-opus-4-8", "Opus 4.8", "January 2026"),
    ("claude-opus-4-7", "Opus 4.7", "January 2026"),
    ("claude-opus-4-6", "Opus 4.6", "May 2025"),
    ("claude-sonnet-4-6", "Sonnet 4.6", "August 2025"),
];

// ---------- 官方 CC 工具名白名单 ----------

/// 官方 Claude Code 客户端声明过的全部工具名（含 deferred 展开后的名字与老版本的旧名）。
///
/// **只有这些名字在上游白名单内**，其余 custom tool 名即使功能正常也会被上游判为第三方应用
/// （扣超额池或 400），故 [`crate::proxy::should_mimic_tool`] 对不在此集合内的 custom tool
/// 统一加 `mcp__` 前缀。
///
/// 来源分三段，每段按字母序：
/// 1. **现版本主线程与延迟池**：`cap/2.1.258`～`cap/2.1.270` 全部 `/v1/messages` 抓包里
///    `tools[*].name` 的并集，加上 OAuth 端 ToolSearch 延迟池列表（正文 attachment 里的
///    「deferred tools」清单）与 Agent 描述里点名的 `Artifact*` 三件。
/// 2. **老版本旧名**：取自 2.1.276 二进制里官方自带的**旧名→新名改名表**
///    （`Task→Agent`、`KillShell/KillBash→TaskStop`、`BashOutput/AgentOutput(Tool)→TaskOutput`、
///    `ListPeers→ListAgents`、`Brief→SendUserMessage`、`ListMcpResources/ReadMcpResource(Dir)→…Tool`），
///    TaskStop / TaskOutput 工具定义上的 `aliases`，以及 SDK 侧 `BUILTIN_TOOL_NAMES`
///    （`Glob` / `Grep` / `Task` / `TodoWrite` / `SendUserMessage`）。这些名字在 2.1.258 之前的
///    版本里是主线程直接声明的（`Glob` / `Grep` 到 2.1.238 仍在正文里，2.1.258 起才并进延迟池），
///    老版本 CC 经 luban 转发时若被混淆成 `mcp__luban__*`，等于把官方名改成了官方从不发的名字。
/// 3. **更早的旧名**：`LS` / `MultiEdit` / `NotebookRead` 在 2.1.276 的权限规则表
///    （`filePatternTools` 等）里仍按工具名处理，1.x～2.0 的客户端曾直接声明。
///
/// 只收**证实官方发过**的名字：二进制里另有一批带 feature gate 的内部工具
/// （`SuggestConnectors` / `ProposeGoal` / `TeamCreate` 之类）与 SDK 的 `REPL` / `JavaScript`，
/// 没在任何抓包或改名表里出现过，不收——白名单收错一个名字的代价是那个名字原样出站被判第三方，
/// 与漏收一个官方名（只是多混淆、功能不受影响、回程还原）不对称。
///
/// 新版 CC 如果加了工具名，在这里补一条即可——漏补的代价只是多混淆一个官方名
/// （功能不受影响，回程会还原），发现后补上即恢复。
pub const CC_TOOL_NAMES: &[&str] = &[
    // ---- 1. 现版本主线程与延迟池（cap/2.1.258～2.1.270） ----
    "Agent",
    "Artifact",
    "ArtifactCheck",
    "ArtifactComments",
    "ArtifactData",
    "AskUserQuestion",
    "Bash",
    "CronCreate",
    "CronDelete",
    "CronList",
    "DeferredToolPlaceholder",
    "DesignSync",
    "Edit",
    "EndConversation",
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
    // ---- 2. 老版本旧名（2.1.276 二进制的改名表 / aliases / SDK BUILTIN_TOOL_NAMES） ----
    "AgentOutput",
    "AgentOutputTool",
    "BashOutput",
    "BashOutputTool",
    "Brief",
    "Glob",
    "Grep",
    "KillBash",
    "KillShell",
    "ListMcpResources",
    "ListMcpResourcesTool",
    "ListPeers",
    "PowerShell",
    "ReadMcpResource",
    "ReadMcpResourceDir",
    "ReadMcpResourceDirTool",
    "ReadMcpResourceTool",
    "SendUserMessage",
    "StructuredOutput",
    "Task",
    "TodoWrite",
    // ---- 3. 更早的旧名（1.x～2.0 直接声明，2.1.276 权限规则表仍按工具名处理） ----
    "LS",
    "MultiEdit",
    "NotebookRead",
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
    // `cap/2.1.277/00028` 的 event_logging 批次（`env.build_time`，`env.version_base` 同为 2.1.277）。
    ("2.1.277", "2026-09-18T15:34:36Z"),
    // `cap/2.1.280/00022`、`00036`、`00041` 三个 event_logging 批次（`env.version_base` 同为 2.1.280）。
    ("2.1.280", "2026-09-21T20:40:17Z"),
    // `cap/2.1.285/00031` 等 event_logging 批次（`env.build_time`），与可执行文件里的
    // `BUILD_TIME` 常量一致。
    ("2.1.285", "2026-09-29T01:34:53Z"),
    // `cap/auto-2.1.291-20261006-full/00028` 起的 event_logging 批次（`env.build_time`，
    // `env.version_base` 同为 2.1.291）。
    ("2.1.291", "2026-10-06T02:24:19Z"),
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
