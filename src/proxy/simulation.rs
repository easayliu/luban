use axum::http::HeaderMap;

use crate::config;
use crate::store;

use super::body::{
    CacheShape, cache_control, cch_value, extract_device_id, extract_session_id, text_block,
    text_block_bare,
};
use super::headers::has_beta;
use super::session_id::{incoming_session_id, pin_session_id, session_id_for};
use super::session_link::{CcRequestKind, CcSessionKey, CcSessionLink};
use super::{
    QUOTA_PROBE_MODEL, count_cache_control, inbound_beta_list, insert_top_level,
    is_quota_probe_shaped, request_max_tokens,
};

/// 一条**非 Claude Code 请求**要装成官方客户端时的全部派生量。
///
/// `Some` 即本条请求走模拟路径：转发头整套换成官方那套（[`official_headers`]）、`system`
/// 补上官方前缀（[`simulate_system`]）、`metadata` 补上身份（[`ensure_cc_metadata`]）。
/// `None` 即来访本来就是 CC 形态、或自带 `metadata.user_id`（同样是 CC 系客户端的记号，
/// 判据见 [`Self::detect`]）、或开关关着，照既有路径走，一个字节都不多改。
///
/// **存在的理由**：订阅(OAuth)凭证在上游是「只授权给 Claude Code 用」的，`system` 里缺那句
/// [`config::CC_SYSTEM_IDENTITY`] 就用不了额度。于是任何非 CC 客户端（各种 SDK、第三方
/// 前端、curl）经 luban 都是死路一条。补齐它等于把这些客户端接进订阅额度，而既然要补，
/// 就得**整条链路一起补**：只补那句身份声明、头却还是 `python-httpx`，反倒是个真实客户端
/// 绝不会产生的组合。
///
/// **代价**：每条请求多一个基座前缀（opus 族 1214 字节、sonnet 族 10682 字节，约 300 /
/// 2700 token）。它带 `ttl:1h` + `scope:global` 断点，全网同一份，稳定后基本走缓存读价；
/// 但**部署后每个模型族的第一条**要按写入价付一次，而且会**改变模型行为**——客户端拿到的
/// 是一个被告知「你是 Claude Code」的模型，输出风格与工具偏好都会随之偏移。不想要就把
/// [`store::SIMULATE_CC`] 关掉，代价是这类请求退回「上游直接拒」。
pub(super) struct Simulation {
    /// 按模型族选出的官方基座提示词；模型认不出来时 `None`——基座是逐字节从抓包取的，
    /// 猜错一族（把 sonnet 的 10682 字节发给 opus）比不发更糟。见 [`cc_system_base`]。
    pub(super) base: Option<&'static str>,
    /// 这条请求要装成哪一类官方请求：beta 串、billing 后缀、`system` 块形态、`thinking`
    /// 形态、`fallbacks` 与顶层键序全在里面，见 [`config::CcProfile`]。
    pub(super) profile: &'static config::CcProfile,
    /// `X-Claude-Code-Session-Id` 与 `metadata.user_id` 里 `session_id` 的**同一个**取值：
    /// 官方两处逐字相同，只对上一处等于自己造一个新判据。
    ///
    /// **优先用来访自己那个**（[`incoming_session_id`]）：客户端各开各的会话，折叠成一个
    /// 就是「一台设备上一个会话打了所有请求」。来访没带才按「账号 + 设备指纹」派生一个
    /// ——那份是同设备恒定的，代价（真实客户端会随会话轮换）记在 [`session_id_for`]。
    pub(super) session_id: String,
    /// 这条请求在会话链条上的位置：`cc_prompt_id` / `cc_prev_req` /
    /// `diagnostics.previous_message_id` 三个关联字段的取值，见 [`CcSessionLink`]。
    pub(super) link: CcSessionLink,
    /// 为什么走了模拟——[`simulates_cc`] 三道判据里没过的那一道，见 [`SimulationReason`]。
    /// 进日志与流水的 `sim_reason` 列：光一个「模拟路径」标签看不出是 UA 不可信、身份写错、
    /// 还是形态不完整，排查官方客户端为何被接管时这是唯一线索。
    pub(super) reason: SimulationReason,
}

/// 一条请求被模拟路径接管的原因：[`simulates_cc`] 要求 UA 可信、身份合法、形态完整三样
/// 同时成立才原样转发，这里记的是**第一道**没过的判据（按 UA → 身份 → 形态的顺序查）。
///
/// 只有走了模拟才有值；判定的口径与 [`simulates_cc`] 同源（[`simulation_reason`] 是它的本体）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SimulationReason {
    /// UA 不是可信的 Claude Code 版本（非 CC 客户端，或自报版本高于已知最新版）。
    NotCcClient,
    /// UA 可信，但 `metadata.user_id` / 会话头的身份字段格式不对（device 不是 64 位 hex、
    /// session 不是 uuid、带空白……），见 [`cc_identity_well_formed`]。
    IdentityMalformed,
    /// UA 与身份都对，但 `system` 里既没有 CC 身份句也没有 billing header，见 [`is_cc_shaped`]
    /// （官方桌面端不带 system 的 `max_tokens=1` 预热、官方 WebSearch 子调用例外，见
    /// [`simulation_reason`] 与 [`is_official_web_search_request`]）。
    NotCcShaped,
    /// CC 形态，但既不是 `max_tokens=1` 预热、也不是官方 Helper 或 WebSearch 子调用、`system`
    /// 里也没有不少于 [`CC_BASE_PROMPT_MIN_LEN`] 字节的基座提示词，见 [`has_cc_base_prompt`]。
    NoBasePrompt,
    /// 前面都对，`tools` 非空却一个官方工具名都没有，见 [`has_cc_tool_profile`]。
    ToolsNotCc,
    /// luban 自己发的探测（连通性测试、额度探测）：不是来访，没有判定，只为把探测装成官方形态。
    Probe,
}

impl SimulationReason {
    /// 日志与流水里的标签。
    pub(super) fn tag(self) -> &'static str {
        match self {
            Self::NotCcClient => "not_cc_client",
            Self::IdentityMalformed => "identity_malformed",
            Self::NotCcShaped => "not_cc_shaped",
            Self::NoBasePrompt => "no_base_prompt",
            Self::ToolsNotCc => "tools_not_cc",
            Self::Probe => "probe",
        }
    }
}

/// 来访体的几项**结构**事实（不含任何正文），随 `identity path: SIMULATED` 那行日志打出，
/// 配合 [`SimulationReason`] 一眼看出被接管的请求长什么样：system 几块、共多少字节、有没有
/// billing header / 身份句、几个 tools、`max_tokens`。流水的 `shape` 列记的是**出站**体，
/// 模拟之后已经是官方形态，来访原本的样子只有这里能看到。
pub(super) struct InboundFacts {
    pub(super) system_blocks: usize,
    pub(super) system_bytes: usize,
    pub(super) billing_header: bool,
    pub(super) identity: bool,
    pub(super) tools: usize,
    pub(super) max_tokens: Option<i64>,
}

pub(super) fn inbound_facts(v: &serde_json::Value) -> InboundFacts {
    let texts: Vec<&str> = match v.get("system") {
        Some(serde_json::Value::Array(blocks)) => {
            blocks.iter().filter_map(|b| b.get("text").and_then(|t| t.as_str())).collect()
        }
        Some(serde_json::Value::String(s)) => vec![s.as_str()],
        _ => Vec::new(),
    };
    InboundFacts {
        system_blocks: texts.len(),
        system_bytes: texts.iter().map(|t| t.len()).sum(),
        billing_header: texts.iter().any(|t| t.starts_with("x-anthropic-billing-header:")),
        identity: texts.iter().any(|t| t.contains(config::CC_SYSTEM_IDENTITY_PREFIX)),
        tools: v.get("tools").and_then(|t| t.as_array()).map_or(0, |t| t.len()),
        max_tokens: request_max_tokens(Some(v)),
    }
}

impl Simulation {
    /// 判定 + 派生一次做完：判定见 [`simulates_cc`]（它说不模拟就返回 `None`），这里只负责
    /// 把模拟身份——profile、会话 id、会话链——派生出来。
    pub(super) fn detect(
        body: Option<&serde_json::Value>,
        headers: &HeaderMap,
        from_cc_client: bool,
        flags: store::ForwardFlags,
        cred: &crate::credentials::Credential,
        device_fp: &str,
    ) -> Option<Self> {
        let v = body?;
        let reason = simulation_reason(Some(v), headers, from_cc_client, flags)?;
        let model = v.get("model").and_then(|m| m.as_str()).unwrap_or_default();
        let profile = cc_profile_for(model);
        // 会话 id **优先用来访自己那个**：客户端各开各的会话，全折叠到一个按设备派生的 id
        // 上，就是「一台设备一个会话打了所有请求」——比每请求一个新 id 更假。来访没带才派生。
        // 来访那个按账号钉住（[`account_session_id`]）：同一条会话换号后不该带着同一个 uuid
        // 出现在另一个组织下；与透传路径同一道闸（`spoof_identity`）。
        let session_id = incoming_session_id(headers, Some(v))
            .map(|sid| pin_session_id(cred, sid, flags.spoof_identity))
            .unwrap_or_else(|| session_id_for(cred, device_fp));
        let link = CcSessionLink::load(
            CcSessionKey { cred_id: cred.id, session_id: &session_id },
            crate::telemetry::last_is_new_prompt_body(v),
            profile.has_billing_header(),
        );
        // 判定结果不在这里记：调用点把三条路（模拟/补身份/原样转发）一起打成一条，
        // 只在这儿打的话，「没走模拟」永远是一片空白，反而看不出发生了什么。
        Some(Self { base: cc_system_base(model), profile, session_id, link, reason })
    }
}

/// 这条请求会不会被模拟路径接管——[`Simulation::detect`] 的判定部分，单拎出来是因为
/// **设备指纹要在 detect 之前就知道出站 UA**（[`device_fingerprint`] / [`outbound_ua`]）：
/// 模拟路径整套换头，UA 恒为 [`config::CC_USER_AGENT`]，与来访自报的那串是两台设备。
/// detect 自己也调它，两处判据只有这一份，不会各判各的。
///
/// 返回 `false`（原样转发）的三种情形：开关关着、请求体不是我们能改的 JSON、
/// 来访是 Claude Code 客户端**且体也是完整的 CC 形态**——身份格式合法
/// （[`cc_identity_well_formed`]），且要么是官方额度探测（[`is_quota_probe_shaped`]，官方
/// 唯一一条没有 system 的），要么 `system` 里带着身份声明或 billing header
/// （[`is_cc_shaped`]）、带着官方那段基座提示词（[`has_cc_base_prompt`]；`max_tokens=1` 的
/// 预热与官方 Helper 除外）、工具列表符合 CC 特征（[`has_cc_tool_profile`]——含官方工具名，
/// 或没带 `tools`）。
/// UA 自报 CC 但体不完整（缺那几样中的任一）、或工具列表全是非官方名的请求，都视为第三方
/// 冒用 CC UA，仍走模拟路径重塑成官方形态。
///
/// **依赖 `merge_beta`**：模拟出来的 `anthropic-beta` 要靠它落位并补上 `oauth`，关掉它
/// 就是「system 装成了 CC、头上却没有 oauth beta」的自相矛盾（且上游直接拒）。同
/// [`rewrite_body`] 里 `system_shape` 依赖 `merge_beta` 是一个道理。
pub(super) fn simulates_cc(
    body: Option<&serde_json::Value>,
    headers: &HeaderMap,
    from_cc_client: bool,
    flags: store::ForwardFlags,
) -> bool {
    simulation_reason(body, headers, from_cc_client, flags).is_some()
}

/// [`simulates_cc`] 的本体：不模拟返回 `None`，模拟则给出**第一道**没过的判据
/// （[`SimulationReason`]，按 UA → 身份 → 形态的顺序查，形态内部再按 CC 形态 → 基座 → 工具）。
pub(super) fn simulation_reason(
    body: Option<&serde_json::Value>,
    headers: &HeaderMap,
    from_cc_client: bool,
    flags: store::ForwardFlags,
) -> Option<SimulationReason> {
    if !flags.simulate_cc || !flags.merge_beta {
        return None;
    }
    let v = body?;
    // 真正的 Claude Code 客户端（含 VSCode 扩展、agent-sdk、子代理）**不模拟**：整套换头
    // 会把它自报的 UA 与 `x-app`/`x-stainless-*` 换成抓包那台机器的取值，凭空造出一台
    // 别的机器。身份仍由 [`spoof_identity`] 按原格式改写，这条路只做它自己的事。
    //
    // 「真正的 CC 客户端」要 UA 与体两头都对得上。只对一头的两种都走模拟：
    // - CC 形态（`system` 里有身份声明 / billing header）但 UA 不是 CC（`Go-http-client`、
    //   `python-httpx`……）：抄了 system 却没配套改 UA，这种头体不一致比不模拟更容易被
    //   上游标记，不如一并接管。[`simulate_system`] 里的 [`strip_cc_preamble`] 会剥掉
    //   客户端已有的那份身份声明和 billing header，再由模拟统一补上官方的，避免重复；
    // - UA 是 CC 但体不是 CC 形态：见下。
    //
    // UA 自报 CC、体是 CC 形态、工具列表看起来也像 CC（含官方工具名，或压根没带 tools）
    // → 跳过模拟。差任何一样都当第三方冒用 CC UA 处理，走模拟：
    // - tools 里一个官方名都没有 → 跳过模拟只会让工具全变成 `mcp__luban__*` 却拿不到
    //   模拟路径的整套头/system 配合，上游仍判第三方；
    // - `system` 里既没有身份声明也没有 billing header → 真 CC（含 VSCode 扩展、
    //   agent-sdk、子代理）每条请求都带 billing header 块，没有它的不是官方客户端发的。
    //   封号复盘（`ban.log`）里那批探活请求正是这个样子：`claude-cli/2.1.220` 的 UA、
    //   官方格式的 `metadata.user_id`，配一份 3 块、无 tools、55 token 的自造 system。
    //   照抄了 UA 却没抄形态，透传出去就是一条头体矛盾的请求；模拟接管后 UA、头、
    //   system 全套换成官方的，反而自洽。上面那段「不模拟真 CC」的两处代价对这类
    //   请求已不成立：模拟 UA 取已知最新版（不会倒退），会话 id 优先沿用来访自己那个；
    // - `system` 里有身份声明却**没有基座提示词**（[`has_cc_base_prompt`]）→ 官方只要写了
    //   身份句就一定带基座（主请求 1.2KB–10KB，haiku 工具调用 3KB），只抄身份句不抄基座
    //   的是去掉基座省 token 的第三方。`ban.log` 里 `claude-cli/2.1.165` 那批就是：billing
    //   header + 身份句两块、`tools: []`、每台新设备跑同样四道题。透传出去是一条官方从不
    //   发的形态；模拟接管后补上基座与官方工具，出去的才是完整的官方请求。唯一的例外是
    //   `max_tokens=1` 的 cache 预热：2.1.187 Claude Desktop 的预热就带这两块 system，
    //   给它补基座和工具只会把一条预热改成一条截到 1 token 的主请求，更假。
    //   另一个例外是**官方 Helper 子代理**（[`is_official_helper_request`]）：它整个 system
    //   只有 `[153, 62]` 两块、没有任何长块（`cap/2.1.260/00024`），按基座阈值判会把一条
    //   官方请求送进模拟、给它接上一份主线程基座——那才是形态异常。例外按官方 Helper 的
    //   形态逐项对（两块 system、子代理 billing header、SDK 身份句、haiku + `tools: []` +
    //   `max_tokens=32000` + 流式），不是看到一个 `cc_is_subagent=true` 标记就放。带长
    //   提示词的 SDK 子代理不需要这个例外，它过的是基座阈值。
    //   身份格式也是一头：device 不是 64 位 hex、session 不是 uuid 的（[`cc_identity_well_formed`]）
    //   同样不当官方客户端，走模拟让身份被重建——抄错的值不该到上游，但也不算探针，不拒。
    //   官方**额度探测**（[`is_quota_probe_shaped`]：haiku、`max_tokens=1`、正文 `quota`、
    //   **没有 system 也没有 tools**）过不了 `is_cc_shaped`，得单独放：它是官方形态里唯一
    //   一条没有 system 的，装成主线程（补 system、基座、工具）正是既定要求里禁止的事。
    //   这里曾经漏过一次——把「CC UA 且工具像 CC」收成「还得是 CC 形态」时忘了它。
    // 三样都对上 = 真正的官方客户端，原样转发；差任何一样都由模拟接管。查的顺序即
    // 记进 [`SimulationReason`] 的顺序：UA 不可信时身份与形态怎样都不看了。
    if !from_cc_client {
        return Some(SimulationReason::NotCcClient);
    }
    if !cc_identity_well_formed(headers, v) {
        return Some(SimulationReason::IdentityMalformed);
    }
    if is_quota_probe_shaped(v) {
        return None;
    }
    // 官方 WebSearch 子调用（[`is_official_web_search_request`]）：没有基座、可能连 billing
    // header 都没有，按下面两道判据会落到 `no_base_prompt` / `not_cc_shaped`。它是主线程会话
    // 中途另发的一条，重建成主线程体等于让同一台机器在几秒内以另一个版本、另一台设备、另一
    // 条会话冒出来发一条搜索。放行走透传：头、身份、会话与主线程同源。
    if is_official_web_search_request(v) {
        return None;
    }
    let prewarm = request_max_tokens(Some(v)) == Some(1);
    if !is_cc_shaped(v) {
        // 官方桌面端（`claude-desktop-3p`）的 cache 预热：`max_tokens=1`，system 缺失或只有
        // 一块几百字节的应用块，没有身份句也没有 billing header（`ban/luban-ban-37/38/42`
        // 里 500 多条，主线程请求则全是完整官方形态）。UA 可信、身份合法、tools 空或含官方
        // 工具名的这种预热原样放行，非模拟路径的 [`ensure_cc_system_prefix`] 会补上 billing
        // header 与身份句——与桌面端自己另一种预热形态 `[billing, 身份句]` 一致；此前送进模拟
        // 被重建成带基座与工具的主线程体，出站 UA 也换成了 cli，同一台机器被劈成两台设备。
        // 带长块（不少于基座阈值）的 1 token 请求不在此列：那是第三方的长 system。**要带官方
        // 身份**（`metadata.user_id` 里有 device_id，格式已在上面判过）：复盘里桌面端的预热全带
        // 186 字节的 user_id；一条什么身份都没有的 1 token 请求仍按第三方走模拟。
        if prewarm
            && !has_cc_base_prompt(v)
            && has_cc_tool_profile(v)
            && extract_device_id(Some(v)).is_some()
        {
            return None;
        }
        return Some(SimulationReason::NotCcShaped);
    }
    if !(prewarm
        || is_official_helper_request(v, &inbound_beta_list(headers))
        || has_cc_base_prompt(v))
    {
        return Some(SimulationReason::NoBasePrompt);
    }
    if !has_cc_tool_profile(v) {
        return Some(SimulationReason::ToolsNotCc);
    }
    None
}

