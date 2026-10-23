//! 2.1.309 的 profile 表（模拟路径用的就是它），及这一版还缺的抓包。

use super::*;

/// 2.1.309 的 profile 表——**模拟路径用的就是它**（[`cc_profile`]），也是 ≥2.1.309 来访的
/// beta 参照（[`cc_profile_at`]）。
///
/// **各行除版本号外与 2.1.308（[`CC_PROFILES_2_1_308`]）完全相同**，所以直接引用那张表的行，只换
/// `version`。依据 `cap/auto-2.1.309-20261023-full`（场景同 2.1.308-full，测试项目换了干净的
/// `scratchpad/cal2`，同从那份 clone）：请求头除 UA 外逐字相同（原始报文里的头名大小写与次序也比过），
/// beta、`metadata.user_id` 与 billing 字段及次序都没变，system 各块逐字不变。
///
/// 两版内嵌 JS 逐模块比过（模块 2776→2804、有搬动，按 acorn 骨架配对）：messages 请求上只有两处文案
/// 变了，且**都不碰模拟路径**——`Bash` 工具描述的 `# Git` 段多一句「GitHub notifies the owner of any
/// `@name`…」、`Artifact` 的 `root` 参数描述改写（并删 `pr_review`）。注入的那份工具资产
/// （[`cc_tools_core`]）是 2.1.291 的，模拟路径一直用它、不追工具文案（上游不校验工具描述），透传路径
/// 原样转发客户端自己带的文案，故两处都不进对齐。可执行文件只换了打包块名（Bun 运行时版本串仍 1.4.4、
/// WebKit 同 308），出口层的 cch 计算、HTTP/1.1 请求头写出与 TLS ClientHello 都没变（cch 全量回归覆盖
/// 这一版）；变的是遥测里报错栈的打包块名（[`crate::telemetry::shape::error_trace`] 与失败 Bash 那条），
/// 另外 5xx 那条**不再带 `error_constructor`**（压缩后的 5xx 类名首字母小写，过不了官方对构造名的校验）。
///
/// **静态看不出、抓包才发现的新行为（本次范围内不模拟）**：这次直连第一方的抓包里，每个会话启动会建一个
/// 「claude-artifacts」笔记本 MCP 入口（`POST /api/artifacts/mcp`，server/discover + initialize，这次服务端
/// 两条都回 404），随之多出启动遥测 `tengu_feature_ok{feature_name: artifact_mcp_laptop_entry}`、一条 404 的
/// `tengu_mcp_server_connection_failed`、`tengu_mcp_servers` 的 `plugin` 0→1 与连接成功事件的
/// `totalServers`/`httpCount` +1。**决定不模拟这组（连接本身与它的 ok / 404 / skip 遥测都不发）**：建不建这个
/// 入口依赖客户端资格、会话宿主、provider、OAuth/frame 基址与服务端响应，不该把这次抓到的临时 404 固化成通用
/// 行为。源码上这个入口由 `isArtifactsMcpLaptopEntry` 一串前置条件把关，其中基址判的是 **OAuth/frame 基址**
/// （`base_not_first_party`），**不等同于设了 `ANTHROPIC_BASE_URL`**，`provider` 也不是由 `ANTHROPIC_BASE_URL`
/// 直接决定；真正走 gateway 的会跳过创建（内部可能生成 `feature_sad`，外发还受遥测开关约束）。
pub const CC_PROFILES: &[CcProfile] = &[
    at_309(CC_PROFILES_2_1_308[0]),
    at_309(CC_PROFILES_2_1_308[1]),
    at_309(CC_PROFILES_2_1_308[2]),
    at_309(CC_PROFILES_2_1_308[3]),
    at_309(CC_PROFILES_2_1_308[4]),
    at_309(CC_PROFILES_2_1_308[5]),
    at_309(CC_PROFILES_2_1_308[6]),
    at_309(CC_PROFILES_2_1_308[7]),
    at_309(CC_PROFILES_2_1_308[8]),
];

const fn at_309(p: CcProfile) -> CcProfile {
    CcProfile { version: "2.1.309", ..p }
}

/// **2.1.309 还缺的抓包**：同 [`cc_2_1_308_missing_samples`]。这一版只抓了 full / model / retry
/// （`cap/auto-2.1.309-20261023-{full,model,retry}`，第一份 full 受别处并发测试污染、重抓了一份干净的），
/// gaps（OAuth 直连补 fable 默认权限模式、haiku-5-5 的 1M 会话、claude-code-guide 子代理）与 API-key 端
/// 没有重抓；两版内嵌 JS 对这些场景逐字相同，沿用 2.1.308 的结论。安全分类一项仍缺（同 308）。
pub mod cc_2_1_309_missing_samples {}
