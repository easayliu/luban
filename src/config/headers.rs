//! 模拟路径的请求头：定值、顺序与尚未对齐的指纹缺口。

use super::*;

/// 模拟模式下整套重建的固定请求头，取值逐字节取自 `cap/2.1.258/00012`（opus-5 直连），
/// 与 2.1.251 的 `00019` 逐字相同。2.1.285（`cap/2.1.285/00030`）起 Stainless SDK 升到 0.127.0
/// （2.1.251 ~ 2.1.280 一直是 0.112.1），node 仍是 v26.3.0。
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
    ("x-stainless-package-version", "0.127.0"),
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

/// `anthropic-dispatch-id` 的取值（`cap/2.1.285` 每条 messages 都是 `v2d`，额度探测 `00017` 不带）。
///
/// 可执行文件里它有三个取值：服务端特性开关 `tengu_dreamy_frost` 开着时所有请求（含标题生成
/// 这类 `auxiliary`）发 `v2d`；关着时只有非 `auxiliary` 请求在 `tengu_cedar_lattice` 开着时发
/// `v2s`；上一次 5xx 之后的重试改发 `v2p`。2.1.285 这一版 `tengu_dreamy_frost` 是强制开
/// （`cap/2.1.285/00008` 的 eval 响应 `"source":"force"`，2.1.280 的 `00007` 是关），即这一版
/// 的官方客户端一律发 `v2d`。模拟路径只发 `v2d`，不模拟重试那个 `v2p`。
///
/// 真 CC 来访自己带着这个头，原样转发（[`CC_HEADER_ORDER`] 给它归位）。
pub const CC_DISPATCH_ID: &str = "v2d";

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
    // 2.1.285 起（`cap/2.1.285/00030` 等），落在上一项与 `anthropic-version` 之间，见 [`CC_DISPATCH_ID`]。
    "anthropic-dispatch-id",
    "anthropic-version",
    "x-app",
    // 2.1.277 起的四个 `x-claude-code-*` 头（`cap/2.1.277`）：子代理带 `agent-id` / `agent-type`
    // （`00049`），工具续轮带 `prev-tool-durations`（`00026`），每条都带 `request-class`；四个都
    // 落在 `x-app` 与 `x-client-request-id` 之间、按这个先后。模拟路径只写 `request-class`
    // （[`CcProfile::request_class`]），其余三个是真 CC 来访自己带的，列在这里是为了归位。
    "x-claude-code-agent-id",
    "x-claude-code-agent-type",
    "x-claude-code-prev-tool-durations",
    // 2.1.285 起（2.1.283 的 CHANGELOG：「gateway hint headers」），凡 billing header 里写了
    // `cc_prompt_id` 的请求都带，值与它相同（`cap/2.1.285/00030` 主线程、`00115` 工具续轮、
    // `00120` 子代理、`00125` helper）；标题、主线程分叉的 auxiliary 与额度探测没有
    // `cc_prompt_id`，也不带它。落在 `prev-tool-durations` 与 `request-class` 之间。
    "x-claude-code-prompt-id",
    "x-claude-code-request-class",
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
/// ~~5. `cch` 的算法~~ —— **已对齐**。官方每次请求都不同（`0848d`、`5cb85`…），因为它是
///    出口层对最终出站 body 做的 xxHash64 取低 20 位（种子 `proxy::body::CCH_SEED`，
///    哈希前把 `model` 值清空、剥掉 `max_tokens`/`fallbacks`/`fallback_credit_token`）。
///    luban 按同一算法回填真值，见 `proxy::body::apply_cch`；`cap/` 263 条全中。
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
///    [`crate::proxy::merge_beta_for`] 补。
///
///    `fallbacks` 分两档开关决定补不补，见 [`crate::proxy::refusal_fallbacks_for`]：
///    `fable_refusal_fallback`（默认开）开着时 fable 主线程补官方那份 `[{"model":"claude-opus-5"}]`
///    （形态与官方逐字相同），关掉则回到「模拟出的 fable-5-1 请求比官方少这一个字段」；
///    `opus_refusal_fallback`（**默认关**，实验开关）开着时 opus-5 主线程补 luban 自定的
///    [`OPUS_REFUSAL_FALLBACKS`]——官方 2.1.260 的 opus 不发这个字段，补上是官方从不产生的
///    形态，故默认保持与官方一致（有 beta 没字段）。
///
/// 比对基准只能用**原始字节**——`cap/raw/*.raw` 那种（HTTPS 隧道内的报文，头名大小写、头序、
/// body 的 key 顺序都留得住）。`cap/*.json` 是抓包工具重新序列化过的：headers 与 body 的 key
/// 全被按字母序重排，只有数组元素的顺序还作数。[`CC_HEADER_ORDER`] 曾照着它抄，抄出一份
/// 官方客户端不会产生的头序。
pub mod known_fingerprint_gaps {}