/// 来访是否已经是 Claude Code 形态——两条判据命中任一即算是：
///
/// 1. `system` 里包含身份声明前缀 [`config::CC_SYSTEM_IDENTITY_PREFIX`]（主代理 + agent-sdk
///    主进程，写法是 `"You are Claude Code, Anthropic's official CLI for Claude…"`）；
/// 2. `system` 里有以 `x-anthropic-billing-header:` 开头的块（所有 CC 形态——包括子代理
///    explore/search——都带 billing header，即使身份句完全不同）。
///
/// 用 `contains` 而不是 `starts_with`：谁把那句话塞在自己提示词中间，那也是在自称 CC，
/// 再给他前面插一份官方前缀只会得到两句身份声明。`system` 是字符串形态的一并认。
pub(super) fn is_cc_shaped(v: &serde_json::Value) -> bool {
    let texts: Vec<&str> = match v.get("system") {
        Some(serde_json::Value::Array(blocks)) => {
            blocks.iter().filter_map(|b| b.get("text").and_then(|t| t.as_str())).collect()
        }
        Some(serde_json::Value::String(s)) => vec![s.as_str()],
        _ => return false,
    };
    texts.iter().any(|t| t.contains(config::CC_SYSTEM_IDENTITY_PREFIX))
        || texts.iter().any(|t| t.starts_with("x-anthropic-billing-header:"))
}

/// `system` 里是否带着官方那段基座提示词：任一块（或字符串形态的整段）长度不小于
/// [`CC_BASE_PROMPT_MIN_LEN`]。官方带身份句的请求都带基座——opus 主线程 1214 字节、
/// sonnet/haiku 主线程一万多、haiku 工具调用 3059（`cap/` 抓包）；只抄了 billing header 与
/// 身份句、system 总共不到两百字节的，是去掉基座的第三方或探针。
fn has_cc_base_prompt(v: &serde_json::Value) -> bool {
    match v.get("system") {
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .any(|t| t.len() >= CC_BASE_PROMPT_MIN_LEN),
        Some(serde_json::Value::String(s)) => s.len() >= CC_BASE_PROMPT_MIN_LEN,
        _ => false,
    }
}

/// 这条请求是不是**官方 Helper 子代理**（`cap/2.1.260/00024`、`00027`）——官方唯一一种带
/// billing header 却没有任何长块的形态，也是基座要求唯一的子代理例外。逐项对：
///
/// 1. `system` 恰好两块；
/// 2. 第一块是 billing header 且带 `cc_is_subagent=true`；
/// 3. 第二块**逐字**是 [`config::CC_SDK_AGENT_IDENTITY`]（子代理不写 CC 那句身份声明，写这句）；
/// 4. 其余 body 取值与官方 Helper 一致（[`is_official_helper_shape`]：haiku 全名、`tools: []`、
///    `thinking.type=disabled`、`max_tokens=32000`、流式）；
/// 5. 来访 `anthropic-beta` 里带齐 Helper profile 的每一项 beta
///    （[`config::CcProfileKind::HelperSubagentHaiku`]，[`has_profile_betas`]）。
///
/// 曾经只看第 2 条那个标记，于是任何请求在 billing header 里加一句 `cc_is_subagent=true` 就能
/// 免掉基座要求、把一条并不完整的 CC 请求送去透传——已知设备或多轮消息还不命中探针判定。
/// 后来补了 system 与 body，仍没看 beta 头：抄两块 system 加五个字段就够。现在三层都要对，
/// 而三层都抄全了它就**是**一条官方 Helper。带长提示词的 SDK 子代理（`00020`，第三块 29465
/// 字节）不走这里，它过的是基座阈值。
pub(super) fn is_official_helper_request(v: &serde_json::Value, beta: &[String]) -> bool {
    let Some(blocks) = v.get("system").and_then(|s| s.as_array()) else { return false };
    if blocks.len() != 2 {
        return false;
    }
    let text = |i: usize| blocks[i].get("text").and_then(|t| t.as_str()).unwrap_or_default();
    text(0).starts_with("x-anthropic-billing-header:")
        && text(0).contains("cc_is_subagent=true")
        && text(1) == config::CC_SDK_AGENT_IDENTITY
        && is_official_helper_shape(v)
        && has_profile_betas(beta, config::CcProfileKind::HelperSubagentHaiku)
}

/// 来访的 `anthropic-beta` 是否带齐了某个官方 profile 的**每一项** beta（多带不算错——
/// 来访那串还有 `oauth`/`afk-mode` 之类 profile 表里刻意去掉的项）。
fn has_profile_betas(beta: &[String], kind: config::CcProfileKind) -> bool {
    config::cc_profile(kind)
        .beta
        .split(',')
        .map(str::trim)
        .filter(|b| !b.is_empty())
        .all(|b| has_beta(beta, b))
}

/// [`has_cc_base_prompt`] 的阈值：1000 字节。官方最短的基座是 opus 那份 1214 字节，留两成
/// 余量；仿冒者那两块加起来不到两百字节，中间空得很宽。
pub(super) const CC_BASE_PROMPT_MIN_LEN: usize = 1000;

/// 这条请求是不是**官方 WebSearch 子调用**。CC 的 WebSearch 工具不在主线程里直接调 server
/// tool，而是另发一条请求：一条用户消息（要搜的问题）、`tools` 只有 `web_search_*` 这一个
/// server tool、`tool_choice` 强制它、`system` 只有一句「You are an assistant for performing a
/// web search tool use」（57 字节，前面可能还有一块 billing header）。它没有基座、没有身份句，
/// 按 [`simulation_reason`] 的判据会落到 `no_base_prompt`（带 billing header）或
/// `not_cc_shaped`（不带），被重建成带基座与四个官方工具的主线程体，出站 UA 换成模拟那版、
/// device_id 与会话 id 也换成模拟派生的——一条 `claude-cli/2.1.220` 的主线程会话中途冒出一台
/// 2.1.260 的新设备发了一条搜索（`ban/luban-ban-13`、`ban-14` 各 2、3 条，`ban-37` 3 条；出站
/// system 是 `[billing, 身份, 基座, 57 字节]`，末块正是那句搜索助手提示）。放行后走透传路径：
/// 头、身份、会话与主线程同源；没带 billing header 的由 [`ensure_cc_system_prefix`] 补前缀，
/// server tool 在工具改名那步本来就按原名保留。
///
/// 逐项对，缺一不算：
/// 1. `tools` 恰好一个，`type` 以 `web_search_` 开头且 `name` 是 `web_search`；
/// 2. `tool_choice` 是 `{"type":"tool","name":"web_search"}`；
/// 3. `messages` 恰好一条且是用户消息；
/// 4. `system` 去掉 billing header 后恰好一块，不到 [`CC_BASE_PROMPT_MIN_LEN`] 字节、不含 CC
///    身份句、且提到 web search（字符串形态的 `system` 一并认）。
///
/// 第三方要冒充得把这四项全抄对，而抄全了它就**是**一条 WebSearch 子调用——透传出去的形态
/// 与官方无异，比重建成主线程体更接近真实。
fn is_official_web_search_request(v: &serde_json::Value) -> bool {
    let Some(tools) = v.get("tools").and_then(|t| t.as_array()) else { return false };
    let [tool] = tools.as_slice() else { return false };
    let ty = tool.get("type").and_then(|t| t.as_str()).unwrap_or_default();
    let name = tool.get("name").and_then(|n| n.as_str()).unwrap_or_default();
    if !ty.starts_with("web_search_") || name != "web_search" {
        return false;
    }
    let Some(choice) = v.get("tool_choice") else { return false };
    if choice.get("type").and_then(|t| t.as_str()) != Some("tool")
        || choice.get("name").and_then(|n| n.as_str()) != Some("web_search")
    {
        return false;
    }
    let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) else { return false };
    let [msg] = msgs.as_slice() else { return false };
    if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
        return false;
    }
    let texts: Vec<&str> = match v.get("system") {
        Some(serde_json::Value::Array(blocks)) => {
            blocks.iter().filter_map(|b| b.get("text").and_then(|t| t.as_str())).collect()
        }
        Some(serde_json::Value::String(s)) => vec![s.as_str()],
        _ => return false,
    };
    let mut prompts = texts.iter().filter(|t| !t.starts_with("x-anthropic-billing-header:"));
    let (Some(prompt), None) = (prompts.next(), prompts.next()) else { return false };
    prompt.len() < CC_BASE_PROMPT_MIN_LEN
        && !prompt.contains(config::CC_SYSTEM_IDENTITY_PREFIX)
        && prompt.to_ascii_lowercase().contains("web search")
}

/// `tools` 列表是否看起来像真正的 CC 客户端：没有 `tools`（count_tokens 等场景）算是，
/// 有 `tools` 但里面至少有一个 [`config::CC_TOOL_NAMES`] 里的官方工具名也算是。
/// **有 tools 却一个官方名都找不到**说明 UA 是第三方中转冒用的。
fn has_cc_tool_profile(v: &serde_json::Value) -> bool {
    let Some(tools) = v.get("tools").and_then(|t| t.as_array()) else {
        return true; // 没有 tools 字段不是判据
    };
    if tools.is_empty() {
        return true;
    }
    tools.iter().any(|t| {
        let name = t.get("name").and_then(|n| n.as_str()).unwrap_or_default();
        config::CC_TOOL_NAMES.contains(&name)
    })
}

/// 按模型族选官方基座。三族各有各的基座（`cap/2.1.258` 五份对话抓包验证）：
///
/// | 模型 | 基座 | 大小 | 来源 |
/// |---|---|---|---|
/// | opus-5 / fable-5-1 | [`config::CC_SYSTEM_BASE_OPUS`] | 1214B | 00012/00013/00025 |
/// | sonnet-5 | [`config::CC_SYSTEM_BASE_SONNET`] | 10520B | 00026 |
/// | haiku-4.5 / opus-4-6[1m] | [`config::CC_SYSTEM_BASE_HAIKU`] | 10622B | 00031（opus-4-6 沿用 2.1.251 的映射） |
///
/// 认不出的模型返回 `None`，只注入身份句。
pub(super) fn cc_system_base(model: &str) -> Option<&'static str> {
    let m = model.to_ascii_lowercase();
    if m.contains("opus-5") || m.contains("fable") {
        Some(config::CC_SYSTEM_BASE_OPUS)
    } else if m.contains("sonnet") {
        Some(config::CC_SYSTEM_BASE_SONNET)
    } else if m.contains("haiku") || m.contains("opus") {
        Some(config::CC_SYSTEM_BASE_HAIKU)
    } else {
        None
    }
}

/// 按模型族选 2.1.260 的**主线程** profile。
///
/// 模拟路径只造主线程形态：来访是第三方客户端，它的用途 luban 猜不出来，而主线程是唯一
/// 一个「带工具、带 system、会多轮」的通用形态。把一条第三方请求装成 SDK 子代理
/// （`cc_is_subagent=true`）或标题生成（`output_config.format=json_schema`）都会给上游一个
/// 与请求内容对不上的用途标记，比装成主线程更假。
///
/// 例外是 luban 自己发的额度探测：那条本来就是官方 `QuotaProbe` 那个形态，由
/// [`probe_simulation`] 直接指定 profile，不走这里。
///
/// 四族的差异见 [`config::CC_PROFILES`]；认不出的模型退回 sonnet 那份（与 2.1.258 时
/// [`config::cc_beta_order_is_not_a_table`] 记的口径一致）。
pub(super) fn cc_profile_for(model: &str) -> &'static config::CcProfile {
    config::cc_profile(cc_profile_kind_for(model))
}

/// 模型名 → 主线程 profile 的 kind。与版本无关，故 [`merge_beta`] 也用它（再按来访自报的
/// 版本经 [`config::cc_profile_at`] 取对应那一版的行）。
pub(super) fn cc_profile_kind_for(model: &str) -> config::CcProfileKind {
    let m = model.to_ascii_lowercase();
    if m.contains("haiku") {
        config::CcProfileKind::MainHaiku
    } else if m.contains("fable") {
        config::CcProfileKind::MainFable
    } else if m.contains("opus") {
        config::CcProfileKind::MainOpus
    } else {
        config::CcProfileKind::MainSonnet
    }
}

/// 官方 Helper / 标题生成两条 haiku 请求共有的 **body 取值**（`cap/2.1.260/00024`、`00027`、
/// `2.1.260-2/00058`）：模型是 haiku 4.5 全名、`tools` 字段**存在且为空数组**、`thinking.type`
/// 是 `disabled`、`max_tokens` 恰为 32000、`stream` 为 true。五项齐了才算。
///
/// 每一项都在堵一个绕法：`thinking: {}` 或 `enabled` 不是这两种请求的写法（带 `enabled` 的
/// 只有 SDK 子代理，它带工具）；`tools` 缺失或 `null` 不是官方写法；`max_tokens` 只认 32000
/// 而不是「不小于某数」；模型换成 opus/sonnet 的不是 Helper——`ban.log` 里 `claude-cli/2.1.165`
/// 那批（opus-5、`max_tokens=10240`、`tools: []`、每台新设备同样四道题）就是被上一版「thinking
/// 对象 + max_tokens >= 4096」的宽豁免放过去的。
///
/// **只是 body 那一半**：单独用它放行不够（抄五个字段就够了），要配上 system 结构与 beta 头，
/// 见 [`is_official_helper_request`] 与 [`is_official_title_request`]。
fn is_official_helper_shape(v: &serde_json::Value) -> bool {
    v.get("model").and_then(|m| m.as_str()) == Some(QUOTA_PROBE_MODEL)
        && v.get("tools").is_some_and(|t| t.as_array().is_some_and(|a| a.is_empty()))
        && thinking_type(v) == Some("disabled")
        && request_max_tokens(Some(v)) == Some(32000)
        && v.get("stream").and_then(|b| b.as_bool()) == Some(true)
}

/// 官方**标题生成**（`cap/2.1.260-2/00058`）的完整样子：[`is_official_helper_shape`] 的 body
/// 取值，加 system 恰好三块——billing header、CC 身份句、不短于 1000 字节的标题提示词
/// （抓包 3059）——加 `structured-outputs` beta。
///
/// 它与 Helper 是两种请求：Helper 是子代理（SDK 身份句、两块），标题生成是主线程身份（CC
/// 身份句、带长提示词）。两条都在第三条判据的豁免里，见 [`probe_signature`] 里标题生成为什么
/// 必须豁免。
pub(super) fn is_official_title_request(v: &serde_json::Value, beta: &[String]) -> bool {
    let Some(blocks) = v.get("system").and_then(|s| s.as_array()) else { return false };
    if blocks.len() != 3 {
        return false;
    }
    let text = |i: usize| blocks[i].get("text").and_then(|t| t.as_str()).unwrap_or_default();
    text(0).starts_with("x-anthropic-billing-header:")
        && text(1) == config::CC_SYSTEM_IDENTITY
        && text(2).len() >= CC_BASE_PROMPT_MIN_LEN
        && has_beta(beta, config::CC_BETA_STRUCTURED_OUTPUTS)
        && is_official_helper_shape(v)
}

/// 官方**安全分类**（`cap/2.1.260/00019`、`00030`）的完整样子：
///
/// - system 恰好三块：billing header、不短于 1000 字节的分类提示词（抓包 123785）、以
///   `## Session Context` 开头的会话上下文块（抓包 248 字节）；
/// - body：`max_tokens` 恰为 64、`stop_sequences` 是非空且不含空串的字符串数组、
///   `thinking.type` 是 `disabled`、没有 `tools` 字段、非流式；
/// - 头：带 `auto-mode-classifier` beta。
///
/// 曾经只看第一块是 billing header，两块 system 就能装成分类请求；三块的结构与各块内容现在
/// 都要对上。
pub(super) fn is_official_classifier_request(v: &serde_json::Value, beta: &[String]) -> bool {
    let Some(blocks) = v.get("system").and_then(|s| s.as_array()) else { return false };
    if blocks.len() != 3 {
        return false;
    }
    let text = |i: usize| blocks[i].get("text").and_then(|t| t.as_str()).unwrap_or_default();
    let stop_ok = v.get("stop_sequences").and_then(|s| s.as_array()).is_some_and(|a| {
        !a.is_empty() && a.iter().all(|x| x.as_str().is_some_and(|t| !t.is_empty()))
    });
    text(0).starts_with("x-anthropic-billing-header:")
        && text(1).len() >= CC_BASE_PROMPT_MIN_LEN
        && text(2).trim_start().starts_with("## Session Context")
        && request_max_tokens(Some(v)) == Some(64)
        && stop_ok
        && thinking_type(v) == Some("disabled")
        && v.get("tools").is_none()
        && v.get("stream").and_then(|b| b.as_bool()) != Some(true)
        && has_beta(beta, config::CC_BETA_AUTO_MODE_CLASSIFIER)
}

/// `thinking.type` 的字串值；`thinking` 不是对象、没有 `type`、或 `type` 不是字串时 `None`。
/// `{}`、`null`、字串形态的 `thinking` 都落到 `None`。
fn thinking_type(v: &serde_json::Value) -> Option<&str> {
    v.get("thinking")?.get("type")?.as_str()
}

/// 来访的 CC 身份是否**格式合法**：`metadata.user_id` 按 CC 格式解析出的 device 段（若有）
/// 是 64 位小写 hex、session 段（若有）是 uuid，`X-Claude-Code-Session-Id` 头（若带了、非空）
/// 也是 uuid。三处都没带算合法——「没带」由 [`bare_session_id`] 那条路补，不是这里的事。
///
/// 是 [`Simulation::detect`] 放行透传的前提之一。官方两处身份恒为 64 位 hex + uuid（`cap/`
/// 37 份样本无一例外）；按 CC 格式写却写错的（`ban.log` 里 `device_id=channel-test`、
/// `session_id=channel-test-claude-code` 那批探活），不当官方客户端，走模拟——那条路会剥掉这份
/// 身份、用凭证 + 平台指纹重建合法的一份，抄错的值到不了上游。
///
/// **头上那个原样看、不过滤**：[`incoming_session_id`] 会把不是 uuid 的头丢掉（给会话链、限流
/// 用的值必须合法），拿它的结果来判就永远判不到非法头。这里直接读原值。
///
/// **内嵌 JSON 里字段的类型也算格式**：`"session_id": null` / 数字 / 对象，在 [`extract_session_id`]
/// 眼里是「没带」（它只认字串），但官方这两个字段恒为字串——写了这个键却不是字串，同样是
/// 抄错了；放它透传的话 `spoof_identity` 只换 device 与 account 段，那个 `null` 会原样留在出站的
/// `user_id` 里。故这里另看一眼原始 JSON：键在，就必须是非空字串。
pub(super) fn cc_identity_well_formed(headers: &HeaderMap, v: &serde_json::Value) -> bool {
    let header_ok = headers
        .get("x-claude-code-session-id")
        .and_then(|h| h.to_str().ok())
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .is_none_or(looks_like_uuid);
    header_ok
        && cc_identity_fields_are_strings(v)
        && extract_device_id(Some(v)).is_none_or(|d| is_hex64(&d))
        && extract_session_id(Some(v)).is_none_or(|s| looks_like_uuid(&s))
}

/// `metadata.user_id` 若是 CC 的内嵌 JSON 形态，其中出现的 `device_id` / `session_id` 键都得是
/// **没有首尾空白的非空字串**。不是内嵌 JSON（扁平串、或随便一个串）、或压根没有
/// `metadata.user_id`，算通过——那两种由 [`extract_device_id`] / [`extract_session_id`] 自己的
/// 解析规则管。
///
/// 空白也算格式：[`extract_session_id`] 会 trim（给会话链用的值得干净），于是 `" <uuid> "`
/// 在它眼里是合法 uuid；但官方从不给这两个字段加空白，写了就是抄错了，放透传的话原串里那两个
/// 空格会留在出站 `user_id` 里、与头上的值不再逐字相同。
fn cc_identity_fields_are_strings(v: &serde_json::Value) -> bool {
    let Some(user_id) = v.get("metadata").and_then(|m| m.get("user_id")).and_then(|u| u.as_str())
    else {
        return true;
    };
    let Ok(inner) = serde_json::from_str::<serde_json::Value>(user_id) else { return true };
    let Some(obj) = inner.as_object() else { return true };
    ["device_id", "session_id"].iter().all(|k| {
        obj.get(*k).is_none_or(|val| val.as_str().is_some_and(|s| !s.is_empty() && s == s.trim()))
    })
}

/// 字段「等于没有」：缺失、`null`、空数组。给 [`probe_signature`] 用——探针加一个 `tools: []`
/// 不该算成「带了 tools」。
pub(super) fn field_is_empty(v: Option<&serde_json::Value>) -> bool {
    match v {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::Array(a)) => a.is_empty(),
        Some(_) => false,
    }
}

/// system 里包含 CC 身份句（[`config::CC_SYSTEM_IDENTITY_PREFIX`]）的块数。字符串形态的
/// system 按一块算。
pub(super) fn cc_identity_blocks(v: &serde_json::Value) -> usize {
    match v.get("system") {
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .filter(|t| t.contains(config::CC_SYSTEM_IDENTITY_PREFIX))
            .count(),
        Some(serde_json::Value::String(s)) => {
            usize::from(s.contains(config::CC_SYSTEM_IDENTITY_PREFIX))
        }
        _ => 0,
    }
}

/// 64 位小写 hex——官方 CC 的 `device_id`（`sha256` 十六进制）恒为此形。
fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// 形如 `8-4-4-4-12` 的小写 hex uuid。只看形状，不校验 version/variant 位——官方发的是
/// v4，但一个 v7 的 uuid 同样是个正常的会话 id，没有理由拦。
pub(super) fn looks_like_uuid(s: &str) -> bool {
    let groups = [8usize, 4, 4, 4, 12];
    let mut parts = s.split('-');
    for want in groups {
        let Some(p) = parts.next() else { return false };
        if p.len() != want || !p.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            return false;
        }
    }
    parts.next().is_none()
}

/// 一次请求最多 4 个缓存断点（`cache_control`），超了上游整条拒。
/// 官方自己用掉 3 个（基座、其余、末条消息），故模拟时得数着加，见 [`simulate_system`]。
pub(super) const MAX_CACHE_BREAKPOINTS: usize = 4;

/// 官方 `system` **最多 5 块**：2.1.258 的 fable-5-1（`cap/2.1.258/00013`）是
/// `[billing, 身份句, reporting, 基座, 其余]`，opus-5 / sonnet-5 / haiku 是去掉 reporting 的
/// 4 块（2.1.251 时四族都是 5 块）；API-key 模式那三份是 3 块合并态（见 [`align_system_shape`]）。
///
/// 块数超了就不再是 CC 形态，上游按第三方应用计费，客户端会看到
/// `Third-party apps now draw from your extra usage, not your plan limits.`
/// ——请求照样有回复，只是从订阅额度转到了超额池。故对齐它是**计费正确性**问题，
/// 不只是形态好看：见 [`cap_system_blocks`] 与 [`merge_system_blocks`]。
const MAX_SYSTEM_BLOCKS: usize = 5;

/// 把非 CC 请求的 `system` 换成官方形态（2.1.260，fable 族五块、其余四块）：
///
/// ```text
/// [0] x-anthropic-billing-header: …            无断点（cch 由 ensure_billing_cch 补）
/// [1] You are Claude Code, …（57B）            无断点
/// [2] # Reporting outcomes …（907B）            无断点，**只有 fable 族有**（CcSystemShape）
/// [·] 官方基座（按模型族）                      {ephemeral, ttl:1h, scope:global}
/// [·] 客户端自己的 system（并成一块）           {ephemeral, ttl:1h}
/// ```
///
/// 块数与断点位置在 2.1.258 → 2.1.260 之间没变（`cap/2.1.260-2/00025` 对 opus、
/// `cap/2.1.260/00018` 对 fable，逐块比对）。
///
/// 客户端的 `system` 是字符串就裹成一个文本块，是数组就并成一块（见
/// [`merge_system_blocks`]），没有就没有末块。
///
/// **客户端那堆块必须并成一块**：官方末块就是「基座之后的全部内容」拼成的一大段，
/// 客户端自己拆成 N 块发过来，照搬就会得到 4+N 块——超过 [`MAX_SYSTEM_BLOCKS`]
/// 即被上游判为第三方应用、改扣超额池。
///
/// **断点是数着加的**：客户端可能自己就用满了 4 个（比如给每条工具定义都标了缓存），这时
/// 再加就会让整条请求被上游拒——那是把「形态更像」换成「根本发不出去」。预算不够时基座与
/// 末块照发，只是不带断点（少一次缓存复用，不影响正确性）。预算在**合并之后**才算：
/// 合并会消掉客户端 `system` 里那几个断点，先算就是按一个已经不存在的数字克扣基座。
pub(super) fn simulate_system(
    v: &mut serde_json::Value,
    sim: &Simulation,
    cache: CacheShape,
) -> bool {
    // 官方就不发 `system` 的 profile（额度探测）：一个字节都不加。给它补 billing header
    // 与基座，等于把一条 `max_tokens:1` 的探测装成了主线程请求。
    if sim.profile.system == config::CcSystemShape::None {
        return false;
    }
    let client: Vec<serde_json::Value> = match v.get("system") {
        Some(serde_json::Value::String(s)) if !s.trim().is_empty() => {
            vec![text_block_bare(s)]
        }
        Some(serde_json::Value::Array(a)) => merge_system_blocks(a.clone()),
        _ => Vec::new(),
    };
    // 客户端可能已经抄了官方的 billing header 和身份声明（`is_cc_shaped` 不再拦截非 CC
    // 客户端的这种请求）。模拟会重新补齐这两块，先剥掉客户端那份以免重复。
    let client = strip_cc_preamble(client);
    // system 之外的断点（tools、messages）+ 合并后的客户端断点，才是本条请求已占的数目。
    let outside = count_cache_control(v) - v.get("system").map(count_cache_control).unwrap_or(0);
    let used = outside + client.iter().map(count_cache_control).sum::<usize>();
    let mut budget = MAX_CACHE_BREAKPOINTS.saturating_sub(used);

    let mut blocks = vec![
        text_block_bare(&simulated_billing_header_text(sim)),
        text_block_bare(config::CC_SYSTEM_IDENTITY),
    ];
    if sim.profile.system == config::CcSystemShape::IdentityReporting {
        blocks.push(text_block_bare(config::CC_SYSTEM_REPORTING));
    }
    if let Some(base) = sim.base {
        if budget > 0 {
            budget -= 1;
            blocks.push(text_block(base, cache_control(cache)));
        } else {
            blocks.push(text_block_bare(base));
        }
    }
    // 末块补断点：官方在 system 末尾必有一个，但客户端自己标过就不重复标。
    let tail_open = client.last().is_some_and(|b| b.get("cache_control").is_none());
    blocks.extend(client);
    if tail_open
        && budget > 0
        && let Some(last) = blocks.last_mut().and_then(|b| b.as_object_mut())
    {
        last.insert("cache_control".into(), cache_control(cache.tail()));
    }

    insert_top_level(v, "system", serde_json::Value::Array(blocks), &["messages", "model"]);
    true
}

/// 模拟后 system 末块（客户端自有内容）超过此字符数时，移到 messages 首条用户消息里。
///
/// 上游对末块有内容级检测：非 CC 特征内容超过 ~2000 字符即触发第三方判定。
/// 实测 1900 字符安全、2021 字符触发，取 1500 留足余量。
const MAX_CLIENT_SYSTEM_CHARS: usize = 1500;

/// 把模拟后 system 末块（客户端自有内容）的超长内容搬到 messages 首条用户消息里。
///
/// 搬走后末块换成一行短占位（保持块数形态），内容作为 `<system_instructions>` 标签
/// 注入到 messages[0] 的第一个 content 块前面。messages[0] 必须是 user role（API 约束），
/// 官方 CC 也恒为 user 开头，正常情况下不会踩空。
///
/// **要么整个搬成，要么一个字节都不动。** 先确认 `messages[0].content` 是可写的形态，
/// 再去动 `system`——反过来的话，落点不可写时（`content` 缺失、是数字、messages 为空）
/// 就会得到「末块已经换成 `(see conversation)` 占位、内容却没搬到任何地方」的请求：
/// 客户端明确下的那段指令**凭空消失**，而调用方只看到一个 `false`，以为什么都没发生。
pub(super) fn relocate_long_client_system(v: &mut serde_json::Value, sim: &Simulation) -> bool {
    // 模拟产出的固定块数：billing + 身份句 (+ reporting) (+ 基座)。多出来的那一块才是客户端
    // 自己的 system；块数不多于它就没有可搬的东西。
    let reporting = sim.profile.system == config::CcSystemShape::IdentityReporting;
    let fixed = 2 + usize::from(reporting) + usize::from(sim.base.is_some());
    let sys = match v.get("system").and_then(|s| s.as_array()) {
        Some(a) if a.len() > fixed => a,
        _ => return false,
    };
    let last = sys.len() - 1;
    let tail_text = match sys[last].get("text").and_then(|t| t.as_str()) {
        Some(t) if t.len() > MAX_CLIENT_SYSTEM_CHARS => t.to_string(),
        _ => return false,
    };
    // 先探路：落点不可写就原地返回，`system` 还没被动过。
    let writable = v
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|m| m.first())
        .and_then(|f| f.get("content"))
        .is_some_and(|c| c.is_array() || c.is_string());
    if !writable {
        tracing::warn!(
            chars = tail_text.len(),
            "messages[0].content is not writable, leaving the long client system in place"
        );
        return false;
    }
    let wrapped = format!("<system_instructions>\n{tail_text}\n</system_instructions>");
    // 走到这里两步都必定成功：上面刚验过 `content` 是数组或字符串。
    if let Some(first) =
        v.get_mut("messages").and_then(|m| m.as_array_mut()).and_then(|m| m.first_mut())
    {
        match first.get_mut("content") {
            Some(serde_json::Value::Array(arr)) => {
                arr.insert(0, serde_json::json!({"type": "text", "text": wrapped}));
            }
            Some(content @ serde_json::Value::String(_)) => {
                let s = content.as_str().unwrap_or_default();
                *content = serde_json::Value::String(format!("{wrapped}\n\n{s}"));
            }
            _ => return false,
        }
    }
    if let Some(blocks) = v.get_mut("system").and_then(|s| s.as_array_mut()) {
        let cc = blocks[last].get("cache_control").cloned();
        let mut placeholder = text_block_bare("(see conversation)");
        if let Some(cc) = cc
            && let Some(o) = placeholder.as_object_mut()
        {
            o.insert("cache_control".into(), cc);
        }
        blocks[last] = placeholder;
    }
    tracing::info!(
        chars = tail_text.len(),
        last,
        "relocated long client system from last block to messages[0]"
    );
    true
}

/// 把一串 `system` 文本块并成**一块**，正文用 `\n\n` 相连。
///
/// **为什么是拼而不是丢**：官方末块本身就是「基座之后的全部内容」拼成的一大段
/// （`cap/raw/00006` 里 12KB 一块），客户端把同样的内容拆成几块发过来，拼回去正是还原
/// 官方的切法——一个字都不少，只是不再各自成块。
///
/// **断点取最后一个**：合并后是连续的一段，末尾那个断点覆盖它前面的全部前缀，缓存语义与
/// 合并前的最后一个断点等价；中间那几个断点没有了，少几次缓存复用，不影响正确性。
///
/// 出现不是文本块的成员（`text` 不是字符串）就**原样交回**——`system` 里只能放文本块，
/// 别的东西是我们不认识的形态，宁可照发也不猜着改。正文全空的一并丢掉：发一个空文本块
/// 既没意义，上游也不收。
fn merge_system_blocks(blocks: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    if blocks.len() <= 1 {
        return blocks;
    }
    let texts: Vec<&str> = blocks.iter().filter_map(|b| b.get("text")?.as_str()).collect();
    if texts.len() != blocks.len() {
        return blocks;
    }
    let text = texts.into_iter().filter(|t| !t.trim().is_empty()).collect::<Vec<_>>().join("\n\n");
    if text.is_empty() {
        return Vec::new();
    }
    let cc = blocks.iter().rev().find_map(|b| b.get("cache_control").cloned());
    vec![match cc {
        Some(cc) => text_block(&text, cc),
        None => text_block_bare(&text),
    }]
}

/// 剥掉客户端 `system` 块里已有的 CC 前缀：`x-anthropic-billing-header` 和
/// [`config::CC_SYSTEM_IDENTITY`]。[`simulate_system`] 会重新补上官方的这两块，不剥就
/// 会重复——两句身份声明比缺了更显眼。
///
/// 两种情况：
/// - 独立块（未合并）：整块 `==` 那句话、或 `starts_with` billing header，整块丢掉。
/// - 合并块（经 [`merge_system_blocks`]）：identity / billing header 嵌在一大段文本里，
///   按子串剥掉再 trim 多余的换行。
fn strip_cc_preamble(mut blocks: Vec<serde_json::Value>) -> Vec<serde_json::Value> {
    // 先丢掉整块命中的。
    blocks.retain(|b| {
        let Some(text) = b.get("text").and_then(|t| t.as_str()) else {
            return true;
        };
        let trimmed = text.trim();
        if trimmed == config::CC_SYSTEM_IDENTITY {
            return false;
        }
        if trimmed.starts_with("x-anthropic-billing-header:") && !trimmed.contains("\n\n") {
            return false;
        }
        true
    });
    // 处理合并后嵌在一大段文本里的情况：子串级剥。
    for b in &mut blocks {
        let Some(text) = b.get("text").and_then(|t| t.as_str()) else {
            continue;
        };
        if !text.contains(config::CC_SYSTEM_IDENTITY)
            && !text.contains("x-anthropic-billing-header:")
        {
            continue;
        }
        let mut s = text.to_string();
        // 剥 billing header：它只占一行。
        if let Some(start) = s.find("x-anthropic-billing-header:") {
            let end = s[start..].find('\n').map(|i| start + i + 1).unwrap_or(s.len());
            s.replace_range(start..end, "");
        }
        s = s.replace(config::CC_SYSTEM_IDENTITY, "");
        // 连续空行归一。
        while s.contains("\n\n\n") {
            s = s.replace("\n\n\n", "\n\n");
        }
        let trimmed = s.trim().to_string();
        if trimmed.is_empty() {
            b.as_object_mut()
                .map(|o| o.insert("text".into(), serde_json::Value::String(String::new())));
        } else {
            b.as_object_mut().map(|o| o.insert("text".into(), serde_json::Value::String(trimmed)));
        }
    }
    // 剥完文本变空的块也丢掉。
    blocks.retain(|b| b.get("text").and_then(|t| t.as_str()).is_none_or(|t| !t.trim().is_empty()));
    blocks
}

/// 把 `system` 压回 [`MAX_SYSTEM_BLOCKS`] 块：超出部分并进末块
/// （见 [`merge_system_blocks`]）。
///
/// 兜住任何在我们之前就把 `system` 拆碎了的中间层：块数超限照样按第三方额度扣。
///
/// 已经不超过限制（含官方的 5 块与 API-key 的 3 块）时不动结构、返回 `false`。
pub(super) fn cap_system_blocks(v: &mut serde_json::Value) -> bool {
    let Some(sys) = v.get_mut("system").and_then(|s| s.as_array_mut()) else {
        return false;
    };
    if sys.len() <= MAX_SYSTEM_BLOCKS {
        return false;
    }
    let tail = sys.split_off(MAX_SYSTEM_BLOCKS - 1);
    let before = tail.len();
    let merged = merge_system_blocks(tail);
    let changed = merged.len() < before;
    sys.extend(merged);
    changed
}

/// `system[0]` 那条 billing header 的正文，给**真实 CC 客户端**缺 billing header 时补用
/// （[`ensure_cc_system_prefix`]）。`cch` 不在这里补——那是 [`ensure_billing_cch`] 的活。
///
/// `cc_version` 的第四段（如 `76b`）由 [`cc_version_suffix`] 从请求 body 动态派生，
/// 算法逆向自 2.1.251：取第一条用户消息 text 的第 4/7/20 位字符，拼上固定 salt 与
/// 主版本号后 SHA-256 取前 3 个 hex 字符。模拟路径不走这条，见
/// [`simulated_billing_header_text`]。
///
/// **主版本取来访自报的那个**（`version`，由调用方从 UA 里解出），不是
/// [`config::CC_VERSION_BASE`]：给一个 UA 写着 2.1.258 的来访补一条 `cc_version=2.1.260.…`
/// 的 billing header，就是把两个版本混进了同一条请求。解不出版本（UA 缺失或不是
/// `claude-cli/x.y.z` 形态）才退回 luban 自己那个。
pub(super) fn billing_header_text(
    v: &serde_json::Value,
    version: Option<&str>,
    kind: CcRequestKind,
) -> String {
    let version = version.unwrap_or(config::CC_VERSION_BASE);
    // **2.1.260 起用 profile 的固定后缀**，派生算法只留给更老的版本。
    //
    // 那套算法逆向自 2.1.251，在 2.1.260 上**已被证否**：六个 profile 各有固定后缀
    // （`222`/`bcd`/`660`/`d95`/`ced`/`3de`），而算法对主线程样本算出的是 `11d`。给一个
    // 2.1.260 的来访补一个算出来的后缀，等于发一个上游从没在这个版本上见过的值。
    let model = v.get("model").and_then(|m| m.as_str()).unwrap_or_default();
    let suffix = match kind.billing_suffix_at(version, model) {
        Some(fixed) => fixed.to_string(),
        None => cc_version_suffix(v, version),
    };
    format!("x-anthropic-billing-header: cc_version={version}.{suffix}; cc_entrypoint=cli;")
}

/// 模拟路径的 billing header 正文，整条由 profile 与会话链条拼出。官方形态
/// （`cap/2.1.260-2/00059`，分号后各有一个空格、末尾也有分号）：
///
/// ```text
/// x-anthropic-billing-header: cc_version=2.1.260.222; cc_entrypoint=cli; cch=f850a;
///   cc_prev_req=req_011CeiBW8Yx9A2uzWiCBsJsU; cc_prompt_id=16d7a19d-…;
/// ```
///
/// 各段的顺序是抓包序，六个 profile 一致：`cc_version` → `cc_entrypoint` → `cch` →
/// `cc_is_subagent` → `cc_prev_req` → `cc_prompt_id`。`cch` 在这里就一次写好，不再等
/// [`ensure_billing_cch`] 事后追加——那个函数只管给**真实 CC 来访**缺的那条补。
///
/// 第四段不再走 [`cc_version_suffix`] 那套派生算法：2.1.260 的六个 profile 各有各的固定
/// 后缀（`222`/`bcd`/`660`/`d95`/`ced`/`3de`），派生算法对主线程样本算出的是 `11d`，
/// 与哪一个都对不上。
fn simulated_billing_header_text(sim: &Simulation) -> String {
    let p = sim.profile;
    let mut s = format!(
        "x-anthropic-billing-header: cc_version={}.{}; cc_entrypoint=cli; cch={};",
        p.version,
        p.billing_suffix,
        cch_value(),
    );
    if p.subagent {
        s.push_str(" cc_is_subagent=true;");
    }
    if let Some(prev) = &sim.link.prev_req {
        s.push_str(&format!(" cc_prev_req={prev};"));
    }
    if let Some(pid) = &sim.link.prompt_id {
        s.push_str(&format!(" cc_prompt_id={pid};"));
    }
    s
}

/// 官方 `cc_version` 第四段的派生算法（逆向自 claude-cli/2.1.251，2.1.258 未复核；模拟路径
/// 已改为写死，只剩 [`billing_header_text`] 在用）。
///
/// ```text
/// salt    = "59cf53e54c78"
/// chars   = text[4] || text[7] || text[20]   （越界用 "0"）
/// suffix  = sha256(salt + chars + VERSION_BASE).hex()[0..3]
/// ```
///
/// `text` 取的是 `messages` 里**第一条 `role:"user"` 消息**的**第一个 `type:"text"` 块**
/// 的文本。官方客户端内部会跳过 `isMeta` 消息，但在实际请求 body 里这等价于第一条 user
/// 消息的第一个 text 块（harness 注入的 system-reminder 也在同一个 user turn 的 content
/// 数组里，排在用户实际输入之前）。
///
/// `version` 参与摘要，故它必须是**这条请求自报的**版本，与 `cc_version` 前三段同值。
///
/// 这套算法在 2.1.260 上**已被证否**：六个 profile 的后缀是固定的
/// （`222`/`bcd`/`660`/`d95`/`ced`/`3de`，见 [`config::CC_PROFILES`]），而算法对主线程样本
/// 算出 `11d`。它只剩「给 2.1.251 一代的来访补一个形状对的值」这一个用途，比不补强，
/// 但别再拿它去解释 2.1.260 的抓包。
pub(super) fn cc_version_suffix(v: &serde_json::Value, version: &str) -> String {
    let text = first_user_text(v);
    let char_at = |i: usize| text.chars().nth(i).unwrap_or('0');
    let chars: String = [char_at(4), char_at(7), char_at(20)].iter().collect();

    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"59cf53e54c78");
    h.update(chars.as_bytes());
    h.update(version.as_bytes());
    let digest = h.finalize();
    format!("{:02x}{:02x}", digest[0], digest[1]).chars().take(3).collect()
}

/// 从 body 的 `messages` 里取第一条 `role:"user"` 消息的第一个 `type:"text"` 块文本。
fn first_user_text(v: &serde_json::Value) -> String {
    let msgs = match v.get("messages").and_then(|m| m.as_array()) {
        Some(a) => a,
        None => return String::new(),
    };
    for msg in msgs {
        if msg.get("role").and_then(|r| r.as_str()) != Some("user") {
            continue;
        }
        match msg.get("content") {
            Some(serde_json::Value::String(s)) => return s.clone(),
            Some(serde_json::Value::Array(blocks)) => {
                for blk in blocks {
                    if blk.get("type").and_then(|t| t.as_str()) == Some("text")
                        && let Some(t) = blk.get("text").and_then(|t| t.as_str())
                    {
                        return t.to_string();
                    }
                }
            }
            _ => {}
        }
        break;
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use crate::proxy::test_support::{
        API_SHAPE_BODY, PLAIN_BODY, all_on, base_block, detect_for, detect_with, parsed,
        platform_headers, rewrite_body, sim_for, test_cred,
    };
    use crate::proxy::{Bytes, HeaderValue, build_forward_headers, config, header, store};

    /// 普通请求 → 官方四块 system（sonnet 族，2.1.258 无 reporting）：billing / 身份句 /
    /// 基座（global）/ 客户端原文。基座按模型族选，且 `system` 落在 `messages` 之后（官方 key 序）。
    #[test]
    fn simulates_official_system_for_plain_request() {
        let body = Bytes::from(
            r#"{"model":"claude-sonnet-5","messages":[],"system":"你是助手","max_tokens":8}"#
                .to_string(),
        );
        let sim = detect_for(&body, all_on()).unwrap();
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
        let s = String::from_utf8(out.to_vec()).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();

        assert_eq!(sys.len(), 4, "sonnet 族应是官方的四块（无 reporting）: {s}");
        assert!(
            sys[0]["text"].as_str().unwrap().starts_with("x-anthropic-billing-header:"),
            "第 0 块应是 billing header: {s}"
        );
        assert!(sys[0]["text"].as_str().unwrap().contains("; cch="), "cch 要在 billing 段里");
        assert!(
            sys[0]["text"].as_str().unwrap().starts_with(
                "x-anthropic-billing-header: cc_version=2.1.260.1e2; cc_entrypoint=cli;"
            ),
            "模拟路径的 cc_version 取 profile 的版本与后缀（sonnet 主线程没有 2.1.260 样本，\
             后缀沿用 2.1.258 那个四族通用值 1e2）: {s}"
        );
        assert!(
            sys[0]["text"].as_str().unwrap().contains("cc_prompt_id="),
            "官方主线程每条都带 cc_prompt_id（cap/2.1.260-2/00013）: {s}"
        );
        assert_eq!(sys[1]["text"], config::CC_SYSTEM_IDENTITY, "第 1 块必须是那句身份声明");
        assert!(sys[1].get("cache_control").is_none(), "身份句不带断点（官方如此）");
        assert_eq!(sys[2]["text"], config::CC_SYSTEM_BASE_SONNET, "sonnet 族应取 sonnet 基座");
        assert_eq!(sys[2]["cache_control"]["scope"], "global");
        assert_eq!(sys[3]["text"], "你是助手", "客户端原 system 应原样留在末块");
        assert_eq!(sys[3]["cache_control"]["type"], "ephemeral");
        assert!(sys[3]["cache_control"].get("scope").is_none(), "只有基座标 global");
        assert_eq!(sys[2]["cache_control"]["ttl"], "1h", "基座该带 ttl: {s}");
        assert_eq!(sys[3]["cache_control"]["ttl"], "1h", "末块也该带 ttl: {s}");

        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec!["model", "messages", "system", "metadata", "max_tokens", "diagnostics"],
            "key 序: {s}"
        );
        assert_eq!(
            v["diagnostics"],
            serde_json::json!({"previous_message_id": serde_json::Value::Null}),
            "会话首条：字段在、值为 null（cap/2.1.260-2/00013）: {s}"
        );

        // 换模型族即换基座：三族三份基座。
        assert_eq!(sim_for(PLAIN_BODY).base, Some(config::CC_SYSTEM_BASE_OPUS), "opus-5 短基座");
        assert_eq!(
            sim_for(r#"{"model":"claude-fable-5-1","messages":[]}"#).base,
            Some(config::CC_SYSTEM_BASE_OPUS),
            "fable-5-1 与 opus-5 共用短基座（cap/2.1.258/00012 与 00013 sha256 相同）"
        );
        assert_eq!(
            sim_for(r#"{"model":"claude-haiku-4-5-20251001","messages":[]}"#).base,
            Some(config::CC_SYSTEM_BASE_HAIKU),
            "haiku 用旧版长基座"
        );
        assert_eq!(
            sim_for(r#"{"model":"claude-fable-5","messages":[]}"#).base,
            Some(config::CC_SYSTEM_BASE_OPUS),
            "fable 与 opus-5 共用短基座"
        );
        assert_eq!(
            sim_for(r#"{"model":"claude-opus-4-6","messages":[]}"#).base,
            Some(config::CC_SYSTEM_BASE_HAIKU),
            "opus-4-6 用旧版长基座"
        );
        assert!(
            sim_for(r#"{"model":"gpt-4o","messages":[]}"#).base.is_none(),
            "认不出的模型不猜基座"
        );
    }

    /// 2.1.260 的 billing 后缀是**逐 profile 定死**的，不能再走那套派生算法。
    ///
    /// 算法逆向自 2.1.251，在 2.1.260 上已被证否：六个 profile 各有固定后缀，而算法对
    /// 主线程样本算出的是 `11d`。更老的版本仍走派生（2.1.258 五份抓包全是 `1e2`，算法
    /// 在 `"hi"` 上正好也算出 `1e2`）。
    #[test]
    fn billing_suffix_is_profile_fixed_on_2_1_260() {
        use crate::proxy::CcRequestKind as K;
        let body = |model: &str| {
            serde_json::json!({
                "model": model,
                "messages": [{"role": "user", "content": "hi"}]})
        };
        let text = |model: &str, ver: &str, kind| {
            crate::proxy::billing_header_text(&body(model), Some(ver), kind)
        };

        // 主线程按模型族：opus `222`、fable `bcd`。
        assert!(
            text("claude-opus-5", "2.1.260", K::Main).contains("cc_version=2.1.260.222;"),
            "{}",
            text("claude-opus-5", "2.1.260", K::Main)
        );
        assert!(text("claude-fable-5-1", "2.1.260", K::Main).contains("2.1.260.bcd;"));
        // 「猜下一句」跟主线程同一档。
        assert!(text("claude-opus-5", "2.1.260", K::Suggestion).contains("2.1.260.222;"));
        // 各辅助 profile 各有各的。
        for (kind, want) in
            [(K::Subagent, "660"), (K::Helper, "d95"), (K::Title, "ced"), (K::Classifier, "3de")]
        {
            let got = text("claude-haiku-4-5-20251001", "2.1.260", kind);
            assert!(got.contains(&format!("2.1.260.{want};")), "{kind:?}: {got}");
        }

        // 2.1.258 仍走派生算法——那一版五份抓包全是 `1e2`。
        let old = text("claude-opus-5", "2.1.258", K::Main);
        assert!(old.contains("cc_version=2.1.258.1e2;"), "{old}");
    }

    /// uuid 形态校验的边界：只认 `8-4-4-4-12` 的小写 hex。
    #[test]
    fn session_id_must_look_like_a_uuid() {
        let ok = crate::proxy::looks_like_uuid;
        assert!(ok("d0c1fb05-9b19-4576-9465-e2b8a206dabf"));
        // 不看 version/variant 位：v7 之类同样是个正常的会话 id，没理由拦。
        assert!(ok("00000000-0000-0000-0000-000000000000"));
        assert!(!ok("D0C1FB05-9B19-4576-9465-E2B8A206DABF"), "大写不是官方形态");
        assert!(!ok("d0c1fb05-9b19-4576-9465-e2b8a206dab"), "末段少一位");
        assert!(!ok("d0c1fb05-9b19-4576-9465-e2b8a206dabff"), "末段多一位");
        assert!(!ok("d0c1fb05-9b19-4576-9465-e2b8a206dabf-x"), "多一段");
        assert!(!ok("d0c1fb05_9b19_4576_9465_e2b8a206dabf"), "分隔符不对");
        assert!(!ok("g0c1fb05-9b19-4576-9465-e2b8a206dabf"), "非 hex");
        assert!(!ok(""));
    }

    /// 超长客户端 system 搬不动时，**一个字节都不许改**。
    ///
    /// 原来的实现先把末块换成 `(see conversation)` 占位、再去写 `messages[0]`，落点不可写
    /// 时就直接 `return false`——客户端明确下的那段指令凭空消失，而调用方只看到一个
    /// `false`，以为什么都没发生。
    #[test]
    fn long_client_system_is_relocated_atomically() {
        let long = "指令".repeat(1200); // 远超 MAX_CLIENT_SYSTEM_CHARS
        // messages[0].content 是数字：既不是数组也不是字符串，搬不过去。
        let body = serde_json::json!({
            "model": "claude-opus-5",
            "max_tokens": 64000,
            "messages": [{"role": "user", "content": 42}],
            "system": long});
        let raw = Bytes::from(serde_json::to_vec(&body).unwrap());
        let sim = detect_for(&raw, all_on()).expect("该请求应走模拟路径");
        let mut v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert!(crate::proxy::simulate_system(
            &mut v,
            &sim,
            crate::proxy::CacheShape { global: true, ttl_1h: true }
        ));
        let before = v.clone();
        assert!(!crate::proxy::relocate_long_client_system(&mut v, &sim), "搬不动就该返回 false");
        assert_eq!(v, before, "搬不动时 body 必须原样不动，不能只剩一个占位块");
        let tail = v["system"].as_array().unwrap().last().unwrap();
        assert!(tail["text"].as_str().unwrap().contains("指令"), "客户端那段 system 还在: {tail}");

        // messages[0] 可写时照常搬走，末块换占位。
        let ok = serde_json::json!({
            "model": "claude-opus-5",
            "max_tokens": 64000,
            "messages": [{"role": "user", "content": "hi"}],
            "system": long});
        let raw = Bytes::from(serde_json::to_vec(&ok).unwrap());
        let mut v: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert!(crate::proxy::simulate_system(
            &mut v,
            &sim,
            crate::proxy::CacheShape { global: true, ttl_1h: true }
        ));
        assert!(crate::proxy::relocate_long_client_system(&mut v, &sim));
        let tail = v["system"].as_array().unwrap().last().unwrap();
        assert_eq!(tail["text"], "(see conversation)", "末块换成占位");
        let first = v["messages"][0]["content"].as_str().unwrap();
        assert!(first.starts_with("<system_instructions>"), "内容搬到了首条消息: {first}");
        assert!(first.contains("指令"), "内容没丢");
    }

    /// 没有 system 的请求同样成立：opus 族三块（billing / 身份句 / 基座），末块拿到断点。
    #[test]
    fn simulates_system_when_client_sent_none() {
        let body = Bytes::from(PLAIN_BODY.to_string());
        let sim = sim_for(PLAIN_BODY);
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();
        assert_eq!(sys.len(), 3, "没有客户端 system 就只有前三块: {v}");
        assert_eq!(sys[2]["cache_control"]["scope"], "global");
    }

    /// fable 族是 2.1.260 里唯一还带 `# Reporting outcomes` 的（`cap/2.1.260/00018`）：
    /// 五块 `[billing, 身份句, reporting, 基座, 客户端原文]`，thinking 补成
    /// `{adaptive, display:"updates"}` 且 `display` 不被 `strip_extra_fields` 剥掉。
    #[test]
    fn simulates_fable_with_reporting_block_and_display_updates() {
        let body = concat!(
            r#"{"model":"claude-fable-5-1","max_tokens":64000,"#,
            r#""messages":[{"role":"user","content":"hi"}],"system":"你是助手"}"#
        );
        let reporting =
            |b: &str| sim_for(b).profile.system == config::CcSystemShape::IdentityReporting;
        let b = Bytes::from(body.to_string());
        let sim = sim_for(body);
        assert!(reporting(body), "fable 族该带 reporting");
        assert!(!reporting(PLAIN_BODY), "opus 族不带");
        assert!(!reporting(r#"{"model":"claude-sonnet-5","messages":[]}"#), "sonnet 族不带");
        assert!(
            !reporting(r#"{"model":"claude-haiku-4-5-20251001","messages":[]}"#),
            "haiku 族不带"
        );
        let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();
        assert_eq!(sys.len(), 5, "fable 族是五块: {v}");
        assert_eq!(sys[2]["text"], config::CC_SYSTEM_REPORTING, "第 2 块是 reporting outcomes");
        assert!(sys[2].get("cache_control").is_none(), "reporting 块不带断点（官方如此）");
        assert_eq!(sys[3]["text"], config::CC_SYSTEM_BASE_OPUS, "fable 与 opus 共用基座");
        assert_eq!(sys[3]["cache_control"]["scope"], "global");
        assert_eq!(sys[4]["text"], "你是助手");
        assert_eq!(
            v["thinking"],
            serde_json::json!({"type": "adaptive", "display": "updates"}),
            "fable 的 thinking 形态（cap/2.1.260/00018）: {v}"
        );
        assert_eq!(
            v["fallbacks"],
            serde_json::Value::Null,
            "没给 fallbacks 字面量（该族 refusal fallback 开关关着）时不替用户开: {v}"
        );
        assert!(
            sim.profile.beta.contains("thinking-display-updates-2026-08-18"),
            "display:updates 要有对应 beta"
        );
    }

    /// 模拟路径要补官方那**第三个**缓存断点：`cap/raw` 八份抓包每条恰好 3 个，前两个在
    /// `system`，第三个恒在最后一条消息的最后一块上（六份非 haiku 落在末尾那条 `role:"system"`
    /// 消息，两份 haiku 没那条消息就落在 `user` 末块——规则是位置不是角色）。
    /// 顺带把裸字符串 `content` 收成官方那样的块数组，否则断点无处可挂。
    #[test]
    fn simulated_body_carries_official_message_breakpoint() {
        // 字符串 content：要转成块数组，断点落在末块。
        let body = Bytes::from(PLAIN_BODY.to_string());
        let sim = sim_for(PLAIN_BODY);
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let blocks = v["messages"][0]["content"].as_array().expect("content 该收成块数组");
        assert_eq!(blocks[0]["type"], "text", "转出来的该是官方那种文本块");
        assert_eq!(blocks[0]["text"], "hi", "正文一个字都不该变");
        assert_eq!(blocks.last().unwrap()["cache_control"]["type"], "ephemeral", "末块该有断点");
        // 消息这个断点不带 `scope`（官方只在基座标），但跟着开关带 `ttl`。
        assert!(
            blocks.last().unwrap()["cache_control"].get("scope").is_none(),
            "只有基座标 global"
        );
        assert_eq!(blocks.last().unwrap()["cache_control"]["ttl"], "1h");

        // 多轮对话：断点只落在**最后一条**消息上，前面的不动。
        let multi = concat!(
            r#"{"model":"claude-opus-5","max_tokens":16,"messages":["#,
            r#"{"role":"user","content":"a"},{"role":"assistant","content":"b"},"#,
            r#"{"role":"user","content":"c"}]}"#
        );
        let b = Bytes::from(multi.to_string());
        let sim = sim_for(multi);
        let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        for (i, m) in msgs.iter().enumerate() {
            let last = m["content"].as_array().unwrap().last().unwrap();
            assert_eq!(
                last.get("cache_control").is_some(),
                i == 2,
                "断点只该在最后一条消息上，第 {i} 条不对: {v}"
            );
        }

        // 客户端自己标过就不再多标一个；总数封顶 4，满了不补。
        // （`ttl` 会由 [`crate::proxy::fill_cache_ttl`] 补齐——三个断点要么都有、要么都没有。）
        let mine = concat!(
            r#"{"model":"claude-opus-5","max_tokens":16,"messages":[{"role":"user","content":["#,
            r#"{"type":"text","text":"hi","cache_control":{"type":"ephemeral"}}]}]}"#
        );
        let b = Bytes::from(mine.to_string());
        let sim = sim_for(mine);
        let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["messages"][0]["content"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral", "ttl": "1h"}),
            "客户端那个断点只该补 ttl，不该多出 scope 或被换掉 type: {v}"
        );
        assert_eq!(v["messages"][0]["content"][0]["text"], "hi", "正文一个字都不该动: {v}");
        assert!(
            crate::proxy::count_cache_control(&v) <= crate::proxy::MAX_CACHE_BREAKPOINTS,
            "断点超上限: {v}"
        );

        // 非模拟路径**不新标断点**：CC 形态的来访自己就标好了第三个断点，替它再标一个只会
        // 多占预算。唯一会动的是给那个断点补 `ttl`（[`crate::proxy::fill_cache_ttl`]），正文与断点
        // 位置都不变。
        let cc = Bytes::from(API_SHAPE_BODY);
        let before: serde_json::Value = serde_json::from_slice(&cc).unwrap();
        let out = rewrite_body(&cc, &test_cred(), "fp", all_on(), None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            crate::proxy::count_cache_control(&v["messages"]),
            crate::proxy::count_cache_control(&before["messages"]),
            "非模拟路径不该给 messages 新加断点: {v}"
        );
        assert_eq!(
            v["messages"][0]["content"][0]["text"], before["messages"][0]["content"][0]["text"],
            "正文不该被动: {v}"
        );
        assert_eq!(
            v["messages"][0]["content"][0]["cache_control"],
            serde_json::json!({"type": "ephemeral", "ttl": "1h"}),
            "来访那个断点该补上 ttl，与 system 两个保持一致: {v}"
        );

        // 末块不是**非空 text** 时一律不标：抓包只有 text 的样本，而 `thinking` 那种块
        // 上游还要验签名，往它上面挂 cache_control 是拿能发的请求去赌没样本的组合。
        for (label, tail) in [
            ("thinking 块", r#"{"type":"thinking","thinking":"想","signature":"AAAA"}"#),
            ("空 text 块", r#"{"type":"text","text":""}"#),
            (
                "image 块",
                r#"{"type":"image","source":{"type":"base64","media_type":"image/png","data":"AA"}}"#,
            ),
        ] {
            let body = format!(
                r#"{{"model":"claude-opus-5","max_tokens":16,"messages":[{{"role":"user","content":"hi"}},{{"role":"assistant","content":[{tail}]}}]}}"#
            );
            let b = Bytes::from(body.clone());
            let sim = sim_for(&body);
            let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
            let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
            let last = v["messages"].as_array().unwrap().last().unwrap();
            let blk = last["content"].as_array().unwrap().last().unwrap();
            assert!(blk.get("cache_control").is_none(), "{label} 不该被标断点: {v}");
        }

        // 空串 content 不转成空 text 块（那种块上游会拒），原样留着。
        let empty = concat!(
            r#"{"model":"claude-opus-5","max_tokens":16,"#,
            r#""messages":[{"role":"user","content":"hi"},{"role":"assistant","content":""}]}"#
        );
        let b = Bytes::from(empty.to_string());
        let sim = sim_for(empty);
        let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["messages"][1]["content"], "", "空串不该被转成空 text 块: {v}");
    }

    /// 已经是 CC 形态的请求一个字节都不该多改——判据是 `system` 里那句身份声明，
    /// 字符串形态与数组形态都认。
    #[test]
    fn cc_shaped_from_non_cc_client_is_simulated() {
        // 非 CC 客户端（无 UA / Go-http-client 等）抄了 CC 的 system 但没带 metadata.user_id：
        // 应走模拟，把 headers 统一换成官方形态，body 里那份身份声明由 strip_cc_preamble
        // 剥掉再重建。
        let cc_no_meta = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"{}"}},{{"type":"text","text":"user prompt"}}]}}"#,
            config::CC_SYSTEM_IDENTITY
        ));
        assert!(detect_for(&cc_no_meta, all_on()).is_some(), "非 CC 客户端抄了 system 应走模拟");
        let as_string = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","system":"{}","messages":[]}}"#,
            config::CC_SYSTEM_IDENTITY
        ));
        assert!(detect_for(&as_string, all_on()).is_some(), "字符串形态也应走模拟");

        // 带 metadata.user_id 但 UA 不是 claude-cli → 照样走模拟（只看 UA）。
        let cc_with_meta = Bytes::from(API_SHAPE_BODY);
        assert!(detect_for(&cc_with_meta, all_on()).is_some(), "非 CC UA 带 user_id 也走模拟");

        // 真正的 CC 客户端（UA 带 claude-cli/）+ CC 形态 body 不模拟——UA 不降级。
        let mut cc_ua = crate::proxy::HeaderMap::new();
        cc_ua.insert(
            header::USER_AGENT,
            HeaderValue::from_static("claude-cli/2.1.226 (external, cli)"),
        );
        // 真 CC 的体：身份句 + 基座；抄了身份句却没基座的 cc_no_meta 在 CC UA 下也走模拟。
        let cc_full = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"{}"}},{},{{"type":"text","text":"user prompt"}}]}}"#,
            config::CC_SYSTEM_IDENTITY,
            base_block()
        ));
        assert!(
            detect_with(&cc_full, &cc_ua, all_on()).is_none(),
            "真 CC 客户端(无tools)不该走模拟"
        );
        assert!(
            detect_with(&cc_no_meta, &cc_ua, all_on()).is_some(),
            "CC UA + 身份句但没基座 → 去掉基座的第三方，走模拟"
        );

        // CC UA + CC 形态 system + 含官方工具名 → 不模拟。
        let cc_with_tools = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"{}"}},{}],"tools":[{{"name":"Bash"}},{{"name":"custom_tool"}}]}}"#,
            config::CC_SYSTEM_IDENTITY,
            base_block()
        ));
        assert!(
            detect_with(&cc_with_tools, &cc_ua, all_on()).is_none(),
            "CC UA + CC 形态 + 有官方工具不该走模拟"
        );

        // CC UA 但 system 里既无身份声明也无 billing header → 冒用 UA，走模拟。
        // 这就是封号复盘里那批探活请求的形态：官方 UA + 官方格式 user_id + 自造的 3 块 system。
        let cc_ua_fake_shape = Bytes::from(
            r#"{"model":"claude-sonnet-5","system":[{"type":"text","text":"probe"},{"type":"text","text":"a"},{"type":"text","text":"b","cache_control":{"type":"ephemeral"}}],"messages":[{"role":"user","content":"hi"}],"max_tokens":1024,"temperature":1,"stream":true}"#,
        );
        assert!(
            detect_with(&cc_ua_fake_shape, &cc_ua, all_on()).is_some(),
            "CC UA + 非 CC 形态 system 应走模拟"
        );
        assert!(
            detect_with(&Bytes::from(PLAIN_BODY), &cc_ua, all_on()).is_some(),
            "CC UA + 没有 system 也应走模拟"
        );
        // billing header 块 + 子代理自己那段长提示词（身份句不同）算 CC 形态 → 不模拟。
        let cc_billing_only = Bytes::from(format!(
            r#"{{"model":"claude-haiku-4-5-20251001","system":[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.260.abcdef; cch=00000"}},{{"type":"text","text":"You are a search agent. {}"}}],"messages":[{{"role":"user","content":"hi"}}]}}"#,
            "x".repeat(1200)
        ));
        assert!(
            detect_with(&cc_billing_only, &cc_ua, all_on()).is_none(),
            "CC UA + billing header + 子代理长提示词不该走模拟"
        );

        // CC UA + billing header + 身份句、却**没有基座**（ban.log 里 claude-cli/2.1.165 那批：
        // 两块 system、tools: []、每台新设备同样四道题）→ 去掉基座的第三方，走模拟补全。
        let no_base = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","system":[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.165.abcdef; cch=00000"}},{{"type":"text","text":"{}"}}],"messages":[{{"role":"user","content":"hi"}}],"max_tokens":10240,"stream":true,"tools":[],"thinking":{{"type":"adaptive"}}}}"#,
            config::CC_SYSTEM_IDENTITY
        ));
        assert!(
            detect_with(&no_base, &cc_ua, all_on()).is_some(),
            "CC UA + 身份句但没有基座的该走模拟"
        );
        // 同样两块 system 但 max_tokens=1 → 2.1.187 Claude Desktop 的 cache 预热，照旧透传。
        let prewarm = Bytes::from(format!(
            r#"{{"model":"claude-haiku-4-5-20251001","messages":[{{"role":"user","content":"hi"}}],"system":[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.187.abcdef"}},{{"type":"text","text":"{}"}}],"max_tokens":1}}"#,
            config::CC_SYSTEM_IDENTITY
        ));
        assert!(
            detect_with(&prewarm, &cc_ua, all_on()).is_none(),
            "max_tokens=1 的预热不该因为没基座而被模拟"
        );
        // 官方额度探测（`cap/2.1.260-2/00004`）：没有 system、没有 tools、haiku、max_tokens=1、
        // 正文 `quota`。它过不了 is_cc_shaped，必须单独放行——装成主线程会给它补 system、基座
        // 和工具，正是既定要求里禁止的。曾在收紧「CC 形态」条件时漏掉过一次。
        let quota = Bytes::from(
            r#"{"model":"claude-haiku-4-5-20251001","max_tokens":1,"messages":[{"role":"user","content":"quota"}],"metadata":{"user_id":"{\"device_id\":\"832cb7e697190bc475b926c7994ef183a0f8a58e29818f182e11f924e1ea2870\",\"account_uuid\":\"a\",\"session_id\":\"d0c1fb05-9b19-4576-9465-e2b8a206dabf\"}"}}"#,
        );
        assert!(
            crate::proxy::is_quota_probe_shaped(&parsed(&quota).unwrap()),
            "测试体本身得是官方额度探测形态"
        );
        assert!(detect_with(&quota, &cc_ua, all_on()).is_none(), "官方额度探测不该被模拟成主线程");
        // 差一点就不是额度探测：正文不是 quota、或模型不是 haiku 4.5 全名 → 没有 system 又不是
        // 额度探测的，照旧走模拟。
        let not_quota = Bytes::from(
            r#"{"model":"claude-haiku-4-5-20251001","max_tokens":1,"messages":[{"role":"user","content":"hi"}]}"#,
        );
        assert!(detect_with(&not_quota, &cc_ua, all_on()).is_some());
        let wrong_model = Bytes::from(
            r#"{"model":"claude-opus-5","max_tokens":1,"messages":[{"role":"user","content":"quota"}]}"#,
        );
        assert!(detect_with(&wrong_model, &cc_ua, all_on()).is_some());
        // 官方 Helper（`cap/2.1.260/00024`）：子代理 billing header + SDK 身份句两块，没有任何
        // 长块，haiku、tools: []、max_tokens 32000、流式。基座阈值对它例外，照旧透传。
        const SUB_BILLING: &str = "x-anthropic-billing-header: cc_version=2.1.260.d95; cc_entrypoint=cli; cch=ca354; cc_is_subagent=true; cc_prompt_id=bd224f00-5a91-4f15-b92f-0f47c663eae8;";
        let helper_body = |model: &str,
                           tools: &str,
                           max_tokens: u32,
                           stream: &str,
                           sdk_line: &str| {
            Bytes::from(format!(
                r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}}],"system":[{{"type":"text","text":"{SUB_BILLING}"}},{{"type":"text","text":"{sdk_line}"}}],"tools":{tools},"max_tokens":{max_tokens},"thinking":{{"type":"disabled"}},"temperature":1{stream}}}"#
            ))
        };
        let helper = helper_body(
            "claude-haiku-4-5-20251001",
            "[]",
            32000,
            r#","stream":true"#,
            config::CC_SDK_AGENT_IDENTITY,
        );
        let mut helper_headers = cc_ua.clone();
        helper_headers.insert(
            "anthropic-beta",
            HeaderValue::from_str(
                config::cc_profile(config::CcProfileKind::HelperSubagentHaiku).beta,
            )
            .unwrap(),
        );
        assert!(
            detect_with(&helper, &helper_headers, all_on()).is_none(),
            "官方 Helper 子代理没有长块也不该被模拟"
        );
        assert!(
            detect_with(&helper, &cc_ua, all_on()).is_some(),
            "同样的 system 与 body、头上没带 Helper 的 beta → 不是官方 Helper，走模拟"
        );
        // 只抄了 `cc_is_subagent=true` 这个标记、其余不是官方 Helper 取值的，例外不成立，
        // 照基座阈值走模拟——每一条都是曾经能绕过的写法。
        for (label, body) in [
            (
                "模型是 opus",
                helper_body(
                    "claude-opus-5",
                    "[]",
                    32000,
                    r#","stream":true"#,
                    config::CC_SDK_AGENT_IDENTITY,
                ),
            ),
            (
                "带了工具",
                helper_body(
                    "claude-haiku-4-5-20251001",
                    r#"[{"name":"Bash"}]"#,
                    32000,
                    r#","stream":true"#,
                    config::CC_SDK_AGENT_IDENTITY,
                ),
            ),
            (
                "max_tokens 不是 32000",
                helper_body(
                    "claude-haiku-4-5-20251001",
                    "[]",
                    1024,
                    r#","stream":true"#,
                    config::CC_SDK_AGENT_IDENTITY,
                ),
            ),
            (
                "非流式",
                helper_body(
                    "claude-haiku-4-5-20251001",
                    "[]",
                    32000,
                    "",
                    config::CC_SDK_AGENT_IDENTITY,
                ),
            ),
            (
                "第二块不是 SDK 身份句",
                helper_body(
                    "claude-haiku-4-5-20251001",
                    "[]",
                    32000,
                    r#","stream":true"#,
                    config::CC_SYSTEM_IDENTITY,
                ),
            ),
        ] {
            assert!(detect_with(&body, &helper_headers, all_on()).is_some(), "{label}");
        }
        // 三块 system（多出一块短的）也不是 Helper：官方 Helper 恰好两块。
        let three_blocks = Bytes::from(format!(
            r#"{{"model":"claude-haiku-4-5-20251001","messages":[{{"role":"user","content":"hi"}}],"system":[{{"type":"text","text":"{SUB_BILLING}"}},{{"type":"text","text":"{}"}},{{"type":"text","text":"extra"}}],"tools":[],"max_tokens":32000,"thinking":{{"type":"disabled"}},"temperature":1,"stream":true}}"#,
            config::CC_SDK_AGENT_IDENTITY
        ));
        assert!(detect_with(&three_blocks, &helper_headers, all_on()).is_some());
        // 身份写错的（device 不是 64 位 hex / session 不是 uuid）也进不了透传，走模拟重建身份：
        // 这就是 ban.log 里 `channel-test` 那批探活的去向。
        let bad_identity = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[{{"role":"user","content":"hi"}}],"system":[{{"type":"text","text":"{}"}},{}],"tools":[{{"name":"Bash"}}],"metadata":{{"user_id":"{{\"device_id\":\"channel-test\",\"account_uuid\":\"a\",\"session_id\":\"channel-test-claude-code\"}}"}}}}"#,
            config::CC_SYSTEM_IDENTITY,
            base_block()
        ));
        assert!(
            detect_with(&bad_identity, &cc_ua, all_on()).is_some(),
            "身份格式不合法的不当官方客户端，走模拟"
        );
        let mut bad_header = cc_ua.clone();
        bad_header.insert(
            crate::proxy::HeaderName::from_static("x-claude-code-session-id"),
            HeaderValue::from_static("channel-test-claude-code"),
        );
        assert!(
            detect_with(&cc_full, &bad_header, all_on()).is_some(),
            "体合法、会话头非法的也走模拟"
        );
        // 只抄一句 cc_is_subagent=true 却没有 billing header 前缀的更不算。
        let fake_sub = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"cc_is_subagent=true"}},{{"type":"text","text":"{}"}}]}}"#,
            config::CC_SYSTEM_IDENTITY
        ));
        assert!(detect_with(&fake_sub, &cc_ua, all_on()).is_some());

        // CC UA + 全是非官方工具名 → 视为冒用，走模拟。
        let spoofed_ua = Bytes::from(
            r#"{"model":"claude-opus-5","messages":[],"tools":[{"name":"exec"},{"name":"read_file"},{"name":"web_search"}]}"#.to_string()
        );
        assert!(
            detect_with(&spoofed_ua, &cc_ua, all_on()).is_some(),
            "CC UA 但零官方工具应走模拟（冒用）"
        );

        // 模拟后 system 里不该有两份身份声明。
        let sim = detect_for(&cc_no_meta, all_on()).unwrap();
        let out = rewrite_body(&cc_no_meta, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();
        let id_count = sys
            .iter()
            .filter(|b| b.get("text").and_then(|t| t.as_str()) == Some(config::CC_SYSTEM_IDENTITY))
            .count();
        assert_eq!(id_count, 1, "身份声明只该出现一次: {v}");
        // 客户端的原始 prompt 不该丢。
        assert!(
            sys.iter().any(|b| b.get("text").and_then(|t| t.as_str()) == Some("user prompt")),
            "客户端原始 prompt 应保留: {v}"
        );

        // 开关关掉、或 merge_beta 关掉时也不模拟。
        let plain = Bytes::from(PLAIN_BODY.to_string());
        let off = store::ForwardFlags { simulate_cc: false, ..all_on() };
        assert!(detect_for(&plain, off).is_none());
        let no_beta = store::ForwardFlags { merge_beta: false, ..all_on() };
        assert!(detect_for(&plain, no_beta).is_none());
        // 解析不了的请求体不 panic、也不模拟。
        assert!(detect_for(&Bytes::from_static(b"not json"), all_on()).is_none());
    }

    /// 客户端已经用满 4 个缓存断点时不再加——加了整条请求会被上游拒，那是把「形态更像」
    /// 换成「根本发不出去」。断点在别处（tools）时同样算数。
    #[test]
    fn respects_cache_breakpoint_budget() {
        let t = |n: &str| format!(r#"{{"name":"{n}","cache_control":{{"type":"ephemeral"}}}}"#);
        let body = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"tools":[{},{},{},{}]}}"#,
            t("t0"),
            t("t1"),
            t("t2"),
            t("t3"),
        ));
        let sim = detect_for(&body, all_on()).unwrap();
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(crate::proxy::count_cache_control(&v), 4, "断点数不得超过 4: {v}");
        assert!(v["system"][2].get("cache_control").is_none(), "预算用完时基座不带断点");
        assert_eq!(v["system"][2]["text"], config::CC_SYSTEM_BASE_OPUS);
    }

    /// 客户端把 `system` 拆成多块时并成官方末块的一块——3+N 块会被上游判第三方应用、
    /// 改扣超额池（`Third-party apps now draw from your extra usage`）。
    ///
    /// 合并腾出来的断点预算要算进去：客户端那 4 个断点合并后只剩 1 个，基座该拿到断点。
    #[test]
    fn merges_client_system_blocks_into_official_tail() {
        let blk = |t: &str| {
            format!(r#"{{"type":"text","text":"{t}","cache_control":{{"type":"ephemeral"}}}}"#)
        };
        let body = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"system":[{},{},{},{}]}}"#,
            blk("a"),
            blk("b"),
            blk("c"),
            blk("d")
        ));
        let sim = detect_for(&body, all_on()).unwrap();
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();

        assert_eq!(sys.len(), 4, "客户端的 4 块应并成末块一块（opus 族无 reporting）: {v}");
        assert_eq!(sys[3]["text"], "a\n\nb\n\nc\n\nd", "正文一个字都不该丢");
        assert_eq!(sys[3]["cache_control"]["type"], "ephemeral", "末块断点取合并前的最后一个");
        assert_eq!(sys[2]["text"], config::CC_SYSTEM_BASE_OPUS);
        assert_eq!(sys[2]["cache_control"]["scope"], "global", "合并腾出的预算该给基座");
        assert_eq!(crate::proxy::count_cache_control(&v), 2, "断点数: {v}");

        // 空块并不进来（发一个空文本块上游不收），只剩前三块。
        let empty = Bytes::from(
            r#"{"model":"claude-opus-5","messages":[],"system":[{"type":"text","text":""},{"type":"text","text":"  "}]}"#
                .to_string(),
        );
        let sim = detect_for(&empty, all_on()).unwrap();
        let out = rewrite_body(&empty, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["system"].as_array().unwrap().len(), 3, "全空的块应丢掉: {v}");
    }

    /// 自称 CC（`system` 里有那句身份声明）却发了 5 块以上的第三方客户端：
    /// 非 CC 客户端现在走模拟，`strip_cc_preamble` 剥掉旧身份、`simulate_system` 补上新的。
    #[test]
    fn cc_shaped_from_non_cc_client_strips_and_rebuilds() {
        let body = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"h"}},{{"type":"text","text":"{}"}},{{"type":"text","text":"c"}},{{"type":"text","text":"d"}},{{"type":"text","text":"e","cache_control":{{"type":"ephemeral"}}}}]}}"#,
            config::CC_SYSTEM_IDENTITY
        ));
        assert!(detect_for(&body, all_on()).is_some(), "非 CC 客户端应走模拟");
        let sim = detect_for(&body, all_on()).unwrap();
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();
        // 身份声明只有一份（模拟补的那份），客户端原来那份已被 strip 掉。
        let id_count = sys
            .iter()
            .filter(|b| b.get("text").and_then(|t| t.as_str()) == Some(config::CC_SYSTEM_IDENTITY))
            .count();
        assert_eq!(id_count, 1, "身份声明应恰好一份: {v}");
        assert_eq!(sys[1]["text"], config::CC_SYSTEM_IDENTITY);

        // 5 块及以内不动结构：cap_system_blocks 的直接验证。
        let four = Bytes::from(API_SHAPE_BODY);
        let before: serde_json::Value = serde_json::from_slice(&four).unwrap();
        let mut after = before.clone();
        assert!(!crate::proxy::cap_system_blocks(&mut after), "3 块不该被改");
        assert_eq!(before, after);
    }

    /// [`crate::proxy::simulates_cc`] 与 [`crate::proxy::Simulation::detect`] 必须给出同一个答案——指纹在
    /// detect 之前先用前者算出站 UA，两处判据一旦分叉，一条请求就会被算到另一台设备名下。
    #[test]
    fn simulates_cc_agrees_with_detect() {
        const CC_UA: &str = "claude-cli/2.1.260 (external, cli)";
        let cc_shaped = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","system":[{{"type":"text","text":"{}"}},{}],"messages":[]}}"#,
            config::CC_SYSTEM_IDENTITY,
            base_block()
        ));
        let plain = Bytes::from(PLAIN_BODY.to_string());
        let sim_off = store::ForwardFlags { simulate_cc: false, ..all_on() };

        let agrees = |body: &Bytes,
                      headers: &crate::proxy::HeaderMap,
                      flags: store::ForwardFlags| {
            let v = parsed(body);
            let from_cc = crate::proxy::trusted_cc_version(&crate::proxy::ua_of(headers)).is_some();
            let by_predicate = crate::proxy::simulates_cc(v.as_ref(), headers, from_cc, flags);
            let by_detect = detect_with(body, headers, flags).is_some();
            assert_eq!(by_predicate, by_detect, "两处判据必须同源");
            by_predicate
        };

        let cc_ua = platform_headers(Some(CC_UA));
        assert!(!agrees(&cc_shaped, &cc_ua, all_on()), "真 CC（UA + 形态都对）不模拟");
        assert!(agrees(&cc_shaped, &platform_headers(None), all_on()), "抄了形态没抄 UA 的走模拟");
        assert!(agrees(&plain, &cc_ua, all_on()), "抄了 UA 没抄形态的走模拟");
        assert!(!agrees(&plain, &cc_ua, sim_off), "开关关着一律不模拟");
    }

    /// [`simulation_reason`]：记的是三道判据里**第一道**没过的（UA → 身份 → 形态，形态内部
    /// CC 形态 → 基座 → 工具）；全过与官方额度探测为 `None`；`detect` 把同一个原因带进
    /// [`Simulation::reason`]，流水的 `sim_reason` 列与 SIMULATED 日志行都取它。
    #[test]
    fn simulation_reason_names_the_first_failed_check() {
        use crate::proxy::SimulationReason::*;
        const CC_UA: &str = "claude-cli/2.1.260 (external, cli)";
        let cc_ua = platform_headers(Some(CC_UA));
        let no_ua = platform_headers(None);
        let reason = |body: &str, headers: &crate::proxy::HeaderMap| {
            let v: serde_json::Value = serde_json::from_str(body).unwrap();
            let from_cc = crate::proxy::trusted_cc_version(&crate::proxy::ua_of(headers)).is_some();
            crate::proxy::simulation_reason(Some(&v), headers, from_cc, all_on())
        };
        let identity = |device: &str| {
            format!(
                r#""metadata":{{"user_id":"{{\"device_id\":\"{device}\",\"account_uuid\":\"9922ef8e-7945-4f5a-ab4f-cf5f521531df\",\"session_id\":\"4dc73702-d904-4887-809d-17b93cc5357c\"}}"}}"#
            )
        };
        let good_dev = "b982b4cdcb0479c11bfa7d89fcc8536b51e4356e043dc0104b3a05b1f356395d";
        let full = format!(
            r#"{{"model":"claude-opus-5","max_tokens":32000,"system":[{{"type":"text","text":"{}"}},{}],"tools":[{{"name":"Bash","input_schema":{{"type":"object"}}}}],"messages":[],{}}}"#,
            config::CC_SYSTEM_IDENTITY,
            base_block(),
            identity(good_dev)
        );
        // 全对：不模拟。
        assert_eq!(reason(&full, &cc_ua), None);
        // UA 不是 CC：形态再对也是 not_cc_client。
        assert_eq!(reason(&full, &no_ua), Some(NotCcClient));
        // 身份写错（device 不是 64 位 hex）：identity_malformed，排在形态之前。
        let bad_identity = full.replace(good_dev, "not-a-device");
        assert_eq!(reason(&bad_identity, &cc_ua), Some(IdentityMalformed));
        // 没有身份句、没有 billing header：not_cc_shaped。
        let plain = format!(
            r#"{{"model":"claude-opus-5","max_tokens":1024,"system":"be brief","messages":[],{}}}"#,
            identity(good_dev)
        );
        assert_eq!(reason(&plain, &cc_ua), Some(NotCcShaped));
        // 只抄了身份句、没抄基座、也不是预热：no_base_prompt。
        let no_base = format!(
            r#"{{"model":"claude-opus-5","max_tokens":1024,"system":[{{"type":"text","text":"{}"}}],"messages":[],{}}}"#,
            config::CC_SYSTEM_IDENTITY,
            identity(good_dev)
        );
        assert_eq!(reason(&no_base, &cc_ua), Some(NoBasePrompt));
        // 同一份体 max_tokens=1 即 cache 预热：基座免检，不模拟。
        let prewarm = no_base.replace(r#""max_tokens":1024"#, r#""max_tokens":1"#);
        assert_eq!(reason(&prewarm, &cc_ua), None);
        // 身份句 + 基座都在，tools 却一个官方名都没有：tools_not_cc。
        let odd_tools = full.replace(r#""name":"Bash""#, r#""name":"my_tool""#);
        assert_eq!(reason(&odd_tools, &cc_ua), Some(ToolsNotCc));
        // 官方桌面端预热：max_tokens=1、没有 system（或只有一块几百字节的应用块）、没有
        // tools——不是 CC 形态也放行；带长 system 的 1 token 请求仍算第三方。
        let desktop_prewarm = format!(
            r#"{{"model":"claude-fable-5-1","max_tokens":1,"messages":[{{"role":"user","content":"warm"}}],{}}}"#,
            identity(good_dev)
        );
        assert_eq!(reason(&desktop_prewarm, &cc_ua), None);
        let desktop_prewarm_app = desktop_prewarm.replace(
            r#""max_tokens":1,"#,
            &format!(
                r#""max_tokens":1,"system":[{{"type":"text","text":"{}"}}],"#,
                "a".repeat(911)
            ),
        );
        assert_eq!(reason(&desktop_prewarm_app, &cc_ua), None);
        let desktop_prewarm_tools = desktop_prewarm.replace(
            r#""max_tokens":1,"#,
            r#""max_tokens":1,"tools":[{"name":"mcp__ccd_session__spawn_task","input_schema":{"type":"object"}}],"#,
        );
        assert_eq!(
            reason(&desktop_prewarm_tools, &cc_ua),
            Some(NotCcShaped),
            "只有非官方名的 tools 不享预热例外，仍按不是 CC 形态走模拟"
        );
        let anonymous_prewarm = r#"{"model":"claude-fable-5-1","max_tokens":1,"messages":[{"role":"user","content":"warm"}]}"#;
        assert_eq!(
            reason(anonymous_prewarm, &cc_ua),
            Some(NotCcShaped),
            "不带身份的 1 token 请求不算桌面端预热"
        );
        let long_prewarm = desktop_prewarm.replace(
            r#""max_tokens":1,"#,
            &format!(r#""max_tokens":1,"system":"{}","#, "b".repeat(1500)),
        );
        assert_eq!(reason(&long_prewarm, &cc_ua), Some(NotCcShaped));
        assert_eq!(reason(&desktop_prewarm, &no_ua), Some(NotCcClient), "UA 不可信照样模拟");
        let prewarm_bad_id = desktop_prewarm.replace(good_dev, "nope");
        assert_eq!(reason(&prewarm_bad_id, &cc_ua), Some(IdentityMalformed));
        // 官方 WebSearch 子调用：一条用户消息、只有 web_search 这个 server tool 且被 tool_choice
        // 强制、system 只有一句搜索助手提示（可带 billing header）。没有基座也放行，否则会被
        // 重建成主线程体、换 UA 换设备换会话（`ban/luban-ban-13/14`）。
        const WS_TOOL: &str = r#"{"type":"web_search_20250305","name":"web_search","max_uses":8}"#;
        const WS_CHOICE: &str = r#""tool_choice":{"type":"tool","name":"web_search"},"#;
        const WS_PROMPT: &str = "You are an assistant for performing a web search tool use";
        let ws_block = |text: &str| {
            format!(r#"{{"type":"text","text":"{text}","cache_control":{{"type":"ephemeral"}}}}"#)
        };
        let web_search = |system: &str, tools: &str, choice: &str, messages: &str| {
            format!(
                r#"{{"model":"claude-opus-5","max_tokens":64000,"system":{system},"tools":[{tools}],{choice}"messages":{messages},{}}}"#,
                identity(good_dev)
            )
        };
        let one_msg = r#"[{"role":"user","content":"Perform a web search for the query: rust 1.90 release notes"}]"#;
        let ws_sys = format!("[{}]", ws_block(WS_PROMPT));
        let ws = web_search(&ws_sys, WS_TOOL, WS_CHOICE, one_msg);
        assert_eq!(reason(&ws, &cc_ua), None, "不带 billing header 的 WebSearch 子调用放行");
        let ws_billing_sys = format!(
            r#"[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.abc; cc_entrypoint=cli; cch=1e2f3;"}},{}]"#,
            ws_block(WS_PROMPT)
        );
        assert_eq!(
            reason(&web_search(&ws_billing_sys, WS_TOOL, WS_CHOICE, one_msg), &cc_ua),
            None,
            "带 billing header 的同样放行"
        );
        let ws_string_sys = format!(r#""{WS_PROMPT}""#);
        assert_eq!(
            reason(&web_search(&ws_string_sys, WS_TOOL, WS_CHOICE, one_msg), &cc_ua),
            None,
            "字符串形态的 system 一并认"
        );
        // 差一项都不算，仍按原判据走模拟：
        assert_eq!(
            reason(&web_search(&ws_sys, WS_TOOL, "", one_msg), &cc_ua),
            Some(NotCcShaped),
            "没有 tool_choice 强制"
        );
        assert_eq!(
            reason(
                &web_search(&ws_sys, WS_TOOL, r#""tool_choice":{"type":"auto"},"#, one_msg),
                &cc_ua
            ),
            Some(NotCcShaped),
            "tool_choice 不是强制 web_search"
        );
        assert_eq!(
            reason(
                &web_search(
                    &ws_sys,
                    &format!(r#"{WS_TOOL},{{"name":"Bash","input_schema":{{"type":"object"}}}}"#),
                    WS_CHOICE,
                    one_msg
                ),
                &cc_ua
            ),
            Some(NotCcShaped),
            "多了别的工具"
        );
        assert_eq!(
            reason(
                &web_search(
                    &ws_sys,
                    r#"{"name":"web_search","input_schema":{"type":"object"}}"#,
                    WS_CHOICE,
                    one_msg
                ),
                &cc_ua
            ),
            Some(NotCcShaped),
            "同名却不是 server tool"
        );
        assert_eq!(
            reason(
                &web_search(
                    &ws_sys,
                    WS_TOOL,
                    WS_CHOICE,
                    r#"[{"role":"user","content":"search a"},{"role":"assistant","content":"ok"},{"role":"user","content":"search b"}]"#
                ),
                &cc_ua
            ),
            Some(NotCcShaped),
            "不止一条消息"
        );
        assert_eq!(
            reason(
                &web_search(
                    &format!("[{}]", ws_block(&"w".repeat(1500))),
                    WS_TOOL,
                    WS_CHOICE,
                    one_msg
                ),
                &cc_ua
            ),
            Some(NotCcShaped),
            "system 是长块"
        );
        assert_eq!(
            reason(
                &web_search(&format!("[{}]", ws_block("be brief")), WS_TOOL, WS_CHOICE, one_msg),
                &cc_ua
            ),
            Some(NotCcShaped),
            "system 与搜索无关"
        );
        assert_eq!(
            reason(
                &web_search(
                    &format!("[{},{}]", ws_block(WS_PROMPT), ws_block("and more")),
                    WS_TOOL,
                    WS_CHOICE,
                    one_msg
                ),
                &cc_ua
            ),
            Some(NotCcShaped),
            "去掉 billing header 后不止一块"
        );
        assert_eq!(
            reason(
                &web_search(
                    &format!("[{}]", ws_block(config::CC_SYSTEM_IDENTITY)),
                    WS_TOOL,
                    WS_CHOICE,
                    one_msg
                ),
                &cc_ua
            ),
            Some(NoBasePrompt),
            "写了身份句就是主线程形态，按基座判"
        );
        assert_eq!(reason(&ws, &no_ua), Some(NotCcClient), "UA 不可信照样模拟");
        assert_eq!(reason(&ws.replace(good_dev, "nope"), &cc_ua), Some(IdentityMalformed));
        // 开关关着：什么原因都没有。
        let v: serde_json::Value = serde_json::from_str(&plain).unwrap();
        let sim_off = store::ForwardFlags { simulate_cc: false, ..all_on() };
        assert_eq!(crate::proxy::simulation_reason(Some(&v), &cc_ua, true, sim_off), None);
        // detect 带出同一个原因，标签与流水列一致。
        let sim = detect_with(&Bytes::from(plain.clone()), &cc_ua, all_on()).expect("走模拟");
        assert_eq!(sim.reason, NotCcShaped);
        assert_eq!(sim.reason.tag(), "not_cc_shaped");
        // 来访事实：块数、字节数、有没有身份句 / billing header、tools 数、max_tokens。
        let f =
            crate::proxy::inbound_facts(&serde_json::from_str::<serde_json::Value>(&full).unwrap());
        assert_eq!(
            (f.system_blocks, f.identity, f.billing_header, f.tools, f.max_tokens),
            (2, true, false, 1, Some(32000))
        );
        assert!(f.system_bytes > crate::proxy::CC_BASE_PROMPT_MIN_LEN);
        let f = crate::proxy::inbound_facts(&v);
        assert_eq!(
            (f.system_blocks, f.system_bytes, f.identity, f.tools),
            (1, "be brief".len(), false, 0)
        );
    }

    /// [`cc_identity_well_formed`]：三处身份都合法（或都没带）才算；头上的非法值**原样**看，
    /// 不像 [`incoming_session_id`] 那样先过滤掉。这就是 `channel-test` 那类探活进不了透传、
    /// 被送去模拟的那道门。
    #[test]
    fn identity_well_formed_checks_all_three_sources() {
        const DEV: &str = "4fef933b15e89f7060000573496ce0eab6e9f0d1cf43e31dd4c7dc1c6801cfb5";
        const SESS: &str = "8f79a3c7-1125-4096-a03d-feb0d4c10d52";
        let body = |dev: &str, sess: &str| -> serde_json::Value {
            serde_json::json!({
                "model": "claude-opus-5",
                "messages": [],
                "metadata": {"user_id": format!(r#"{{"device_id":"{dev}","account_uuid":"a","session_id":"{sess}"}}"#)}
            })
        };
        let ok = body(DEV, SESS);
        let none = crate::proxy::HeaderMap::new();
        assert!(crate::proxy::cc_identity_well_formed(&none, &ok));
        assert!(
            crate::proxy::cc_identity_well_formed(
                &none,
                &serde_json::json!({"model": "m", "messages": []})
            ),
            "三处都没带算合法（补身份是另一条路的事）"
        );
        assert!(!crate::proxy::cc_identity_well_formed(&none, &body("channel-test", SESS)));
        assert!(!crate::proxy::cc_identity_well_formed(
            &none,
            &body(DEV, "channel-test-claude-code")
        ));
        // 扁平串格式的 device 段同样要 64 位 hex。
        let flat = serde_json::json!({"model": "m", "messages": [], "metadata": {"user_id": format!("user_abc_account_x_session_{SESS}")}});
        assert!(!crate::proxy::cc_identity_well_formed(&none, &flat));

        // 头上的非法值原样看：会话链那条路把它过滤掉了，这里不能跟着丢。
        let mut h = crate::proxy::HeaderMap::new();
        h.insert(
            crate::proxy::HeaderName::from_static("x-claude-code-session-id"),
            HeaderValue::from_static("  channel-test-claude-code "),
        );
        assert_eq!(crate::proxy::incoming_session_id(&h, None), None, "对照：会话链那边会丢掉它");
        assert!(!crate::proxy::cc_identity_well_formed(&h, &ok), "体合法、头非法 → 不合法");
        let mut good = crate::proxy::HeaderMap::new();
        good.insert(
            crate::proxy::HeaderName::from_static("x-claude-code-session-id"),
            HeaderValue::from_static(SESS),
        );
        assert!(crate::proxy::cc_identity_well_formed(&good, &ok));
        let mut blank = crate::proxy::HeaderMap::new();
        blank.insert(
            crate::proxy::HeaderName::from_static("x-claude-code-session-id"),
            HeaderValue::from_static("   "),
        );
        assert!(crate::proxy::cc_identity_well_formed(&blank, &ok), "空白头当没带");

        // 内嵌 JSON 里 session_id / device_id 写了键却不是字串：extract_* 会当成「没带」，但官方
        // 恒为字串，这是抄错了——不合法。透传会把那个 null 原样留在出站 user_id 里。
        for (label, uid) in [
            (
                "session_id 为 null",
                format!(r#"{{"device_id":"{DEV}","account_uuid":"a","session_id":null}}"#),
            ),
            (
                "session_id 为数字",
                format!(r#"{{"device_id":"{DEV}","account_uuid":"a","session_id":42}}"#),
            ),
            (
                "session_id 为对象",
                format!(r#"{{"device_id":"{DEV}","account_uuid":"a","session_id":{{}}}}"#),
            ),
            (
                "session_id 为空串",
                format!(r#"{{"device_id":"{DEV}","account_uuid":"a","session_id":""}}"#),
            ),
            (
                "device_id 为 null",
                format!(r#"{{"device_id":null,"account_uuid":"a","session_id":"{SESS}"}}"#),
            ),
            (
                "device_id 为数字",
                format!(r#"{{"device_id":1,"account_uuid":"a","session_id":"{SESS}"}}"#),
            ),
        ] {
            let bad =
                serde_json::json!({"model": "m", "messages": [], "metadata": {"user_id": uid}});
            assert!(!crate::proxy::cc_identity_well_formed(&none, &bad), "{label}");
        }
        // 首尾空白也是抄错：extract_session_id 会 trim 后认成合法 uuid，这里不能跟着放。
        let padded = serde_json::json!({"model": "m", "messages": [], "metadata": {"user_id": format!(r#"{{"device_id":"{DEV}","account_uuid":"a","session_id":" {SESS} "}}"#)}});
        assert!(
            !crate::proxy::cc_identity_well_formed(&none, &padded),
            "带空白的 session_id 不合法"
        );
        let padded_dev = serde_json::json!({"model": "m", "messages": [], "metadata": {"user_id": format!(r#"{{"device_id":"{DEV} ","account_uuid":"a","session_id":"{SESS}"}}"#)}});
        assert!(
            !crate::proxy::cc_identity_well_formed(&none, &padded_dev),
            "带空白的 device_id 不合法"
        );
        // 键干脆不写是「没带」，合法；内嵌 JSON 只有 account_uuid 也合法。
        let only_account = serde_json::json!({"model": "m", "messages": [], "metadata": {"user_id": r#"{"account_uuid":"a"}"#}});
        assert!(crate::proxy::cc_identity_well_formed(&none, &only_account));
    }

    /// 「是官方客户端」要 UA 与体两头都对得上：UA 自报 `claude-cli/<版本>` **且** `system`
    /// 是 CC 形态（身份声明或 billing header 块，[`is_cc_shaped`]）。只有 UA、体不是 CC
    /// 形态的请求是抄了 UA 的第三方——封号复盘里那批探活请求正是这样——走模拟；`metadata`
    /// 与 session 头本身不构成跳过理由。
    ///
    /// 真 CC 不模拟的代价仍然成立（换头会把它自报的 UA 换掉、`x-stainless-*` 换成抓包机器
    /// 的取值），故 CC 形态的真客户端照旧原样转发。
    #[test]
    fn cc_client_needs_both_ua_and_cc_shape() {
        let plain = Bytes::from(PLAIN_BODY.to_string());

        // 1) metadata.user_id 在、但 UA 不是 claude-cli → 仍走模拟。
        let with_meta = Bytes::from(
            r#"{"model":"claude-opus-5","system":"you are a helpful bot","messages":[],"metadata":{"user_id":"{\"device_id\":\"d0\",\"account_uuid\":\"a0\",\"session_id\":\"11111111-1111-4111-8111-111111111111\"}"}}"#
                .to_string(),
        );
        assert!(detect_for(&with_meta, all_on()).is_some(), "非 CC UA 带 user_id 照样走模拟");

        // 2) user_id 是认不出的格式、UA 不是 claude-cli → 同样走模拟。
        let odd_meta = Bytes::from(
            r#"{"model":"claude-opus-5","messages":[],"metadata":{"user_id":"whatever-new-format"}}"#
                .to_string(),
        );
        assert!(detect_for(&odd_meta, all_on()).is_some(), "非 CC UA 带奇异 user_id 也走模拟");

        // 3) 只带 X-Claude-Code-Session-Id 头、UA 不是 claude-cli → 走模拟。
        let mut cc_header = crate::proxy::HeaderMap::new();
        cc_header.insert(
            crate::proxy::HeaderName::from_static("x-claude-code-session-id"),
            HeaderValue::from_static("bc201916-d0bc-4b4e-adba-caf41fb58746"),
        );
        assert!(
            detect_with(&plain, &cc_header, all_on()).is_some(),
            "非 CC UA 带 session 头也走模拟"
        );

        // 4) 裸请求、裸 UA → 走模拟。
        assert!(detect_for(&plain, all_on()).is_some(), "裸第三方请求仍应走模拟");

        // 5) UA 自报 `claude-cli/<版本>` 但体不是 CC 形态（没 system / 自造 system）→ 抄了
        //    UA 的第三方，**走模拟**。
        const VSCODE_UA: &str = "claude-cli/2.1.226 (external, claude-vscode, agent-sdk/0.3.226)";
        let mut cc_ua = crate::proxy::HeaderMap::new();
        cc_ua.insert(header::USER_AGENT, HeaderValue::from_static(VSCODE_UA));
        assert!(
            detect_with(&plain, &cc_ua, all_on()).is_some(),
            "只有 claude-cli UA、没有 CC 形态的该走模拟"
        );
        assert!(
            detect_with(&with_meta, &cc_ua, all_on()).is_some(),
            "claude-cli UA + 自造 system + 官方格式 user_id 仍该走模拟"
        );
        // 5b) UA 与形态都对上 → **不模拟**，且非模拟路径原样转发它自报的 UA。
        let cc_shaped = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","system":[{{"type":"text","text":"{}"}},{}],"messages":[]}}"#,
            config::CC_SYSTEM_IDENTITY,
            base_block()
        ));
        assert!(
            detect_with(&cc_shaped, &cc_ua, all_on()).is_none(),
            "claude-cli UA + CC 形态的不该走模拟"
        );
        let out = build_forward_headers(&cc_ua, "tok", all_on(), None, None);
        assert_eq!(
            out.get(header::USER_AGENT).and_then(|v| v.to_str().ok()),
            Some(VSCODE_UA),
            "非模拟路径必须原样转发客户端自报的 UA"
        );

        // 6) UA 里读不出 `claude-cli/<版本>` → 走模拟。
        let mut sdk_ua = crate::proxy::HeaderMap::new();
        sdk_ua.insert(header::USER_AGENT, HeaderValue::from_static("python-httpx/0.27.0"));
        assert!(detect_with(&plain, &sdk_ua, all_on()).is_some(), "第三方 UA 仍应走模拟");

        // 7) 模拟路径下客户端原有的 metadata.user_id 被剥掉、由 ensure_cc_metadata 重建，
        //    确保 session_id 与出站头同值。
        let sim = detect_for(&with_meta, all_on()).unwrap();
        let out = rewrite_body(&with_meta, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let user_id = v["metadata"]["user_id"].as_str().expect("应重建 metadata.user_id");
        let inner: serde_json::Value = serde_json::from_str(user_id).unwrap();
        assert_eq!(
            inner["session_id"].as_str().unwrap(),
            &sim.session_id,
            "body 里的 session_id 必须与 sim.session_id 一致"
        );

        // 8) 真实 CC 客户端没带 `metadata.user_id` 时也要补——上游对无 metadata 的请求
        //    走更严的限流通道，不补会裸 429。
        assert!(
            crate::proxy::bare_session_id(&cc_ua, all_on(), None, true, false, &test_cred(), "fp")
                .is_some(),
            "真实 CC 客户端无 metadata 也应补身份"
        );
    }

    /// 基座资产是逐字节从抓包取出来的，别被编辑器/格式化工具动过。
    #[test]
    fn system_base_assets_are_verbatim() {
        assert_eq!(
            config::CC_SYSTEM_BASE_OPUS.len(),
            1214,
            "opus/fable 基座字节数（cap/2.1.258/00012）"
        );
        assert_eq!(
            config::CC_SYSTEM_BASE_SONNET.len(),
            10520,
            "sonnet 基座字节数（cap/2.1.258/00026）"
        );
        assert_eq!(
            config::CC_SYSTEM_BASE_HAIKU.len(),
            10622,
            "haiku 基座字节数（cap/2.1.258/00031）"
        );
        assert_eq!(config::CC_SYSTEM_IDENTITY.len(), 57, "身份句字节数");
        assert_eq!(config::CC_SYSTEM_REPORTING.len(), 911, "reporting 块字节数");
        for base in [
            config::CC_SYSTEM_BASE_OPUS,
            config::CC_SYSTEM_BASE_SONNET,
            config::CC_SYSTEM_BASE_HAIKU,
        ] {
            assert!(
                base.starts_with("\nYou are an interactive agent"),
                "开头那个 \\n 是官方就有的"
            );
            assert!(!base.ends_with('\n'), "结尾多出的换行是编辑器加的，官方没有");
        }
        // 基座是「切点之前」那一段，锚点属于其余段，不该出现在基座里。
        for anchor in config::CC_SYSTEM_BASE_ANCHORS {
            assert!(!config::CC_SYSTEM_BASE_OPUS.contains(anchor), "opus 基座里不该有拆块锚点");
        }
    }

    /// cc_version 后缀与官方客户端的算法对齐（逆向自 2.1.251）：
    /// sha256("59cf53e54c78" + chars_at(4,7,20) + 自报版本).hex()[..3]
    ///
    /// 2.1.258 的五份抓包用户消息都是 "hi"，全为 `1e2`，与算法结论一致。**2.1.260 已经
    /// 证否了这套算法**：六个 profile 各有固定后缀（`222`/`bcd`/…），算法算出来的是 `11d`。
    /// 故这条路只剩 [`crate::proxy::billing_header_text`]（给真实 CC 补 billing header）在用，
    /// 模拟路径改从 [`config::CcProfile::billing_suffix`] 取。
    #[test]
    fn cc_version_suffix_matches_official_algorithm() {
        // 用户消息 "hi"（短于 5 字符），位置 4/7/20 全取不到 → "000"
        // sha256("59cf53e54c780002.1.258") 的前 3 个 hex = "1e2"
        let suffix = |v: &serde_json::Value| crate::proxy::cc_version_suffix(v, "2.1.258");
        let body: serde_json::Value = serde_json::json!({
            "model": "claude-sonnet-5",
            "messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(suffix(&body), "1e2", "短消息 'hi'（cap/2.1.258 五份全是 1e2）");
        // 版本参与摘要：换个版本号，同一条消息就是另一个后缀。给一个 2.1.258 的来访写
        // 2.1.260 的版本，连后缀都会跟着错。
        assert_ne!(
            crate::proxy::cc_version_suffix(&body, "2.1.260"),
            "1e2",
            "版本进摘要，换版本必换后缀"
        );

        // 消息足够长时取 text[4], text[7], text[20]
        let body2: serde_json::Value = serde_json::json!({
            "model": "claude-sonnet-5",
            "messages": [{"role": "user", "content": "abcdefghijklmnopqrstuvwxyz"}]});
        // chars = text[4]='e', text[7]='h', text[20]='u'
        assert_eq!(suffix(&body2).len(), 3, "始终 3 个 hex 字符");

        // content 是数组时取第一个 text 块
        let body3: serde_json::Value = serde_json::json!({
        "model": "claude-sonnet-5",
        "messages": [{"role": "user", "content": [
            {"type": "text", "text": "hi"}
        ]}]});
        assert_eq!(suffix(&body3), "1e2", "数组形式与字符串形式结果一致");

        // 没有 messages 时退化为全 0
        let empty: serde_json::Value = serde_json::json!({"model": "x"});
        assert_eq!(suffix(&empty), "1e2", "无消息退化为 '000' → 同 'hi'");
    }
}
