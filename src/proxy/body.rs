use axum::body::Bytes;
use axum::http::{HeaderMap, header};
use futures_util::StreamExt;
use rand::RngExt;

use crate::config;
use crate::store;

use super::ban::parse_upstream_error;
use super::learned_rules::{DeprecatedFieldMemory, LEARNED_KIND_DEPRECATED, SHAPE_MEMORY_CAP};
use super::session_link::{
    CachePrefix, CcRequestKind, CcSessionKey, CcSessionLink, cache_prefix_stable,
};
use super::simulation::{
    MAX_CACHE_BREAKPOINTS, Simulation, billing_header_text, cap_system_blocks, cc_profile_for,
    cc_profile_kind_for, is_cc_shaped, relocate_long_client_system, simulate_system,
};
use super::thinking::{preserve_thinking_encoding, strip_empty_thinking_blocks};
use super::{count_cache_control, ensure_cc_metadata, insert_top_level};

/// 该路径是否会消耗订阅额度——设备身份校验、出站体改写、裸请求限流计数都只对它生效。
///
/// 排除 `count_tokens`：官方该端点的请求体压根没有 `metadata` 字段（只接
/// model/messages/system/tools/tool_choice/thinking），CC 自然也不会塞，于是
/// [`extract_device_id`] 在这条路径上恒为 `None`——开着设备校验时它 100% 被拒，
/// 客户端的 `/context` 显示与压缩前的 token 预估直接失效。而拦它并没有收益：
/// 不产生 usage、不消耗额度、不返回内容，既无身份可伪装，也本就不该占设备名额。
/// 放行后走 `select_for_device(None)`，即不写绑定、不占名额、按优先级档 + 档内负载
/// 均衡挑一个号——正是想要的语义（计 token 与选中哪个账号无关）。同理它也**不计入**裸请求
/// 速率上限：拿一条不产生 usage、不消耗额度的请求去占名额，只会把真正的请求挤掉。
///
/// **豁免必须精确匹配，且吃的是不含查询串的 `uri.path()`**：这个判定的两端不对称——
/// 判成计费只是多一道校验，判成不计费却是放掉设备校验，所以拿不准时必须倒向计费。
/// 若这里用前缀匹配，`/v1/messages/count_tokens/../` 这类路径就会被判成豁免，而出站 URL
/// 交给 wreq 时点段会按 RFC 3986 归一化掉，上游看到的其实是 `/v1/messages/`——等于给了
/// 一条绕开 `device_limit` 的路。精确匹配后这类路径一律落回计费侧，先过校验再说。
pub(super) fn is_billable_messages(path: &str) -> bool {
    path.starts_with("/v1/messages") && path != "/v1/messages/count_tokens"
}

/// 从请求体提取「客户端设备标识」，用于粘性选择与设备指纹派生。
/// 兼容两种 `metadata.user_id` 格式：
/// - CC 内嵌 JSON（`{"device_id":...}`）：取 `device_id`。
/// - 扁平串 `user_<hash>_account_<acct>_session_<sess>`（如 Windows 客户端）：取 `<hash>`。
///
/// 解析失败或标识为空时返回 `None`（退化为纯优先级选择、不做粘性绑定）。
pub(super) fn extract_device_id(body: Option<&serde_json::Value>) -> Option<String> {
    let user_id = body?.get("metadata")?.get("user_id")?.as_str()?;
    // CC 内嵌 JSON 优先。
    if let Ok(inner) = serde_json::from_str::<serde_json::Value>(user_id)
        && let Some(dev) = inner.get("device_id").and_then(|d| d.as_str())
        && !dev.is_empty()
    {
        return Some(dev.to_string());
    }
    // 退化：扁平串格式，取 device 段。
    let flat = parse_flat_user_id(user_id)?;
    (!flat.device.is_empty()).then_some(flat.device)
}

/// 从请求体提取会话标识，兼容与 [`extract_device_id`] 相同的两种 `metadata.user_id` 格式
/// （内嵌 JSON 的 `session_id` 字段 / 扁平串的 `_session_` 段）。
///
/// **体里那个会话 id 只有这一个解析器。** 曾经还有一份只认内嵌 JSON 的副本，于是
/// Windows 那种扁平串（`user_<hash>_account_<acct>_session_<uuid>`）在
/// [`incoming_session_id`] 与 [`session_id_conflict`] 眼里等于「体里没有会话 id」——
/// 头体不一致检测对整整一类客户端形同虚设，默认拒的开关也拦不住。两种格式的差异只该
/// 在一个函数里，别再复制一份。
pub(super) fn extract_session_id(body: Option<&serde_json::Value>) -> Option<String> {
    let user_id = body?.get("metadata")?.get("user_id")?.as_str()?;
    if let Ok(inner) = serde_json::from_str::<serde_json::Value>(user_id)
        && let Some(sid) = inner.get("session_id").and_then(|s| s.as_str()).map(str::trim)
        && !sid.is_empty()
    {
        return Some(sid.to_string());
    }
    let flat = parse_flat_user_id(user_id)?;
    let sid = flat.session.trim();
    (!sid.is_empty()).then(|| sid.to_string())
}

/// 来访体里有没有 `metadata.user_id`。
///
/// 与 [`extract_device_id`] 的区别：那个要求能**解析出设备标识**，格式认不出就是 `None`；
/// 这里只问「这个字段在不在」——决定的是要不要给它补一份官方身份（见 [`ensure_cc_metadata`]），
/// 而字段已经在的话，改写它是 [`spoof_identity`] 的活，两条路只能有一条动它。
pub(super) fn body_has_user_id(body: Option<&serde_json::Value>) -> bool {
    body.and_then(|v| Some(v.get("metadata")?.get("user_id")?.is_string())).unwrap_or(false)
}

/// 扁平 `metadata.user_id` 的三段：`user_<device>_account_<account>_session_<session>`。
///
/// [`spoof_identity`] 只用 device 与 session（account 段由凭证真实值覆盖），
/// [`outbound_identity`] 三段都要——它读的是**已经发出去**的那份，不能再替换任何一段。
pub(super) struct FlatUserId {
    device: String,
    account: String,
    session: String,
}

/// 解析扁平 user_id；不匹配该形态时返回 `None`。
/// 按标记切分，允许 account 段为空（`account__session`）。
pub(super) fn parse_flat_user_id(s: &str) -> Option<FlatUserId> {
    let rest = s.strip_prefix("user_")?;
    let (device, rest) = rest.split_once("_account_")?;
    let (account, session) = rest.split_once("_session_")?;
    Some(FlatUserId {
        device: device.to_string(),
        account: account.to_string(),
        session: session.to_string(),
    })
}

/// 来访有没有要流式响应（顶层 `stream:true`）。
///
/// **口径与上游一致**：只有布尔 `true` 算流式。字段缺失、`false`、以及 `"true"` 这种字符串
/// 都不是——上游那边它们同样得到一份整段 JSON，判断口径跟着响应形态走才不会错配。
pub(super) fn stream_requested(body: &serde_json::Value) -> bool {
    body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false)
}

/// 把顶层 `stream` 置为 `true`；已经是 `true` 就返回 `false`（无改动）。
///
/// 位置由 `preserve_order` 保证：字段已在则原位改值，不在则追加到末尾——而官方线序里
/// `stream` 本来就是最后一个（见 [`insert_top_level`] 的说明），两条路都落在官方位置上。
pub(super) fn set_stream_true(v: &mut serde_json::Value) -> bool {
    let Some(obj) = v.as_object_mut() else { return false };
    if obj.get("stream").and_then(|s| s.as_bool()) == Some(true) {
        return false;
    }
    obj.insert("stream".into(), serde_json::Value::Bool(true));
    true
}

/// 给出站 URL 补上官方客户端恒带的 `?beta=true`（已经有 `beta=` 就原样返回）。
///
/// **依据**：`cap/raw` 八份抓包（四份直连、四份经 luban 的 API-key 模式）的请求行**无一例外**
/// 是 `POST /v1/messages?beta=true`。而 Anthropic 公开的 API 里没有这个参数——文档与各语言 SDK
/// 一律发裸 `/v1/messages`，beta 能力全靠 `anthropic-beta` 头开。两边合起来说明它是 **CC 客户端
/// 自己的标记**，不是 beta 功能的开关：补它是形态对齐，漏它不影响功能（模拟路径现在就能用）。
///
/// 只在[`Simulation`]那条路上补——那条路已经把头和体整套装成了 CC，URL 上再漏掉这个参数，
/// 就是「头上声明了一整串官方 beta、URL 却没开 beta 模式」这种真实客户端不产生的组合。
///
/// 客户端自己写了 `beta=`（含 `beta=false`）时不动：那是它自己的选择，替它改属于越权。
pub(super) fn ensure_beta_query(url: &str) -> String {
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or("");
    if query.split('&').any(|kv| kv.split_once('=').map(|(k, _)| k) == Some("beta")) {
        return url.to_string();
    }
    let sep = if query.is_empty() { '?' } else { '&' };
    format!("{url}{sep}beta=true")
}

/// `body` 里有没有出现过这串字节。给 [`rewrite_body`] 的入口快速路径用：拿字面量粗筛
/// 「要不要解析」比解析一遍便宜得多。
///
/// 单独一个函数是为了**让窗口宽度不可能写错**：原先三处各自写着
/// `body.windows(N).any(|w| w == b"…")`，其中 `"role":"system"` 那处的 `N` 比字面量宽了一位，
/// 比较恒为 `false`，那一项白白当了一版死代码。
pub(super) fn body_contains(body: &[u8], needle: &[u8]) -> bool {
    body.windows(needle.len()).any(|w| w == needle)
}

/// 体里有没有 `"键": "值"` 这一对，**键与冒号、冒号与值之间允许任意 JSON 空白**
/// （空格、制表、换行、回车）。`key` / `value` 都要自带引号，如
/// `body_has_pair(body, b"\"role\"", b"\"system\"")`。
///
/// 粗筛为什么要容空白：缩进过的请求体（不少中转、SDK 的调试模式会 pretty-print）里写的是
/// `"role": "system"`，按紧凑字面量找一定落空，[`rewrite_body`] 的快速路径就直接原样返回，
/// 空壳 system 的清理与空 text 块的剥除全被跳过——判据不该取决于客户端的缩进风格。
///
/// 仍然只认**字面量形态的键与值**：把键写成 `"\u0072ole"` 这种转义的绕得过去。现实里没有
/// 客户端这么发（serde / encoding/json / Python 的 json 都不转义 ASCII 字母），真出现了也只是
/// 退回「不解析、原样转发」，上游照常给它一条 400，不会得出错误的结论。
pub(super) fn body_has_pair(body: &[u8], key: &[u8], value: &[u8]) -> bool {
    let is_ws = |b: u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r');
    body.windows(key.len()).enumerate().any(|(i, w)| {
        if w != key {
            return false;
        }
        let mut j = i + key.len();
        while body.get(j).is_some_and(|&b| is_ws(b)) {
            j += 1;
        }
        if body.get(j) != Some(&b':') {
            return false;
        }
        j += 1;
        while body.get(j).is_some_and(|&b| is_ws(b)) {
            j += 1;
        }
        body.get(j..).is_some_and(|rest| rest.starts_with(value))
    })
}

/// 转发前改写请求体，各项分别受 [`store::ForwardFlags`] 里的开关控制（默认全开；全关即
/// 请求体逐字节原样转发）：
///
/// 0. **模拟**（`simulate_cc`，仅当 `sim` 为 `Some`，即来访不是 CC 形态）：补上官方
///    `system` 前缀与 `metadata` 身份，见 [`Simulation`]。它先跑——后面几项都是在
///    「已经是 CC 形态」的前提下做微调。
/// 1. **system 形态**（`system_shape`）：把 API-key 模式的 3 块改写成订阅模式的 4 块，
///    见 [`align_system_shape`]。含拆块与基座标 `scope:"global"`（后者另受
///    `cache_scope_global` 管）。模拟路径已经直接产出 5 块，故两者互斥，不叠加。同一开关还管**块数封顶**
///    （[`cap_system_blocks`]）：超过 [`MAX_SYSTEM_BLOCKS`] 块的 `system` 会被上游判成第三方应用、改扣超额池。
/// 2. **身份伪装**（`spoof_identity`）：把 `metadata.user_id` 里的 `account_uuid`/`device_id`
///    换成该凭证自洽的身份（真实 account_uuid + 由其稳定派生的 device_id），避免
///    「真账号 + 陌生设备」的矛盾。它也管着模拟路径的 `metadata` 注入——凭空造一份身份，
///    本来就是同一件事。
/// 3. **cch**（`billing_cch`）：给 `x-anthropic-billing-header` 补订阅模式独有的 `cch`。
/// 4. **流式化**（`force_stream`，由 `nonstream_as_sse` 拨）：把 `stream` 置成 `true`。
///    官方 CC 恒为 `true`，回程由 [`aggregate_sse`] 聚合回整段 JSON，客户端无感。
///
/// **key 顺序**：改写要把 body 重新序列化，serde_json 默认的 `Map = BTreeMap` 会把**整个
/// body**（含 tools/messages/content/cache_control 里每一个对象）的 key 按字母序重排，得到
/// 官方客户端不会产生的排列——集合对了顺序错，一次精确比对即可判定中间有代理。故本 crate
/// 开了 serde_json 的 `preserve_order`（见 Cargo.toml），解析出的顺序原样写回，
/// 新增字段追加在末尾。回归测试见 [`tests::preserves_key_order`]。
///
/// 解析失败或结构异常时原样返回——绝不因改写失败而阻断转发。
// 参数多是有意的：这些全是「一次改写要知道的上下文」，打包成结构体只会多一层间接，
// 而调用点只有 `Upstream::shape` 一处。
#[allow(clippy::too_many_arguments)]
pub(super) fn rewrite_body(
    body: &Bytes,
    cred: &crate::credentials::Credential,
    device_fp: &str,
    flags: store::ForwardFlags,
    sim: Option<&Simulation>,
    bare_session: Option<&str>,
    // 出站两处要落的同一个会话 id（非模拟路径），见 [`outbound_session_id`]。头那侧由
    // [`build_forward_headers_for`] 落，体这侧由 [`sync_metadata_session`] 落。
    session_out: Option<&str>,
    force_stream: bool,
    tool_names: Option<&ToolNameMap>,
    // 出站头里已经带了 `thinking-display-updates` beta（由调用方从实际发出的头上判定）。
    // 只有它为真，fable 请求的 body 才补 `thinking.display:"updates"`，见 [`fill_thinking_display`]。
    display_beta: bool,
    // 出站头里已经带了 `advanced-tool-use` beta（同样由调用方从实际发出的头上判定）。
    // 真 CC 路径只有它为真才给工具补 `eager_input_streaming`，见 [`eager_tools_wanted`]。
    adv_beta: bool,
    // 来访**自报**的客户端版本（`claude-cli/x.y.z`，解不出为 `None`）。只用在给真实 CC
    // 补 billing header 时，见 [`ensure_cc_system_prefix`]。
    client_version: Option<&str>,
    // 真实 CC 来访要补的会话关联字段（`cc_prev_req` / `cc_prompt_id` /
    // `diagnostics.previous_message_id`）；模拟路径为 `None`，它的链在 `sim.link` 里。
    // 判据见 [`client_session_link`]。
    client_link: Option<&CcSessionLink>,
    // 这条来访属于哪一类官方 profile，见 [`CcRequestKind`]。
    cc_kind: CcRequestKind,
    // 要补的 `fallbacks` 字面量（[`refusal_fallbacks_for`]），`None` 即不补、只归一形态。
    fallbacks: Option<&str>,
) -> Bytes {
    // `system_shape` 不连着 `merge_beta`：它只负责拆块，而裸的 `{"type":"ephemeral"}` 是 GA
    // 能力，不需要任何 beta 声明。断点上那两项可选字段才各自要一个 beta。
    let shape = flags.system_shape;
    // `scope:"global"` 要 `prompt-caching-scope-2026-01-05`、`ttl:"1h"` 要
    // `extended-cache-ttl-2025-04-11`，两个都由 `merge_beta` 补。故各自的开关之外还得叠上
    // 它——否则就是「body 里写了字段、头上没声明」的自相矛盾。
    let cache = CacheShape {
        global: flags.cache_scope_global && flags.merge_beta,
        ttl_1h: flags.cache_ttl_1h && flags.merge_beta,
    };
    // 全关且不模拟：连解析都不必做，原样返回。
    // 额外检查：body 里含 allOf/oneOf/anyOf 或空 text 块时仍需解析（须对应开关开着）。
    // 要补 `fallbacks` 的也不能走这条：调用方已按同一个判断在头上补了 `server-side-fallback`
    // beta（[`build_forward_headers_for`]），体里不写字段就是「有 beta 没字段」——对 fable 而言
    // 恰是官方 2.1.260 之前的旧形态，且拒答时上游不会换模型重跑，开关等于没开。
    let may_need_schema_fix = flags.flatten_tool_schemas
        && [b"allOf", b"oneOf", b"anyOf"].iter().any(|n| body_contains(body, *n));
    let may_have_empty_text = flags.strip_empty_text && body_has_pair(body, b"\"text\"", b"\"\"");
    // 体里出现过 `"role": "system"` 的一律解析：`hoist_system_role` 关着时提升那步不跑，但
    // **空壳照丢**（[`drop_empty_system_messages`]，上游对它恒 400），所以这一项不挂在那个
    // 开关上。键值之间的空白由 [`body_has_pair`] 容掉，缩进过的体不会从这里漏过去。
    let has_system_role_msg = body_has_pair(body, b"\"role\"", b"\"system\"");
    // 真 CC 路径要不要补 `eager_input_streaming` 得解析了才知道；没有 `tools` 字面量的体
    // 一定不补，不必为它解析。
    let may_fill_eager =
        flags.eager_tool_streaming && adv_beta && body_contains(body, b"\"tools\"");
    if sim.is_none()
        && !shape
        && !may_fill_eager
        && !flags.spoof_identity
        && !flags.billing_cch
        && !flags.strip_extra_fields
        && !force_stream
        && tool_names.is_none()
        && !may_need_schema_fix
        && !may_have_empty_text
        && !has_system_role_msg
        && fallbacks.is_none()
    {
        return body.clone();
    }
    // 补 metadata 用的 session_id：模拟模式取 Simulation 那份，CC 形态来访取 `bare_session`
    // （见 [`Upstream::bare_session`]）。两者都与出站头上的 `X-Claude-Code-Session-Id` 同值。
    let meta_session = sim.map(|s| s.session_id.as_str()).or(bare_session);
    let mut v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return body.clone(),
    };
    // 空壳 `role:"system"` 消息：一个内容块都没有的那种，上游恒 400
    // （`messages.N: system content must contain at least one block`）。放在提升之前，
    // 两条路都要过它——见 [`drop_empty_system_messages`] 里为什么不受 `hoist_system_role`
    // 与 CC 形态那道豁免管。
    let empty_system_dropped = drop_empty_system_messages(&mut v);
    // role:"system" 提升：litellm 等第三方客户端把 system 放在 messages 里，
    // 上游不支持该 role，提前挪到顶层 system 字段。必须在 simulate_system 之前——
    // 后者和 align_system_shape 都只读顶层 system。
    // CC 形态的请求跳过：CC 在 messages 里合法使用 role:"system"（如 deferred tools），
    // 强行提升会破坏形态。
    let system_hoisted =
        flags.hoist_system_role && !is_cc_shaped(&v) && hoist_system_role_messages(&mut v);
    // 来访自己是不是 CC 形态要在模拟之前看——模拟一跑，body 就都是 CC 形态了。
    // 只喂给 `strip_extra_fields` 判 `thinking.display` 该不该剥。
    let cc_inbound = is_cc_shaped(&v);
    let simulated = sim.is_some_and(|sim| simulate_system(&mut v, sim, cache));
    // 模拟后末块是客户端的自有 system。上游对该块有内容级检测——非 CC 特征内容超过
    // ~2000 字符就触发第三方判定。把超长内容移到 messages 首条用户消息里，末块只留
    // 一个短占位，绕过内容检测且不丢失指令语义。
    let sys_relocated = simulated && sim.is_some_and(|s| relocate_long_client_system(&mut v, s));
    // `context_management` 只补在模拟路径上：声明它的 `context-management-2025-06-27` 出自模拟
    // seed，而 [`Simulation::detect`] 本身就要求 `merge_beta` 开着，故「体里有 `edits`、头上没
    // 声明」这个反向矛盾在这条路上构造不出来——不必像 `scope_global` 那样再叠一次 `merge_beta`。
    // 模拟路径下客户端没发 `thinking` 时补上官方默认值。官方 CC 恒带
    // `thinking: {type: "enabled", budget_tokens: N}`，缺了等于自证不是 CC。
    // 放在 `ensure_context_management` 之前：后者依赖 `thinking` 才补 `context_management`。
    let thinking_filled =
        flags.inject_thinking && sim.is_some_and(|sim| ensure_thinking(&mut v, sim.profile));
    // 真实 CC（API-key 模式）的 fable 请求：头上 `merge_beta` 补了 `thinking-display-updates`，
    // body 侧才配套补 `thinking.display:"updates"`（订阅端官方形态，`cap/2.1.258/00013`）。
    // `display_beta` 取自实际发出的头——头上没那项 beta 时体里写 `updates` 是一发稳定 400
    // （agent-sdk / VSCode 扩展的 beta 串没有 `advisor-tool`，`merge_beta` 不给它补）。
    let display_filled = sim.is_none()
        && cc_inbound
        && flags.merge_beta
        && display_beta
        && fill_thinking_display(&mut v);
    let ctx_mgmt = sim.is_some() && ensure_context_management(&mut v);
    // `diagnostics.previous_message_id`：官方主线程**每条**都带（首轮是 null），
    // 见 [`ensure_diagnostics`]。只给带 billing header 的 profile 补——额度探测、标题生成
    // 与安全分类官方都不发这个字段。
    let diag =
        sim.is_some_and(|s| s.profile.has_billing_header() && ensure_diagnostics(&mut v, &s.link));
    // `fallbacks`：调用方给了字面量就补（[`ensure_fallbacks`]），没给只归一形态
    // （[`normalize_fallbacks`]）。
    let fallbacks_shaped = match fallbacks {
        Some(plan) => ensure_fallbacks(&mut v, plan),
        None => sim.is_some_and(|s| normalize_fallbacks(&mut v, s.profile)),
    };
    // 官方那第三个断点在最后一条消息上，模拟路径此前从不碰 `messages`，故要补。
    // 跟在 `simulate_system` 之后：断点预算得把它已经用掉的那些算进去。
    // 只对带 `system` 的 profile 补：额度探测那条官方一个断点都没有（`cap/2.1.260-2/00004`），
    // 给它标一个反倒是新破绽。
    let msg_shape =
        sim.is_some_and(|s| s.profile.has_billing_header()) && align_message_shape(&mut v, cache);
    // 真 CC 来访 `messages` 里一个断点都没有时也补一个，最小改动、抄客户端自己的 ttl，
    // 见 [`ensure_cc_message_breakpoint`]。额度探测不补（官方那条一个断点都没有）。
    // **只在 tools + system 与上一轮相同时补**（[`cache_prefix_stable`]）：前缀变了，后面的
    // messages 标了断点也是未命中，只会把裸算换成更贵的写入；会话第一轮同样不补。
    let cc_msg_shape = shape
        && sim.is_none()
        && cc_inbound
        && cc_kind.allows_system_prefix()
        && session_out.is_some_and(|sid| {
            cache_prefix_stable(
                CcSessionKey { cred_id: cred.id, session_id: sid },
                cache_prefix_of(&v),
            )
        })
        && ensure_cc_message_breakpoint(&mut v);
    // 模拟已经产出官方的 5 块形态，再走一遍三块拆分器只会切错地方。
    let shaped = shape && !simulated && align_system_shape(&mut v, cache);
    // CC 子代理/desktop-3p 有时不带 billing header，上游按第三方计、限流更严。
    // 补上 billing + 身份句让上游按订阅额度计。放在 ensure_billing_cch 之前——后者给
    // billing header 追加 cch，得先有 billing header 它才有东西追加。
    // simulate_cc 开着但这条请求没走模拟（CC 客户端）→ 可能缺 billing header。
    // simulate_cc 关着时不注入——用户明确不要模拟，不该凭空加 system。
    //
    // 判据是 `sim.is_none()` 而不是「`simulate_system` 有没有动过」：额度探测那个 profile
    // 官方就不发 `system`（[`config::CcSystemShape::None`]），模拟路径特意没给它造，
    // 这里再补一条 billing header 就把刚省下的形态又加了回去。
    // **额度探测不补**（[`CcRequestKind::allows_system_prefix`]）：官方那条没有 `system`、
    // 没有 billing header，补一份就把一条 `max_tokens:1` 的探测改成了「带身份声明的请求」。
    // 判在这里而不是靠 `has_billing` 早退——那个判据只看「有没有 billing header」，
    // 而额度探测正是「本来就不该有」的那一类。
    let prefix_injected = flags.simulate_cc
        && sim.is_none()
        && cc_kind.allows_system_prefix()
        && ensure_cc_system_prefix(&mut v, client_version, cc_kind);
    let cch_added = flags.billing_cch && ensure_billing_cch(&mut v);
    // 真实 CC 来访的会话关联字段：API-key 端一个都不发，而订阅端官方每条主线程请求都有。
    // 跟在 `ensure_billing_cch` 之后——官方段序是 `cch` 在前、这两项在后。
    let link_added = client_link.is_some_and(|l| {
        // 没有 billing header 就整条不补：那种请求（额度探测那类）官方连 `cc_version` 都
        // 不发，单给它一个 `diagnostics` 反而造出一个新组合。判在 `append_billing_link`
        // 之外，因为后者「两项都已经在了」也返回 false，那种情况 `diagnostics` 还是要补。
        if !has_billing_header(&v) {
            return false;
        }
        let billing = append_billing_link(&mut v, l);
        // `diagnostics` 同样是订阅端才有的（`cap/2.1.258-api` 六份一个都没有）。
        let diag = l.diagnostics && ensure_diagnostics(&mut v, l);
        billing || diag
    });
    // 封顶排在**所有会增加 system 块数的步骤之后**：两条整形产出的都是 ≤5 块，而补前缀会在
    // 最前面加一到两块。原先它排在补前缀之前，一条客户端 5 块、没 billing header 的来访
    // （现网 2.1.238，req_ujomarOOPtXL38jx）过了封顶再被补成 6 或 7 块，出站正好超过
    // `MAX_SYSTEM_BLOCKS`，上游按第三方应用计——封顶本来要防的正是这个。
    let capped = shape && cap_system_blocks(&mut v);
    // 收尾：把客户端自己那些断点的 `ttl` 也补齐，否则就是「system 有、消息没有」这种官方
    // 不产生的半对齐（见 [`fill_cache_ttl`]）。放在所有整形之后，才能覆盖到全部断点。
    //
    // **只在整形真的成了才补**：`ttl:"1h"` 属于订阅形态，API-key 的三块形态官方
    // 一个 ttl 都不带（`cap/raw/00012`）。整形没做成（比如锚点漂了、`system_shape` 关着）
    // 时 body 还是三块，这时补 ttl 就是把半对齐换了个方向，比不补更糟。
    let ttl_filled = cache.ttl_1h && (simulated || shaped) && fill_cache_ttl(&mut v);
    tracing::debug!(
        metadata = %v.get("metadata").map(|m| m.to_string()).unwrap_or_else(|| "<none>".into()),
        "inbound metadata"
    );
    // 模拟路径下客户端可能已带 `metadata.user_id`——它的 session_id 是客户端原值，
    // 而出站头上的 `X-Claude-Code-Session-Id` 取自 `sim.session_id`。两处不同值就是
    // 官方不产生的矛盾，先剥掉再让 `ensure_cc_metadata` 用 sim.session_id 重建。
    //
    // **必须和重建同一个条件。** 剥这一步原先只看 `sim.is_some()`，而下面重建那步要
    // `flags.spoof_identity`：用户一旦关掉身份伪装，客户端自己带的 device/account/session
    // 就被删掉、且没人补回来——头上还有会话 id、体里却什么都没有。那既违背这个开关的语义
    // （「别改身份」被执行成了「把身份删了」），也违背客户端数据透传契约。
    if sim.is_some()
        && flags.spoof_identity
        && let Some(meta) = v.get_mut("metadata").and_then(|m| m.as_object_mut())
    {
        meta.remove("user_id");
        if meta.is_empty() {
            v.as_object_mut().map(|o| o.remove("metadata"));
        }
    }
    let sim_meta = flags.spoof_identity
        && meta_session.is_some_and(|sid| ensure_cc_metadata(&mut v, cred, device_fp, sid));
    let spoofed =
        flags.spoof_identity && spoof_identity(&mut v, cred, device_fp, flags.spoof_device_id);
    // 客户端自带的那份 user_id 里，会话段要和出站头同值。跟在 [`spoof_identity`] 之后——
    // 那一步刻意保留 session 段，这一步只在「luban 选的和它写的不是一个」时才动它。
    //
    // 与 `sim_meta`/`spoofed` 同一道闸（`spoof_identity`）：这仍是在改客户端写的身份字段，
    // 用户把身份伪装整个关掉时，体照旧原样透传（头那侧的归一不受此闸影响——它落的就是
    // 客户端自己给的那个合法值，见 [`outbound_session_id`]）。
    let session_synced = flags.spoof_identity
        && sim.is_none()
        && session_out.is_some_and(|sid| sync_metadata_session(&mut v, sid));
    // 流式化：`stream` 在官方线序里就在队尾，来访带了它就原位改值、没带就追加，两条路
    // 落点都与官方一致（`preserve_order` 下 `insert` 对已有键不动位置）。
    let streamed = force_stream && set_stream_true(&mut v);
    // OpenAI 风格的 `tool_choice`（字符串 `"auto"`/`"none"`/`"required"`、`null`，或
    // `{"type":"function","function":{"name":…}}`）归一成 Anthropic 的对象形态——上游对非对象
    // 直接 400 `tool_choice: Input should be an object`。**无条件做**：这种形态在 Anthropic 这边
    // 永远无效，没有「保留原样」的价值。放在剥字段之前：归一出来的 `{"type":"auto"}` 正好由
    // 下一步按缺省剥掉。
    let tool_choice_normalized = normalize_tool_choice(&mut v);
    // 剥掉官方不发的顶层字段。放在最后：前面几步只增不减，剥这一步与它们无交集，
    // 摆在队尾就不必操心谁先谁后。
    // `display` 的去留：来访本来就是 CC 形态，或 `thinking` 整个是刚按官方形态补的，都留。
    let stripped =
        flags.strip_extra_fields && strip_extra_fields(&mut v, cc_inbound || thinking_filled);
    // 来访已有的顶层字段仍可能带着第三方客户端的键序。模拟路径既然已在整体
    // 替换客户端形态，就在所有增删之后对齐整个顶层对象，不只安排 luban 新增的键。
    let top_level_ordered =
        sim.is_some_and(|sim| align_cc_top_level_order(&mut v, sim.profile.body_key_order));
    // 模拟路径把官方主线程恒带的 11 个真工具（[`cc_tools_core`]）对齐进工具列表：客户端没
    // 声明的补上，声明了的同名工具换成官方那条，其余原样。上游判第三方的信号之一是
    // 「自称 CC 但没有 CC 工具」，光加 mcp__ 前缀不够——零个 CC 工具等于自证不是 CC；而只注
    // 四个也不是官方形态：2.1.258 / 2.1.260 / 2.1.270 的主线程抓包最少 13 个工具。注入的
    // 工具在白名单内，混淆不会动它们。
    //
    // **只给主线程 profile 注**：官方的标题生成、安全分类、无工具 helper 与额度探测本来就
    // 一个工具都不发（`tools: []` 或整个字段都没有），给它们塞 Bash 是把一条辅助请求装成
    // 了主线程。判据是 profile，不是「有没有 tools 字段」。
    let cc_tools_injected =
        sim.is_some_and(|s| s.profile.has_billing_header() && inject_cc_tools(&mut v, s.profile));
    // 工具声明的 `eager_input_streaming`：跟在注入之后——注入的官方工具资产自带正确取值
    // （opus 那份带、fable 那份不带），这一步只管客户端自己声明、保留下来的那些。条件来源两条
    // 路径不同（真 CC 看来访版本 × 模型 × 用途，模拟看出站 profile），规则共用，见
    // [`eager_tools_wanted`] 与 [`fill_eager_tools`]。
    let eager_filled = flags.eager_tool_streaming
        && eager_tools_wanted(&v, sim, cc_inbound, cc_kind, client_version, adv_beta)
        && fill_eager_tools(&mut v);
    // 工具去重：客户端可能声明同名工具多次，上游会直接拒（`Tool names must be unique`）。
    // 放在混淆之前：混淆依赖 `tools` 里的名字集合算 seed，重复名进去会白占一个序号。
    let tools_deduped = dedup_tools(&mut v);
    // 空 text 块剥除：上游要求 text 块非空，第三方客户端常发空块。
    let empty_text_stripped = flags.strip_empty_text && strip_empty_text_blocks(&mut v);
    // 无签名空 thinking 块剥除：上游要求 thinking 块有内容或有签名。带签名的空块是上游
    // 自己回的合法形态（官方 CC 原样回传，上游接受），不动；只剥第三方客户端拼出来的
    // 无签名空块。无条件处理——这种块永远是无效的。
    let empty_thinking_stripped = strip_empty_thinking_blocks(&mut v);
    // input_schema 顶层的 allOf/oneOf/anyOf 展平：上游不支持，直接 400。
    let schemas_flattened = flags.flatten_tool_schemas && flatten_tool_schemas(&mut v);
    // 工具名混淆放在最末：它只改 `name` 字段，与前面每一步都无交集。
    let tools_mimicked = tool_names.is_some_and(|m| apply_tool_names(&mut v, m));
    tracing::debug!(
        empty_system_dropped,
        system_hoisted,
        simulated,
        sys_relocated,
        sim_meta,
        shaped,
        capped,
        spoofed,
        session_synced,
        prefix_injected,
        cch_added,
        link_added,
        thinking_filled,
        display_filled,
        ctx_mgmt,
        diag,
        fallbacks_shaped,
        msg_shape,
        cc_msg_shape,
        ttl_filled,
        streamed,
        tool_choice_normalized,
        stripped,
        top_level_ordered,
        cc_tools_injected,
        eager_filled,
        tools_deduped,
        empty_text_stripped,
        empty_thinking_stripped,
        schemas_flattened,
        tools_mimicked,
        device_fp = %device_fp,
        spoof_device = %cred.spoof_device_id(device_fp).as_deref().unwrap_or("-"),
        "rewrote body"
    );
    if !empty_system_dropped
        && !system_hoisted
        && !shaped
        && !capped
        && !spoofed
        && !cch_added
        && !link_added
        && !simulated
        && !sys_relocated
        && !sim_meta
        && !thinking_filled
        && !display_filled
        && !ctx_mgmt
        && !diag
        && !fallbacks_shaped
        && !msg_shape
        && !cc_msg_shape
        && !ttl_filled
        && !streamed
        && !tool_choice_normalized
        && !stripped
        && !top_level_ordered
        && !cc_tools_injected
        && !eager_filled
        && !tools_deduped
        && !empty_text_stripped
        && !empty_thinking_stripped
        && !schemas_flattened
        && !tools_mimicked
    {
        return body.clone();
    }
    match serde_json::to_vec(&v) {
        Ok(bytes) => Bytes::from(preserve_thinking_encoding(body, bytes)),
        Err(_) => body.clone(),
    }
}

/// 这条请求的客户端工具该不该补 `eager_input_streaming: true`。证据与规则见
/// [`config::CcEagerTools`]，两条路径的**条件来源不同**：
///
/// - **真 CC 路径**（`sim` 为 `None`、来访是 CC 形态）：按来访**自报的版本 × 模型**查证据
///   （[`config::cc_eager_tools_at`]，版本要**精确命中**抓包那一版，不沿 beta 参照那套
///   「落回最近一版」的兜底——2.1.270 的 opus 没样本就不补），且用途得是主线程或猜下一句（[`CcRequestKind::Main`] /
///   [`CcRequestKind::Suggestion`]——2.1.258/00025 那条猜下一句同样全带；子代理 / helper /
///   标题 / 分类没有主线程的证据，不猜），且出站头里真有 `advanced-tool-use`（带 eager 的官方
///   请求头上都有它，API-key 端两样都没有，只补体不补头就是另一个官方不产生的组合）。
///   读不出版本一律不补——不知道是哪一版就没法查表。
/// - **模拟路径**：按**出站的模拟 profile** 判，与来访客户端自报什么版本无关（出站 UA 是
///   profile 那一版）。profile 记了 On/Off 就照记的；[`config::CcEagerTools::Unknown`]
///   （2.1.260 的 sonnet / haiku 外推行）跟随注入的那份官方工具资产（[`cc_tools_core`]）——
///   资产带则客户端保留的工具也带，一条请求里不出现「注入的带、客户端的不带」。只给主线程
///   profile 判（`has_billing_header`，与注入同一道闸）。
fn eager_tools_wanted(
    v: &serde_json::Value,
    sim: Option<&Simulation>,
    cc_inbound: bool,
    cc_kind: CcRequestKind,
    client_version: Option<&str>,
    adv_beta: bool,
) -> bool {
    use config::CcEagerTools::{Off, On, Unknown};
    match sim {
        Some(sim) => {
            let profile = sim.profile;
            if !profile.has_billing_header()
                || !profile.beta.split(',').any(|b| b.trim() == config::CC_BETA_ADVANCED_TOOL_USE)
            {
                return false;
            }
            match profile.eager_tools {
                On => true,
                Off => false,
                Unknown => {
                    let asset = cc_tools_core(profile);
                    !asset.is_empty()
                        && asset.iter().all(|t| {
                            t.get("eager_input_streaming").and_then(|e| e.as_bool()) == Some(true)
                        })
                }
            }
        }
        None => {
            if !cc_inbound
                || !adv_beta
                || !matches!(cc_kind, CcRequestKind::Main | CcRequestKind::Suggestion)
            {
                return false;
            }
            let Some(version) = client_version.and_then(parse_version) else { return false };
            let Some(model) = v.get("model").and_then(|m| m.as_str()) else { return false };
            config::cc_eager_tools_at(cc_profile_kind_for(model), Some(version)) == On
        }
    }
}

/// 给客户端声明的**内建形态**工具补 `eager_input_streaming: true`。该不该补由
/// [`eager_tools_wanted`] 定，这里只管「补到哪些工具上」，两条路径共用：
///
/// - 客户端已写了这个键（`true` 或 `false`）的不覆盖——那是它的显式设置；
/// - 只补**有 `input_schema`、名字不以 `mcp__` 开头、没有 `defer_loading: true`** 的工具：
///   订阅端样本里 MCP 工具全在延迟池里、正文里一个没有，带不带无从证实；`DeferredToolPlaceholder`
///   占位在每条样本里都不带；服务端工具（`type: web_search_…`）没有 `input_schema`，也没有
///   带着 eager 的样本。没有证据的工具类型不猜。
///
/// 键追加在对象末尾：官方声明序是 `name, description, input_schema, eager_input_streaming`
/// （`cap/2.1.258/00012` 每一条）。
pub(super) fn fill_eager_tools(v: &mut serde_json::Value) -> bool {
    let Some(tools) = v.get_mut("tools").and_then(|t| t.as_array_mut()) else { return false };
    let mut changed = false;
    for tool in tools.iter_mut() {
        let Some(obj) = tool.as_object_mut() else { continue };
        let builtin_name =
            obj.get("name").and_then(|n| n.as_str()).is_some_and(|n| !n.starts_with("mcp__"));
        let deferred = obj.get("defer_loading").and_then(|d| d.as_bool()) == Some(true);
        if !builtin_name
            || deferred
            || !obj.contains_key("input_schema")
            || obj.contains_key("eager_input_streaming")
        {
            continue;
        }
        obj.insert("eager_input_streaming".into(), serde_json::Value::Bool(true));
        changed = true;
    }
    changed
}

/// 裸客户端（无 `metadata.user_id`）在请求日志里用的设备标识：出站那份**伪装** device_id，
/// 加 `sim:` 前缀。没伪装过就返回 `None`（日志照旧是 `-`）。
///
/// **只在真伪装过时才记**：要求 [`ensure_cc_metadata`] 确实把这个 id 写进了出站体，也就是
/// `spoof_identity` 开着、且走了会补身份的那两条路之一——模拟路径（`sim` 为 `Some`）或
/// CC 形态补身份（`bare_session` 为 `Some`，见 [`Upstream::bare_session`]）。否则记出来的是
/// 一个上游根本没见过的 id，比留个 `-` 更误导。
///
/// **前缀不是装饰**：这个值只随「账号 + 平台指纹」变（裸客户端没有自己的 device_id，指纹退化
/// 成 `"|<arch>|<os>"`，同账号同平台的所有裸客户端共用一个），看着就像「一台设备打了全部
/// 请求」。前缀让它在日志与 `usage_logs` 里一眼可辨，不至于被当成真实设备读。它也**不写设备绑定**，故不占 `device_limit` 名额、不会出现在设备列表里
/// （[`store::CredentialStore::list_devices`] 从 `device_bindings` 出发）。
pub(super) fn sim_device_id(
    sim: Option<&Simulation>,
    bare_session: Option<&str>,
    flags: store::ForwardFlags,
    cred: &crate::credentials::Credential,
    device_fp: &str,
) -> Option<String> {
    if (sim.is_none() && bare_session.is_none()) || !flags.spoof_identity {
        return None;
    }
    cred.spoof_device_id(device_fp).map(|d| format!("sim:{d}"))
}

/// 构造设备指纹：客户端原始 `device_id` + 平台 `arch`/`os` + **这条请求实际发往上游的 UA**，
/// 用于派生每设备唯一的伪装 device_id。
///
/// **出站 UA 必须在里面**，否则伪装 device_id 与它自己发出去的客户端版本是脱钩的：
/// [`NORMALIZE_DEVICE_FP`](store::NORMALIZE_DEVICE_FP) 开着时同平台的所有客户端收敛成一个
/// device_id，可它们各自的 UA 仍原样透传，上游看到的就是**同一台设备同时跑着好几个版本**。
/// 封号复盘（`luban-ban-3`，Pro 号建号第 3 天挂起）里这条最刺眼：一个出站 device_id 上
/// 223 条请求在 `2.1.141 (sdk-cli)` / `2.1.263 (claude-vscode)` / `2.1.260 (claude-vscode)` /
/// `2.1.223 (sdk-cli)` 四串 UA 之间来回跳了 44 次，**最小间隔 0 秒**；另一个 device_id 上
/// 122 条跳了 50 次，还带 `2.1.260 → 2.1.220` 的降级。一台真机不可能在同一秒既是 2.1.141 的
/// sdk-cli 又是 2.1.263 的 VSCode 扩展——这是任何用量特征都掩不住的自证。
///
/// 代价是**客户端升级会换一个 device_id**（真实 CC 的 device_id 是跨升级恒定的机器标识）。
/// 两害相权：升级换 id 在上游看来是「这台机器重装了一次」，真实用户里常见；同一秒里版本
/// 反复横跳则是官方客户端**不可能**产生的形态。同理，设备数从「每平台 1 个」变成
/// 「每 (平台, 客户端版本) 1 个」，上限仍受设备绑定名额（[`store::DEFAULT_DEVICE_LIMIT`]）约束。
///
/// 除 UA 外仍只取**稳定的硬件/系统身份**：runtime 版本、SDK 包版本这些同一版本客户端也会
/// 各不相同的字段不进指纹，免得同一台机器碎成一堆设备。
pub(super) fn device_fingerprint(
    client_device_id: Option<&str>,
    headers: &HeaderMap,
    out_ua: &str,
) -> String {
    let h = |k: &str| headers.get(k).and_then(|v| v.to_str().ok()).unwrap_or("");
    format!(
        "{}|{}|{}|{}",
        client_device_id.unwrap_or(""),
        h("x-stainless-arch"),
        h("x-stainless-os"),
        out_ua,
    )
}

/// 这条请求实际发往上游的 UA：模拟路径整套换头，UA 恒为 [`config::CC_USER_AGENT`]
/// （[`config::CC_SIM_HEADERS`]）；其余路径原样转发来访那份。
///
/// 只用于算设备指纹——真正装头的是 [`build_forward_headers_for`]，两处的取值规则必须同源，
/// 不然指纹会把一条请求算到另一台设备名下。
pub(super) fn outbound_ua(client_ua: &str, simulated: bool) -> &str {
    if simulated { config::CC_USER_AGENT } else { client_ua }
}

/// 从一组头里取 `User-Agent` 供日志与落库用：没有该头或不是可打印 ASCII 时为 `-`。
///
/// 来访头与出站头两侧都用它——[`ReqLog`] 两份 UA 各存各的，取值规则必须是同一套，
/// 否则「入站 == 出站」这个判断会因为两边截断/回退方式不同而失真。
///
/// 截断到 120 字符：官方 CC 那串（`claude-cli/2.1.220 (external, cli)`）只有 35 字符，
/// 浏览器与各路 SDK 拼出来的能有几百，整条打出来会把日志行撑得没法看。截断只影响日志与
/// 落库，转发出去的那份头一个字节都不动。
///
/// 取值恒为可见 ASCII：`to_str()` 对非 ASCII 头值直接失败，那类一律落 `-`。按 `char` 截而不是
/// `&s[..120]` 只是不给未来留坑——真按字节切，哪天换个不做此保证的取值方式就会切出 panic。
pub(super) fn ua_of(headers: &HeaderMap) -> String {
    const MAX: usize = 120;
    match headers.get(header::USER_AGENT).and_then(|v| v.to_str().ok()) {
        Some(ua) if !ua.trim().is_empty() => ua.chars().take(MAX).collect(),
        _ => "-".into(),
    }
}

/// 把版本串解析成可比较的三元组：`2` → `(2,0,0)`、`2.1` → `(2,1,0)`、`2.1.220` → `(2,1,220)`。
///
/// 三段以后的（`1.2.3.4`）忽略尾巴，预发布后缀（`2.1.220-beta.1`）按主版本 `2.1.220` 算——
/// 这道闸只用来卡「太旧」，把 beta 判成比正式版旧会误伤真正在用新版的人。任何一段不是数字、
/// 或压根没有第一段时返回 `None`（调用方据此当成「读不出版本」，一律放行）。
pub(crate) fn parse_version(s: &str) -> Option<(u64, u64, u64)> {
    // 先截掉预发布/构建后缀，只留 `数字.数字…` 那一截。
    let head: &str = s.trim().split(['-', '+']).next().unwrap_or("");
    let mut parts = head.split('.').map(|p| p.trim().parse::<u64>().ok());
    let major = parts.next().flatten()?;
    // 缺失的段按 0 补（`2` == `2.0.0`）；写了但不是数字的段则整串作废。
    let mut seg = || match parts.next() {
        None => Some(0),
        Some(v) => v,
    };
    Some((major, seg()?, seg()?))
}

/// 从 `User-Agent` 里抠出 Claude Code 自报的版本：`claude-cli/2.1.220 (external, cli)`
/// → `(2, 1, 220)`。UA 里没有 `claude-cli/`、或后面那串不是版本号时返回 `None`。
pub(super) fn cc_cli_version(ua: &str) -> Option<(u64, u64, u64)> {
    let rest = ua.split_once("claude-cli/")?.1;
    // 版本串到第一个非「数字/点」字符为止（官方那串后面跟的是空格 + `(external, cli)`）。
    let end = rest.find(|c: char| !c.is_ascii_digit() && c != '.').unwrap_or(rest.len());
    parse_version(&rest[..end])
}

/// 官方已发布的最新 Claude Code 版本：从 `downloads.claude.ai/claude-code-releases/latest`
/// 学来的（[`crate::oauth::latest_release`]）与写死的 [`config::CC_LATEST_KNOWN_RELEASE`]
/// 取大者。
///
/// 取大者是为了两头兜底：进程刚起还没拉到 `latest` 时有个不至于太旧的下限——下限是**抓包
/// 证实过的**最新版，不是模拟路径那个更旧的 [`config::CC_VERSION_BASE`]，否则启动窗口里真实
/// 新版的来访会被判成冒充；反过来那个端点若哪天回了个更旧的数（缓存、回滚），也不能把已经
/// 证实存在的版本判成「不存在」。写死的那个不低于模拟版本，有测试钉着，故 luban 自己发出去
/// 的版本也在上限之内。
pub(crate) fn known_latest_release() -> (u64, u64, u64) {
    let base = parse_version(config::CC_LATEST_KNOWN_RELEASE).unwrap_or((0, 0, 0));
    crate::oauth::latest_release().map_or(base, |l| l.max(base))
}

/// 来访 UA 自报的 CC 版本，**且这个版本说得通**——不高于 [`known_latest_release`]。
///
/// 高于官方最新版的自报版本按「读不出版本」处理（`None`）：这不是官方客户端，跳过模拟、
/// 沿用它的版本去补 billing header / 跑握手 / 发额度探测，都是在替一个不存在的版本背书。
/// 低于最新版的一律认——用户不升级是常态，下限另有 [`below_min_client_version`] 管。
pub(super) fn trusted_cc_version(ua: &str) -> Option<(u64, u64, u64)> {
    trusted_cc_version_against(ua, known_latest_release())
}

/// [`trusted_cc_version`] 的纯函数形态：`latest` 由调用方给，供测试不碰全局缓存。
pub(super) fn trusted_cc_version_against(
    ua: &str,
    latest: (u64, u64, u64),
) -> Option<(u64, u64, u64)> {
    let v = cc_cli_version(ua)?;
    (v <= latest).then_some(v)
}

/// 最低客户端版本闸：来访 UA 自报的 CC 版本低于 `min` 时，返回 `(自报版本, 要求版本)` 供
/// 日志与错误消息使用；放行时返回 `None`。
///
/// 三种情况一律放行，都是刻意的：
/// - `min` 没配（`None`/空串）或不是版本号 —— 闸没开；
/// - UA 里没有 `claude-cli/` —— 非 CC 客户端（SDK、浏览器、自写脚本），无版本可比；
/// - `claude-cli/` 后面读不出版本号 —— 宁可放过，也不为一个解析不了的串把人挡在门外。
///
/// 注意这只是一道**引导升级**的闸，不是安全边界：UA 是客户端自报的，随手改一个头就能绕过。
pub(super) fn below_min_client_version(ua: &str, min: Option<&str>) -> Option<(String, String)> {
    let min = min?;
    let want = parse_version(min)?;
    let got = cc_cli_version(ua)?;
    (got < want).then(|| (format!("{}.{}.{}", got.0, got.1, got.2), min.trim().to_string()))
}

/// 把 `metadata.user_id` 里的 `account_uuid`/`device_id` 换成凭证自洽身份，**保持原格式**：
/// - CC 内嵌 JSON：**字符串级定点替换**这两个字段的值，字段顺序与其余内容原样不动。
///   真实 CC 发的是紧凑 JSON `{"device_id":..,"account_uuid":..,"session_id":..}`。外层 body
///   已靠 serde_json 的 `preserve_order` 保住顺序，但这层仍绕开 serde：内层是**字符串里的
///   JSON**，重新序列化会连空白、转义写法一起归一化，只有定点替换才逐字节不变。
/// - 扁平串 `user_<hash>_account_<acct>_session_<sess>`（如 Windows）：换掉 device 段与
///   account 段，保留 session 段，仍以扁平串回写——不把 Windows 请求伪装成 CC 的 JSON 形态。
///
/// `spoof_device` 关掉时**只换 account 段**，来访自带的 `device_id` 原样保留——依据与代价
/// 见 [`store::ForwardFlags::spoof_device_id`]（一句话：抓包证明两种官方模式的 `device_id`
/// 相同，换掉它是反关联策略而非形态要求）。account 段照换：那才是两种模式真正的差别。
///
/// 凭证无 `account_uuid`（如旧库未回填）或 user_id 结构无法识别时不改动，返回 `false`。
/// 把来访自带的 `metadata.user_id` 里那个 `session_id` 段对齐到出站头上的取值，
/// **保持原格式**（内嵌 JSON 定点替换 / 扁平串重拼），已经同值时不动。
///
/// [`spoof_identity`] 刻意不碰 session 段——那一步只管 account / device 两段，会话段由这里
/// 统一对齐到 [`outbound_session_id`] 选定的那个：身份伪装开着时是按账号钉住的派生值
/// （[`account_session_id`]，同一条来访会话换号后不再带着同一个 uuid 出现在另一个组织下），
/// 关着时就是客户端自己那个合法值。头是 `sess-42`、体里是合法 uuid，或两处给了两个不同的合法
/// uuid 时，[`incoming_session_id`] 已经替这条请求选定了一个，出站两处就都得是它——否则发出
/// 去的是一份官方绝不产生的请求（那两处逐字相同）。
///
/// 那份 user_id **没有会话段**时补上：内嵌 JSON 在收尾 `}` 前追加 `"session_id"`（官方键序
/// device → account → session，追加在末尾正好对齐），扁平串追加 `_session_<sid>` 段。
/// 客户端自带 user_id 又带了合法会话头、体里却没有会话段，是官方绝不产生的组合——放行等于
/// 把矛盾原样送到上游。这是 [`replace_json_str_field`]「不新增字段」取舍的唯一例外，且只发
/// 生在头体本就该同值的这一处。
///
/// 两种格式都认不出来（既不是 JSON 对象也不是 `user_<dev>_account_<acct>` 形态）时不动。
/// 返回是否改动过。
pub(super) fn sync_metadata_session(v: &mut serde_json::Value, session_id: &str) -> bool {
    let Some(user_id) = v.get_mut("metadata").and_then(|m| m.get_mut("user_id")) else {
        return false;
    };
    let Some(inner) = user_id.as_str().map(str::to_string) else { return false };

    // 格式一：CC 内嵌 JSON。先确认那个字段确实在、且值不同，再对原串定点替换——
    // 重新序列化会把空白与转义写法一起归一化，只有定点替换逐字节不变。
    if let Some(obj) =
        serde_json::from_str::<serde_json::Value>(&inner).ok().as_ref().and_then(|v| v.as_object())
    {
        // **逐字节比，不 trim**：`" <uuid> "` 与头上的 `<uuid>` 不是同一个值。校验那侧
        // （[`extract_session_id`]）trim 过，这里若也 trim 就会判成「已同值」而放着不改，
        // 出站头体就差了两个空格——官方那两处逐字相同。
        match obj.get("session_id").and_then(|s| s.as_str()) {
            Some(cur) if cur == session_id => return false,
            Some(_) => {
                if let Some(next) = replace_json_str_field(&inner, "session_id", session_id) {
                    *user_id = serde_json::Value::String(next);
                    return true;
                }
                return false;
            }
            // 没有会话段：在收尾 `}` 前追加。官方把 session_id 放在最后一位，追加即对齐；
            // 对原串定点插入而非重新序列化，其余内容逐字节不变。
            None => {
                let Some(next) = append_json_str_field(&inner, "session_id", session_id) else {
                    return false;
                };
                *user_id = serde_json::Value::String(next);
                return true;
            }
        }
    }

    // 格式二：扁平串（Windows 那类）——device 与 account 段原样，只换 session 段。
    if let Some(flat) = parse_flat_user_id(&inner) {
        // 同上，逐字节比。
        if flat.session == session_id {
            return false;
        }
        *user_id = serde_json::Value::String(format!(
            "user_{}_account_{}_session_{}",
            flat.device, flat.account, session_id
        ));
        return true;
    }
    // 扁平串缺 session 段（`user_<dev>_account_<acct>`）：整段追加。[`parse_flat_user_id`]
    // 要求三段齐全（其余调用方读的是完整身份），故这里单独判前两段。
    if inner.starts_with("user_") && inner.contains("_account_") {
        *user_id = serde_json::Value::String(format!("{inner}_session_{session_id}"));
        return true;
    }
    false
}

/// 在紧凑 JSON **对象**字符串的收尾 `}` 前追加一个字符串字段 `"key":"val"`，其余内容逐字节
/// 不变。与 [`replace_json_str_field`] 配对：那边只改已有字段，这边只加不存在的。调用方须已
/// 确认该字段不存在且 `s` 是对象；`val` 与 `key` 同为 hex/uuid/标识符，无需 JSON 转义。
/// 串不以 `}` 收尾（前后有空白时也算）返回 `None`。
fn append_json_str_field(s: &str, key: &str, val: &str) -> Option<String> {
    let body = s.strip_suffix('}')?;
    let sep = if body.trim_end().ends_with('{') { "" } else { "," };
    Some(format!("{body}{sep}\"{key}\":\"{val}\"}}"))
}

pub(super) fn spoof_identity(
    v: &mut serde_json::Value,
    cred: &crate::credentials::Credential,
    device_fp: &str,
    spoof_device: bool,
) -> bool {
    let account_uuid = match cred.account_uuid.as_deref() {
        Some(u) if !u.trim().is_empty() => u,
        _ => return false,
    };
    // 关掉时不必派生，也就不该因为派生不出来而放弃改写 account 段。
    let device_id = match spoof_device {
        true => match cred.spoof_device_id(device_fp) {
            Some(d) => Some(d),
            None => return false,
        },
        false => None,
    };
    let user_id = match v.get_mut("metadata").and_then(|m| m.get_mut("user_id")) {
        Some(u) => u,
        None => return false,
    };
    let inner_str = match user_id.as_str() {
        Some(s) => s.to_string(),
        None => return false,
    };

    // 格式一：CC 内嵌 JSON——先确认是 JSON 对象，再对原始字符串做定点值替换，
    // 保持字段顺序与其余内容（session_id 等）逐字节不变。
    if serde_json::from_str::<serde_json::Value>(&inner_str)
        .ok()
        .as_ref()
        .and_then(|v| v.as_object())
        .is_some()
    {
        let mut s = inner_str;
        let mut changed = false;
        if let Some(next) = replace_json_str_field(&s, "account_uuid", account_uuid) {
            s = next;
            changed = true;
        }
        if let Some(d) = device_id.as_deref()
            && let Some(next) = replace_json_str_field(&s, "device_id", d)
        {
            s = next;
            changed = true;
        }
        if changed {
            *user_id = serde_json::Value::String(s);
        }
        return changed;
    }

    // 格式二：扁平串——保持格式，只换 device 与 account，保留 session。
    // `spoof_device` 关掉时 device 段也一并保留，只换 account 段。
    if let Some(flat) = parse_flat_user_id(&inner_str) {
        let device = device_id.as_deref().unwrap_or(&flat.device);
        let rebuilt = format!("user_{}_account_{}_session_{}", device, account_uuid, flat.session);
        *user_id = serde_json::Value::String(rebuilt);
        return true;
    }

    false
}

/// 在紧凑 JSON 字符串里，把 `"key":"<旧值>"` 的值原地替换成 `new_val`，字段位置与其余
/// 内容逐字节不变。仅处理**字符串型且值内无转义引号**的字段——`device_id`(hex)、
/// `account_uuid`(UUID，可能为空串)均满足，`new_val` 同为 hex/UUID，无需 JSON 转义。
/// 找不到该字段（或其不是 `"key":"` 形态）时返回 `None`，**不新增字段**，以免改变结构。
pub(super) fn replace_json_str_field(s: &str, key: &str, new_val: &str) -> Option<String> {
    let needle = format!("\"{key}\":\"");
    let val_start = s.find(&needle)? + needle.len();
    // 值到下一个引号为止（值内无转义引号，故直接找 '"'）。
    let val_end = val_start + s[val_start..].find('"')?;
    let mut out = String::with_capacity(s.len() - (val_end - val_start) + new_val.len());
    out.push_str(&s[..val_start]);
    out.push_str(new_val);
    out.push_str(&s[val_end..]);
    Some(out)
}

/// 给 `system[0]` 的 `x-anthropic-billing-header` 补上 `cch=<值>`，对齐订阅客户端。
///
/// 官方客户端只在订阅(OAuth)模式下发这个字段，API-key 模式（接入 luban 的形态）不发，
/// 于是「OAuth token + 无 cch」是个确定性判据。抓包实测补齐后与真实客户端形态一致：
/// `…cc_version=2.1.260.222; cc_entrypoint=cli; cch=f850a;`
///
/// 只在该块确实是 billing header、且尚无 `cch=` 时改写；其余情况返回 `false` 不动结构。
///
/// **这条路只服务真实 CC 来访。** 模拟路径的 billing header 由
/// [`simulated_billing_header_text`] 一次拼好（cch 也在里面），走不到这里。
///
/// `system[0]` 位于第一个缓存断点之前，直觉上「每请求变的 cch 会打爆 prompt cache」——
/// **抓包证否了这一点**，上游不把 billing header 算进缓存键，见 [`cch_value`]。
pub(super) fn ensure_billing_cch(v: &mut serde_json::Value) -> bool {
    let blk = match v.get_mut("system").and_then(|s| s.as_array_mut()).and_then(|a| a.first_mut()) {
        Some(b) => b,
        None => return false,
    };
    let text = match blk.get("text").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return false,
    };
    if !text.starts_with("x-anthropic-billing-header:") || text.contains("cch=") {
        return false;
    }
    let mut s = text.trim_end().to_string();
    if !s.ends_with(';') {
        s.push(';');
    }
    s.push_str(&format!(" cch={};", cch_value()));
    match blk.get_mut("text") {
        Some(t) => {
            *t = serde_json::Value::String(s);
            true
        }
        None => false,
    }
}

/// `system[0]` 是不是一条 billing header。
pub(super) fn has_billing_header(v: &serde_json::Value) -> bool {
    v.get("system")
        .and_then(|s| s.as_array())
        .and_then(|a| a.first())
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
        .is_some_and(|t| t.starts_with("x-anthropic-billing-header:"))
}

/// 给**真实 CC 来访**那条 billing header 追加会话关联字段：`cc_prev_req` 与
/// `cc_prompt_id`，落在 `cch` 之后（官方段序，见 [`simulated_billing_header_text`]）。
///
/// API-key 端的 CC 一个都不发，而订阅端官方每条主线程请求都有；luban 拿 OAuth token 转出去
/// 之后缺着，就是「OAuth 请求没有会话关联」这个官方不产生的形态。要不要补由
/// [`client_session_link`] 判，这里只管拼串。
///
/// 客户端**自己已经写了**哪一项就不动那一项——它比我们更清楚自己的链。
fn append_billing_link(v: &mut serde_json::Value, link: &CcSessionLink) -> bool {
    let blk = match v.get_mut("system").and_then(|s| s.as_array_mut()).and_then(|a| a.first_mut()) {
        Some(b) => b,
        None => return false,
    };
    let text = match blk.get("text").and_then(|t| t.as_str()) {
        Some(t) => t,
        None => return false,
    };
    if !text.starts_with("x-anthropic-billing-header:") {
        return false;
    }
    let mut s = text.trim_end().to_string();
    let mut changed = false;
    if !s.ends_with(';') {
        s.push(';');
    }
    if let Some(prev) = &link.prev_req
        && !s.contains("cc_prev_req=")
    {
        s.push_str(&format!(" cc_prev_req={prev};"));
        changed = true;
    }
    if let Some(pid) = &link.prompt_id
        && !s.contains("cc_prompt_id=")
    {
        s.push_str(&format!(" cc_prompt_id={pid};"));
        changed = true;
    }
    if !changed {
        return false;
    }
    match blk.get_mut("text") {
        Some(t) => {
            *t = serde_json::Value::String(s);
            true
        }
        None => false,
    }
}

/// [`ensure_thinking`] 补 `thinking` 的 `max_tokens` 下限：再小的请求，思考预算本身就塞不进去，
/// 不值得加，见该函数文档的「三种情况不补」。
pub(super) const THINKING_MIN_MAX_TOKENS: u64 = 1024;

/// 模拟路径下补 `thinking`，形态取自 profile（[`config::CcThinking`]，2.1.260 抓包）：
///
/// - `EnabledUpdates`（haiku 族）：`{"budget_tokens": N, "type": "enabled",
///   "display": "updates"}`（`cap/2.1.260/00020`，`budget_tokens` 在前），
///   `N = max_tokens - 1`（`max_tokens: 32000` → `31999`）；
/// - `AdaptiveUpdates`（opus / fable / sonnet 主线程）：
///   `{"type": "adaptive", "display": "updates"}`（`cap/2.1.260-2/00025`、`cap/2.1.260/00018`）。
///   2.1.258 时只有 fable 带 `display`，2.1.260 起 opus 也带了；
/// - `Disabled` / `Absent`：不补——helper / 标题 / 分类那几个 profile 官方就是
///   `{"type":"disabled"}` 或整个不发，而模拟路径只造主线程形态，走不到这里。
///
/// 别给 opus-5 / sonnet-5 / fable 发 `enabled + budget_tokens`：这几个模型上 `budget_tokens`
/// 直接 400。
///
/// 三种情况不补：
/// - 客户端自己带了 `thinking`（`disabled`/`null`/`enabled` 都算——那是它自己的选择）；
/// - `tool_choice` 强制工具调用（上游不允许两者并存）；
/// - `max_tokens` 太小（< 1024）：thinking 本身要消耗 token 预算，探测级请求不值得加。
pub(super) fn ensure_thinking(v: &mut serde_json::Value, profile: &config::CcProfile) -> bool {
    let Some(obj) = v.as_object_mut() else { return false };
    if obj.contains_key("thinking") {
        return false;
    }
    // tool_choice 强制工具调用时上游不允许 thinking，不注入。
    // 客户端明确要强制工具，thinking 是我们补的，客户端优先。
    if obj
        .get("tool_choice")
        .and_then(|tc| tc.get("type"))
        .and_then(|t| t.as_str())
        .is_some_and(|t| t == "tool")
    {
        return false;
    }
    let max_tokens = obj.get("max_tokens").and_then(|m| m.as_u64()).unwrap_or(32000);
    if max_tokens < THINKING_MIN_MAX_TOKENS {
        return false;
    }
    let value = match profile.thinking {
        config::CcThinking::Enabled | config::CcThinking::EnabledUpdates => {
            let budget = max_tokens.saturating_sub(1).max(1);
            // 官方 key 序是 `budget_tokens` → `type` → `display`，手工插入以保住顺序
            // （`cap/2.1.260/00020`）。
            let mut m = serde_json::Map::new();
            m.insert("budget_tokens".into(), serde_json::Value::Number(budget.into()));
            m.insert("type".into(), "enabled".into());
            if profile.thinking == config::CcThinking::EnabledUpdates {
                m.insert("display".into(), "updates".into());
            }
            serde_json::Value::Object(m)
        }
        config::CcThinking::Adaptive => serde_json::json!({"type": "adaptive"}),
        config::CcThinking::AdaptiveUpdates => {
            serde_json::json!({"type": "adaptive", "display": "updates"})
        }
        // 模拟路径只造主线程 profile，这两支走不到；真走到了就是「客户端没写、官方也不写」，
        // 不补才是对的。
        config::CcThinking::Disabled | config::CcThinking::Absent => return false,
    };
    insert_top_level(
        v,
        "thinking",
        value,
        &["max_tokens", "metadata", "tools", "system", "messages", "model"],
    );
    true
}

/// 补 `diagnostics.previous_message_id`：同会话上一条回复的 `message.id`，会话第一条写
/// `null`。
///
/// **字段恒在，值可为 null**——`cap/2.1.260-2/00013`、`00025`、`00057` 三份首轮全是
/// `{"previous_message_id":null}`，而不是不发 `diagnostics`。少这个字段与写错值同样是
/// 一个稳定差异。
///
/// 官方位置在 `output_config` 之后、`stream` 之前；`insert_top_level` 找不到锚点时追加，
/// 随后 [`align_cc_top_level_order`] 会按 profile 的键序归位，故这里的锚点只是省一次搬动。
///
/// 客户端自己带了 `diagnostics` 就不动——那是它自己的字段，替它改属于越权。
/// 官方不发这个字段的 profile（`cap/2.1.260-2/00063` 那条「猜下一句」、额度探测、
/// 标题生成、安全分类）由调用方按 profile 判，不在这里判。
fn ensure_diagnostics(v: &mut serde_json::Value, link: &CcSessionLink) -> bool {
    if v.get("diagnostics").is_some() {
        return false;
    }
    let value = match &link.prev_message_id {
        Some(id) => serde_json::json!({ "previous_message_id": id }),
        None => serde_json::json!({ "previous_message_id": serde_json::Value::Null }),
    };
    insert_top_level(
        v,
        "diagnostics",
        value,
        &["output_config", "context_management", "thinking", "max_tokens", "metadata", "model"],
    );
    true
}

/// 这条请求该补哪份 `fallbacks`（计费路径、主线程 profile，且该族的开关开着时）：
///
/// - fable 族（`fable_refusal_fallback`，默认关）：官方 2.1.260 那份
///   `[{"model":"claude-opus-5"}]`（profile 里逐字取自抓包），补上反而更像官方；
/// - opus-5 族（`opus_refusal_fallback`，**默认关**）：luban 自定的
///   [`config::OPUS_REFUSAL_FALLBACKS`]（4.8 → 4.6）。官方 opus 客户端不发这个字段，补了就是
///   官方从不产生的请求形态，是风控层面的自证风险，故只作为独立实验开关保留；
/// - 其余模型（sonnet / haiku / 4.x）：不补。**不是**它们没有分类器——官方文档明说 Sonnet 5
///   与 Opus 4.7/4.8 同样带实时网络安全分类器、同样以 200 + `stop_reason: "refusal"` 拒答——
///   而是官方客户端在这些模型上不发 `fallbacks` 字段，luban 不凭空造官方从不产生的请求形态
///   （opus-5 那条自定链就是为此才默认关的）。它们的拒答照样被嗅探器记流水、按提示词学
///   （[`UsageSniffer::classifier_refusal`] 不看模型族），只是不替它们换模型重跑。
///
/// 上游曾以 400 拒过这个模型的 fallback 目标（[`remember_fallback_rejection`]）的，也不补。
pub(super) fn refusal_fallbacks_for(
    model: Option<&str>,
    flags: store::ForwardFlags,
    billable: bool,
    cc_kind: CcRequestKind,
    learned: &DeprecatedFieldMemory,
) -> Option<&'static str> {
    if !billable || cc_kind != CcRequestKind::Main {
        return None;
    }
    let model = model?;
    let m = model.to_ascii_lowercase();
    let plan = if m.contains("fable") {
        if !flags.fable_refusal_fallback {
            return None;
        }
        cc_profile_for(model).fallbacks?
    } else if m.starts_with("claude-opus-5") {
        if !flags.opus_refusal_fallback {
            return None;
        }
        config::OPUS_REFUSAL_FALLBACKS
    } else {
        return None;
    };
    if learned.read().contains_key(&(model.to_string(), FALLBACKS_FIELD.to_string())) {
        return None;
    }
    Some(plan)
}

/// 客户端自己带了**数组形态**的 `fallbacks`（[`ensure_fallbacks`] 对它一个字不动）。有则
/// luban 不算补过：`refusal_fallbacks` 留 `None`，上游 400 拒它时不学、不剥掉重试。字符串
/// `"default"` 不算——那一份会被 luban 换成数组（[`ensure_fallbacks`] / [`normalize_fallbacks`]），
/// 出站的是 luban 的字面量。
pub(super) fn client_supplied_fallbacks(body: Option<&serde_json::Value>) -> bool {
    body.and_then(|v| v.get("fallbacks")).is_some_and(|f| !f.is_string())
}

/// 这条请求出站时会不会带一份**上游会照着换模型重跑**的 `fallbacks`——客户端自己写了合法的
/// 数组（[`valid_fallback_array`]，[`rewrite_body`] 一字不动送出），或 luban 按族开关要补
/// （[`refusal_fallbacks_for`]）。带的请求上游拒答后会自己换模型重跑，本地的「已拒答提示词」
/// 规则（[`known_refused_prompt`]）不该拦它。
///
/// **字符串形态一律不算**，与 [`client_supplied_fallbacks`] 同口径。2.1.258 那种 `"default"`
/// （`cap/2.1.258/00013`）在 luban 有计划时会被 [`ensure_fallbacks`] 换成计划（上面那条已经
/// 算进去了）；没计划时它原样出站，而头那侧按「客户端没带」处理、不补 `server-side-fallback`
/// beta，是「体里有字段、头上没声明」的形态，上游不会为它换模型重跑——放行只是白送一次拒答，
/// 该本地回放上游那次的 200。此前这里把没计划的 `"default"` 也当成「带了」，门禁与实际出站
/// 体不一致。空串或别的字面量上游一定 400，同样不算。
/// `cc_kind` 在这里按体与 beta 头现算：调用点在 `handle_inner` 早于主流程算 `cc_kind` 的
/// 位置，而 [`CcRequestKind::of`] 是纯函数。
pub(super) fn outbound_carries_fallbacks(
    body: Option<&serde_json::Value>,
    model: Option<&str>,
    flags: store::ForwardFlags,
    inbound_beta: &[String],
    learned: &DeprecatedFieldMemory,
) -> bool {
    let Some(v) = body else { return false };
    let client = v.get("fallbacks");
    // 客户端带了非字符串：luban 一个字不动（[`client_supplied_fallbacks`]），出站就是它那份——
    // 算不算「带了」看它是不是一份上游会认的数组；`[]`、`null`、`{}`、`[null]`、`[{}]` 上游
    // 一定 400，本地规则不能为它让路。
    if let Some(f) = client
        && !f.is_string()
    {
        return valid_fallback_array(f);
    }
    // 字段缺失或是字符串：luban 有计划就写计划（[`ensure_fallbacks`] 会把任何字符串换掉），
    // 没计划就是没带——字符串不算，见函数文档。
    let cc_kind = CcRequestKind::of(v, inbound_beta);
    refusal_fallbacks_for(model, flags, true, cc_kind, learned).is_some()
}

/// 一份上游会认的 `fallbacks` 数组：非空，每一项是带非空 `model` 字符串的对象。官方定义就
/// 这一种元素形态（可选 `max_tokens` 覆盖不在此判），别的写法上游 400。
pub(super) fn valid_fallback_array(f: &serde_json::Value) -> bool {
    f.as_array().is_some_and(|a| {
        !a.is_empty()
            && a.iter()
                .all(|e| e.get("model").and_then(|m| m.as_str()).is_some_and(|m| !m.is_empty()))
    })
}

/// 记忆表里「这个模型不收 `fallbacks`」那条的字段名。与已废弃字段同一张表、同一套落库
/// （`kind = "deprecated"`），但**不在** [`DEPRECATABLE_FIELDS`] 里：那张名单还管
/// `sampling_policy` 的静态拒绝与剥离，把 `fallbacks` 混进去会让 4.7+ 模型上客户端自带的
/// `fallbacks` 被当成采样参数剥掉或拒掉。
pub(super) const FALLBACKS_FIELD: &str = "fallbacks";

/// 上游那条 400 是不是冲着 `fallbacks` 来的（目标模型不在 `allowed_fallback_models`、
/// 与请求模型重复、条数超限……）。只按 message 判：这类错误归在 `invalid_request_error`
/// 名下，靠类型分不出来；而 `fallback` 这个词只在这一件事上出现。
pub(super) fn is_fallback_rejection(err: &[u8]) -> bool {
    let (_, message) = parse_upstream_error(err);
    message.to_lowercase().contains("fallback")
}

/// 上游以 400 拒了 luban 补的 `fallbacks` → 记进 [`DeprecatedFieldMemory`]（模型 +
/// `fallbacks`），之后 [`refusal_fallbacks_for`] 对该模型不再补。返回**这次新学到**的那条，
/// 调用方拿去落库（同 [`remember_deprecated_field`]）。
pub(super) fn remember_fallback_rejection(
    mem: &DeprecatedFieldMemory,
    model: &str,
    err: &[u8],
) -> Option<store::LearnedRejection> {
    let (_, message) = parse_upstream_error(err);
    let mut table = mem.write();
    let key = (model.to_string(), FALLBACKS_FIELD.to_string());
    if table.contains_key(&key) || table.len() >= SHAPE_MEMORY_CAP {
        return None;
    }
    table.insert(key, message.clone());
    tracing::warn!(
        model = %model,
        upstream_message = %message.chars().take(300).collect::<String>(),
        "upstream rejected the fallbacks luban added; this model will be sent without them from now on"
    );
    Some(store::LearnedRejection {
        kind: LEARNED_KIND_DEPRECATED.into(),
        model: model.to_string(),
        field: FALLBACKS_FIELD.into(),
        value: String::new(),
        message,
        reply: None,
    })
}

/// 补 `fallbacks`：客户端没写、或写的是 2.1.258 的字符串 `"default"`，都换成 `plan`
/// 那份数组；客户端自己已经发了数组形态（自己就是 2.1.260 一代）时原样不动。
///
/// 位置按 [`config::CC_BODY_ORDER_MAIN`]：`context_management` 之后、`output_config` 之前；
/// 找不到锚点时追加，随后 [`align_cc_top_level_order`] 归位。要补什么、补给谁由
/// [`refusal_fallbacks_for`] 决定，这里只管写。
pub(super) fn ensure_fallbacks(v: &mut serde_json::Value, plan: &str) -> bool {
    if v.get("fallbacks").is_some_and(|f| !f.is_string()) {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(plan) else { return false };
    if v.get("fallbacks").is_some() {
        let Some(obj) = v.as_object_mut() else { return false };
        obj.insert("fallbacks".into(), value);
    } else {
        insert_top_level(
            v,
            "fallbacks",
            value,
            &["context_management", "temperature", "thinking", "max_tokens", "metadata", "model"],
        );
    }
    true
}

/// `fallbacks` 的**形态**归一（该族的 refusal fallback 开关关着时走这条）：2.1.258 的官方 fable 发
/// 字符串 `"default"`，2.1.260 换成了数组 `[{"model":"claude-opus-5"}]`（`cap/2.1.260/00018`）。
///
/// 只在客户端自己已经要了 fallback 时改形态，不替它凭空开——开关关着即用户明确不要
/// luban 替他换模型跑。客户端已经发了数组形态时原样不动。
pub(super) fn normalize_fallbacks(v: &mut serde_json::Value, profile: &config::CcProfile) -> bool {
    let Some(official) = profile.fallbacks else { return false };
    // 只认字符串形态的旧写法；已经是数组（或别的我们不认识的形态）就不动。
    if !v.get("fallbacks").is_some_and(|f| f.is_string()) {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(official) else { return false };
    let Some(obj) = v.as_object_mut() else { return false };
    // `insert` 对已有键原位改值（`preserve_order`），键序不动。
    obj.insert("fallbacks".into(), value);
    true
}

/// CC 请求补 `thinking.display:"updates"`：订阅端官方发的是
/// `{"type":"adaptive","display":"updates"}`（2.1.258 只有 fable-5-1 这样，`00013`；
/// 2.1.260 起 opus 主线程也是，`cap/2.1.260-2/00025`），API-key 端发裸 `adaptive`。
/// 只在 `thinking.type == "adaptive"` 且客户端没写 `display` 时补；模型族由
/// [`cc_profile_for`] 判（该族的官方串里有 `thinking-display-updates` 的才算）。
///
/// **调用方必须先确认出站头里真有那项 beta**（[`rewrite_body`] 的 `display_beta`）：`updates`
/// 是 beta 才认的取值，头上没声明时上游回 400 `Input should be 'summarized', 'omitted'`。
/// 2026-09-02 一条 `claude-vscode, agent-sdk/0.3.258` 的 fable-5-1 请求就是这样被拒的——它的
/// beta 串没有 `advisor-tool`，[`merge_beta`] 按老世代处理不补，体里却写了。
///
/// 那个前提也是**唯一**的门槛：本函数不再自己按模型族判一遍。哪一族在哪一版发这项 beta
/// 是 [`merge_beta`] 的事（它按来访自报的版本查 [`config::cc_profile_at`]），在这里再判
/// 一次只会两处口径分头漂移——2.1.260 起 opus 主线程也发 `display:"updates"`，
/// 原来那句「只有 fable」的判断当场就成了错的。
pub(super) fn fill_thinking_display(v: &mut serde_json::Value) -> bool {
    let Some(th) = v.get_mut("thinking").and_then(|t| t.as_object_mut()) else { return false };
    if th.get("type").and_then(|t| t.as_str()) != Some("adaptive") || th.contains_key("display") {
        return false;
    }
    th.insert("display".into(), "updates".into());
    true
}

/// 补上官方客户端恒发的 `context_management`，落在官方位置（`thinking` 之后、
/// `output_config`/`stream` 之前）。已经有这个字段就原样不动，返回 `false`。
///
/// **依据**：`cap/raw` 八份抓包（四份直连、四份经 luban）的顶层 `context_management`
/// **逐字节相同**——`{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}`，
/// 四个模型族无一例外，连 haiku 那两份也一样。这与 `thinking`/`output_config` 那种逐族不同
/// 的字段不是一类，不存在「补哪一份」的选择问题。
///
/// **为什么该补**：这个字段要 `context-management-2025-06-27` 认，而 [`config::CC_PROFILES`]
/// 里**每一个** profile 的 beta 串都带着它。
/// 不补就是「头上声明了 context-management、体里零个 `edits`」——与
/// [`ensure_beta_query`] 要消灭的那个组合同一个形状，只是落在体上。
///
/// `keep:"all"` 意为「一条都不清」，故补它不改变本次请求的语义，也不动计价：与
/// `known_fingerprint_gaps` 第 7 条的 `fallbacks`（补上等于替用户决定换模型）正相反，
/// 那条不补的理由在这里不成立。
///
/// **但它不是独立字段——依赖 `thinking`**。上游对「有 `clear_thinking` 却没开 thinking」
/// 的请求直接回 400：
///
/// ```text
/// `clear_thinking_20251015` strategy requires `thinking` to be enabled or adaptive
/// ```
///
/// 抓包看不出这层依赖：八份**全都**开着 thinking（opus/sonnet/fable 是 `{"type":"adaptive"}`，
/// haiku 是 `{"budget_tokens":31999,"type":"enabled"}`），于是 8/8 共现让它看着像个独立字段。
/// 这是一次「共现不等于无依赖」的教训——v0.2.51 上线后普通请求即因此 400。
///
/// **不替客户端补 `thinking`** 来满足这个依赖，三条理由都写在抓包里：
/// 1. haiku 那份是 `budget_tokens:31999` 配 `max_tokens:32000`，budget 必须小于 max_tokens。
///    客户端发 `max_tokens:1024` 时这个值根本塞不进去，要么改它的 max_tokens（改掉它明确
///    要的上限与费用天花板），要么自己算一个 budget——两条都是替它做决定。
/// 2. 开了 thinking，响应里就多出 thinking 块，客户端未必认得，直接把它弄坏。
/// 3. thinking token 按输出计费，等于未经同意加钱。
///
/// 故只在客户端**自己已经开着** thinking 时才补，其余情形一个字节都不动。
///
/// **注意**：模拟路径下 [`ensure_thinking`] 会先补上 `thinking`，然后本函数就能自然补上
/// `context_management`，两者配合才完整。
pub(super) fn ensure_context_management(v: &mut serde_json::Value) -> bool {
    let Some(obj) = v.as_object_mut() else { return false };
    // 客户端自己带了就不动——那是它自己的编辑策略，替它改属于越权（同 [`ensure_beta_query`]
    // 对客户端自带 `beta=` 的口径）。
    if obj.contains_key("context_management") {
        return false;
    }
    // 没开 thinking 就不补：`clear_thinking` 依赖它，硬补上游直接 400（见函数文档）。
    // 上游认的是 `enabled`/`adaptive` 两种，`disabled` 与字段缺失都不算。
    let thinking_on = obj
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|t| t.as_str())
        .is_some_and(|t| matches!(t, "enabled" | "adaptive"));
    if !thinking_on {
        return false;
    }
    let value = serde_json::json!({
        "edits": [{ "type": "clear_thinking_20251015", "keep": "all" }]
    });
    // 官方顺序是 `… max_tokens, thinking, context_management, output_config, stream`。
    // 走到这里必有 `thinking`，故锚点首选它，落位与官方一致。
    insert_top_level(
        v,
        "context_management",
        value,
        &["thinking", "max_tokens", "metadata", "tools", "system", "messages", "model"],
    );
    true
}

/// `cch` 的取值：**每请求一个随机的 5 位小写 hex**。
///
/// 真实算法仍未知（同账号内逐请求变化，18 组候选输入 × 6 种摘要均未命中），所以这只是个
/// **形态模拟值**——形状对了，语义没有对齐，别当成已经对齐来读。
///
/// 从原先那个跨账号恒定的 `00000` 改成随机，是因为恒定值本身
/// 就是判据：所有经由 luban 的请求都带同一个真实客户端从不产生的 `cch`，上游一按此聚类
/// 就把所有账号串成一串。而抓包里同一账号相邻两条请求是 `993e1`、`e2d04`、`b504f`……
/// 每条都不同（`cap/2.1.260-2`）。
///
/// **不会打爆 prompt cache**：这一点是抓包证出来的，不是推的。`cap/2.1.260-2` 的 00057 →
/// 00059 是同一会话的连续两条，`system[0]` 的 cch 与 `cc_prompt_id` 都变了，00059 的
/// `cache_creation_input_tokens` 仍只有 81（首条是 8482），也就是前缀照样命中。上游显然
/// 不把 billing header 那一块算进缓存键——否则官方客户端自己也一次都缓存不上。
pub(super) fn cch_value() -> String {
    let n: u32 = rand::rng().random_range(0..0x10_0000);
    format!("{n:05x}")
}

/// 把 API-key 模式的 3 块 `system` 改写成订阅模式的 4 块，并把全部缓存断点对齐到官方形态。
///
/// 两种形态的差别只有「切法」，文本本身逐字节相同（`cap/raw` 那对同机同版本抓包验证过）：
///
/// ```text
/// 官方直连(00006)                     API-key 模式(00002)
/// [0] billing header      无断点      [0] billing header      无断点
/// [1] 身份句 57B          无断点      [1] 身份句 57B          {ephemeral}   ← 多余断点
/// [2] 基座 1210B  {type,ttl:1h,scope:global}
/// [3] 其余        {type,ttl:1h}       [2] 基座‖"\n\n"‖其余    {ephemeral}
///                                     （luban 拆出来的两块不写 ttl，见 cache_control）
/// ```
///
/// 故改写是三件事，**同受一个开关控制**：
/// 1. 在 [`config::CC_SYSTEM_BASE_ANCHOR`] 前的 `\n\n` 处把合并块切成基座 + 其余；
/// 2. 基座标 `{type:ephemeral, scope:global}`，其余标 `{type:ephemeral}`；
/// 3. 去掉身份句上那个断点——它的缓存前缀只有 127 字节（约 35 token），远低于最小可缓存长度，
///    本就是空转，官方也不发。
///
/// **不含 `ttl`**：官方那三个断点全是 `1h`，但缓存时长是客户端掏钱买的，替它翻倍不合适，
/// 理由见 [`cache_control`]。客户端自己写了 `ttl` 的照原样转发。
///
/// **`scope:global` 只标基座**，且另受 [`store::ForwardFlags::cache_scope_global`] 管。
/// 之前是「标 text 最长的那块」，在三块形态下必然选中合并块，而合并块含 `# Environment` 的
/// cwd/git、技能清单这些本机内容——跨账号不可能撞上，标了换不来复用。拆开之后基座是纯静态的，
/// 全网同一份，这个标记才真正有意义。
///
/// 保守起见只处理「确实是 API-key 三块形态」：`system` 长度不为 3、锚点匹配不到、或锚点前不是
/// `\n\n`，一律不动结构返回 `false`。客户端本来就是 4 块（订阅形态）时同样不动。
///
/// **预算**：拆基座是净 +1（身份句原本带断点时抵回来，净 0），断点总数不能顶过
/// [`MAX_CACHE_BREAKPOINTS`]，满了就整形不做——与 [`align_message_shape`] 同一口径，
/// 理由见函数体里那道闸。
pub(super) fn align_system_shape(v: &mut serde_json::Value, cache: CacheShape) -> bool {
    // 拆开前先数一遍整个 body 的断点，见下面那道预算闸（`sys` 一借出去就数不了了）。
    let total = count_cache_control(v);
    let sys = match v.get_mut("system").and_then(|s| s.as_array_mut()) {
        Some(s) if s.len() == 3 || s.len() == 4 => s,
        _ => return false,
    };
    let last = sys.len() - 1;
    // 四块只认 `[billing, 身份, reporting, 合并块]`——fable 族（2.1.258 起唯一带 reporting 的）
    // 在 API-key 模式下就是这个样子（`cap/2.1.258-api/00013`：reporting 单独成块、无断点，
    // 合并块仍是 `基座 ‖ "\n\n" ‖ 其余`）。第三块不是逐字节的 reporting 块（或带了断点）就不是
    // 我们认识的形态。官方的四块订阅形态 `[billing, 身份, 基座, 其余]` 第三块是基座，在这里
    // 自然落到 `false`，不会被再切一次。
    let reporting = last == 3;
    if reporting
        && (sys[2].get("text").and_then(|t| t.as_str()) != Some(config::CC_SYSTEM_REPORTING)
            || sys[2].get("cache_control").is_some())
    {
        return false;
    }
    // 合并块必须本来就是个带断点的文本块，否则不是我们认识的形态。
    if sys[last].get("cache_control").is_none() {
        return false;
    }
    let text = match sys[last].get("text").and_then(|t| t.as_str()) {
        Some(t) => t.to_string(),
        None => return false,
    };
    let body = text.as_str();
    // 逐个模型族的锚点找，取**最早**命中的那个：基座是前缀，切得越靠前越不会把基座切碎。
    // 锚点前必须紧跟 `\n\n`——那两个字节是两块的分隔符，切开后两边都不保留它。
    // 用字节比较：`find` 给的是字节偏移，`p - 2` 未必落在字符边界上，直接切片会 panic。
    let at = config::CC_SYSTEM_BASE_ANCHORS
        .iter()
        .filter_map(|anchor| body.find(anchor))
        .filter(|&p| p >= 2 && &body.as_bytes()[p - 2..p] == b"\n\n")
        .min();
    let Some(at) = at else { return false };

    // 预算闸：断点总数封顶 [`MAX_CACHE_BREAKPOINTS`]，超了上游整条拒
    // （`A maximum of 4 blocks with cache_control may be provided. Found 5.`）。
    //
    // 这次改写的净变化是 **+1 减掉身份句上那个**：合并块那一个断点拆成基座与其余两个（+1），
    // 身份句上那个若在则一并去掉（-1）。官方 API-key 三块形态里身份句**带**断点，净变化为 0，
    // 这道闸永远不响；净 +1 只出现在身份句没标断点的那一种来访上，而它若又在 `messages` 里
    // 自己标满了断点，拆开就正好顶到 5。
    //
    // 满了就整形不做（`false`，一个字节不动），不做「拆开但其余那块不标断点」：官方两块都有
    // 断点，标一个不标一个是个官方不产生的半对齐形态，与 [`fill_cache_ttl`] 那处
    // 「整形没做成就别补 ttl」同一个取舍。代价是这一条请求走客户端自己那份三块形态出去，
    // 少一次基座级缓存命中——总好过整条被拒。判据与 [`align_message_shape`] 同一口径。
    if total + 1 - usize::from(sys[1].get("cache_control").is_some()) > MAX_CACHE_BREAKPOINTS {
        tracing::debug!(breakpoints = total, "system 整形会把缓存断点顶过上限，这一条按原样转发");
        return false;
    }

    if let Some(obj) = sys[1].as_object_mut() {
        obj.remove("cache_control");
    }
    let base = text_block(&body[..at - 2], cache_control(cache));
    let rest = text_block(&body[at..], cache_control(cache.tail()));
    sys.truncate(2);
    if reporting {
        sys.push(text_block_bare(config::CC_SYSTEM_REPORTING));
    }
    sys.push(base);
    sys.push(rest);
    true
}

/// 把 `messages` 对齐到官方形态：内容一律块数组，并给**最后一条消息的最后一块**补上官方那
/// 第三个缓存断点。返回是否改动过。
///
/// **依据**：`cap/raw` 八份抓包每条都是**恰好 3 个断点**，前两个在 `system`（基座、其余），
/// 第三个恒在最后一条消息的最后一个内容块上——`role` 是什么无关：六份非 haiku 落在末尾那条
/// `role:"system"` 消息上，两份 haiku 没有那条消息，就落在 `user` 消息的末块。规则是位置，
/// 不是角色。而模拟路径此前从不碰 `messages`，第三方 SDK 自己一般也不标，于是出去的请求
/// 只有 1~2 个断点。
///
/// **内容字符串化归一**：官方 8/8 的 `content` 都是块数组，而第三方 SDK 常发裸字符串。
/// 断点是块的属性，字符串上挂不住，所以要转。**转就全转**：只转最后一条会得到「一部分消息
/// 是字符串、一部分是数组」这种两边都不像的形态。两种写法在 API 上语义完全相同，转换只改
/// 表示、不改内容，与 [`simulate_system`] 把字符串 `system` 收成块是同一个路子。
///
/// **只在模拟路径调用**：CC 形态的来访自己就标好了第三个断点（`cap/raw/00012` 那条经 luban
/// 的真实请求即如此），替它再标一次只会多占预算。
///
/// **预算**：断点总数封顶 [`MAX_CACHE_BREAKPOINTS`]，超了上游整条拒。这里数的是**改写后
/// 整个 body** 的现存断点，故 [`simulate_system`] 已经用掉的那些都算在内；满了就不补——
/// 少一次缓存命中，总好过整条请求被拒。
///
/// **只往非空的 `text` 块与 `tool_result` 块上标**，两条理由各自独立：
/// - 有样本的才标。`cap/raw` 八份那第三个断点都在 `text` 块上；`tool_result` 的样本是
///   `cap/2.1.260/00025`、`00029`（SDK 子代理 haiku）——最后一条 `user` 消息的末块是
///   `tool_result`，官方照样在它上面标了 `{type:ephemeral}`。规则是「最后一块」，块型不是判据。
///   `tool_result` 曾被排除在外，后果是 agent 循环的每一轮都拿不到消息级缓存：
///   `req_Fxs76cgvc57N5GNr` 那条 77 条消息、35 对 tool_use/tool_result 的请求，出站 `messages`
///   一个断点都没有，10 万 token 全部裸算、cache_creation 为 0，且每轮都是这个形态。
/// - 末块未必是这两种。会话以 assistant 轮结尾时（prefill）末块可能是 `thinking`——那种块
///   连签名都要上游验（见 [`is_thinking_signature_error`] 那条重试路），往上面挂 `cache_control`
///   是拿一条能发出去的请求去赌一个没有样本的组合。`image` 同样没有样本，一并不碰。
///
/// 空 `text` 块一并跳过：发一个空文本块本身就会被上游拒，见 [`merge_system_blocks`]。
pub(super) fn align_message_shape(v: &mut serde_json::Value, shape: CacheShape) -> bool {
    let mut changed = false;
    let Some(msgs) = v.get_mut("messages").and_then(|m| m.as_array_mut()) else { return false };
    for m in msgs.iter_mut() {
        let Some(content) = m.get_mut("content") else { continue };
        // 空串不转：`{"type":"text","text":""}` 是个上游会拒的块，而原样的 `""` 至少还是
        // 客户端自己发出来的形态——改写不该把一条请求的失败方式换个花样。
        match content.as_str() {
            Some(s) if !s.is_empty() => {
                *content = serde_json::Value::Array(vec![text_block_bare(s)]);
                changed = true;
            }
            _ => {}
        }
    }
    // 断点要在归一之后再数：刚转出来的块本身不带断点，但它得先存在才挂得上。
    if count_cache_control(v) >= MAX_CACHE_BREAKPOINTS {
        return changed;
    }
    let last = v
        .get_mut("messages")
        .and_then(|m| m.as_array_mut())
        .and_then(|a| a.last_mut())
        .and_then(|m| m.get_mut("content"))
        .and_then(|c| c.as_array_mut())
        .and_then(|blocks| blocks.last_mut())
        .and_then(|b| b.as_object_mut());
    let Some(block) = last else { return changed };
    // 客户端自己标过就不动——那是它自己的缓存策略。
    if block.contains_key("cache_control") {
        return changed;
    }
    // 只往非空 `text` 块与 `tool_result` 块上标：这两种有抓包样本；`thinking` 那种还要上游
    // 验签名，`image` 没样本，都不碰（见函数文档）。
    let markable = match block.get("type").and_then(|t| t.as_str()) {
        Some("text") => block.get("text").and_then(|t| t.as_str()).is_some_and(|t| !t.is_empty()),
        Some("tool_result") => true,
        _ => false,
    };
    if !markable {
        return changed;
    }
    // 用 `tail()`：官方只在基座标 `scope`，消息这个断点是 `{type, ttl}`。
    block.insert("cache_control".into(), cache_control(shape.tail()));
    true
}

/// 这条请求缓存前缀里**会进缓存键**的部分：`tools` 整段的指纹，加 `system` 各块正文，
/// 不含 billing header 那一块。给 [`cache_prefix_stable`] 跨轮比对，变了还能对出差异。
///
/// billing header 不算：官方每条请求的 `cch` / `cc_prev_req` / `cc_prompt_id` 都在变，抓包里
/// 前缀照样命中（`cap/2.1.260-2` 00057 → 00059，见 [`cch_value`]），上游显然不把那一块算
/// 进缓存键。`cache_control` 本身也不算——它决定在哪里切、不决定内容。在 luban 自己动
/// `system` 之前算：补前缀、cch 这些是逐轮确定的改写，客户端两轮发的一样，改完也一样。
pub(super) fn cache_prefix_of(v: &serde_json::Value) -> CachePrefix {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    if let Some(tools) = v.get("tools") {
        tools.to_string().hash(&mut h);
    }
    let system = match v.get("system") {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .filter(|t| !t.starts_with("x-anthropic-billing-header:"))
            .map(str::to_string)
            .collect(),
        _ => Vec::new(),
    };
    CachePrefix { tools_fp: h.finish(), system }
}

/// 真 CC 来访的 `messages` 里**一个断点都没有**时，给最后一条消息的末块补上第三个断点。
/// 返回是否改动过。只在非模拟路径、来访是 CC 形态（[`is_cc_shaped`]）、不是额度探测、
/// **且这一轮的缓存前缀与上一轮相同**（[`cache_prefix_stable`]）时调用，
/// 受 [`store::ForwardFlags::system_shape`] 管。
///
/// **为什么要看前缀稳不稳**：prompt cache 是前缀缓存，tools → system → messages 顺序拼起来
/// 逐断点匹配。`system` 里有一块每轮都变，它后面的 `messages` 不管标不标断点都是未命中，
/// 标了只是把「按输入价裸算」换成「按 1.25 倍写入价裸算」。v0.3.121 只加断点不看前缀，
/// 上线后那个会话 24 轮每轮把 25 万 token 的历史整段写进缓存、`cache_read` 始终停在
/// 27,126——比不标还贵两成半。前缀稳定的客户端才标，第一轮也不标（没有上一轮可比）。
///
/// **起因**：现网 `claude-cli/2.1.273 (external, claude-vscode, agent-sdk/0.3.273)` 的一个
/// 会话（`req_zu6ELzACscXlpGSg` 及前后 24 轮）：客户端把 4 个断点里的 3 个花在 `system` 上
/// （身份句、基座、尾块），`messages` 上一个没标。用量上就是 `cache_read` 恒等于工具声明加
/// `system` 前三块，之后 6 万到 16 万 token 的对话历史每轮裸算、写入为 0，24 轮累计约
/// 240 万 token 原价——走缓存读只要一成。luban 此前在非模拟路径不碰消息断点，前提是
/// 「CC 自己会标」（官方 CLI 的第三个断点落在末尾那条 `role:"system"` 消息上，
/// `cap/2.1.260-2/00061`），这个客户端不是这样。
///
/// **与 [`align_message_shape`] 的分工**：那一个是模拟路径的，会把全部字符串 `content`
/// 收成块数组、按开关写 `ttl`。这里对真 CC 的 body 只做最小改动：
/// - 客户端 `messages` 里已有任何断点就不动——那是它自己的策略；
/// - 不改 `content` 的表示：官方 CLI 自己就是新旧混着发（旧 reminder 是字符串、新的是
///   块数组），末条是字符串时不转也不标；
/// - 断点的 `ttl` **抄客户端 `system` 里最后一个断点的**（去掉 `scope`）：上游要求 `ttl`
///   按 tools → system → messages 单调不增，客户端 `system` 写 `5m`、这里写 `1h` 是一发
///   400。`system` 里没有断点就写裸的 `{type:ephemeral}`；
/// - 只往非空 `text` 与 `tool_result` 块上标、预算封顶 [`MAX_CACHE_BREAKPOINTS`]，与模拟
///   路径同一口径。
pub(super) fn ensure_cc_message_breakpoint(v: &mut serde_json::Value) -> bool {
    let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) else { return false };
    if msgs.is_empty() || msgs.iter().map(count_cache_control).sum::<usize>() > 0 {
        return false;
    }
    if count_cache_control(v) >= MAX_CACHE_BREAKPOINTS {
        return false;
    }
    let template = v
        .get("system")
        .and_then(|s| s.as_array())
        .and_then(|blocks| blocks.iter().rev().find_map(|b| b.get("cache_control")))
        .and_then(|cc| cc.as_object())
        .filter(|cc| cc.contains_key("type"))
        .map(|cc| {
            let mut out = serde_json::Map::new();
            for k in ["type", "ttl"] {
                if let Some(val) = cc.get(k) {
                    out.insert(k.into(), val.clone());
                }
            }
            serde_json::Value::Object(out)
        })
        .unwrap_or_else(|| serde_json::json!({"type": "ephemeral"}));
    let last = v
        .get_mut("messages")
        .and_then(|m| m.as_array_mut())
        .and_then(|a| a.last_mut())
        .and_then(|m| m.get_mut("content"))
        .and_then(|c| c.as_array_mut())
        .and_then(|blocks| blocks.last_mut())
        .and_then(|b| b.as_object_mut());
    let Some(block) = last else { return false };
    let markable = match block.get("type").and_then(|t| t.as_str()) {
        Some("text") => block.get("text").and_then(|t| t.as_str()).is_some_and(|t| !t.is_empty()),
        Some("tool_result") => true,
        _ => false,
    };
    if !markable {
        return false;
    }
    block.insert("cache_control".into(), template);
    tracing::info!(
        "added the third cache breakpoint to the last message of a CC request that had none"
    );
    true
}

/// 剥掉官方客户端**从不发送**的顶层字段，返回是否改动过。只在
/// [`store::ForwardFlags::strip_extra_fields`] 开着时调用。
///
/// 判据逐条取自 `cap/raw/00006`（opus-5）与 `00009`（sonnet-5）两份直连抓包——两份的顶层键
/// 恒为 `model, messages, system, tools, metadata, max_tokens, thinking, context_management,
/// output_config, stream`，多一个就是白送的判据。
///
/// 目前三项：
///
/// 1. **`tool_choice`**：官方两份抓包里这个键**压根不存在**。但只删**等价于默认值**的那一种
///    （恰好只有 `{"type":"auto"}` 一个键）——`{"type":"tool", "name":…}`/`{"type":"any"}`
///    是客户端在强制选工具，`disable_parallel_tool_use` 也是它要的行为，删了就是改语义。
///    删掉的那种对模型零影响：`auto` 本来就是缺省。
///
/// 2. **`thinking.type == "disabled"`**：fable 族不支持显式关闭思考，会直接 400。
///    删掉整个 `thinking` 字段让上游走 adaptive 默认值。
///
///    **只对 fable 族删。** 这一条曾是无条件的，那是错的：`{"type":"disabled"}` 是
///    2.1.260 三个官方 profile（无工具 helper、标题生成、安全分类）的**正常形态**
///    （`cap/2.1.260/00024`、`cap/2.1.260-2/00058`、`cap/2.1.260/00019`，模型分别是 haiku
///    与 sonnet）。把它当成「官方从不发的多余字段」删掉，等于把一条官方形态的请求改成了
///    官方不产生的形态，还顺带把客户端「不要思考」的意图翻成了「随你」——那是要花钱的。
///
/// 3. **`thinking.display`**：2.1.251 及之前官方发的是裸的 `{"type":"adaptive"}`；**2.1.258 起
///    fable 族官方自己也发 `display:"updates"`**（`cap/2.1.258/00013`，配着
///    `thinking-display-updates-2026-08-18` beta）。故这一项由 `keep_display` 拨：来访本来
///    就是 CC 形态（真 CC 带什么 `display` 就发什么），或 `thinking` 整个是
///    [`ensure_thinking`] 按官方形态补的，都不剥；只剥第三方客户端自己写的 `display`。
///
///    **这一项有代价，不是零影响**：`display:"summarized"` 是客户端主动要思考摘要，剥掉之后
///    上游按缺省的 `omitted` 走，回程的 `thinking` 块文本为空，客户端那边的「思考过程」就空了。
///    功能不坏（块还在、签名照旧），只是看不到内容。拿「一条 400 直接打不通」换「思考摘要看不
///    到」是划算的，但划算不等于无损，故写在这里，并由开关兜底——不接受这个代价就关掉它。
///
/// **对真实 CC**：前两项本来就是空操作（官方不发 `tool_choice`、不发 `disabled`），第三项
/// 由调用方传 `keep_display = true` 跳过——2.1.258 起 `display` 是官方形态的一部分。
/// 判定要在模拟**之前**做（[`rewrite_body`] 里的 `cc_inbound`）：模拟一跑 body 就都是 CC 形态了。
pub(super) fn strip_extra_fields(v: &mut serde_json::Value, keep_display: bool) -> bool {
    let fable = v
        .get("model")
        .and_then(|m| m.as_str())
        .is_some_and(|m| m.to_ascii_lowercase().contains("fable"));
    let Some(obj) = v.as_object_mut() else { return false };
    let mut changed = false;
    if obj.get("tool_choice").is_some_and(is_default_tool_choice) {
        obj.remove("tool_choice");
        changed = true;
    }
    // `thinking.type == "disabled"`：fable 族不支持，直接 400。删掉整个 `thinking`
    // 字段让上游走 adaptive 默认值——客户端的意图（不要深度思考）近似保留，好过打不通。
    // 别的族**不动**：那是 2.1.260 三个官方辅助 profile 的正常形态，见函数文档第 2 项。
    if fable
        && obj.get("thinking").and_then(|t| t.get("type")).and_then(|t| t.as_str())
            == Some("disabled")
    {
        obj.remove("thinking");
        changed = true;
    }
    // tool_choice 强制工具调用时 thinking 必须关——上游硬限制 400。客户端同时发了两者时
    // 删 thinking 保 tool_choice：强制工具是客户端明确要的语义，thinking 可缺省。
    let forces_tool = obj.get("tool_choice").and_then(|tc| tc.get("type")).and_then(|t| t.as_str())
        == Some("tool");
    if forces_tool && obj.contains_key("thinking") {
        obj.remove("thinking");
        changed = true;
    }
    if let Some(thinking) = obj.get_mut("thinking").and_then(|t| t.as_object_mut()) {
        // CC 自己发的 / luban 按官方形态补的 `display` 照发；见函数文档第 3 项。
        if !keep_display && thinking.remove("display").is_some() {
            changed = true;
        }
        // thinking.type == "enabled" 时 budget_tokens 必须 >= 1024，否则上游 400。
        if thinking.get("type").and_then(|t| t.as_str()) == Some("enabled")
            && let Some(budget) = thinking.get("budget_tokens").and_then(|b| b.as_u64())
            && budget < 1024
        {
            thinking.insert("budget_tokens".into(), serde_json::Value::Number(1024.into()));
            changed = true;
        }
    }
    // thinking 开着时 temperature 必须是 1（上游强制），客户端设了别的值直接 400。
    // 删掉即可——默认值就是 1。判据同 ensure_context_management 那里的口径。
    let thinking_on = obj
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|t| t.as_str())
        .is_some_and(|t| matches!(t, "enabled" | "adaptive"));
    if thinking_on
        && obj.get("temperature").and_then(|t| t.as_f64()) != Some(1.0)
        && obj.remove("temperature").is_some()
    {
        changed = true;
    }
    // 同理 top_p：thinking 开着时上游要求「不传或 >= 0.95」（`top_p must be greater than or
    // equal to 0.95 or unset when thinking is enabled or in adaptive mode`）。这条是条件句，
    // 学习机制有意不学（见 `CONDITIONAL_MARKS`），只能在这里静态兜住。>= 0.95 的照发。
    // 非数字的取值也剥：上游一样 400，留着只是换一种死法。
    if thinking_on
        && obj.get("top_p").is_some_and(|p| !p.as_f64().is_some_and(|p| p >= 0.95))
        && obj.remove("top_p").is_some()
    {
        changed = true;
    }
    changed
}

/// 把 OpenAI 风格的 `tool_choice` 归一成 Anthropic 的对象形态，返回是否改动过。
///
/// Anthropic 只认 `{"type":"auto"|"any"|"tool"|"none", …}` 这一种对象；其它任何形态上游都回
/// 400 `tool_choice: Input should be an object`。OpenAI 兼容层与各类 SDK 常见的几种写法及其对应：
///
/// | 来访 | 出站 |
/// |---|---|
/// | `null` | 删掉（缺省） |
/// | `"auto"` | `{"type":"auto"}`（随后可被 [`strip_extra_fields`] 按缺省剥掉） |
/// | `"none"` | `{"type":"none"}` |
/// | `"required"` / `"any"` | `{"type":"any"}` |
/// | `{"type":"function","function":{"name":X}}` | `{"type":"tool","name":X}` |
/// | `{"type":"function"}`（没指定名字） | `{"type":"any"}` |
///
/// 认不出的形态**原样放行**，让上游报它自己的错——这里只翻译已知的方言，不替客户端猜。
/// 已经是 Anthropic 对象形态的一律不动（含 `disable_parallel_tool_use` 等附加键）。
pub(super) fn normalize_tool_choice(v: &mut serde_json::Value) -> bool {
    let Some(obj) = v.as_object_mut() else { return false };
    let Some(tc) = obj.get("tool_choice") else { return false };
    let replacement = match tc {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "auto" => Some(serde_json::json!({ "type": "auto" })),
            "none" => Some(serde_json::json!({ "type": "none" })),
            "required" | "any" => Some(serde_json::json!({ "type": "any" })),
            _ => return false,
        },
        serde_json::Value::Object(o)
            if o.get("type").and_then(|t| t.as_str()) == Some("function") =>
        {
            match o.get("function").and_then(|f| f.get("name")).and_then(|n| n.as_str()) {
                Some(name) => Some(serde_json::json!({ "type": "tool", "name": name })),
                None => Some(serde_json::json!({ "type": "any" })),
            }
        }
        _ => return false,
    };
    match replacement {
        // `insert` 对已有键原位改值（`preserve_order`），键序不动。
        Some(val) => {
            obj.insert("tool_choice".into(), val);
        }
        None => {
            obj.remove("tool_choice");
        }
    }
    true
}

/// `tool_choice` 是否等价于「不写这个字段」，即恰好只有 `{"type":"auto"}` 一个键。
/// 多带任何一个键（如 `disable_parallel_tool_use`）都是客户端在要一种非缺省行为，不能删。
pub(super) fn is_default_tool_choice(v: &serde_json::Value) -> bool {
    v.as_object()
        .is_some_and(|o| o.len() == 1 && o.get("type").and_then(|t| t.as_str()) == Some("auto"))
}

/// 把请求对象改成 profile 的顶层键序（[`config::CcProfile::body_key_order`]），
/// 并保证 `stream` 在最后。
///
/// **键序按 profile 分**：主线程那串（[`config::CC_BODY_ORDER_MAIN`]）套不到安全分类
/// （`max_tokens` 在第二位、`system` 在 `messages` 前）与额度探测（只有四个键）上，
/// 硬套出来的是官方从不产生的排列。
///
/// 不认识的字段可能有语义，不能丢；保留它们彼此的原始顺序，放在已知字段与 `stream`
/// 之间。本函数只在 [`Simulation`] 路径调用，真 CC 请求继续保留客户端的字节与顺序。
pub(super) fn align_cc_top_level_order(v: &mut serde_json::Value, order: &[&str]) -> bool {
    let Some(obj) = v.as_object_mut() else { return false };
    let before: Vec<String> = obj.keys().cloned().collect();
    let mut old = std::mem::take(obj);
    let mut ordered = serde_json::Map::new();

    for key in order {
        if let Some(value) = old.shift_remove(*key) {
            ordered.insert((*key).to_string(), value);
        }
    }
    let stream = old.shift_remove("stream");
    ordered.extend(old);
    if let Some(value) = stream {
        ordered.insert("stream".to_string(), value);
    }

    let changed = before.iter().map(String::as_str).ne(ordered.keys().map(String::as_str));
    *obj = ordered;
    changed
}

/// MCP 形态假名的工具段前缀池。来访原名加 `mcp__hermes__` 后，同一条探测由 400 变为
/// 200，证明 MCP 命名空间是上游豁免的形态。`manage_bfl00` 之类普通假名仍可被判成
/// 第三方，故生成的假名统一放在 `mcp__luban__*` 下。
const FAKE_TOOL_PREFIXES: &[&str] = &[
    "analyze_",
    "compute_",
    "fetch_",
    "generate_",
    "lookup_",
    "modify_",
    "process_",
    "query_",
    "render_",
    "resolve_",
    "sync_",
    "update_",
    "validate_",
    "convert_",
    "extract_",
    "manage_",
    "monitor_",
    "parse_",
    "review_",
    "search_",
    "transform_",
    "handle_",
];

/// 已知触发上游第三方判定的工具名（已弃用，仅为文档留存）。
///
/// **判据在工具名而不在 system**：同一条请求，工具名换成官方 CC 那套（`Read`/`Bash` 之类）
/// 回 200，换回业务名回 400；而 `system` 里放 56KB 的非官方内容完全不影响。
///
/// 原先用黑名单——只混淆 `skill_manage`/`skill_view`/`skills_list`，其余原样透传。
/// 但上游的触发集合在扩大：任何不在 [`config::CC_TOOL_NAMES`] 白名单内的 custom tool 名
/// 都可能触发第三方判定（实测 `sessions_spawn`/`memory_search` 等同样 400）。
/// 故改为**白名单**：只有官方 CC 工具名与 `mcp__` 前缀工具原样通过，其余一律混淆。
/// 漏加一个官方名的代价仅是多混淆（功能不受影响，回程还原），远好于漏列一个触发名的硬 400。
#[allow(dead_code)]
const BLOCKED_TOOL_NAMES: &[&str] = &["skill_manage", "skill_view", "skills_list"];

/// 一次请求内的工具名混淆映射。
///
/// **为什么要混淆**：`tools[*].name` 是上游判定「这是不是第三方应用」的一个已验证判据，
/// 命中 [`BLOCKED_TOOL_NAMES`] 就把额度改扣超额池并回 400。加 `mcp__` 前缀后实测豁免。
pub(super) struct ToolNameMap {
    /// 真名 → 假名，请求侧用。
    pub(super) forward: std::collections::HashMap<String, String>,
    /// (假名, 真名)，按假名长度**倒序**——短假名可能是长假名的子串，先替长的才不会被吃掉。
    pub(super) reverse: Vec<(String, String)>,
    /// 最长假名的字节数。回程滑动窗口靠它决定留多少字节，见 [`Self::feed`]。
    pub(super) max_fake: usize,
}

/// 某个 tool 是否该混淆：**不在 [`config::CC_TOOL_NAMES`] 白名单内**的 custom tool 一律改写。
///
/// 三类保留原名：
/// - server tool（`web_search_20250305` 等）——改了上游直接拒；
/// - `mcp__` 前缀——实测豁免，且改了会破坏 MCP 命名空间语义；
/// - [`config::CC_TOOL_NAMES`] 里的官方 CC 工具名。
fn should_mimic_tool(t: &serde_json::Value) -> bool {
    let kind = t.get("type").and_then(|k| k.as_str()).unwrap_or_default();
    if !matches!(kind, "" | "custom" | "function") {
        return false;
    }
    let Some(name) = t.get("name").and_then(|n| n.as_str()) else { return false };
    if name.starts_with("mcp__") {
        return false;
    }
    !config::CC_TOOL_NAMES.contains(&name)
}

/// 从请求体扫出要混淆的工具名，生成映射。没有可混淆的（`tools` 不是数组／全是官方名或 `mcp__` 前缀）
/// 返回 `None`，此后请求与回程两侧都零开销。
///
/// **假名对同一组工具名恒定**：seed 取 `sha256(名字集合)`，同一会话内每轮请求得到同一套假名，
/// 上游的 prompt cache 才命中得了。客户端中途增删工具会让整套假名全变——历史里的
/// `tool_use.name` 由 [`apply_tool_names`] 用**新**映射一起重写，故仍然自洽，代价只是缓存失效。
pub(super) fn build_tool_name_map(body: Option<&serde_json::Value>) -> Option<ToolNameMap> {
    let tools = body?.get("tools")?.as_array()?;
    let declared: std::collections::HashSet<&str> =
        tools.iter().filter_map(|t| t.get("name").and_then(|n| n.as_str())).collect();
    let real: Vec<&str> = tools
        .iter()
        .filter(|t| should_mimic_tool(t))
        .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
        .collect();
    if real.is_empty() {
        return None;
    }

    let mut hasher = <sha2::Sha256 as sha2::Digest>::new();
    for (i, name) in real.iter().enumerate() {
        if i > 0 {
            sha2::Digest::update(&mut hasher, b"\0");
        }
        sha2::Digest::update(&mut hasher, name.as_bytes());
    }
    let digest = sha2::Digest::finalize(hasher);
    let seed = u64::from_be_bytes(digest[..8].try_into().expect("sha256 至少 8 字节"));

    let mut forward = std::collections::HashMap::with_capacity(real.len());
    let mut reverse = Vec::with_capacity(real.len());
    let mut max_fake = 0usize;
    for (i, name) in real.iter().enumerate() {
        if forward.contains_key(*name) {
            continue; // 同名工具重复声明：一个映射就够。
        }
        let prefix = FAKE_TOOL_PREFIXES
            [(seed.wrapping_add(i as u64) % FAKE_TOOL_PREFIXES.len() as u64) as usize];
        // 取真名开头三个 ASCII 字母数字，纯粹为了假名在日志里还认得出是谁。
        let head: String = name.chars().filter(|c| c.is_ascii_alphanumeric()).take(3).collect();
        let stem = format!("mcp__luban__{prefix}{head}{i:02}");
        let mut fake = stem.clone();
        // 假名撞上任何已声明工具都会让上游分不清该调谁。序号已保证假名之间唯一，
        // 这里再兜住来访本来就声明了同名 MCP 工具的极端情形。
        let mut collision = 0usize;
        while declared.contains(fake.as_str()) {
            collision += 1;
            fake = format!("{stem}_{collision}");
        }
        max_fake = max_fake.max(fake.len());
        reverse.push((fake.clone(), (*name).to_string()));
        forward.insert((*name).to_string(), fake);
    }
    if forward.is_empty() {
        return None;
    }
    reverse.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
    Some(ToolNameMap { forward, reverse, max_fake })
}

/// 模拟路径下注入的官方主线程工具声明，**逐 profile 一份**：opus 逐字节取自
/// `cap/2.1.260-2/00025`，fable 取自 `cap/2.1.260/00018`。
///
/// **为什么要注入**：上游判第三方的信号之一是「自称 CC 但没有 CC 工具」。光把客户端自有
/// 工具名加 `mcp__` 前缀不够——那只是消去负面信号（被 blocklist 的名字），而正面信号
/// （存在 CC 官方工具声明）仍然缺失。注入之后请求的工具组合是「CC 内建 + MCP 扩展」，
/// 与真实 CC 接 MCP server 的形态一致（`cap/2.1.258-api/00006`：内建在前，`mcp__*` 在尾）。
///
/// **为什么是 11 个而不是 4 个**：2.1.258（四族）、2.1.260（opus / fable）、2.1.270（sonnet）
/// 的主线程抓包里没有一条只带四个工具——最少 13 个，其中所有样本共有的是 13 个：
/// 下面这 11 个真工具，加上 `ToolSearch` 与 `DeferredToolPlaceholder` 那一对延迟加载机制。
/// 只带 Bash/Edit/Read/Write 是一个官方不产生的组合，与「零个工具」一样是自证。
/// 那一对**故意不注**：`ToolSearch` 被模型调起来时客户端拿到一个自己没声明的
/// tool_use 且没法执行，而 `DeferredToolPlaceholder` 只是它的占位；两者都不是「工具」。
/// opus 多出的 Artifact / SendFeedback / ShareOnboardingGuide 是环境相关的，fable 那条
/// 抓包就没有，也不注。
///
/// **顺序也是抓包的一部分**：按官方声明序 `Agent → AskUserQuestion → Bash → Edit →
/// ListAgents → Read → ReportFindings → ScheduleWakeup → Skill → Workflow → Write`，
/// 不是字母序（`Write` 官方排在 `DeferredToolPlaceholder` 之后、`Workflow` 之后）。
///
/// **`eager_input_streaming`**：opus / sonnet / haiku 的 OAuth 主线程每个工具都带
/// `eager_input_streaming: true`（2.1.258 四族、2.1.260 opus、2.1.270 sonnet），fable 一个
/// 都不带。资产原样保留，不另加也不剥。客户端改名后的 `mcp__luban__*` 带不带这个键
/// **没有 OAuth 样本**（`2.1.258-api` 的 `mcp__ide__*` 不带，但那是 API-key 模式，内建也
/// 全不带），这里不猜。
///
/// **模型会不会调这些工具**：概率低。客户端的 system prompt 会指名自己的工具
/// （被混淆成 `mcp__luban__*`），模型优先响应 system 的指令。万一调了，客户端收到一个
/// 自己没声明的 tool_use，按协议返回错误 tool_result 即可，不影响会话继续。
///
/// **同一版本里不同模型族的工具描述并不相同**：opus 与 fable 的 11 个工具 schema
/// **无一相同**——除 `eager_input_streaming` 之外，opus 的 Bash 多了
/// `Foreground sleep is blocked; use Monitor with an until-loop` 那句，Read 的换行说明
/// 也换了写法。原先一份 2.1.258 的资产给所有族用，等于把上一版、别的族的措辞发出去。
///
/// **代价**：两份资产各约 29KB，每条模拟主线程请求都带，首轮进 prompt cache 之前按
/// 输入 token 计费；同一会话后续轮次命中缓存。
static CC_TOOLS_CORE_OPUS: std::sync::LazyLock<Vec<serde_json::Value>> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../assets/cc_tools_core_opus.json"))
            .expect("cc_tools_core_opus.json must be a valid JSON array of tool objects")
    });

static CC_TOOLS_CORE_FABLE: std::sync::LazyLock<Vec<serde_json::Value>> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../assets/cc_tools_core_fable.json"))
            .expect("cc_tools_core_fable.json must be a valid JSON array of tool objects")
    });

/// 按 profile 取注入用的工具声明。
///
/// sonnet / haiku 主线程**没有 2.1.260 样本**（同 [`config::CC_PROFILES`] 里那两行外推的
/// beta），退回 opus 那份：至少版本对得上——发一份 2.1.258 的措辞是「版本混用」，而这里
/// 只是「同版本里族别可能不对」，后者更小。抓到样本后在这里加一行即可。
pub(super) fn cc_tools_core(profile: &config::CcProfile) -> &'static [serde_json::Value] {
    match profile.kind {
        config::CcProfileKind::MainFable => &CC_TOOLS_CORE_FABLE,
        _ => &CC_TOOLS_CORE_OPUS,
    }
}

/// [`inject_cc_tools`] 会往这条请求里**补**哪几个工具名（客户端没声明的那些）；**不改体**。
///
/// 判据只写这一份，注入与流水两边共用：注入按它改体，[`ReqLog`] 按它在回复里认「模型调了
/// 一个客户端没声明的注入工具」。分开写两份判据，早晚有一边漂掉，流水就会把客户端自己的
/// 工具记成注入的、或反过来。
///
/// 返回空的两种情形：没有 `tools` 键（官方的无工具 helper / 标题 / 分类就是这个样子，别
/// 凭空造一个）、该 profile 的每个工具名客户端都已声明。
///
/// **客户端已带部分官方名时照样补缺的**。原先「有任何一个官方名就一个都不注」，理由是
/// 「真 CC 或抄了 CC 声明的中转，别动」——但真 CC 不走模拟路径，会走到这里的是抄了一部分的：
/// 现网一条 Go-http-client 声明了 15 个官方名，其中 TaskCreate / TaskGet / TaskUpdate /
/// TaskList 是 2.1.258 API-key 端才有的拼法，2.1.260 恒带的 11 个里又缺 ListAgents /
/// ReportFindings / ScheduleWakeup / Workflow 四个，出站 UA 却自报 2.1.260。「自称 2.1.260、
/// 工具集是上一版的拼法、还缺四个恒带的」这个组合官方同样不产生。只加不删：老版本多出来
/// 的那几个是客户端的能力，删了是改它的行为。
///
/// **不能借 [`has_cc_tool_profile`] 判**：那个函数回答的是「这看起来像不像真的 CC 客户端」，
/// 对「没带 tools」和「`tools: []`」都答**是**（判不出来就不冤枉人）。而这里问的是「这条
/// 请求缺哪些官方工具」——空数组的答案显然是**全缺**。借用之后，一条 `tools: []` 的
/// 主线程请求就永远注不进工具，正是「零个 CC 工具等于自证不是 CC」那个要消灭的形态。
pub(super) fn cc_tools_to_inject(
    v: &serde_json::Value,
    profile: &config::CcProfile,
) -> Vec<&'static str> {
    let Some(tools) = v.get("tools").and_then(|t| t.as_array()) else {
        return Vec::new();
    };
    let declared: Vec<&str> = tools.iter().filter_map(|t| t.get("name")?.as_str()).collect();
    cc_tools_core(profile)
        .iter()
        .filter_map(|stub| stub.get("name")?.as_str())
        .filter(|name| !declared.contains(name))
        .collect()
}

/// 客户端自带的同名工具与官方声明的**参数表面**是否一致：`input_schema.properties` 的键集相同、
/// `required` 的集合相同。
///
/// 只用于日志，不再决定换不换。换是一律换的（见 [`inject_cc_tools`]）；这个判据标出的是换了
/// 之后**可能**执行不了的那几条——客户端若要一个官方 schema 里没有的必填参数，模型按官方
/// schema 永远不会给。那种客户端的工具在换之前本来也和官方不是一回事，出了问题至少日志里
/// 点得出名字。
fn same_schema_surface(client: &serde_json::Value, official: &serde_json::Value) -> bool {
    fn surface(t: &serde_json::Value) -> Option<(Vec<&str>, Vec<&str>)> {
        let schema = t.get("input_schema")?.as_object()?;
        let mut props: Vec<&str> = match schema.get("properties") {
            Some(p) => p.as_object()?.keys().map(String::as_str).collect(),
            None => Vec::new(),
        };
        let mut required: Vec<&str> = match schema.get("required") {
            Some(r) => r.as_array()?.iter().filter_map(|x| x.as_str()).collect(),
            None => Vec::new(),
        };
        props.sort_unstable();
        required.sort_unstable();
        Some((props, required))
    }
    matches!((surface(client), surface(official)), (Some(a), Some(b)) if a == b)
}

/// 把该 profile 的 11 个官方主线程工具对齐进 `tools`：出站列表**以这 11 条按官方声明序开头**，
/// 每一条都是资产里那个对象（客户端没声明的是补的，声明了同名的是换的），客户端其余工具
/// 跟在后面、相对次序不变。补哪几个由 [`cc_tools_to_inject`] 定，流水那侧对的也是这一份。
///
/// **同名为什么一律换**：会走到这里的客户端本来就在模拟路径上，它用了官方名却自己写描述、
/// 自己拼 schema，这条声明与官方的差别正是上游最容易盯的形态之一（官方客户端连
/// `toolSchemaCharLengths` 都逐条上报）。既然整条请求已在按官方形态重建，同名工具留一份
/// 自己写的版本只是留一处破绽。代价：参数表面与官方不一致的客户端（多要一个必填参数之类），
/// 模型按官方 schema 拼的入参它可能不认——换之前 [`same_schema_surface`] 把这些名字打进
/// 日志，出了事对得上是哪个客户端的哪条工具。
///
/// **为什么不是原位换、缺的插头部**：那样客户端只缺 ListAgents 等四个时，补的四个全排在
/// Agent 前面，11 条的相对次序就不是官方的了。官方的内建工具是一段固定次序，MCP 工具跟在
/// 最后（`cap/2.1.258-api/00006`：`Write` 之后才是 `mcp__ide__*`），这里照这个形态排。
///
/// **同名声明里显式写的 `eager_input_streaming` 保留**：那是客户端的设置（true 或 false 都是），
/// 换成官方对象时不能顺手抹掉（fable 资产没有这个键）或改成资产的值（opus 资产是 true）。
/// 与 [`fill_eager_tools`]「已有值不覆盖」是同一条约定，只是这里发生在替换那一步。其余字段
/// 一律取资产的。
///
/// **换不换不看 JSON 值相等**：`Value` 的相等忽略对象键序，客户端一条内容全同、键序不同的
/// 声明会被当成「已经是官方的」跳过，出站就不是逐字节的官方声明了。故 11 条一律以资产对象
/// 落位，「有没有变」按紧凑序列化的字节比——这只影响日志计数与 [`rewrite_body`] 那条
/// 「什么都没改就原样透传」的快路。
fn inject_cc_tools(v: &mut serde_json::Value, profile: &config::CcProfile) -> bool {
    let missing = cc_tools_to_inject(v, profile);
    let Some(tools) = v.get_mut("tools").and_then(|t| t.as_array_mut()) else {
        return false;
    };
    let stubs = cc_tools_core(profile);
    let official_names: Vec<&str> =
        stubs.iter().filter_map(|s| s.get("name").and_then(|n| n.as_str())).collect();
    let name_of = |t: &serde_json::Value| t.get("name").and_then(|n| n.as_str()).map(str::to_owned);

    // 客户端的同名声明与官方那条差在哪：只为日志与计数，落位一律用官方对象。
    let mut replaced = 0usize;
    let mut surface_differs: Vec<&str> = Vec::new();
    for stub in stubs {
        let Some(name) = stub.get("name").and_then(|n| n.as_str()) else { continue };
        for t in tools.iter().filter(|t| t.get("name").and_then(|n| n.as_str()) == Some(name)) {
            if serde_json::to_string(t).ok() != serde_json::to_string(stub).ok() {
                replaced += 1;
                if !same_schema_surface(t, stub) {
                    surface_differs.push(name);
                }
            }
        }
    }

    let before: Vec<Option<String>> = tools.iter().map(name_of).collect();
    let mut aligned: Vec<serde_json::Value> = stubs
        .iter()
        .map(|stub| {
            let mut out = stub.clone();
            let name = stub.get("name").and_then(|n| n.as_str());
            let explicit = tools
                .iter()
                .find(|t| t.get("name").and_then(|n| n.as_str()) == name)
                .and_then(|t| t.get("eager_input_streaming"))
                .cloned();
            if let Some(explicit) = explicit
                && let Some(obj) = out.as_object_mut()
            {
                obj.insert("eager_input_streaming".into(), explicit);
            }
            out
        })
        .collect();
    aligned.extend(
        tools
            .iter()
            .filter(|t| {
                !t.get("name").and_then(|n| n.as_str()).is_some_and(|n| official_names.contains(&n))
            })
            .cloned(),
    );
    let reordered = aligned.iter().map(name_of).collect::<Vec<_>>() != before;
    if missing.is_empty() && replaced == 0 && !reordered {
        return false;
    }
    *tools = aligned;

    if !surface_differs.is_empty() {
        tracing::info!(
            tools = %surface_differs.join(","),
            "replaced same-named client tools whose parameter surface differs from the official one"
        );
    }
    tracing::info!(
        injected = missing.len(),
        replaced,
        reordered,
        "aligned CC main-thread tool stubs for simulation"
    );
    true
}

/// `tools` 数组按 `name` 去重：保留每个名字的首次出现，丢弃后续重复声明。
/// 上游对重复名直接 400（`Tool names must be unique`），而客户端侧不一定能改。
/// 上游不支持 `input_schema` 顶层的 `allOf` / `oneOf` / `anyOf`（直接 400），
/// 这里把它们展平为一个普通 `object` schema。
///
/// - **`allOf`**：按序合并——`properties` 取并集（后覆前），`required` 取并集，其余键后覆前。
///   顶层如果还有 `type`/`properties` 等，先当第 0 块参与合并。
/// - **`oneOf` / `anyOf`**：单元素直接解包；多元素按 `allOf` 策略合并（properties 取并集，
///   required 取并集——比「丢掉所有分支」保留了更多信息）。
/// - 嵌套不管：只修顶层，深层的 `allOf` 等留给上游——它只对顶层报错。
pub(super) fn flatten_tool_schemas(v: &mut serde_json::Value) -> bool {
    let Some(tools) = v.get_mut("tools").and_then(|t| t.as_array_mut()) else {
        return false;
    };
    let mut changed = false;
    for tool in tools.iter_mut() {
        let Some(schema) = tool.get_mut("input_schema").and_then(|s| s.as_object_mut()) else {
            continue;
        };
        // 取出 compound 关键字（只看顶层）。
        let compound = ["allOf", "oneOf", "anyOf"].iter().find_map(|k| schema.remove(*k));
        let Some(serde_json::Value::Array(parts)) = compound else {
            continue;
        };
        // 把当前顶层属性也算进去作为「第 0 块」。
        let mut merged = serde_json::Value::Object(std::mem::take(schema));
        for part in &parts {
            merge_schema_into(&mut merged, part);
        }
        if let Some(obj) = merged.as_object_mut() {
            obj.entry("type").or_insert_with(|| serde_json::Value::String("object".into()));
        }
        let serde_json::Value::Object(m) = merged else { continue };
        *schema = m;
        changed = true;
    }
    if changed {
        tracing::info!("flattened top-level allOf/oneOf/anyOf in tool input_schema");
    }
    changed
}

/// 把 `src` 的字段合并进 `dst`：`properties` 取并集，`required` 取并集，其余后覆前。
fn merge_schema_into(dst: &mut serde_json::Value, src: &serde_json::Value) {
    let (Some(dst_obj), Some(src_obj)) = (dst.as_object_mut(), src.as_object()) else {
        return;
    };
    for (k, v) in src_obj {
        match k.as_str() {
            "properties" => {
                let props = dst_obj
                    .entry("properties")
                    .or_insert_with(|| serde_json::Value::Object(Default::default()));
                if let (Some(existing), Some(new)) = (props.as_object_mut(), v.as_object()) {
                    for (pk, pv) in new {
                        existing.insert(pk.clone(), pv.clone());
                    }
                }
            }
            "required" => {
                let req =
                    dst_obj.entry("required").or_insert_with(|| serde_json::Value::Array(vec![]));
                if let (Some(existing), Some(new)) = (req.as_array_mut(), v.as_array()) {
                    for item in new {
                        if !existing.contains(item) {
                            existing.push(item.clone());
                        }
                    }
                }
            }
            _ => {
                dst_obj.insert(k.clone(), v.clone());
            }
        }
    }
}

/// 上游要求 `text` 内容块的 `text` 字段非空（`text content blocks must be non-empty`），
/// 部分第三方客户端会发 `{"type":"text","text":""}` 的空块。
///
/// 此函数遍历 `messages`，从每条消息的 `content` 数组里剥掉空 text 块。
/// **安全守则**：剥完后若 content 变空则不动——空数组是另一种上游必拒的形态，
/// 不该把一种 400 换成另一种。
pub(super) fn strip_empty_text_blocks(v: &mut serde_json::Value) -> bool {
    let Some(msgs) = v.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return false;
    };
    let mut changed = false;
    for msg in msgs.iter_mut() {
        let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        let non_empty_count = content
            .iter()
            .filter(|blk| {
                let is_empty_text = blk.get("type").and_then(|t| t.as_str()) == Some("text")
                    && blk.get("text").and_then(|t| t.as_str()).is_some_and(|t| t.is_empty());
                !is_empty_text
            })
            .count();
        if non_empty_count == content.len() || non_empty_count == 0 {
            continue;
        }
        content.retain(|blk| {
            let is_empty_text = blk.get("type").and_then(|t| t.as_str()) == Some("text")
                && blk.get("text").and_then(|t| t.as_str()).is_some_and(|t| t.is_empty());
            !is_empty_text
        });
        changed = true;
    }
    if changed {
        tracing::info!("stripped empty text content blocks from messages");
    }
    changed
}

/// 丢掉 `content` 为**空壳**的 `role:"system"` 消息：空数组、空串、字段缺失或为 `null`、
/// 以及整条只有空 `text` 块的。返回是否确有丢掉的。
///
/// 上游对这种消息恒回 400（`messages.N: system content must contain at least one block`；
/// 全是空 text 块的那种是 `text content blocks must be non-empty`）。实跑里撞上它的是一条
/// `claude-cli/2.1.270 (external, claude-vscode, agent-sdk/0.3.270)` 的正经 CC 请求
/// （`req_grlwDAtQQpqvf54d`，透传、没走模拟）：官方在 `messages` 里合法使用 `role:"system"`
/// （deferred tools），这次那条是个空壳。
///
/// **不受 `hoist_system_role` 开关与「CC 形态跳过」那道豁免管**，理由是两者的取舍在这里都不
/// 成立：豁免是怕把官方合法的 `role:"system"` 提升掉、破坏形态，而空壳一个块都没有，不携带
/// 任何语义，留着必是一次 400、丢掉什么也不丢；开关管的是「要不要替第三方客户端把 system
/// 挪位置」，也与「上游必拒的形态」无关。只在上游本来就会拒的请求上动手，所以它不可能把一条
/// 本来能过的请求改坏。
///
/// **只碰 `role:"system"`**：空 content 的 user / assistant 消息同样会被上游拒，但删掉它们会
/// 改变轮次交替（末轮变成 assistant、整个 messages 变空……），那是另一回事，不在这里处理。
pub(super) fn drop_empty_system_messages(v: &mut serde_json::Value) -> bool {
    let Some(msgs) = v.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return false;
    };
    let total = msgs.len();
    let mut dropped: Vec<String> = Vec::new();
    let is_empty_shell = |msg: &serde_json::Value| {
        if msg.get("role").and_then(|r| r.as_str()) != Some("system") {
            return false;
        }
        match msg.get("content") {
            // 字段缺失或写成 null。
            None | Some(serde_json::Value::Null) => true,
            // 空串。**不 trim**：一个空格在上游那边是合法的非空文本，判它为空就是替客户端
            // 删掉一条它认为有内容的消息。
            Some(serde_json::Value::String(s)) => s.is_empty(),
            // 空数组，或整条只有空 `text` 块——后者 `strip_empty_text_blocks` 按约定不会去剥
            // （剥完会变空），留下来同样是一次 400。
            Some(serde_json::Value::Array(arr)) => arr.iter().all(|blk| {
                blk.get("type").and_then(|t| t.as_str()) == Some("text")
                    && blk.get("text").and_then(|t| t.as_str()).is_some_and(str::is_empty)
            }),
            // 别的形态（对象、数字……）不是本函数的事，交给上游去说。
            Some(_) => false,
        }
    };
    for (i, msg) in msgs.iter().enumerate() {
        if is_empty_shell(msg) {
            dropped.push(format!("{i}/{total}"));
        }
    }
    if dropped.is_empty() {
        return false;
    }
    msgs.retain(|msg| !is_empty_shell(msg));
    tracing::info!(
        count = dropped.len(),
        at = %dropped.join(", "),
        "dropped empty role:\"system\" messages: upstream rejects a system message with no content blocks"
    );
    true
}

/// 把 `messages` 里 `role:"system"` 的消息提升到顶层 `system` 字段。
///
/// litellm 等第三方客户端采用 OpenAI 格式，把 system 指令放在 `messages` 数组里
/// （`{"role":"system","content":"..."}`），Anthropic API 不认这个 role（直接 400）。
///
/// 处理逻辑：
/// 1. 从 `messages` 里找出所有 `role:"system"` 的消息，按原序收集其 content。
/// 2. 将收集到的 content 块**前置**到顶层 `system`（已有则合并，没有则新建）。
/// 3. 从 `messages` 里移除这些消息。
///
/// content 的形态：OpenAI 格式通常是纯字符串（`"content":"You are a helpful assistant"`），
/// 也可能是 Anthropic 格式的内容块数组。两种都处理。
pub(super) fn hoist_system_role_messages(v: &mut serde_json::Value) -> bool {
    let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) else {
        return false;
    };
    let mut hoisted_blocks: Vec<serde_json::Value> = Vec::new();
    let mut indices_to_remove: Vec<usize> = Vec::new();
    for (i, msg) in msgs.iter().enumerate() {
        if msg.get("role").and_then(|r| r.as_str()) != Some("system") {
            continue;
        }
        indices_to_remove.push(i);
        match msg.get("content") {
            Some(serde_json::Value::String(s)) => {
                if !s.is_empty() {
                    hoisted_blocks.push(serde_json::json!({"type": "text", "text": s}));
                }
            }
            Some(serde_json::Value::Array(arr)) => {
                hoisted_blocks.extend(arr.iter().cloned());
            }
            _ => {}
        }
    }
    if indices_to_remove.is_empty() {
        return false;
    }
    // 从 messages 里移除（倒序，避免索引偏移）。
    let msgs = v.get_mut("messages").and_then(|m| m.as_array_mut()).unwrap();
    for &i in indices_to_remove.iter().rev() {
        msgs.remove(i);
    }
    // 合并到顶层 system：已有的内容追加在 hoisted 之后（system 消息在前、原有 system 在后）。
    if !hoisted_blocks.is_empty() {
        let existing: Vec<serde_json::Value> = match v.get_mut("system").map(|s| s.take()) {
            Some(serde_json::Value::String(s)) => {
                if s.is_empty() {
                    Vec::new()
                } else {
                    vec![serde_json::json!({"type": "text", "text": s})]
                }
            }
            Some(serde_json::Value::Array(arr)) => arr,
            _ => Vec::new(),
        };
        hoisted_blocks.extend(existing);
        v.as_object_mut()
            .unwrap()
            .insert("system".into(), serde_json::Value::Array(hoisted_blocks));
    }
    tracing::info!(
        removed = indices_to_remove.len(),
        "hoisted role:system messages to top-level system field"
    );
    true
}

pub(super) fn dedup_tools(v: &mut serde_json::Value) -> bool {
    let Some(tools) = v.get_mut("tools").and_then(|t| t.as_array_mut()) else {
        return false;
    };
    let before = tools.len();
    let mut seen = std::collections::HashSet::new();
    tools.retain(|t| {
        let name = t.get("name").and_then(|n| n.as_str()).unwrap_or_default();
        seen.insert(name.to_string())
    });
    let removed = before - tools.len();
    if removed > 0 {
        tracing::info!(removed, "deduped tools array (duplicate tool names)");
    }
    removed > 0
}

/// 把映射应用到请求体，返回是否改动过。三处必须**同时**改：
///
/// - `$.tools[*].name`
/// - `$.tool_choice.name`（仅 `type == "tool"`，即客户端强制指定了某个工具）
/// - `$.messages[*].content[*].name`（仅 `type == "tool_use"`，即历史里的工具调用）
///
/// 漏掉第三处的话，上游会因为 `tool_use` 引用了一个 `tools` 里没声明的名字而拒掉整条请求。
pub(super) fn apply_tool_names(v: &mut serde_json::Value, map: &ToolNameMap) -> bool {
    let mut changed = false;
    let mut rename = |obj: &mut serde_json::Value| {
        let Some(name) = obj.get("name").and_then(|n| n.as_str()) else { return };
        let Some(fake) = map.forward.get(name) else { return };
        obj["name"] = serde_json::Value::String(fake.clone());
        changed = true;
    };

    if let Some(tools) = v.get_mut("tools").and_then(|t| t.as_array_mut()) {
        for t in tools.iter_mut() {
            if should_mimic_tool(t) {
                rename(t);
            }
        }
    }
    if let Some(tc) = v.get_mut("tool_choice")
        && tc.get("type").and_then(|t| t.as_str()) == Some("tool")
    {
        rename(tc);
    }
    if let Some(messages) = v.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            let Some(blocks) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
                continue;
            };
            for b in blocks.iter_mut() {
                if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    rename(b);
                }
            }
        }
    }
    changed
}

impl ToolNameMap {
    /// 回程还原：假名 → 真名。按假名长度倒序逐个替换。
    ///
    /// **按字节而不是按 `str` 做**：回程是流式的，一个 chunk 可以在任意字节处切断，
    /// `String::from_utf8` 会在半个多字节字符上失败。假名全是 ASCII，字节级替换在 UTF-8 上
    /// 安全（ASCII 不会出现在多字节序列内部）。
    pub(super) fn restore(&self, buf: &[u8]) -> Vec<u8> {
        let mut out = buf.to_vec();
        for (fake, real) in &self.reverse {
            out = replace_bytes(&out, fake.as_bytes(), real.as_bytes());
        }
        out
    }

    /// 流式还原的一步：吃进一块，吐出**可以安全发走**的部分。
    ///
    /// 假名可能被 TCP 分块从中间切开（`analyze_ski00` 拆成 `analyze_sk` + `i00`），那一次就
    /// 还原不了，客户端会拿到假名，下一轮请求带着假名回来，请求侧映射表里查不到，上游收到
    /// 未声明的工具名再回一个 400。
    ///
    /// **顺序是「先整体还原、再留尾」，不能反过来**：先按长度切、只还原切出去的那半，
    /// 跨在切点上的假名照样被劈开——留多少字节都挡不住，因为切点可以落在假名内部的任意位置。
    /// 先对 `pending ‖ chunk` 整体做一次替换，完整的假名就都换掉了；剩下最多
    /// `max_fake - 1` 个字节可能是某个假名的前半截，留到下一轮与后续字节拼起来再替。
    /// 重复还原是幂等的（真名里不含假名），故留下来那段下一轮再过一遍也不会出错。
    pub(super) fn feed(&self, pending: &mut Vec<u8>, chunk: &[u8]) -> Bytes {
        pending.extend_from_slice(chunk);
        let restored = self.restore(pending);
        let hold = self.max_fake.saturating_sub(1).min(restored.len());
        let cut = restored.len() - hold;
        *pending = restored[cut..].to_vec();
        Bytes::copy_from_slice(&restored[..cut])
    }

    /// 流结束时把留存的尾巴吐出来。**不能省**：SSE 以 `\n\n` 收尾，尾巴扣着不发的话
    /// 客户端的解析器会一直等那个终止符。
    pub(super) fn flush(&self, pending: &mut Vec<u8>) -> Bytes {
        if pending.is_empty() {
            return Bytes::new();
        }
        Bytes::from(self.restore(&std::mem::take(pending)))
    }
}

/// 字节级子串替换。`from` 为空时原样返回（否则会死循环）。
fn replace_bytes(haystack: &[u8], from: &[u8], to: &[u8]) -> Vec<u8> {
    if from.is_empty() || haystack.len() < from.len() {
        return haystack.to_vec();
    }
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i <= haystack.len() - from.len() {
        if &haystack[i..i + from.len()] == from {
            out.extend_from_slice(to);
            i += from.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out.extend_from_slice(&haystack[i..]);
    out
}

/// 把上游响应流包一层工具名还原。滑动窗口的状态跟着流走，流结束时 flush 尾巴。
///
/// 用 `unfold` 而不是 `map`：`map` 收不到「上游流结束」这个事件，没法把留存的尾字节吐出去。
pub(super) fn restore_tool_names_stream<S>(
    inner: S,
    map: std::sync::Arc<ToolNameMap>,
) -> impl futures_util::Stream<Item = Result<Bytes, wreq::Error>>
where
    S: futures_util::Stream<Item = Result<Bytes, wreq::Error>> + Unpin,
{
    futures_util::stream::unfold(
        (inner, map, Vec::<u8>::new(), false),
        |(mut inner, map, mut pending, done)| async move {
            if done {
                return None;
            }
            match inner.next().await {
                Some(Ok(bytes)) => {
                    let out = map.feed(&mut pending, &bytes);
                    Some((Ok(out), (inner, map, pending, false)))
                }
                // 上游把流掐了：错误原样交给下游（行为与不还原时一致），本次不再吐尾巴——
                // 半截的假名还原出来也是半截，交给客户端反而更糟。
                Some(Err(e)) => Some((Err(e), (inner, map, pending, true))),
                None => {
                    let tail = map.flush(&mut pending);
                    (!tail.is_empty()).then(|| (Ok(tail), (inner, map, pending, true)))
                }
            }
        },
    )
}

/// 把 body 里**所有**缓存断点的 `ttl` 统一成 `1h`，返回是否改动过。只在
/// [`store::ForwardFlags::cache_ttl_1h`] 开着时调用。
///
/// **为什么要走一遍全身**：[`align_system_shape`] 只重建 `system` 那两块，客户端自己标在
/// `messages`/`tools` 上的断点不在它手里。于是 0.2.50 之后出现过一种官方不产生的组合——
/// `cap/raw/00012`（真 CC 经 luban）复现：system 两个断点有 `ttl:"1h"`、消息那个没有。
/// 而官方三个断点**要么都有**（订阅模式 00009）、**要么都没有**（API-key 模式 00012），
/// 没有中间态。这与 [`ensure_beta_query`] 当初要消灭的是同一个形状：只对齐了一半，
/// 拼出个两边都不像的组合。
///
/// **客户端已有的短 `ttl` 也升级**：上游要求 `ttl` 按处理序（tools → system → messages）
/// 单调不增；客户端 `tools` 带 `ttl:"5m"` 而 luban 在 `system` 写 `ttl:"1h"` 会导致
/// `5m → 1h` 被拒（400）。既然本开关的意图就是全部走 1h，统一升级既安全又消除排序冲突。
///
/// 键序按官方 `type` → `ttl` → `scope` **重建**而非追加：客户端若已写了 `scope`，
/// 直接追加会得到 `{type,scope,ttl}` 这个官方不产生的排列。
fn fill_cache_ttl(v: &mut serde_json::Value) -> bool {
    let mut changed = false;
    match v {
        serde_json::Value::Object(map) => {
            if let Some(cc) = map.get_mut("cache_control").and_then(|c| c.as_object_mut())
                && cc.get("ttl").and_then(|t| t.as_str()) != Some("1h")
            {
                let mut rebuilt = serde_json::Map::new();
                if let Some(t) = cc.get("type") {
                    rebuilt.insert("type".into(), t.clone());
                }
                rebuilt.insert("ttl".into(), "1h".into());
                for (k, val) in cc.iter() {
                    if k != "type" && k != "ttl" {
                        rebuilt.insert(k.clone(), val.clone());
                    }
                }
                *cc = rebuilt;
                changed = true;
            }
            for (k, val) in map.iter_mut() {
                if k != "cache_control" {
                    changed |= fill_cache_ttl(val);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for it in items.iter_mut() {
                changed |= fill_cache_ttl(it);
            }
        }
        _ => {}
    }
    changed
}

/// 构造一个 `system` 文本块，key 序与官方一致：`type` → `text` → `cache_control`。
pub(super) fn text_block(text: &str, cache_control: serde_json::Value) -> serde_json::Value {
    let mut blk = serde_json::Map::new();
    blk.insert("type".into(), "text".into());
    blk.insert("text".into(), text.into());
    blk.insert("cache_control".into(), cache_control);
    serde_json::Value::Object(blk)
}

/// 给没有 billing header 的请求补上最小 CC 前缀：`[billing, 身份句]` 前插到 `system`。
///
/// CC 子代理（explore/search agent）和 desktop-3p 有时不带 system 字段或不含 billing
/// header。不补的话上游按第三方应用计——扣超额池、限流更严。补上 billing + 身份句后
/// 上游按订阅额度计，与标准 CC 请求一致。
///
/// **不是模拟**——不换头、不改工具名、不加基座，只在 system 最前面插两块。
/// 已有 billing header 的（`is_cc_shaped` 命中 billing 那条路、或模拟已补过的）跳过。
///
/// **身份句已在就只补 billing header。** 老版本 API-key 模式的 CC（现网 `claude-cli/2.1.238`，
/// req_ujomarOOPtXL38jx）发的 system 是 `[身份句(带断点), 基座, …]`——有身份句、没 billing
/// header。原先只查 billing header 在不在，于是两块都插，出站变成
/// `[billing, 身份句, 身份句(带断点), …]`：身份句重复、块数还多了一块。改成按块判：身份句
/// （[`config::CC_SYSTEM_IDENTITY_PREFIX`]，含 agent-sdk 那种逗号变体）已在任一块里，就只在
/// 最前面插 billing header；官方序本来就是 billing 在身份句之前，客户端的身份句连同它自己的
/// `cache_control` 原样留在第二块。
///
/// `version` 是这个来访**自报**的客户端版本（从它自己的 UA 里解出），补出来的
/// `cc_version` 就用它，见 [`billing_header_text`]。
fn ensure_cc_system_prefix(
    v: &mut serde_json::Value,
    version: Option<&str>,
    kind: CcRequestKind,
) -> bool {
    let has_billing = match v.get("system") {
        Some(serde_json::Value::Array(blocks)) => blocks.iter().any(|b| {
            b.get("text")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.starts_with("x-anthropic-billing-header:"))
        }),
        Some(serde_json::Value::String(s)) => s.contains("x-anthropic-billing-header:"),
        _ => false,
    };
    if has_billing {
        return false;
    }
    let has_identity = match v.get("system") {
        Some(serde_json::Value::Array(blocks)) => blocks.iter().any(|b| {
            b.get("text")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t.contains(config::CC_SYSTEM_IDENTITY_PREFIX))
        }),
        Some(serde_json::Value::String(s)) => s.contains(config::CC_SYSTEM_IDENTITY_PREFIX),
        _ => false,
    };
    let mut prefix = vec![text_block_bare(&billing_header_text(v, version, kind))];
    if !has_identity {
        prefix.push(text_block_bare(config::CC_SYSTEM_IDENTITY));
    }
    match v.get_mut("system") {
        Some(serde_json::Value::Array(blocks)) => {
            for (i, blk) in prefix.into_iter().rev().enumerate() {
                let _ = i;
                blocks.insert(0, blk);
            }
        }
        Some(serde_json::Value::String(s)) => {
            let mut blocks = prefix;
            if !s.is_empty() {
                blocks.push(text_block_bare(s));
            }
            *v.get_mut("system").unwrap() = serde_json::Value::Array(blocks);
        }
        _ => {
            insert_top_level(v, "system", serde_json::Value::Array(prefix), &["messages", "model"]);
        }
    }
    if has_identity {
        tracing::info!(
            "injected billing header into system for a CC client that had only the identity line"
        );
    } else {
        tracing::info!(
            "injected billing header + identity into system for a CC client without them"
        );
    }
    true
}

/// 不带缓存断点的 `system` 文本块（官方的 `system[0]`/`system[1]` 都是这个形态）。
pub(super) fn text_block_bare(text: &str) -> serde_json::Value {
    let mut blk = serde_json::Map::new();
    blk.insert("type".into(), "text".into());
    blk.insert("text".into(), text.into());
    serde_json::Value::Object(blk)
}

/// 缓存断点的两项可选形态，各由一个开关拨。合成一个结构体而不是并排传两个 `bool`：
/// 相邻同型参数换了位置编译器不会吭声，而这两项落错地方产出的都是官方不发的组合。
#[derive(Clone, Copy)]
pub(crate) struct CacheShape {
    /// 标 `scope:"global"`。**只有基座那块**该带，见 [`store::ForwardFlags::cache_scope_global`]。
    pub(super) global: bool,
    /// 写 `ttl:"1h"`。官方**每个断点都带**，见 [`store::ForwardFlags::cache_ttl_1h`]。
    pub(super) ttl_1h: bool,
}

impl CacheShape {
    /// 非基座断点的形态：去掉 `scope`、保留 `ttl`——官方只在基座标 `scope`
    /// （`cap/raw/00006` 三个断点里仅一个有），而三个断点**都**有 `ttl`。
    pub(super) fn tail(self) -> Self {
        Self { global: false, ..self }
    }
}

/// 构造 `cache_control`，key 序与官方一致：`type` → `ttl` → `scope`
/// （逐字节取自 `cap/raw/00006`：`{"type":"ephemeral","ttl":"1h","scope":"global"}`）。
///
/// `ttl:"1h"` **默认写**，对齐官方——四份订阅直连抓包的三个断点 3/3 全是 `1h`，不写就是
/// 每条请求上一处稳定差异。代价要知情：1h 的缓存**写入**单价是默认 5m 的 2 倍,故
/// [`store::ForwardFlags::cache_ttl_1h`] 可以关掉，关掉即沿用客户端自己传的时长。
/// 长会话里 1h 通常反而更省（5m 内没接上话就得按写入价重写一遍），但那取决于使用节奏，
/// 所以给了开关。客户端自己写了 `ttl` 的照发，两条路都不覆盖它。
///
/// `global` 同理由 [`store::ForwardFlags::cache_scope_global`] 拨。两项各要一个 beta 认
/// （`prompt-caching-scope` / `extended-cache-ttl`），故都还连着 `merge_beta`，
/// 见 [`rewrite_body`]。
pub(super) fn cache_control(shape: CacheShape) -> serde_json::Value {
    let mut cc = serde_json::Map::new();
    cc.insert("type".into(), "ephemeral".into());
    if shape.ttl_1h {
        cc.insert("ttl".into(), "1h".into());
    }
    if shape.global {
        cc.insert("scope".into(), "global".into());
    }
    serde_json::Value::Object(cc)
}

/// 测试用的最小请求体：一条 `ping`、`max_tokens=1`。
///
/// 其余部分（官方 `system` 四块、`metadata` 身份）由 [`rewrite_body`] 在模拟路径上补齐，
/// 与真实转发用的是同一份代码——这里手抄一份官方形态，只会得到「测试通过但转发失败」。
///
/// key 序按官方的 `model → messages → … → max_tokens` 写；补出来的 `system`/`metadata`
/// 会被 [`insert_top_level`] 放到它们的官方位置上。
///
/// 不发 `stream: true`（官方客户端恒为流式）：一条 1 token 的响应用非流式读最省事，而这
/// 属于任何 API 客户端都会产生的常规形态，不是「真实客户端不产生」的那类破绽。
/// 这条请求**实际发出去的** `metadata.user_id` 里那份身份。
#[derive(Debug, Clone, Default)]
pub(super) struct OutboundIdentity {
    pub(super) device_id: String,
    pub(super) account_uuid: String,
    /// 出站体里那串 `user_id` 的**原文**（连编码形态一起）。
    ///
    /// 额度探测直接复用它，而不是拿上面两个字段重新拼一份 JSON：客户端可能用的是
    /// Windows 那种扁平串（`user_<device>_account_<account>_session_<session>`，
    /// 见 [`parse_flat_user_id`]），[`spoof_identity`] 改写完仍是扁平串。重新拼成 JSON
    /// 就会出现「同一个会话的两条请求，一条扁平一条 JSON」这种官方不产生的组合。
    ///
    /// 出站体压根没有 `metadata.user_id` 时为 `None`。
    pub(super) raw_user_id: Option<String>,
}

/// 从**已经改写完的出站体**里读身份，而不是重新按 `(cred, device_fp)` 派生一份。
///
/// 两者在默认配置下相同，但开关一改就分家：
///
/// - `spoof_device_id = false`（严格抓包对齐模式支持的行为）：主请求里的 `device_id`
///   **保留客户端自己的**，只换 `account_uuid`；
/// - `spoof_identity = false`：整份身份原样透传，一个字段都不动。
///
/// 这两种配置下再去派生一份，握手/额度探测/启动遥测报的就是另一台设备，而主请求报的是
/// 客户端那台——同一个会话在上游看来来自两台机器。所以只能读出站体。
///
/// **两种编码都要认。** 只解 JSON 的话，Windows 那种扁平串会解析失败、退回「device 为空
/// + 凭证账号」——主请求有设备、握手却没有，比不补更显眼。
///
/// 只有**整个 `user_id` 都不存在**时才退回凭证的 `account_uuid`（事件的 `auth` 块总得有个
/// 账号）。字段存在但为空时照实报空——那才是「实际出站身份」，`spoof_identity` 关掉时
/// 尤其如此。
///
/// 这里会把整个出站体解析一遍。**只在会话第一条请求上调用一次**，那点开销可以接受；
/// 换成按 `(cred, device_fp, flags)` 重算一份逻辑，就得把 `spoof_identity` /
/// `ensure_cc_metadata` 的分支在这里抄第二份，迟早对不上。
pub(super) fn outbound_identity(
    sent: &Bytes,
    cred: &crate::credentials::Credential,
) -> OutboundIdentity {
    let no_identity = || OutboundIdentity {
        device_id: String::new(),
        account_uuid: cred.account_uuid.clone().unwrap_or_default(),
        raw_user_id: None,
    };
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(sent) else { return no_identity() };
    let Some(raw) = v.get("metadata").and_then(|m| m.get("user_id")).and_then(|u| u.as_str())
    else {
        return no_identity();
    };
    // 形态一：CC 的内嵌 JSON。
    if let Ok(inner) = serde_json::from_str::<serde_json::Value>(raw)
        && inner.is_object()
    {
        let pick = |k: &str| inner.get(k).and_then(|x| x.as_str()).unwrap_or_default().to_string();
        return OutboundIdentity {
            device_id: pick("device_id"),
            account_uuid: pick("account_uuid"),
            raw_user_id: Some(raw.to_string()),
        };
    }
    // 形态二：Windows 那种扁平串。
    if let Some(flat) = parse_flat_user_id(raw) {
        return OutboundIdentity {
            device_id: flat.device,
            account_uuid: flat.account,
            raw_user_id: Some(raw.to_string()),
        };
    }
    // 认不出的第三种形态：原文照样留着给额度探测复用，字段只能空着。
    OutboundIdentity {
        device_id: String::new(),
        account_uuid: cred.account_uuid.clone().unwrap_or_default(),
        raw_user_id: Some(raw.to_string()),
    }
}

/// 把额度探测的 `metadata.user_id` 换成**主请求实际发出去的那串原文**。
///
/// 官方那条额度探测与同会话的首条 messages 是同一个进程发的，`metadata.user_id` 逐字节
/// 相同——**包括编码形态**。所以这里复用原文而不是拿字段重拼：客户端可能用的是 Windows
/// 那种扁平串，重拼成 JSON 就成了「同一会话一条扁平一条 JSON」。
///
/// 主请求没发身份（`raw_user_id` 为 `None`）时原样交回：那种情况下探测体里
/// [`ensure_cc_metadata`] 造的那份就是它唯一能有的身份，换掉反而更不一致。
pub(super) fn with_outbound_identity(body: Bytes, ident: &OutboundIdentity) -> Bytes {
    let Some(raw) = ident.raw_user_id.as_deref() else { return body };
    let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(&body) else { return body };
    match v.get_mut("metadata").and_then(|m| m.as_object_mut()) {
        Some(meta) => {
            meta.insert("user_id".into(), raw.into());
        }
        None => {
            let mut meta = serde_json::Map::new();
            meta.insert("user_id".into(), raw.into());
            insert_top_level(&mut v, "metadata", serde_json::Value::Object(meta), &["messages"]);
        }
    }
    serde_json::to_vec(&v).map(Bytes::from).unwrap_or(body)
}

#[cfg(test)]
mod tests {
    use crate::proxy::test_support::{
        ACCOUNT_UUID, API_SHAPE_BODY, PLAIN_BODY, all_on, base_block, detect_for, detect_with,
        err_json, parsed, platform_headers, rewrite_body, sim_for, test_cred,
    };
    use crate::proxy::{
        Bytes, HeaderValue, apply_tool_names, build_forward_headers, build_tool_name_map, config,
        ensure_billing_cch, header, is_billable_messages, merge_beta, normalize_tool_choice,
        replace_json_str_field, store, strip_extra_fields,
    };

    /// 设备身份校验与出站体改写的作用域：只认 `/v1/messages`，且 `count_tokens` 除外
    /// ——那条路径的请求体没有 `metadata` 可带，卡它等于把客户端的 token 预估打死。
    ///
    /// 入参是 `uri.path()`（不含查询串），故 `?beta=true` 不影响判定。
    #[test]
    fn count_tokens_is_not_billable() {
        assert!(is_billable_messages("/v1/messages"));
        assert!(!is_billable_messages("/v1/messages/count_tokens"));
        assert!(!is_billable_messages("/v1/models"));
    }

    /// 豁免精确匹配：任何「顶着 count_tokens 前缀但归一化后不是它」的路径都必须落回计费侧。
    /// 出站 URL 交给 wreq 时点段会按 RFC 3986 消解，`…/count_tokens/../` 到上游就成了
    /// `/v1/messages/`——前缀匹配会在这里漏掉设备校验，等于放开 `device_limit`。
    #[test]
    fn count_tokens_exemption_does_not_leak_via_prefix() {
        assert!(is_billable_messages("/v1/messages/count_tokens/.."));
        assert!(is_billable_messages("/v1/messages/count_tokens/../"));
        assert!(is_billable_messages("/v1/messages/count_tokens/"));
        assert!(is_billable_messages("/v1/messages/count_tokensX"));
    }

    /// 会话 id 的 body 兜底提取：两种 `metadata.user_id` 格式都要认得，且与设备 id 取的是
    /// **同一串里的不同段**——两者串了的话，会话闸会按设备分桶（同机多会话又挤在一起），
    /// 而这恰好是它要解决的问题。
    #[test]
    fn session_id_comes_from_either_user_id_format() {
        // 1) CC 内嵌 JSON。
        let inner = Bytes::from(
            r#"{"messages":[],"metadata":{"user_id":"{\"device_id\":\"d0\",\"account_uuid\":\"a0\",\"session_id\":\"5e3f\"}"}}"#
                .to_string(),
        );
        assert_eq!(
            crate::proxy::extract_session_id(parsed(&inner).as_ref()).as_deref(),
            Some("5e3f")
        );
        assert_eq!(crate::proxy::extract_device_id(parsed(&inner).as_ref()).as_deref(), Some("d0"));

        // 2) 扁平串（Windows 客户端那种形态），account 段允许为空。
        let flat = Bytes::from(
            r#"{"messages":[],"metadata":{"user_id":"user_dev9_account__session_sess9"}}"#
                .to_string(),
        );
        assert_eq!(
            crate::proxy::extract_session_id(parsed(&flat).as_ref()).as_deref(),
            Some("sess9")
        );
        assert_eq!(
            crate::proxy::extract_device_id(parsed(&flat).as_ref()).as_deref(),
            Some("dev9")
        );

        // 3) 认不出的格式 / 没有 metadata → None，此时这条请求不受会话闸管（由设备闸兜）。
        let odd = Bytes::from(
            r#"{"messages":[],"metadata":{"user_id":"whatever-new-format"}}"#.to_string(),
        );
        assert!(crate::proxy::extract_session_id(parsed(&odd).as_ref()).is_none());
        assert!(crate::proxy::extract_session_id(parsed(&Bytes::from("{}")).as_ref()).is_none());
    }

    /// 真实 CC（API-key 模式）的请求，body 侧要配套补 `thinking.display:"updates"`
    /// （fable：`cap/2.1.258/00013`；2.1.260 起 opus 主线程也发，`cap/2.1.260-2/00025`）。
    /// 客户端自己写了 `display` 的不动；`merge_beta` 关着就不补。
    ///
    /// **判据只有一个**：出站头里有没有那项 beta（这里的 `display_beta` 参数恒为 true）。
    /// 哪一族在哪一版发它由 [`crate::proxy::merge_beta`] 决定，见
    /// [`skips_thinking_display_without_the_beta`]。模拟路径不走这条（它由
    /// `ensure_thinking` 直接产出完整形态）。
    #[test]
    fn fills_thinking_display_for_cc_fable_requests() {
        let body = |model: &str, thinking: &str| {
            Bytes::from(format!(
                r#"{{"model":"{model}","messages":[{{"role":"user","content":"hi"}}],"system":[{{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."}}],"thinking":{thinking}}}"#
            ))
        };
        let run = |b: &Bytes, flags: store::ForwardFlags| -> serde_json::Value {
            serde_json::from_slice(&rewrite_body(b, &test_cred(), "fp", flags, None, None)).unwrap()
        };
        let fable = run(&body("claude-fable-5-1", r#"{"type":"adaptive"}"#), all_on());
        assert_eq!(
            fable["thinking"],
            serde_json::json!({"type": "adaptive", "display": "updates"}),
            "{fable}"
        );
        let fable_1m = run(&body("claude-fable-5-1[1m]", r#"{"type":"adaptive"}"#), all_on());
        assert_eq!(fable_1m["thinking"]["display"], "updates", "{fable_1m}");
        // 2.1.260 起 opus 主线程也发这项 beta，头上有了体里就该配套写。
        let opus = run(&body("claude-opus-5", r#"{"type":"adaptive"}"#), all_on());
        assert_eq!(
            opus["thinking"],
            serde_json::json!({"type": "adaptive", "display": "updates"}),
            "头上有 beta 就补，与模型族无关: {opus}"
        );
        let own = run(
            &body("claude-fable-5-1", r#"{"type":"adaptive","display":"summarized"}"#),
            all_on(),
        );
        assert_eq!(own["thinking"]["display"], "summarized", "客户端自己写的不动: {own}");
        let off = store::ForwardFlags { merge_beta: false, ..all_on() };
        let v = run(&body("claude-fable-5-1", r#"{"type":"adaptive"}"#), off);
        assert!(v["thinking"].get("display").is_none(), "merge_beta 关着就不补: {v}");
    }

    /// 回归 2026-09-02 的 400：`claude-vscode, agent-sdk/0.3.258` 发来的 fable-5-1 请求，
    /// beta 串没有 `advisor-tool`，`merge_beta` 不给它补 `thinking-display-updates`，
    /// body 却被写了 `display:"updates"`，上游回 `Input should be 'summarized', 'omitted'`。
    /// 体侧的补写必须跟着「出站头里到底有没有那项 beta」走。
    #[test]
    fn skips_thinking_display_without_the_beta() {
        let body = Bytes::from(
            r#"{"model":"claude-fable-5-1","messages":[{"role":"user","content":"hi"}],"system":[{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."}],"thinking":{"type":"adaptive"}}"#,
        );
        let out = crate::proxy::rewrite_body(
            &body,
            &test_cred(),
            "fp",
            all_on(),
            None,
            None,
            None,
            false,
            None,
            false,
            false,
            None,
            None,
            crate::proxy::CcRequestKind::Main,
            None,
        );
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["thinking"],
            serde_json::json!({"type": "adaptive"}),
            "头上没 beta 就不补: {v}"
        );

        // agent-sdk 那串（无 advisor-tool）经 merge_beta 也确实不会带上那项 beta——两边口径一致。
        let sdk_beta = "claude-code-20250219,interleaved-thinking-2025-05-14,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,effort-2025-11-24";
        let merged = merge_beta(Some(sdk_beta), Some("claude-fable-5-1"), Some((2, 1, 258)));
        assert!(!merged.contains("thinking-display-updates"), "老世代的串不补: {merged}");
    }

    /// 真 CC（API-key 三块形态）请求，工具五种形态各一个：内建、客户端显式写了 `false` 的内建、
    /// `mcp__*`、`defer_loading` 占位、服务端工具。
    fn eager_body(model: &str, with_identity: bool) -> Bytes {
        // 非 CC 形态：既没有身份句也没有 billing header（[`is_cc_shaped`] 两样任一都算 CC）。
        let system = if with_identity {
            serde_json::json!([
                {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.258.1e2; cc_entrypoint=cli;"},
                {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude.", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "\nBASE\n\nWrite code that reads like the surrounding code.", "cache_control": {"type": "ephemeral"}}
            ])
        } else {
            serde_json::json!([{"type": "text", "text": "You are a helpful assistant."}])
        };
        serde_json::json!({
            "model": model,
            "messages": [{"role": "user", "content": "hi"}],
            "system": system,
            "tools": [
                {"name": "Bash", "description": "run", "input_schema": {"type": "object"}},
                {"name": "Read", "description": "read", "input_schema": {"type": "object"}, "eager_input_streaming": false},
                {"name": "mcp__ide__getDiagnostics", "description": "d", "input_schema": {"type": "object"}},
                {"name": "DeferredToolPlaceholder", "description": "p", "input_schema": {"type": "object"}, "defer_loading": true},
                {"type": "web_search_20250305", "name": "web_search", "max_uses": 3}
            ],
            "max_tokens": 64000,
            "stream": true
        })
        .to_string()
        .into()
    }

    /// 真 CC 路径跑一遍 [`rewrite_body`]，只拨 eager 相关的几个入参。
    fn run_eager(
        model: &str,
        version: Option<&str>,
        kind: crate::proxy::CcRequestKind,
        adv_beta: bool,
        flags: store::ForwardFlags,
        with_identity: bool,
    ) -> serde_json::Value {
        let out = crate::proxy::rewrite_body(
            &eager_body(model, with_identity),
            &test_cred(),
            "fp",
            flags,
            None,
            None,
            None,
            false,
            None,
            true,
            adv_beta,
            version,
            None,
            kind,
            None,
        );
        serde_json::from_slice(&out).unwrap()
    }

    fn eager_of<'a>(v: &'a serde_json::Value, name: &str) -> Option<&'a serde_json::Value> {
        v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == name)
            .and_then(|t| t.get("eager_input_streaming"))
    }

    /// 真 CC 路径：已证「全带」的 profile（2.1.258 四族、2.1.260 opus、2.1.270 sonnet）给内建工具
    /// 补 `eager_input_streaming: true`，键落在对象末尾（官方声明序）；客户端显式写的 `false`、
    /// `mcp__*`、占位、服务端工具一个不动。猜下一句那条用途同样补（`cap/2.1.258/00025`）。
    #[test]
    fn fills_eager_input_streaming_on_verified_real_cc_profiles() {
        use crate::proxy::CcRequestKind::{Main, Suggestion};
        for (model, version, kind) in [
            ("claude-opus-5", "2.1.258", Main),
            ("claude-haiku-4-5-20251001", "2.1.258", Main),
            ("claude-fable-5-1", "2.1.258", Main),
            ("claude-opus-5", "2.1.260", Main),
            ("claude-sonnet-5", "2.1.270", Main),
            ("claude-opus-5", "2.1.258", Suggestion),
        ] {
            let v = run_eager(model, Some(version), kind, true, all_on(), true);
            let tag = format!("{model} {version} {kind:?}");
            assert_eq!(eager_of(&v, "Bash"), Some(&serde_json::json!(true)), "{tag}: 内建该补");
            assert_eq!(
                eager_of(&v, "Read"),
                Some(&serde_json::json!(false)),
                "{tag}: 显式 false 不覆盖"
            );
            assert!(eager_of(&v, "mcp__ide__getDiagnostics").is_none(), "{tag}: mcp 不猜");
            assert!(eager_of(&v, "DeferredToolPlaceholder").is_none(), "{tag}: 占位不动");
            assert!(eager_of(&v, "web_search").is_none(), "{tag}: 服务端工具不动");
            let bash = v["tools"].as_array().unwrap().iter().find(|t| t["name"] == "Bash").unwrap();
            assert_eq!(
                bash.as_object().unwrap().keys().next_back().map(String::as_str),
                Some("eager_input_streaming"),
                "{tag}: 键在末尾"
            );
        }
    }

    /// 真 CC 路径不补的每一种：profile 证实不带（2.1.260 fable）、没有样本（2.1.260 sonnet /
    /// haiku、2.1.270 opus、样本之间的 2.1.261）、出站头没有 `advanced-tool-use`、读不出版本、
    /// 用途不是主线程、开关关着、来访不是 CC 形态。
    ///
    /// 2.1.270 / 2.1.261 的 opus 钉的是「证据查表不走 beta 那套兜底」：beta 参照给 2.1.270 的
    /// opus 落回 2.1.260 那行是兼容需要，eager 没抓过就是没证据。
    #[test]
    fn skips_eager_input_streaming_without_evidence() {
        use crate::proxy::CcRequestKind::{Helper, Main, Subagent};
        let none = |v: &serde_json::Value, why: &str| {
            assert!(eager_of(v, "Bash").is_none(), "{why}: {}", v["tools"]);
        };
        none(
            &run_eager("claude-fable-5-1", Some("2.1.260"), Main, true, all_on(), true),
            "fable 2.1.260 证实不带",
        );
        none(
            &run_eager("claude-sonnet-5", Some("2.1.260"), Main, true, all_on(), true),
            "sonnet 2.1.260 没样本",
        );
        none(
            &run_eager("claude-haiku-4-5-20251001", Some("2.1.260"), Main, true, all_on(), true),
            "haiku 2.1.260 没样本",
        );
        none(
            &run_eager("claude-opus-5", Some("2.1.270"), Main, true, all_on(), true),
            "opus 2.1.270 没样本，不继承 2.1.260",
        );
        none(
            &run_eager("claude-opus-5", Some("2.1.261"), Main, true, all_on(), true),
            "样本之间的版本没样本",
        );
        none(
            &run_eager("claude-opus-5", Some("2.1.258"), Main, false, all_on(), true),
            "头上没 advanced-tool-use",
        );
        none(&run_eager("claude-opus-5", None, Main, true, all_on(), true), "读不出版本");
        none(
            &run_eager("claude-opus-5", Some("2.1.258"), Subagent, true, all_on(), true),
            "子代理没证据",
        );
        none(
            &run_eager("claude-opus-5", Some("2.1.258"), Helper, true, all_on(), true),
            "helper 没证据",
        );
        let off = store::ForwardFlags { eager_tool_streaming: false, ..all_on() };
        none(&run_eager("claude-opus-5", Some("2.1.258"), Main, true, off, true), "开关关着");
        none(
            &run_eager("claude-opus-5", Some("2.1.258"), Main, true, all_on(), false),
            "非 CC 形态",
        );
    }

    /// 模拟路径：按**出站 profile** 判，与来访自报的版本无关。opus（On）给客户端保留的工具补；
    /// fable（Off）不补；sonnet（Unknown）跟随注入的 opus 资产，也补。注入的 11 个官方工具不受
    /// 影响——它们自带取值（opus 全带、fable 全不带）。
    #[test]
    fn simulated_eager_input_streaming_follows_the_outbound_profile() {
        let body = |model: &str| -> Bytes {
            serde_json::json!({
                "model": model,
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [
                    {"name": "my_tool", "description": "t", "input_schema": {"type": "object"}},
                    {"name": "explicit_off", "description": "t", "input_schema": {"type": "object"}, "eager_input_streaming": false},
                    {"name": "mcp__x__y", "description": "t", "input_schema": {"type": "object"}}
                ],
                "stream": true
            })
            .to_string()
            .into()
        };
        for (model, expect) in
            [("claude-opus-5", true), ("claude-fable-5-1", false), ("claude-sonnet-5", true)]
        {
            let b = body(model);
            let sim = sim_for(std::str::from_utf8(&b).unwrap());
            let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
            let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
            // 客户端工具在混淆之后带 `mcp__luban__` 前缀，按后缀找。
            let find = |suffix: &str| {
                v["tools"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|t| t["name"].as_str().is_some_and(|n| n.ends_with(suffix)))
                    .cloned()
                    .unwrap_or_else(|| panic!("{model}: 找不到 {suffix}: {}", v["tools"]))
            };
            assert_eq!(
                find("my_tool").get("eager_input_streaming"),
                expect.then(|| serde_json::json!(true)).as_ref(),
                "{model}: 客户端工具"
            );
            assert_eq!(
                find("explicit_off")["eager_input_streaming"],
                false,
                "{model}: 显式 false 不覆盖"
            );
            assert!(
                find("mcp__x__y").get("eager_input_streaming").is_none(),
                "{model}: 来访自带的 mcp 不猜"
            );
            // 注入的官方工具与资产一致。
            let bash = find("Bash");
            assert_eq!(
                bash.get("eager_input_streaming").is_some(),
                model != "claude-fable-5-1",
                "{model}: 注入的 Bash"
            );
        }
    }

    /// 三块改写成官方的四块，且逐字段与 `cap/raw/00006` 的形态一致：
    /// 身份句不再带断点、基座 `{type,ttl:1h,scope:global}`、其余 `{type,ttl:1h}`，
    /// 消息里的断点也补上 `ttl`。切开处那个 `\n\n` 两边都不保留。
    #[test]
    fn aligns_system_to_official_four_blocks() {
        let out =
            rewrite_body(&Bytes::from(API_SHAPE_BODY), &test_cred(), "fp", all_on(), None, None);
        let s = String::from_utf8(out.to_vec()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();
        let sys = v["system"].as_array().unwrap();

        assert_eq!(sys.len(), 4, "应拆成四块: {s}");
        assert!(sys[0].get("cache_control").is_none(), "billing header 不该有断点: {s}");
        assert!(sys[1].get("cache_control").is_none(), "身份句上的断点应去掉: {s}");
        assert_eq!(sys[2]["text"], serde_json::json!("\nBASE — 基座"), "基座切错: {s}");
        assert!(
            sys[3]["text"].as_str().unwrap().starts_with("Write code that reads like"),
            "其余部分应从锚点开始: {s}"
        );
        assert!(sys[3]["text"].as_str().unwrap().ends_with("\n\nREST"), "其余部分被截断: {s}");

        // 键序也要对：type → text → cache_control，cache_control 内 type → ttl → scope
        // （逐字节取自 `cap/raw/00006`）。
        assert!(
            s.contains(r#""cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}"#),
            "基座的 cache_control 形态不对: {s}"
        );
        // 官方三个断点**都**带 ttl，只有基座带 scope——包括来访自己标在消息上的那个：
        // 只补 system 那两个会得到「两个有、一个没有」这种官方不产生的组合，见
        // [`crate::proxy::fill_cache_ttl`]。
        assert_eq!(
            s.matches(r#""cache_control":{"type":"ephemeral","ttl":"1h"}"#).count(),
            2,
            "system 末块与消息断点都该带 ttl、不带 scope: {s}"
        );
        assert!(
            !s.contains(r#""cache_control":{"type":"ephemeral"}"#),
            "不该再有裸 ephemeral（半对齐）: {s}"
        );

        // 关掉 `cache_ttl_1h` 即回到「沿用客户端时长」：一个 ttl 都不写。
        let no_ttl = store::ForwardFlags { cache_ttl_1h: false, ..all_on() };
        let out =
            rewrite_body(&Bytes::from(API_SHAPE_BODY), &test_cred(), "fp", no_ttl, None, None);
        let s = String::from_utf8(out.to_vec()).unwrap();
        assert!(!s.contains(r#""ttl""#), "关掉后不该替客户端写 ttl: {s}");
        assert!(s.contains(r#""cache_control":{"type":"ephemeral","scope":"global"}"#), "{s}");
    }

    /// 客户端 `tools` 上带 `ttl:"5m"` 时，`fill_cache_ttl` 应升级为 `"1h"`——
    /// 否则处理序 tools(5m) → system(1h) 违反上游单调不增约束，产生 400。
    #[test]
    fn upgrades_short_ttl_to_1h() {
        let body = API_SHAPE_BODY.replace(
            r#""metadata":{""#,
            r#""tools":[{"name":"t","cache_control":{"type":"ephemeral","ttl":"5m"}}],"metadata":{""#,
        );
        let out = rewrite_body(&Bytes::from(body), &test_cred(), "fp", all_on(), None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["tools"][0]["cache_control"]["ttl"], "1h", "tools 上的 5m 应升级为 1h: {v}");
        assert!(
            !out.as_ref().windows(3).any(|w| w == b"5m\""),
            "body 里不该残留 5m: {}",
            String::from_utf8_lossy(&out)
        );
    }

    /// 一份 body 里可能**同时**含多条锚点，此时必须切在最早的那个上。
    ///
    /// 实例是 fable-5（`cap/raw/00035` 直连 ↔ `00037` 经 luban）：它自己的锚点
    /// `# Communicating with the user` 在合并块偏移 1212，而 opus 那句
    /// `Write code that reads like…` 也在正文里、偏移 3284。按表序先到先得会切在 3282，
    /// 基座凭空多出 2072 字节；取最早命中才得到官方那 1210B 的基座。
    #[test]
    fn splits_at_earliest_anchor_when_several_match() {
        let raw = Bytes::from(API_SHAPE_BODY.replace(
            r#"\nBASE — 基座\n\nWrite code that reads like the surrounding code: match its comment density, naming, and idiom.\n\nREST"#,
            r#"\nBASE — 基座\n\n# Communicating with the user\n\nWrite code that reads like the surrounding code: match its comment density, naming, and idiom.\n\nREST"#,
        ));
        let out = rewrite_body(&raw, &test_cred(), "fp", all_on(), None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();

        assert_eq!(sys.len(), 4, "应拆成四块: {v}");
        assert_eq!(sys[2]["text"], serde_json::json!("\nBASE — 基座"), "该切在最早的锚点上: {v}");
        assert!(
            sys[3]["text"].as_str().unwrap().starts_with("# Communicating with the user"),
            "其余部分应从最早那个锚点开始: {v}"
        );
    }

    /// 锚点是**按模型族**的：sonnet-5 的基座后面跟的不是 opus 那句，而是 `# Text output …`
    /// （`cap/raw/00009` 直连 10676B 基座 ↔ `00012` 经 luban 合并块偏移 10678）。
    /// haiku-4.5 与 sonnet-5 共用基座，命中的也是这一条。
    #[test]
    fn aligns_sonnet_shape_by_its_own_anchor() {
        let raw = Bytes::from(API_SHAPE_BODY.replace(
            "Write code that reads like the surrounding code: match its comment density, naming, and idiom.",
            "# Text output (does not apply to tool calls)",
        ));
        let out = rewrite_body(&raw, &test_cred(), "fp", all_on(), None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();

        assert_eq!(sys.len(), 4, "sonnet 锚点应能切块: {v}");
        assert_eq!(sys[2]["text"], serde_json::json!("\nBASE — 基座"), "基座切错: {v}");
        assert!(
            sys[3]["text"].as_str().unwrap().starts_with("# Text output"),
            "其余部分应从 sonnet 锚点开始: {v}"
        );
    }

    /// 锚点还**按 CC 版本**漂：claude-cli/2.1.258 下 fable-5-1 的其余部分以
    /// `Before you start, say in a line what you're about to do; …` 开头（`cap/2.1.258/00013`
    /// 直连，基座 1214B，合并块偏移 1216），2.1.251 的三句一句都不在 body 里。没有这条锚点，
    /// fable-5-1 的请求整形退回三块，`ttl:"1h"` 与 `scope:"global"` 一个都不写。
    #[test]
    fn aligns_fable_5_1_shape_by_its_2_1_258_anchor() {
        let raw = Bytes::from(
            API_SHAPE_BODY
                .replace("claude-opus-5", "claude-fable-5-1")
                .replace(
                    "Write code that reads like the surrounding code: match its comment density, naming, and idiom.",
                    "Before you start, say in a line what you're about to do; brief updates while you work help the user follow along.",
                ),
        );
        let out = rewrite_body(&raw, &test_cred(), "fp", all_on(), None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();

        assert_eq!(sys.len(), 4, "fable-5-1 @2.1.258 锚点应能切块: {v}");
        assert_eq!(sys[2]["text"], serde_json::json!("\nBASE — 基座"), "基座切错: {v}");
        assert!(
            sys[3]["text"].as_str().unwrap().starts_with("Before you start, say in a line"),
            "其余部分应从 2.1.258 锚点开始: {v}"
        );
        assert_eq!(
            sys[2]["cache_control"],
            serde_json::json!({"type": "ephemeral", "ttl": "1h", "scope": "global"}),
            "整形成了才有 ttl:1h + scope:global: {v}"
        );
        assert_eq!(
            sys[3]["cache_control"],
            serde_json::json!({"type": "ephemeral", "ttl": "1h"}),
            "其余那块只带 ttl: {v}"
        );
    }

    /// fable 族 API-key 模式是四块 `[billing, 身份(断点), reporting, 合并块(断点)]`
    /// （`cap/2.1.258-api/00013`），要拆成订阅端官方的五块 `[billing, 身份, reporting, 基座, 其余]`
    /// （`cap/2.1.258/00013`）。reporting 块逐字节匹配才认，其它四块形态不动。
    #[test]
    fn splits_fable_api_shape_with_reporting_block() {
        let merged = "\nBASE — 基座\n\nBefore you start, say in a line what you're about to do; brief updates.";
        let body = |system: serde_json::Value| {
            let mut v: serde_json::Value = serde_json::from_str(API_SHAPE_BODY).unwrap();
            v["model"] = "claude-fable-5-1".into();
            v["system"] = system;
            Bytes::from(serde_json::to_vec(&v).unwrap())
        };
        let billing = serde_json::json!({"type":"text","text":"x-anthropic-billing-header: cc_entrypoint=cli;"});
        let identity = serde_json::json!({"type":"text","text":config::CC_SYSTEM_IDENTITY,"cache_control":{"type":"ephemeral"}});
        let reporting = serde_json::json!({"type":"text","text":config::CC_SYSTEM_REPORTING});

        let four = body(serde_json::json!([
            billing, identity, reporting,
            {"type":"text","text":merged,"cache_control":{"type":"ephemeral"}},
        ]));
        {
            let (name, raw) = ("四块", four);
            let out = rewrite_body(&raw, &test_cred(), "fp", all_on(), None, None);
            let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
            let sys = v["system"].as_array().unwrap();
            assert_eq!(sys.len(), 5, "{name}: 应拆成官方五块: {v}");
            assert!(sys[1].get("cache_control").is_none(), "{name}: 身份句不带断点");
            assert_eq!(sys[2]["text"], config::CC_SYSTEM_REPORTING, "{name}: 第 2 块是 reporting");
            assert!(sys[2].get("cache_control").is_none(), "{name}: reporting 块不带断点");
            assert_eq!(sys[3]["text"], "\nBASE — 基座", "{name}: 基座切错: {v}");
            assert_eq!(
                sys[3]["cache_control"],
                serde_json::json!({"type":"ephemeral","ttl":"1h","scope":"global"}),
                "{name}: 基座断点"
            );
            assert!(
                sys[4]["text"].as_str().unwrap().starts_with("Before you start"),
                "{name}: 其余部分应从锚点开始"
            );
            assert_eq!(sys[4]["cache_control"], serde_json::json!({"type":"ephemeral","ttl":"1h"}));
        }

        // 四块但第三块不是 reporting（比如官方订阅四块形态 `[billing, 身份, 基座, 其余]`，或
        // 别的中间层塞的东西）：不动。
        let other = body(serde_json::json!([
            billing, identity,
            {"type":"text","text":"not the reporting block"},
            {"type":"text","text":merged,"cache_control":{"type":"ephemeral"}},
        ]));
        let out = rewrite_body(&other, &test_cred(), "fp", all_on(), None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["system"].as_array().unwrap().len(), 4, "认不出的四块不该动: {v}");
        assert!(!String::from_utf8_lossy(&out).contains("\"ttl\""), "没整形就不补 ttl");
    }

    /// 锚点匹配不到（未知模型族/新版本改了措辞）时**不动结构**，退回三块原样转发——
    /// 宁可不拆，也不切在错误的位置上。其余两项改写照常。
    #[test]
    fn leaves_system_alone_when_anchor_missing() {
        let raw = Bytes::from(API_SHAPE_BODY.replace("Write code that reads like", "改了措辞的"));
        let out = rewrite_body(&raw, &test_cred(), "fp", all_on(), None, None);
        let s = String::from_utf8(out.to_vec()).unwrap();
        let v: serde_json::Value = serde_json::from_str(&s).unwrap();

        assert_eq!(v["system"].as_array().unwrap().len(), 3, "不该拆块: {s}");
        assert!(!s.contains("\"ttl\""), "不拆块时不应注入 ttl: {s}");
        assert!(!s.contains("\"scope\""), "不拆块时不应标 scope: {s}");
        assert!(s.contains("; cch="), "其余改写仍应生效: {s}");
    }

    /// 客户端本来就是订阅形态（四块）时不动 `system`——它已经是目标形态了。
    #[test]
    fn leaves_official_four_block_shape_alone() {
        let raw = Bytes::from(
            r#"{"system":[{"type":"text","text":"x-anthropic-billing-header: cc_entrypoint=cli; cch=0848d;"},
                          {"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."},
                          {"type":"text","text":"base","cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}},
                          {"type":"text","text":"Write code that reads like the surrounding code: match its comment density, naming, and idiom.","cache_control":{"type":"ephemeral","ttl":"1h"}}]}"#,
        );
        let out = rewrite_body(&raw, &test_cred(), "fp", all_on(), None, None);
        assert_eq!(out, raw, "四块形态应原样返回");
    }

    /// 一份 body JSON 文本里 `system` 的块数——拆没拆块看这个，不要去数 `cache_control`：
    /// 拆块会同时**去掉**身份句上那个多余断点，总数不变（3 → 3），数不出差别。
    fn sys_len(body: &str) -> usize {
        serde_json::from_str::<serde_json::Value>(body).unwrap()["system"]
            .as_array()
            .map(Vec::len)
            .unwrap_or(0)
    }

    /// 三项 body 改写全关 = **逐字节原样透传**：不重新序列化，故连缩进、换行、转义写法
    /// 这些 serde 会归一化掉的细节都保持不变（重新序列化本身就是个形态 tell）。
    #[test]
    fn body_flags_off_passes_through_byte_for_byte() {
        // 刻意带上多余空白与换行：一旦走了 serde 往返，这些都会被抹平。
        let raw = Bytes::from(format!(" {}\n", API_SHAPE_BODY));
        let flags = store::ForwardFlags {
            spoof_identity: false,
            spoof_device_id: false,
            normalize_device_fp: false,
            billing_cch: false,
            fill_client_headers: false,
            merge_beta: false,
            system_shape: false,
            orig_header_case: false,
            thinking_signature_retry: false,
            thinking_modified_retry: false,
            redacted_thinking_retry: false,
            simulate_cc: false,
            fill_metadata: false,
            rate_limit_retry: false,
            cache_scope_global: false,
            cache_ttl_1h: false,
            eager_tool_streaming: false,
            nonstream_as_sse: false,
            strip_extra_fields: false,
            tool_name_mimic: false,
            inject_thinking: false,
            flatten_tool_schemas: true,
            strip_empty_text: true,
            hoist_system_role: false,
            reject_openai_shape: false,
            reject_session_conflict: false,
            reject_probes: false,
            reject_probes_strict: false,
            reject_refusals: false,
            reject_empty_replies: false,
            api_telemetry: false,
            keepalive_telemetry: false,
            fable_refusal_fallback: false,
            opus_refusal_fallback: false,
        };
        let out = rewrite_body(&raw, &test_cred(), "fp", flags, None, None);
        assert_eq!(out, raw, "全关时必须原样返回");

        // 逐项开一个，就只有那一项生效，其余仍不动。
        let only_cch = store::ForwardFlags { billing_cch: true, ..flags };
        let s = String::from_utf8(
            rewrite_body(&raw, &test_cred(), "fp", only_cch, None, None).to_vec(),
        )
        .unwrap();
        assert!(s.contains("; cch="), "只开 cch 时应补 cch: {s}");
        assert_eq!(sys_len(&s), 3, "system_shape 关着不应拆块: {s}");
        assert!(s.contains(r#"\"account_uuid\":\"\""#), "spoof 关着应保留空 uuid: {s}");

        // 拆块只需要 system_shape：它标的是裸 `{"type":"ephemeral"}`，GA 能力，不吃任何 beta。
        // cache_scope_global 开着但 merge_beta 关着：scope 仍不该出现。
        let shape_only =
            store::ForwardFlags { system_shape: true, cache_scope_global: true, ..flags };
        let s = String::from_utf8(
            rewrite_body(&raw, &test_cred(), "fp", shape_only, None, None).to_vec(),
        )
        .unwrap();
        assert_eq!(sys_len(&s), 4, "只开 system_shape 也该拆成四块: {s}");
        assert!(!s.contains(r#""scope""#), "scope 要 merge_beta 补的 beta 认，此时不该出现: {s}");

        // `scope:"global"` 才连着 merge_beta（prompt-caching-scope beta 由它补）。
        let with_beta = store::ForwardFlags { merge_beta: true, ..shape_only };
        let s = String::from_utf8(
            rewrite_body(&raw, &test_cred(), "fp", with_beta, None, None).to_vec(),
        )
        .unwrap();
        assert!(s.contains(r#""scope":"global""#), "两个开关都开时才标 global: {s}");
        assert!(!s.contains("cch="), "billing_cch 关着不应补 cch: {s}");

        // 单独关掉 cache_scope_global：照样拆块，只是不标 global。
        let no_scope = store::ForwardFlags { cache_scope_global: false, ..with_beta };
        let s = String::from_utf8(
            rewrite_body(&raw, &test_cred(), "fp", no_scope, None, None).to_vec(),
        )
        .unwrap();
        assert_eq!(sys_len(&s), 4, "关 scope 不影响拆块: {s}");
        assert!(!s.contains(r#""scope""#), "关掉后不该标 global: {s}");
    }

    /// 改写后 body 的 key 顺序必须与入站逐字节一致，只允许新增字段追加在末尾。
    ///
    /// serde_json 默认 `Map = BTreeMap`，会把整个 body（含嵌套对象）的 key 按字母序重排，
    /// 得到官方客户端不会产生的排列。靠 `preserve_order` feature 兜住，本测试是它的看门狗：
    /// 一旦该 feature 被摘掉，这里立刻失败。
    #[test]
    fn preserves_key_order() {
        // 客户端的真实字段次序，取自 cap/raw/00002 的原始报文体：顶层是
        // model→messages→system→tools→metadata→max_tokens→…→stream，system 块是 type→text，
        // cache_control 是 type→ttl→scope（luban 自己写的那份没有 ttl），metadata.user_id 内层是
        // device_id→account_uuid→session_id。字母序全都不是这样。
        //
        // （cap/*.json 里看到的字母序是抓包工具重新序列化的产物，不是线上的样子。）
        let raw = concat!(
            r#"{"model":"claude-opus-5","messages":[],"#,
            r#""system":[{"type":"text","text":"x-anthropic-billing-header: cc_entrypoint=cli;"},"#,
            r#"{"type":"text","text":"ident","cache_control":{"type":"ephemeral"}},"#,
            r#"{"type":"text","text":"base\n\nWrite code that reads like the surrounding code: "#,
            r#"match its comment density, naming, and idiom.","cache_control":{"type":"ephemeral"}}],"#,
            r#""tools":[],"#,
            r#""metadata":{"user_id":"{\"device_id\":\"dddd\",\"account_uuid\":\"\",\"session_id\":\"ssss\"}"},"#,
            r#""max_tokens":64000,"stream":true}"#
        );
        let out = rewrite_body(&Bytes::from(raw), &test_cred(), "fp", all_on(), None, None);
        let s = String::from_utf8(out.to_vec()).unwrap();

        // 三项改写都生效了（否则会走 body.clone() 早退，测试空过）。
        assert!(s.contains("; cch="), "应补 cch: {s}");
        assert!(s.contains(r#""scope":"global""#), "应对齐 system 形态: {s}");
        assert!(s.contains(&format!(r#"\"account_uuid\":\"{}\""#, ACCOUNT_UUID)), "应填 uuid: {s}");

        // 顶层顺序不变，未被字母序重排（重排后 max_tokens/messages 会跑到 model 前）。
        let mut at = 0;
        for k in ["model", "messages", "system", "tools", "metadata", "max_tokens", "stream"] {
            let needle = format!("\"{k}\":");
            let pos = s[at..].find(&needle).unwrap_or_else(|| panic!("顶层 key {k} 顺序错乱: {s}"));
            at += pos + needle.len();
        }

        // 嵌套对象同样不重排：system 块是 type→text（字母序会变成 text→type），
        // 拆块后新建的两块也按这个键序写回，cache_control 内是 type→ttl→scope
        // （字母序会变成 scope→ttl→type）。
        assert!(s.contains(r#"{"type":"text","text":"base""#), "system 块 key 被重排: {s}");
        assert!(
            s.contains(r#""cache_control":{"type":"ephemeral","ttl":"1h","scope":"global"}"#),
            "cache_control key 被重排: {s}"
        );

        // 内层 user_id 仍走定点替换，device_id→account_uuid→session_id 原序。
        assert!(
            s.contains(r#"\"device_id\":\""#)
                && s.find(r#"\"device_id\":\""#) < s.find(r#"\"account_uuid\":\""#),
            "内层 user_id key 被重排: {s}"
        );
    }

    fn body_with_system0(text: &str) -> serde_json::Value {
        serde_json::json!({"system": [{"type": "text", "text": text}]})
    }

    /// 补出的 billing header 与订阅模式的真实形态一致（抓包 040 的 `; cch=…;` 形态）：
    /// 追加在末尾、**5 位小写 hex**、每请求都不一样。
    ///
    /// 钉「每次不同」是有理由的：原先补的是常量 `00000`，跨账号恒定，上游一按它聚类就把
    /// 所有账号串成一串。抓包里同账号相邻请求是 `993e1`/`e2d04`/`b504f`，各不相同。
    #[test]
    fn adds_cch_in_official_shape() {
        const HEAD: &str = "x-anthropic-billing-header: cc_version=2.1.218.0b9; cc_entrypoint=cli;";
        let cch_of = |v: &serde_json::Value| -> String {
            let text = v["system"][0]["text"].as_str().unwrap().to_string();
            let rest = text.strip_prefix(HEAD).unwrap_or_else(|| panic!("前缀不该被动: {text}"));
            let cch = rest
                .strip_prefix(" cch=")
                .and_then(|s| s.strip_suffix(';'))
                .unwrap_or_else(|| panic!("cch 段形态不对: {text}"));
            assert_eq!(cch.len(), 5, "5 位: {text}");
            assert!(
                cch.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
                "小写 hex: {text}"
            );
            cch.to_string()
        };

        let mut v = body_with_system0(HEAD);
        assert!(ensure_billing_cch(&mut v));
        let first = cch_of(&v);

        // 连补 20 次，不该 20 次都一样——恒定值正是要消灭的那个判据。
        let mut seen = std::collections::HashSet::new();
        seen.insert(first);
        for _ in 0..20 {
            let mut v = body_with_system0(HEAD);
            assert!(ensure_billing_cch(&mut v));
            seen.insert(cch_of(&v));
        }
        assert!(seen.len() > 1, "cch 该逐请求变化，20 次全同说明又写死了: {seen:?}");
    }

    /// 已带 cch（订阅模式客户端）不重复追加；非 billing 块不动。
    #[test]
    fn cch_is_idempotent_and_scoped() {
        let mut has = body_with_system0(
            "x-anthropic-billing-header: cc_version=2.1.218.2d7; cc_entrypoint=cli; cch=0848d;",
        );
        assert!(!ensure_billing_cch(&mut has));

        let mut other =
            body_with_system0("You are Claude Code, Anthropic's official CLI for Claude.");
        assert!(!ensure_billing_cch(&mut other));

        let mut empty = serde_json::json!({"messages": []});
        assert!(!ensure_billing_cch(&mut empty));
    }

    /// 白名单策略：官方名/MCP前缀/server tool 保留原名，其余 custom tool 一律混淆。
    #[test]
    fn tool_map_skips_official_mcp_and_server_tools() {
        let body = serde_json::json!({"tools": [
            {"name": "Bash"},                                      // 官方白名单 → 保留
            {"name": "Read"},                                      // 官方白名单 → 保留
            {"name": "mcp__hermes__skill_manage"},                 // MCP前缀 → 保留
            {"type": "web_search_20250305", "name": "web_search"}, // server tool → 保留
            {"name": "delegate_task"},                             // 非官方 custom → 混淆
            {"name": "skill_manage"},                              // 非官方 custom → 混淆
            {"name": "sessions_spawn"},                            // 非官方 custom → 混淆
            {"name": "memory_search"},                             // 非官方 custom → 混淆
        ]});
        let map = build_tool_name_map(Some(&body)).expect("有非官方 custom tool 就该有映射");
        assert_eq!(map.forward.len(), 4, "应混淆 4 个非官方名: {:?}", map.forward);
        for should_mimic in ["delegate_task", "skill_manage", "sessions_spawn", "memory_search"] {
            assert!(map.forward.contains_key(should_mimic), "{should_mimic} 该被混淆");
        }
        for kept in ["Bash", "Read", "mcp__hermes__skill_manage", "web_search"] {
            assert!(!map.forward.contains_key(kept), "{kept} 该保留原名");
        }
        // 假名必须走已验证豁免的 MCP 命名空间。
        for fake in map.forward.values() {
            assert!(fake.starts_with("mcp__luban__"), "假名必须是 mcp__luban__ 前缀: {fake}");
        }

        // 全是官方名或 MCP 前缀 → 无映射。
        let clean = serde_json::json!({"tools": [
            {"name": "Bash"}, {"name": "mcp__x__y"}, {"name": "Edit"},
        ]});
        assert!(build_tool_name_map(Some(&clean)).is_none());
        assert!(build_tool_name_map(Some(&serde_json::json!({"tools": []}))).is_none());
        assert!(build_tool_name_map(Some(&serde_json::json!({}))).is_none());
    }

    /// 老版本 CC（2.1.258 之前）主线程直接声明 `Glob` / `Grep`，更老的还叫 `Task` / `TodoWrite` /
    /// `KillShell` / `BashOutput`——都是官方名，混淆成 `mcp__luban__*` 反而是官方从不发的形态。
    /// 同一条请求里 OpenClaw 那种小写业务名（`read` / `exec` / `sessions_spawn`）照旧混淆：
    /// 白名单按大小写精确匹配，`read` 不因为有个 `Read` 就放行。
    #[test]
    fn tool_map_keeps_legacy_official_names_but_still_mimics_lookalikes() {
        let body = serde_json::json!({"tools": [
            {"name": "Glob"}, {"name": "Grep"}, {"name": "Task"}, {"name": "TodoWrite"},
            {"name": "KillShell"}, {"name": "BashOutput"}, {"name": "EndConversation"},
            {"name": "ArtifactComments"}, {"name": "MultiEdit"},
            {"name": "read"}, {"name": "exec"}, {"name": "sessions_spawn"},
            {"name": "qieman__GetFundDiagnosis"},
        ]});
        let map = build_tool_name_map(Some(&body)).expect("小写业务名该有映射");
        assert_eq!(map.forward.len(), 4, "只混淆 4 个非官方名: {:?}", map.forward);
        for kept in [
            "Glob",
            "Grep",
            "Task",
            "TodoWrite",
            "KillShell",
            "BashOutput",
            "EndConversation",
            "ArtifactComments",
            "MultiEdit",
        ] {
            assert!(!map.forward.contains_key(kept), "{kept} 是官方旧名，该保留");
        }
        for mimic in ["read", "exec", "sessions_spawn", "qieman__GetFundDiagnosis"] {
            assert!(map.forward.contains_key(mimic), "{mimic} 该被混淆");
        }

        // 只有老版本官方名 → 无映射，请求与回程两侧零开销。
        let legacy_only = serde_json::json!({"tools": [
            {"name": "Glob"}, {"name": "Grep"}, {"name": "Task"}, {"name": "Bash"},
        ]});
        assert!(build_tool_name_map(Some(&legacy_only)).is_none());
    }

    /// 同一组工具名两次构造得到同一套假名——否则每轮请求的假名都变，上游 prompt cache 全丢。
    #[test]
    fn tool_map_is_stable_for_the_same_tool_set() {
        let body = serde_json::json!({"tools": [
            {"name": "skill_manage"}, {"name": "skill_view"}, {"name": "skills_list"},
        ]});
        let a = build_tool_name_map(Some(&body)).unwrap();
        let b = build_tool_name_map(Some(&body)).unwrap();
        assert_eq!(a.forward, b.forward);

        // 工具集变了假名就该变（否则新旧两套名字会撞在一起）。
        let other = serde_json::json!({"tools": [{"name": "skill_manage"}]});
        let c = build_tool_name_map(Some(&other)).unwrap();
        assert_ne!(a.forward.get("skill_manage"), c.forward.get("skill_manage"));
    }

    /// 来访若恰好已有一个和生成假名同名的 MCP 工具，第三方工具仍必须得到映射，
    /// 不能为了避免撞名就把真名漏给上游。
    #[test]
    fn tool_map_resolves_declared_mcp_alias_collision() {
        let one = serde_json::json!({"tools": [{"name": "skill_manage"}]});
        let first = build_tool_name_map(Some(&one)).unwrap();
        let occupied = first.forward["skill_manage"].clone();
        let collided = serde_json::json!({"tools": [
            {"name": "skill_manage"},
            {"name": occupied},
        ]});
        let map = build_tool_name_map(Some(&collided)).expect("撞名不得让映射消失");
        let alias = &map.forward["skill_manage"];
        assert_ne!(alias, &occupied);
        assert!(alias.starts_with(&format!("{occupied}_")), "应以稳定后缀解决撞名: {alias}");
    }

    /// 请求侧三处必须同时改：`tools[]`、`tool_choice`、历史里的 `tool_use`。
    /// 漏掉第三处的话上游会因为 `tool_use` 引用未声明的工具名而拒掉整条请求。
    #[test]
    fn applies_tool_names_to_all_three_places() {
        let mut v = serde_json::json!({
        "tools": [{"name": "skill_manage"}, {"name": "Bash"}],
        "tool_choice": {"type": "tool", "name": "skill_manage"},
        "messages": [
            {"role": "assistant", "content": [
                {"type": "tool_use", "name": "skill_manage", "input": {}},
                {"type": "text", "text": "skill_manage 只是正文，不该动"},
            ]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "x"}]},
        ]});
        let snapshot = v.clone();
        let map = build_tool_name_map(Some(&snapshot)).unwrap();
        assert!(apply_tool_names(&mut v, &map));
        let fake = map.forward["skill_manage"].clone();

        assert_eq!(v["tools"][0]["name"], serde_json::json!(fake));
        assert_eq!(v["tools"][1]["name"], serde_json::json!("Bash"), "白名单不该动");
        assert_eq!(v["tool_choice"]["name"], serde_json::json!(fake));
        assert_eq!(v["messages"][0]["content"][0]["name"], serde_json::json!(fake));
        assert!(
            v["messages"][0]["content"][1]["text"].as_str().unwrap().contains("skill_manage"),
            "正文里的同名字符串不该被请求侧改写"
        );
    }

    /// 回程还原：假名换回真名，且**必须扛得住分块从假名中间切开**。
    /// 切断那次还原不了的话，客户端会拿到假名、下一轮带着假名回来，上游再回一个 400。
    #[test]
    fn restores_tool_names_across_chunk_boundaries() {
        let body = serde_json::json!({"tools": [
            {"name": "skill_manage"}, {"name": "skill_view"}, {"name": "skills_list"},
        ]});
        let map = build_tool_name_map(Some(&body)).unwrap();
        let fake = map.forward["skill_manage"].clone();
        let wire = format!(r#"data: {{"type":"tool_use","name":"{fake}"}}"#) + "\n\n";

        // 一次性还原。
        assert_eq!(
            String::from_utf8(map.restore(wire.as_bytes())).unwrap(),
            wire.replace(&fake, "skill_manage")
        );

        // 逐字节喂（最坏的分块），滑动窗口必须拼回同样的结果，且尾巴要 flush 出来。
        let mut pending = Vec::new();
        let mut out = Vec::new();
        for b in wire.as_bytes() {
            out.extend_from_slice(&map.feed(&mut pending, &[*b]));
        }
        out.extend_from_slice(&map.flush(&mut pending));
        assert_eq!(
            String::from_utf8(out).unwrap(),
            wire.replace(&fake, "skill_manage"),
            "分块还原结果必须与整段一致"
        );
    }

    /// 短假名是长假名的子串时，必须先替长的——否则长假名会被先吃掉一截。
    #[test]
    fn restore_replaces_longer_aliases_first() {
        let map = crate::proxy::ToolNameMap {
            forward: Default::default(),
            reverse: vec![
                ("fetch_abc00_long".to_string(), "REAL_LONG".to_string()),
                ("fetch_abc00".to_string(), "REAL_SHORT".to_string()),
            ],
            max_fake: "fetch_abc00_long".len(),
        };
        assert_eq!(
            String::from_utf8(map.restore(b"x fetch_abc00_long y fetch_abc00 z")).unwrap(),
            "x REAL_LONG y REAL_SHORT z"
        );
    }

    /// OpenAI 方言的 `tool_choice` 翻译成 Anthropic 对象形态；上游对非对象直接 400
    /// `tool_choice: Input should be an object`。已是 Anthropic 形态或认不出的，一律不动。
    #[test]
    fn normalizes_openai_style_tool_choice() {
        let run = |tc: serde_json::Value| {
            let mut v = serde_json::json!({ "model": "claude-sonnet-5", "tool_choice": tc, "messages": [] });
            let changed = normalize_tool_choice(&mut v);
            (changed, v.get("tool_choice").cloned())
        };
        assert_eq!(
            run(serde_json::json!("auto")),
            (true, Some(serde_json::json!({"type": "auto"})))
        );
        assert_eq!(
            run(serde_json::json!("none")),
            (true, Some(serde_json::json!({"type": "none"})))
        );
        assert_eq!(
            run(serde_json::json!("required")),
            (true, Some(serde_json::json!({"type": "any"})))
        );
        assert_eq!(run(serde_json::json!("ANY")), (true, Some(serde_json::json!({"type": "any"}))));
        assert_eq!(run(serde_json::Value::Null), (true, None), "null 等于没写，删掉");
        assert_eq!(
            run(serde_json::json!({"type": "function", "function": {"name": "get_weather"}})),
            (true, Some(serde_json::json!({"type": "tool", "name": "get_weather"})))
        );
        assert_eq!(
            run(serde_json::json!({"type": "function"})),
            (true, Some(serde_json::json!({"type": "any"})))
        );
        // Anthropic 形态原样不动，附加键也不动。
        for keep in [
            serde_json::json!({"type": "auto"}),
            serde_json::json!({"type": "tool", "name": "x"}),
            serde_json::json!({"type": "any", "disable_parallel_tool_use": true}),
            serde_json::json!({"type": "none"}),
        ] {
            assert_eq!(run(keep.clone()), (false, Some(keep.clone())), "不该动: {keep}");
        }
        // 认不出的方言放行，让上游报它自己的错。
        assert_eq!(
            run(serde_json::json!("whatever")),
            (false, Some(serde_json::json!("whatever")))
        );
        assert_eq!(run(serde_json::json!(42)), (false, Some(serde_json::json!(42))));
        // 没有这个字段：零操作。
        let mut none = serde_json::json!({ "model": "claude-sonnet-5", "messages": [] });
        assert!(!normalize_tool_choice(&mut none));
        // 归一后与剥字段接力：`"auto"` 最终整个消失，与官方形态一致。
        let mut chain = serde_json::json!({ "model": "claude-sonnet-5", "tool_choice": "auto", "messages": [] });
        assert!(normalize_tool_choice(&mut chain));
        assert!(strip_extra_fields(&mut chain, false));
        assert!(chain.get("tool_choice").is_none(), "{chain}");
    }

    /// 官方从不发的顶层字段要剥掉，客户端真正要的语义不能动。
    ///
    /// 判据取自 `cap/raw/00006`/`00009`：两份直连抓包都没有 `tool_choice`，
    /// `thinking` 也都是裸的 `{"type":"adaptive"}`。
    #[test]
    fn strips_only_the_fields_official_never_sends() {
        // 等价于缺省的 tool_choice + thinking.display：都该剥。
        let mut v = serde_json::json!({
            "model": "claude-opus-5",
            "tool_choice": {"type": "auto"},
            "thinking": {"type": "adaptive", "display": "summarized"}});
        assert!(strip_extra_fields(&mut v, false));
        assert!(v.get("tool_choice").is_none(), "官方不发 tool_choice: {v}");
        assert_eq!(v["thinking"], serde_json::json!({"type": "adaptive"}), "display 应剥掉: {v}");

        // 强制选工具 / 强制用工具 / 关并行：都是客户端要的语义，一个都不能动。
        for keep in [
            serde_json::json!({"type": "tool", "name": "Bash"}),
            serde_json::json!({"type": "any"}),
            serde_json::json!({"type": "auto", "disable_parallel_tool_use": true}),
        ] {
            let mut v = serde_json::json!({ "tool_choice": keep.clone() });
            assert!(!strip_extra_fields(&mut v, false), "不该动: {keep}");
            assert_eq!(v["tool_choice"], keep);
        }

        // thinking.type == "disabled"：**只有 fable 族**要删（它不支持，上游直接 400），
        // 删掉整个字段让上游走 adaptive 默认值。
        let mut v = serde_json::json!({
            "model": "claude-fable-5",
            "thinking": {"type": "disabled"}});
        assert!(strip_extra_fields(&mut v, false));
        assert!(v.get("thinking").is_none(), "fable 上 disabled 应整个删掉: {v}");

        // 别的族不动：`{"type":"disabled"}` 是 2.1.260 三个官方辅助 profile 的正常形态
        // （无工具 helper / 标题生成是 haiku，安全分类是 sonnet）。删了既造出一个官方不
        // 产生的形态，又把客户端「不要思考」翻成了「随你」——那是要花钱的。
        for model in ["claude-haiku-4-5-20251001", "claude-sonnet-5", "claude-opus-5"] {
            let mut v = serde_json::json!({
                "model": model,
                "thinking": {"type": "disabled"}});
            assert!(!strip_extra_fields(&mut v, false), "{model}: 不该动");
            assert_eq!(v["thinking"], serde_json::json!({"type": "disabled"}), "{model}");
        }

        // thinking.type == "enabled" 不动。
        let mut v = serde_json::json!({
            "thinking": {"type": "enabled", "budget_tokens": 10000}});
        assert!(!strip_extra_fields(&mut v, false));
        assert_eq!(v["thinking"]["type"], "enabled");

        // 官方形态本身：走一遍什么也不改（对真实 CC 是空操作）。
        let mut official = serde_json::json!({
            "model": "claude-opus-5",
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "high"}});
        let before = official.clone();
        assert!(!strip_extra_fields(&mut official, false));
        assert_eq!(official, before);
    }

    /// thinking 开着时上游要求 `top_p` 「不传或 >= 0.95」（线上撞到的原话：`top_p must be
    /// greater than or equal to 0.95 or unset when thinking is enabled or in adaptive mode`）。
    /// 与 temperature 那条同源：客户端设了不合规的值就剥掉，合规的与 thinking 关着的都不动。
    #[test]
    fn strips_low_top_p_when_thinking_is_on() {
        let req = |thinking: serde_json::Value, top_p: serde_json::Value| {
            serde_json::json!({
                "model": "claude-opus-4-6",
                "messages": [{"role": "user", "content": "hi"}],
                "thinking": thinking,
                "top_p": top_p})
        };
        // enabled / adaptive 两种开法，低于 0.95 都剥；非数字也剥（上游一样 400）。
        for thinking in [
            serde_json::json!({"type": "enabled", "budget_tokens": 2048}),
            serde_json::json!({"type": "adaptive"}),
        ] {
            for bad in [serde_json::json!(0.9), serde_json::json!(0.949), serde_json::json!("x")] {
                let mut v = req(thinking.clone(), bad.clone());
                assert!(strip_extra_fields(&mut v, false), "{thinking} + top_p={bad}: 应有改动");
                assert!(v.get("top_p").is_none(), "{thinking} + top_p={bad}: 应剥掉: {v}");
                assert!(v.get("thinking").is_some(), "thinking 自己不能动: {v}");
            }
            // 合规的取值照发。
            for ok in [serde_json::json!(0.95), serde_json::json!(1.0)] {
                let mut v = req(thinking.clone(), ok.clone());
                assert!(!strip_extra_fields(&mut v, false), "{thinking} + top_p={ok}: 不该动");
                assert_eq!(v["top_p"], ok);
            }
        }
        // thinking 关着（disabled 且非 fable 族）或压根没传：top_p 随便填，不归这里管。
        let mut v = req(serde_json::json!({"type": "disabled"}), serde_json::json!(0.5));
        assert!(!strip_extra_fields(&mut v, false), "disabled: 不该动: {v}");
        assert_eq!(v["top_p"], 0.5);
        let mut v = serde_json::json!({ "model": "claude-opus-4-6", "top_p": 0.5 });
        assert!(!strip_extra_fields(&mut v, false), "无 thinking: 不该动: {v}");
        assert_eq!(v["top_p"], 0.5);
    }

    /// 2.1.258 起官方 CC 自己发 `thinking: {type: adaptive, display: "updates"}`
    /// （`cap/2.1.258/00013`）。CC 形态的来访不剥 `display`；非 CC 形态照剥。
    #[test]
    fn keeps_thinking_display_for_cc_shaped_requests() {
        let mut cc = serde_json::json!({
            "model": "claude-fable-5-1",
            "system": [{"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."}],
            "thinking": {"type": "adaptive", "display": "updates"}});
        assert!(!strip_extra_fields(&mut cc, true), "官方形态无可剥: {cc}");
        assert_eq!(
            cc["thinking"],
            serde_json::json!({"type": "adaptive", "display": "updates"}),
            "CC 自己发的 display 不能动: {cc}"
        );

        let mut third_party = serde_json::json!({
            "model": "claude-fable-5-1",
            "system": "You are a helpful assistant.",
            "thinking": {"type": "adaptive", "display": "updates"}});
        assert!(strip_extra_fields(&mut third_party, false));
        assert_eq!(
            third_party["thinking"],
            serde_json::json!({"type": "adaptive"}),
            "非 CC 形态照剥: {third_party}"
        );
    }

    /// 空壳 system 的清理要能**自己撑起整条改写**：所有改写开关都关着时，入口的快速返回与
    /// 末尾的「什么都没改就回原体」都不能把它漏掉——漏掉就是空壳照样出站、上游照样 400。
    #[test]
    fn dropping_an_empty_system_message_survives_both_early_returns() {
        let flags = store::ForwardFlags {
            system_shape: false,
            spoof_identity: false,
            billing_cch: false,
            strip_extra_fields: false,
            flatten_tool_schemas: false,
            strip_empty_text: false,
            // 提升那步关掉：空壳的清理不该挂在它身上。
            hoist_system_role: false,
            ..store::ForwardFlags::default()
        };
        let body = Bytes::from(
            r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"hi"},{"role":"system","content":[]}]}"#,
        );
        let out = rewrite_body(&body, &test_cred(), "fp", flags, None, None);
        let s = String::from_utf8(out.to_vec()).unwrap();
        assert!(!s.contains(r#""role":"system""#), "空壳该被丢掉: {s}");
        assert!(s.contains(r#""content":"hi""#), "用户消息要留着: {s}");
        // 反向：同一套开关下，没有空壳的体一个字节都不该动。
        let clean =
            Bytes::from(r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"hi"}]}"#);
        assert_eq!(rewrite_body(&clean, &test_cred(), "fp", flags, None, None), clean);
    }

    /// 入口快速路径的粗筛容得下缩进：`"role": "system"`（键值之间有空白、还带换行）与紧凑写法
    /// 一样要进解析路径，否则 pretty-print 过的体会带着空壳原样出站。
    #[test]
    fn the_fast_path_probe_tolerates_pretty_printed_json() {
        let pair = |b: &str| crate::proxy::body_has_pair(b.as_bytes(), b"\"role\"", b"\"system\"");
        assert!(pair(r#"{"role":"system"}"#));
        assert!(pair("{\"role\": \"system\"}"));
        assert!(pair("{\n  \"role\"\t:\r\n    \"system\"\n}"));
        assert!(!pair(r#"{"role":"user","system":"x"}"#), "别的键值对不算");
        assert!(!pair(r#"{"role":"systematic"}"#), "值要整段对上：闭引号把它钉死，systematic 不算");
        assert!(!pair(r#"{"role" "system"}"#), "缺冒号不算");
        assert!(!pair(r#"{"rolex":"system"}"#), "键不是 role 不算");
        assert!(!pair(r#"{"role":"#), "截断的体不算，也不能越界");

        // 空 text 块那一项同样容空白。
        let text = |b: &str| crate::proxy::body_has_pair(b.as_bytes(), b"\"text\"", b"\"\"");
        assert!(text(r#"{"text":""}"#));
        assert!(text("{\"text\" : \"\"}"));
        assert!(!text(r#"{"text":"x"}"#));

        // 端到端：所有改写开关都关着 + 缩进过的体，空壳照样被丢掉。
        let flags = store::ForwardFlags {
            system_shape: false,
            spoof_identity: false,
            billing_cch: false,
            strip_extra_fields: false,
            flatten_tool_schemas: false,
            strip_empty_text: false,
            hoist_system_role: false,
            ..store::ForwardFlags::default()
        };
        let pretty = Bytes::from(
            "{\n  \"model\": \"claude-opus-5\",\n  \"messages\": [\n    \
             {\"role\": \"user\", \"content\": \"hi\"},\n    \
             {\"role\": \"system\", \"content\": []}\n  ]\n}",
        );
        let out = rewrite_body(&pretty, &test_cred(), "fp", flags, None, None);
        let s = String::from_utf8(out.to_vec()).unwrap();
        assert!(!s.contains(r#""role":"system""#), "缩进过的体里的空壳也该被丢掉: {s}");
        assert!(s.contains(r#""content":"hi""#), "用户消息要留着: {s}");
    }

    /// 空壳 `role:"system"` 消息在出站前被丢掉：五种空形态都算（空数组、空串、`null`、
    /// 字段缺失、整条只有空 text 块），带内容的一字不动，别的角色一概不碰。
    ///
    /// 最后一段走完整条 `rewrite_body`：**CC 形态的请求同样会丢**——`hoist_system_role` 的
    /// 「CC 形态跳过」保的是官方带内容的那条 `role:"system"`（deferred tools），不是空壳；
    /// 实跑里正是一条 agent-sdk 的 CC 请求带着空壳换回一次 400（`req_grlwDAtQQpqvf54d`）。
    #[test]
    fn empty_system_messages_are_dropped_before_going_out() {
        let sys = |content: Option<serde_json::Value>| match content {
            Some(c) => serde_json::json!({ "role": "system", "content": c }),
            None => serde_json::json!({ "role": "system" }),
        };
        let user = serde_json::json!({ "role": "user", "content": "hi" });

        // 五种空形态，逐个单独验：丢掉之后只剩那条用户消息。
        for content in [
            Some(serde_json::json!([])),
            Some(serde_json::json!("")),
            Some(serde_json::Value::Null),
            None,
            Some(
                serde_json::json!([{ "type": "text", "text": "" }, { "type": "text", "text": "" }]),
            ),
        ] {
            let mut v = serde_json::json!({ "messages": [user.clone(), sys(content.clone())] });
            assert!(crate::proxy::drop_empty_system_messages(&mut v), "这条该算空壳: {content:?}");
            assert_eq!(v["messages"], serde_json::json!([user.clone()]));
        }

        // 带内容的一律不动：官方 deferred tools 那条、空格、数组里混着一个非空块。
        for content in [
            serde_json::json!("deferred"),
            serde_json::json!(" "),
            serde_json::json!([{ "type": "text", "text": "x" }]),
            serde_json::json!([{ "type": "text", "text": "" }, { "type": "text", "text": "x" }]),
            serde_json::json!({ "type": "text", "text": "" }),
        ] {
            let mut v = serde_json::json!({ "messages": [sys(Some(content.clone()))] });
            assert!(!crate::proxy::drop_empty_system_messages(&mut v), "这条不该算空壳: {content}");
            assert_eq!(v["messages"], serde_json::json!([sys(Some(content))]));
        }

        // 只碰 role:"system"：空 content 的 user / assistant 留着（删了会改轮次交替）。
        let mut v = serde_json::json!({
            "messages": [
                { "role": "user", "content": [] },
                { "role": "assistant", "content": [] },
            ]
        });
        assert!(!crate::proxy::drop_empty_system_messages(&mut v));
        assert_eq!(v["messages"].as_array().unwrap().len(), 2);

        // 多条空壳一起丢，其余消息的相对顺序不变；没有 messages 的体不动。
        let mut v = serde_json::json!({
            "messages": [sys(Some(serde_json::json!([]))), user.clone(), sys(None), user.clone()]
        });
        assert!(crate::proxy::drop_empty_system_messages(&mut v));
        assert_eq!(v["messages"], serde_json::json!([user.clone(), user.clone()]));
        let mut v = serde_json::json!({ "model": "claude-opus-5" });
        assert!(!crate::proxy::drop_empty_system_messages(&mut v));

        // 整条 rewrite_body：CC 形态（system 里有身份句）的请求，空壳照丢。
        let body = Bytes::from(
            serde_json::json!({
                "model": "claude-opus-5",
                "messages": [user.clone(), sys(Some(serde_json::json!([]))), user.clone()],
                "system": [{
                    "type": "text",
                    "text": "You are Claude Code, Anthropic's official CLI for Claude."}]})
            .to_string(),
        );
        let out: serde_json::Value =
            serde_json::from_slice(&rewrite_body(&body, &test_cred(), "fp", all_on(), None, None))
                .unwrap();
        let msgs = out["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2, "空壳该被丢掉: {out}");
        assert!(msgs.iter().all(|m| m["role"] == "user"), "留下的必须是那两条用户消息: {out}");
    }

    #[test]
    fn strip_extra_fields_is_wired_and_switchable() {
        let body = br#"{"model":"claude-opus-5","tool_choice":{"type":"auto"},"thinking":{"type":"adaptive","display":"summarized"},"messages":[]}"#;
        let only_strip = store::ForwardFlags {
            strip_extra_fields: true,
            ..store::ForwardFlags {
                spoof_identity: false,
                spoof_device_id: false,
                normalize_device_fp: false,
                billing_cch: false,
                fill_client_headers: false,
                merge_beta: false,
                system_shape: false,
                orig_header_case: false,
                thinking_signature_retry: false,
                thinking_modified_retry: false,
                redacted_thinking_retry: false,
                simulate_cc: false,
                fill_metadata: false,
                rate_limit_retry: false,
                cache_scope_global: false,
                cache_ttl_1h: false,
                eager_tool_streaming: false,
                nonstream_as_sse: false,
                strip_extra_fields: false,
                tool_name_mimic: false,
                inject_thinking: false,
                flatten_tool_schemas: true,
                strip_empty_text: true,
                hoist_system_role: false,
                reject_openai_shape: false,
                reject_session_conflict: false,
                reject_probes: false,
                reject_probes_strict: false,
                reject_refusals: false,
                reject_empty_replies: false,
                api_telemetry: false,
                keepalive_telemetry: false,
                fable_refusal_fallback: false,
                opus_refusal_fallback: false,
            }
        };
        let out = rewrite_body(&Bytes::from(&body[..]), &test_cred(), "fp", only_strip, None, None);
        let s = String::from_utf8(out.to_vec()).unwrap();
        assert!(!s.contains("tool_choice"), "{s}");
        assert!(!s.contains("display"), "{s}");

        let off = store::ForwardFlags { strip_extra_fields: false, ..only_strip };
        let out = rewrite_body(&Bytes::from(&body[..]), &test_cred(), "fp", off, None, None);
        assert_eq!(out.as_ref(), &body[..], "关掉后必须逐字节透传");
    }

    // 真实 CC 抓包形态：字段顺序 device_id → account_uuid → session_id。
    const CC: &str = r#"{"device_id":"dddd","account_uuid":"aaaa","session_id":"ssss"}"#;

    #[test]
    fn replaces_value_and_preserves_order() {
        let s = replace_json_str_field(CC, "account_uuid", "NEW").unwrap();
        let s = replace_json_str_field(&s, "device_id", "DEV").unwrap();
        assert_eq!(s, r#"{"device_id":"DEV","account_uuid":"NEW","session_id":"ssss"}"#);
    }

    #[test]
    fn fills_empty_account_uuid() {
        let empty = r#"{"device_id":"dddd","account_uuid":"","session_id":"ssss"}"#;
        let s = replace_json_str_field(empty, "account_uuid", "FILLED").unwrap();
        assert_eq!(s, r#"{"device_id":"dddd","account_uuid":"FILLED","session_id":"ssss"}"#);
    }

    #[test]
    fn missing_field_returns_none_no_insert() {
        assert!(replace_json_str_field(CC, "not_here", "X").is_none());
    }

    // ---------- 空 text 块剥除 ----------

    #[test]
    fn strips_empty_text_blocks_mixed() {
        let mut v = serde_json::json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": ""},
                    {"type": "text", "text": "hello"},
                    {"type": "text", "text": ""}
                ]}
            ]
        });
        assert!(crate::proxy::strip_empty_text_blocks(&mut v));
        let content = v["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "hello");
    }

    #[test]
    fn keeps_all_empty_text_blocks_when_nothing_else() {
        let mut v = serde_json::json!({
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "text", "text": ""}
                ]}
            ]
        });
        assert!(!crate::proxy::strip_empty_text_blocks(&mut v));
        assert_eq!(v["messages"][0]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn noop_when_no_empty_text() {
        let mut v = serde_json::json!({
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hi"}]}
            ]
        });
        assert!(!crate::proxy::strip_empty_text_blocks(&mut v));
    }

    // ---------- input_schema allOf/oneOf/anyOf 展平 ----------

    #[test]
    fn flattens_allof_in_tool_schema() {
        let mut v = serde_json::json!({
            "tools": [{
                "name": "my_tool",
                "input_schema": {
                    "allOf": [
                        {"type": "object", "properties": {"a": {"type": "string"}}},
                        {"properties": {"b": {"type": "number"}}, "required": ["a", "b"]}
                    ]
                }
            }]
        });
        assert!(crate::proxy::flatten_tool_schemas(&mut v));
        let schema = &v["tools"][0]["input_schema"];
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["a"].is_object());
        assert!(schema["properties"]["b"].is_object());
        let req = schema["required"].as_array().unwrap();
        assert!(req.contains(&serde_json::json!("a")));
        assert!(req.contains(&serde_json::json!("b")));
        assert!(schema.get("allOf").is_none());
    }

    #[test]
    fn flattens_oneof_single_element() {
        let mut v = serde_json::json!({
            "tools": [{
                "name": "t",
                "input_schema": {
                    "oneOf": [{"type": "object", "properties": {"x": {"type": "string"}}}]
                }
            }]
        });
        assert!(crate::proxy::flatten_tool_schemas(&mut v));
        let schema = &v["tools"][0]["input_schema"];
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["x"].is_object());
        assert!(schema.get("oneOf").is_none());
    }

    #[test]
    fn flattens_allof_with_existing_top_level_props() {
        let mut v = serde_json::json!({
            "tools": [{
                "name": "t",
                "input_schema": {
                    "type": "object",
                    "description": "desc",
                    "allOf": [
                        {"properties": {"a": {"type": "string"}}, "required": ["a"]}
                    ]
                }
            }]
        });
        assert!(crate::proxy::flatten_tool_schemas(&mut v));
        let schema = &v["tools"][0]["input_schema"];
        assert_eq!(schema["type"], "object");
        assert_eq!(schema["description"], "desc");
        assert!(schema["properties"]["a"].is_object());
        assert!(schema.get("allOf").is_none());
    }

    #[test]
    fn noop_when_no_compound_schema() {
        let mut v = serde_json::json!({
            "tools": [{
                "name": "t",
                "input_schema": {"type": "object", "properties": {"a": {"type": "string"}}}
            }]
        });
        assert!(!crate::proxy::flatten_tool_schemas(&mut v));
    }

    /// `refusal_fallbacks_for`：只给主线程、计费、且该族开关开着的 fable / opus-5 补；fable 用
    /// 官方那份（默认开），opus-5 用 luban 自定的 4.8 → 4.6 链（默认关、要显式开）；两档互不
    /// 影响；sonnet/haiku 不补；上游拒过的模型不补。
    #[test]
    fn refusal_fallbacks_are_chosen_per_family_and_gated() {
        use crate::proxy::CcRequestKind::*;
        let mem = crate::proxy::DeprecatedFieldMemory::default();
        let defaults = all_on();
        assert!(
            !defaults.fable_refusal_fallback && !defaults.opus_refusal_fallback,
            "两档都默认关"
        );
        let on = store::ForwardFlags {
            fable_refusal_fallback: true,
            opus_refusal_fallback: true,
            ..defaults
        };
        let off = store::ForwardFlags {
            fable_refusal_fallback: false,
            opus_refusal_fallback: false,
            ..defaults
        };
        let pick = |m: &str, flags: store::ForwardFlags, billable: bool, kind| {
            crate::proxy::refusal_fallbacks_for(Some(m), flags, billable, kind, &mem)
        };
        assert_eq!(
            pick("claude-fable-5-1", on, true, Main),
            Some(r#"[{"model":"claude-opus-5"}]"#),
            "fable 补官方 2.1.260 那份（cap/2.1.260/00018）"
        );
        assert_eq!(pick("claude-fable-5", on, true, Main), Some(r#"[{"model":"claude-opus-5"}]"#));
        assert_eq!(pick("claude-opus-5", on, true, Main), Some(config::OPUS_REFUSAL_FALLBACKS));
        assert_eq!(pick("claude-opus-5[1m]", on, true, Main), Some(config::OPUS_REFUSAL_FALLBACKS));
        assert_eq!(
            pick("claude-sonnet-5", on, true, Main),
            None,
            "sonnet 官方客户端不发该字段，不补"
        );
        assert_eq!(pick("claude-opus-4-8", on, true, Main), None, "4.x 不补");
        assert_eq!(pick("claude-haiku-4-5-20251001", on, true, Main), None);
        assert_eq!(pick("claude-fable-5-1", off, true, Main), None, "开关关着不补");
        assert_eq!(pick("claude-opus-5", off, true, Main), None, "开关关着不补");
        // 默认值：两档都不补——fable 那份替用户决定换模型作答，opus 那份是官方不产生的形态，
        // 都得显式打开。
        assert_eq!(pick("claude-fable-5-1", defaults, true, Main), None, "fable 默认关");
        assert_eq!(pick("claude-opus-5", defaults, true, Main), None, "opus 默认关");
        assert_eq!(pick("claude-opus-5[1m]", defaults, true, Main), None, "opus 默认关");
        // 两档各管各的：只开 opus 时 fable 不补，反之亦然。
        let opus_only = store::ForwardFlags { fable_refusal_fallback: false, ..on };
        assert_eq!(
            pick("claude-fable-5-1", opus_only, true, Main),
            None,
            "fable 关着不受 opus 影响"
        );
        assert_eq!(
            pick("claude-opus-5", opus_only, true, Main),
            Some(config::OPUS_REFUSAL_FALLBACKS)
        );
        assert_eq!(pick("claude-fable-5-1", on, false, Main), None, "count_tokens 不补");
        for kind in [Subagent, Suggestion, Helper, Title, Classifier, QuotaProbe] {
            assert_eq!(pick("claude-fable-5-1", on, true, kind), None, "辅助请求不补: {kind:?}");
            assert_eq!(pick("claude-opus-5", on, true, kind), None, "辅助请求不补: {kind:?}");
        }
        assert_eq!(crate::proxy::refusal_fallbacks_for(None, on, true, Main, &mem), None);

        // 上游以 400 拒了 opus-5 的 fallback 目标：学下来，之后不补；fable 不受影响。
        let err = err_json("fallbacks.1.model: 'claude-opus-4-6' is not an allowed fallback model");
        assert!(crate::proxy::is_fallback_rejection(&err));
        assert!(!crate::proxy::is_fallback_rejection(&err_json("max_tokens: must be positive")));
        let row = crate::proxy::remember_fallback_rejection(&mem, "claude-opus-5", &err)
            .expect("首次学到");
        assert_eq!(
            (row.kind.as_str(), row.model.as_str(), row.field.as_str(), row.value.as_str()),
            ("deprecated", "claude-opus-5", "fallbacks", "")
        );
        assert!(crate::proxy::remember_fallback_rejection(&mem, "claude-opus-5", &err).is_none());
        assert_eq!(pick("claude-opus-5", on, true, Main), None, "学过就不补");
        assert_eq!(
            pick("claude-fable-5-1", on, true, Main),
            Some(r#"[{"model":"claude-opus-5"}]"#)
        );
        // 这条 deprecated 规则能经 seed 回填（`fallbacks` 不在 DEPRECATABLE_FIELDS 里，seed 单独放行）。
        let shape2 = crate::proxy::ShapeMemory::default();
        let dep2 = crate::proxy::DeprecatedFieldMemory::default();
        let empty2 = crate::proxy::EmptyReplyMemory::default();
        let seeded =
            crate::proxy::seed_learned_memories(&shape2, &dep2, &empty2, vec![row.clone()]);
        assert_eq!(seeded.deprecated, 1);
        assert_eq!(
            crate::proxy::refusal_fallbacks_for(Some("claude-opus-5"), on, true, Main, &dep2),
            None
        );
        // 单条删除也认。
        assert!(crate::proxy::forget_learned_memory(&shape2, &dep2, &empty2, &row));
        assert_eq!(
            crate::proxy::refusal_fallbacks_for(Some("claude-opus-5"), on, true, Main, &dep2),
            Some(config::OPUS_REFUSAL_FALLBACKS)
        );
        // `fallbacks` 不在采样参数名单里：客户端自带的不会被 sampling_policy 当采样参数剥掉。
        assert!(!crate::proxy::DEPRECATABLE_FIELDS.contains(&"fallbacks"));
    }

    /// [`client_supplied_fallbacks`]：客户端带了数组（哪怕是空数组）算它自己的；字符串
    /// `"default"`、缺失都不算——那两种出站的是 luban 的字面量。
    #[test]
    fn client_supplied_fallbacks_means_any_non_string_field() {
        let body = |f: serde_json::Value| {
            let mut v = serde_json::json!({"model": "claude-fable-5-1", "messages": []});
            v["fallbacks"] = f;
            v
        };
        assert!(crate::proxy::client_supplied_fallbacks(Some(&body(
            serde_json::json!([{"model": "claude-opus-4-8"}])
        ))));
        assert!(crate::proxy::client_supplied_fallbacks(Some(&body(serde_json::json!([])))));
        assert!(!crate::proxy::client_supplied_fallbacks(Some(&body(serde_json::json!(
            "default"
        )))));
        assert!(!crate::proxy::client_supplied_fallbacks(Some(
            &serde_json::json!({"model": "claude-fable-5-1"})
        )));
        assert!(!crate::proxy::client_supplied_fallbacks(None));
    }

    /// [`outbound_carries_fallbacks`]：客户端自带 `fallbacks`（数组非空或字符串 "default"）的、
    /// 或 luban 按族开关要补的请求算「带」；空数组不算；开关关着且客户端没带的不算；helper
    /// 之类非主线程请求 luban 不补，也不算。带的请求 2.3a4 不本地 403。
    #[test]
    fn outbound_carries_fallbacks_sees_client_arrays_and_luban_padding() {
        let mem = crate::proxy::DeprecatedFieldMemory::default();
        let defaults = store::ForwardFlags::default();
        let off = store::ForwardFlags {
            fable_refusal_fallback: false,
            opus_refusal_fallback: false,
            ..defaults
        };
        let main = serde_json::json!({
            "model": "claude-fable-5-1", "max_tokens": 32000, "stream": true,
            "system": [{"type": "text", "text": "You are Claude Code"}],
            "tools": [{"name": "Bash", "input_schema": {"type": "object"}}],
            "messages": [{"role": "user", "content": "hi"}]
        });
        let carries = |body: &serde_json::Value, model: &str, flags| {
            crate::proxy::outbound_carries_fallbacks(Some(body), Some(model), flags, &[], &mem)
        };
        // fable 默认关 → 不带；显式开 → luban 会补 → 带。
        assert!(!carries(&main, "claude-fable-5-1", defaults));
        let fable_on = store::ForwardFlags { fable_refusal_fallback: true, ..defaults };
        assert!(carries(&main, "claude-fable-5-1", fable_on));
        // 全关、客户端也没带 → 不带。
        assert!(!carries(&main, "claude-fable-5-1", off));
        // opus-5 默认关 → 不带；显式开 → 带。
        assert!(!carries(&main, "claude-opus-5", defaults));
        assert!(carries(
            &main,
            "claude-opus-5",
            store::ForwardFlags { opus_refusal_fallback: true, ..defaults }
        ));
        // sonnet：luban 不补 → 不带。
        assert!(!carries(&main, "claude-sonnet-5", defaults));
        // 客户端自带合法数组：开关关着也算带；空数组不算。
        let mut with_arr = main.clone();
        with_arr["fallbacks"] = serde_json::json!([{"model": "claude-opus-4-8"}]);
        assert!(carries(&with_arr, "claude-sonnet-5", off));
        // 字符串 "default"：有计划时会被换成计划 → 带；没计划时原样出站、头上不补 beta，
        // 上游不会换模型重跑 → 不算带，命中已学到的拒答就本地回放。
        let mut with_default = main.clone();
        with_default["fallbacks"] = serde_json::json!("default");
        assert!(carries(&with_default, "claude-fable-5-1", fable_on));
        assert!(!carries(&with_default, "claude-fable-5-1", off));
        assert!(!carries(&with_default, "claude-sonnet-5", off));
        assert!(!carries(&with_default, "claude-sonnet-5", defaults));
        // 客户端带的非字符串形态 luban 不动，出站就是它那份：空数组、null、对象、元素不是
        // 带 model 的对象——上游一定 400，开关开着也不算带了 fallback。
        for bogus in [
            serde_json::json!([]),
            serde_json::json!(null),
            serde_json::json!({}),
            serde_json::json!([null]),
            serde_json::json!([{}]),
            serde_json::json!(["bogus"]),
            serde_json::json!([{"model": ""}]),
            serde_json::json!([{"model": "claude-opus-4-8"}, {}]),
        ] {
            let mut with_bogus = main.clone();
            with_bogus["fallbacks"] = bogus.clone();
            assert!(!carries(&with_bogus, "claude-fable-5-1", fable_on), "{bogus} 不算");
            assert!(!carries(&with_bogus, "claude-fable-5-1", off), "{bogus} 不算");
        }
        // 客户端字符串写错、但开关开着：luban 会把字符串换成自己的计划 → 带。
        let mut bad_string = main.clone();
        bad_string["fallbacks"] = serde_json::json!("auto");
        assert!(carries(&bad_string, "claude-fable-5-1", fable_on));
        // 别的字符串同样不算：官方从没发过、上游一定 400。
        for bogus in ["", "auto", "Default"] {
            let mut with_bogus = main.clone();
            with_bogus["fallbacks"] = serde_json::json!(bogus);
            assert!(!carries(&with_bogus, "claude-sonnet-5", off), "{bogus:?} 不算");
        }
        // 非主线程（无 tools 的 helper）luban 不补 → 不带。
        let mut helper = main.clone();
        helper.as_object_mut().unwrap().remove("tools");
        assert!(!carries(&helper, "claude-fable-5-1", defaults));
        // 无体 → 不带。
        assert!(!crate::proxy::outbound_carries_fallbacks(
            None,
            Some("claude-fable-5-1"),
            defaults,
            &[],
            &mem
        ));
        // 上游 400 拒过这个模型的 fallback 目标：luban 不再补 → 不带。
        mem.write().insert(
            ("claude-fable-5-1".to_string(), crate::proxy::FALLBACKS_FIELD.to_string()),
            "rejected".into(),
        );
        assert!(!carries(&main, "claude-fable-5-1", defaults));
    }

    /// 真 CC 来访 `messages` 里一个断点都没有时补第三个断点（[`ensure_cc_message_breakpoint`]），
    /// 且只在缓存前缀与上一轮相同时补（[`cache_prefix_stable`]）。
    /// 形态照现网 `req_zu6ELzACscXlpGSg`：claude-vscode 2.1.273 的 agent-sdk 构建，5 块
    /// `system`（billing、身份句、基座、无断点块、尾块），身份句 / 基座 / 尾块各带 `5m` 断点，
    /// 末条是 `tool_result`、`messages` 里零断点；它的尾块每轮长 51 字节，那种轮次不能标。
    #[test]
    fn cc_request_without_message_breakpoint_gets_one_on_the_last_block() {
        let system = |ttl: &str| {
            format!(
                concat!(
                    r#"[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.273.abc; cc_entrypoint=claude-vscode;"}},"#,
                    r#"{{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude, running within the Claude Agent SDK.","cache_control":{{"type":"ephemeral","ttl":"{ttl}"}}}},"#,
                    r#"{{"type":"text","text":"{base}","cache_control":{{"type":"ephemeral","ttl":"{ttl}"}}}},"#,
                    r#"{{"type":"text","text":"env"}},"#,
                    r#"{{"type":"text","text":"tail","cache_control":{{"type":"ephemeral","ttl":"{ttl}"}}}}]"#
                ),
                ttl = ttl,
                base = "x".repeat(1200),
            )
        };
        let tool_loop = concat!(
            r#"[{"role":"user","content":"ls"},"#,
            r#"{"role":"system","content":"<total_tokens>15000000 tokens left</total_tokens>"},"#,
            r#"{"role":"assistant","content":[{"type":"tool_use","id":"tu_1","name":"Read","input":{"file_path":"a"}}]},"#,
            r#"{"role":"user","content":[{"type":"tool_result","tool_use_id":"tu_1","content":"a.txt"}]}]"#
        );
        let body = |system: &str, messages: &str| {
            Bytes::from(format!(
                r#"{{"model":"claude-opus-5","system":{system},"messages":{messages},"max_tokens":64000,"stream":true,"metadata":{{"user_id":"{{\"device_id\":\"dddd\",\"account_uuid\":\"\",\"session_id\":\"ssss\"}}"}}}}"#
            ))
        };
        // 每个用例一个新会话 id；同一份体发两轮，第二轮的前缀与第一轮相同，闸才放行。
        let once =
            |b: &Bytes, flags: store::ForwardFlags, sid: Option<&str>| -> serde_json::Value {
                serde_json::from_slice(&crate::proxy::test_support::rewrite_body_with_session(
                    b,
                    &test_cred(),
                    "fp",
                    flags,
                    None,
                    None,
                    sid,
                ))
                .unwrap()
            };
        let run = |b: &Bytes, flags: store::ForwardFlags| -> serde_json::Value {
            let sid = crate::proxy::uuid_v4();
            once(b, flags, Some(&sid));
            once(b, flags, Some(&sid))
        };

        // 会话第一轮：没有上一轮可比，不标。
        let sid = crate::proxy::uuid_v4();
        let v = once(&body(&system("5m"), tool_loop), all_on(), Some(&sid));
        assert_eq!(crate::proxy::count_cache_control(&v["messages"]), 0, "第一轮不标: {v}");
        // 第二轮同一份前缀 → 标。
        let v = once(&body(&system("5m"), tool_loop), all_on(), Some(&sid));
        assert_eq!(crate::proxy::count_cache_control(&v["messages"]), 1, "第二轮该标: {v}");
        // 第三轮 system 尾块变了（现网那种每轮长 51 字节）→ 不标：前缀变了，标了也是未命中。
        let grown = system("5m").replace(
            r#""text":"tail""#,
            r#""text":"tail\n\n<total_tokens>1 tokens left</total_tokens>""#,
        );
        let v = once(&body(&grown, tool_loop), all_on(), Some(&sid));
        assert_eq!(crate::proxy::count_cache_control(&v["messages"]), 0, "尾块变了不标: {v}");
        // 第四轮尾块又稳住 → 再标。
        let v = once(&body(&grown, tool_loop), all_on(), Some(&sid));
        assert_eq!(crate::proxy::count_cache_control(&v["messages"]), 1, "稳住后再标: {v}");
        // tools 变了同样算前缀变了。
        let with_tools = body(&grown, tool_loop);
        let with_tools = Bytes::from(String::from_utf8(with_tools.to_vec()).unwrap().replace(
            r#""max_tokens":64000"#,
            r#""tools":[{"name":"Read","input_schema":{"type":"object"}}],"max_tokens":64000"#,
        ));
        let v = once(&with_tools, all_on(), Some(&sid));
        assert_eq!(crate::proxy::count_cache_control(&v["messages"]), 0, "tools 变了不标: {v}");
        // 只有 billing header 变（cch / cc_prev_req 逐轮不同）不算前缀变。
        let cch = body(&grown, tool_loop);
        let cch = Bytes::from(
            String::from_utf8(cch.to_vec())
                .unwrap()
                .replace("cc_entrypoint=claude-vscode;", "cc_entrypoint=claude-vscode; cch=abcde;"),
        );
        let v = once(&body(&grown, tool_loop), all_on(), Some(&sid));
        assert_eq!(
            crate::proxy::count_cache_control(&v["messages"]),
            0,
            "tools 刚变回来这一轮不标: {v}"
        );
        let v = once(&cch, all_on(), Some(&sid));
        assert_eq!(
            crate::proxy::count_cache_control(&v["messages"]),
            1,
            "只有 billing header 变仍算稳定: {v}"
        );
        // 没有会话 id 可作键 → 不标。
        let v = once(&body(&system("5m"), tool_loop), all_on(), None);
        assert_eq!(crate::proxy::count_cache_control(&v["messages"]), 0, "没有会话键不标: {v}");

        // 正例：末块 tool_result 拿到断点，ttl 抄 system 的 5m 而不是开关的 1h，不带 scope；
        // 字符串形态的旧 reminder 不被转成块数组；总数正好 4。
        let v = run(&body(&system("5m"), tool_loop), all_on());
        let msgs = v["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 4, "messages 不该增删: {v}");
        assert!(msgs[1]["content"].is_string(), "旧 reminder 的字符串形态不该被转: {v}");
        let last = msgs[3]["content"].as_array().unwrap().last().unwrap();
        assert_eq!(last["type"], "tool_result");
        assert_eq!(
            last["cache_control"],
            serde_json::json!({"type": "ephemeral", "ttl": "5m"}),
            "断点该抄 system 尾块的 ttl: {v}"
        );
        assert_eq!(last["content"], "a.txt", "正文不动");
        assert_eq!(crate::proxy::count_cache_control(&v), 4, "总数正好封顶: {v}");
        assert_eq!(v["system"].as_array().unwrap().len(), 5, "system 块数不变: {v}");

        // 客户端 system 断点不带 ttl → 消息断点也不带。
        let bare = system("5m").replace(r#","ttl":"5m""#, "");
        let v = run(&body(&bare, tool_loop), all_on());
        let last = v["messages"][3]["content"].as_array().unwrap().last().unwrap();
        assert_eq!(last["cache_control"], serde_json::json!({"type": "ephemeral"}), "{v}");

        // 反例一：客户端 messages 里自己标过（哪怕标在中间那条）→ 一个字节不动。
        let marked = tool_loop.replace(
            r#"{"type":"tool_use","id":"tu_1","name":"Read","input":{"file_path":"a"}}"#,
            r#"{"type":"tool_use","id":"tu_1","name":"Read","input":{"file_path":"a"},"cache_control":{"type":"ephemeral"}}"#,
        );
        let v = run(&body(&system("5m"), &marked), all_on());
        assert!(
            v["messages"][3]["content"][0].get("cache_control").is_none(),
            "客户端自己标过就不再标: {v}"
        );
        assert_eq!(crate::proxy::count_cache_control(&v["messages"]), 1);

        // 反例二：末条是字符串 content → 不转、不标（官方 CLI 自己就混着发）。
        let str_tail = concat!(
            r#"[{"role":"user","content":"ls"},"#,
            r#"{"role":"assistant","content":[{"type":"text","text":"ok"}]},"#,
            r#"{"role":"user","content":"and then?"}]"#
        );
        let v = run(&body(&system("5m"), str_tail), all_on());
        assert!(v["messages"][2]["content"].is_string(), "末条字符串不该被转: {v}");
        assert_eq!(crate::proxy::count_cache_control(&v["messages"]), 0);

        // 反例三：预算满（system 里 4 个断点）→ 不标。
        let full = system("5m").replace(
            r#"{"type":"text","text":"env"}"#,
            r#"{"type":"text","text":"env","cache_control":{"type":"ephemeral","ttl":"5m"}}"#,
        );
        let v = run(&body(&full, tool_loop), all_on());
        assert_eq!(crate::proxy::count_cache_control(&v["messages"]), 0, "预算满不标: {v}");
        assert_eq!(crate::proxy::count_cache_control(&v), 4);

        // 反例四：末块是 thinking → 不标。
        let thinking_tail = concat!(
            r#"[{"role":"user","content":"ls"},"#,
            r#"{"role":"assistant","content":[{"type":"thinking","thinking":"想","signature":"AAAA"}]}]"#
        );
        let v = run(&body(&system("5m"), thinking_tail), all_on());
        assert_eq!(crate::proxy::count_cache_control(&v["messages"]), 0, "{v}");

        // 反例五：system_shape 开关关着 → 不标。
        let mut off = all_on();
        off.system_shape = false;
        let v = run(&body(&system("5m"), tool_loop), off);
        assert_eq!(crate::proxy::count_cache_control(&v["messages"]), 0, "开关关着不标: {v}");

        // 反例六：非 CC 形态（没有身份句、没有 billing header）走非模拟路径 → 不标，
        // 这一步只给真 CC 补。
        let plain_sys = r#"[{"type":"text","text":"You are a helpful bot.","cache_control":{"type":"ephemeral"}}]"#;
        let v = run(&body(plain_sys, tool_loop), all_on());
        assert_eq!(crate::proxy::count_cache_control(&v["messages"]), 0, "非 CC 形态不标: {v}");
    }

    /// 体侧 `ensure_fallbacks`：没写的补在 `context_management` 之后、`output_config` 之前
    /// （官方键序），字符串 `"default"` 换成数组，客户端自己的数组不动。
    /// 整形不能把缓存断点顶过 4 个。合并块那一个拆成基座 + 其余是净 +1，身份句没标断点时
    /// 抵不回来；客户端又在 `messages` 里标满三个，出去就是上游那条
    /// `A maximum of 4 blocks with cache_control may be provided. Found 5.`——整条被拒，
    /// 而少拆一次只是少一次基座级缓存命中。
    #[test]
    fn align_system_shape_respects_the_breakpoint_budget() {
        let merged =
            format!("base text\n\n{}\nrest of it", crate::config::CC_SYSTEM_BASE_ANCHORS[0]);
        // `msg_breakpoints` 条客户端自己标在 messages 里的断点，加 system 合并块那一个。
        let mk = |msg_breakpoints: usize| {
            let blocks: Vec<serde_json::Value> = (0..msg_breakpoints)
                .map(|i| {
                    serde_json::json!({
                        "type": "text",
                        "text": format!("m{i}"),
                        "cache_control": {"type": "ephemeral"}
                    })
                })
                .collect();
            serde_json::json!({
                "model": "claude-opus-5",
                "system": [
                    {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.260.222; cc_entrypoint=cli;"},
                    // 身份句**不带**断点：这一档的净变化才是 +1，官方 API-key 形态带着它、净 0。
                    {"type": "text", "text": crate::config::CC_SYSTEM_IDENTITY},
                    {"type": "text", "text": merged, "cache_control": {"type": "ephemeral"}},
                ],
                "messages": [{"role": "user", "content": blocks}],
            })
        };
        let shape = crate::proxy::CacheShape { global: true, ttl_1h: true };

        // 3 + 1 = 4，已经满了：不整形，且一个字节都不动。
        let mut full = mk(3);
        let before = full.clone();
        assert!(!crate::proxy::align_system_shape(&mut full, shape), "满了不该再拆");
        assert_eq!(full, before, "不拆就该原样留着，别留下半拆的形态");
        assert_eq!(crate::proxy::count_cache_control(&full), 4);

        // 2 + 1 = 3，还差一个：照常拆，拆完正好顶到 4。
        let mut room = mk(2);
        assert!(crate::proxy::align_system_shape(&mut room, shape));
        assert_eq!(
            crate::proxy::count_cache_control(&room),
            crate::proxy::MAX_CACHE_BREAKPOINTS,
            "还有位置就该拆"
        );
        assert_eq!(room["system"].as_array().unwrap().len(), 4, "拆成 [billing, 身份, 基座, 其余]");

        // 身份句自带断点的那一档（官方 API-key 三块形态）：2 + 2 = 4 已经满着，但拆开是净 0
        // ——身份句那个被去掉、合并块那个变两个——这道闸不该拦它。
        let mut official = mk(2);
        official["system"][1]["cache_control"] = serde_json::json!({"type": "ephemeral"});
        assert_eq!(crate::proxy::count_cache_control(&official), 4, "拆之前就已经满了");
        assert!(crate::proxy::align_system_shape(&mut official, shape), "净 0，这道闸不该拦它");
        assert_eq!(crate::proxy::count_cache_control(&official), 4, "拆完还是 4");
        assert!(official["system"][1].get("cache_control").is_none(), "身份句那个该被去掉");
    }

    /// `rewrite_body` 的「全关且不模拟」快路径不能吞掉 `fallbacks`：头上按同一个判断补了
    /// beta，体里必须写字段，否则 fable 的拒答换模型重跑名存实亡。反例：不补时快路径照走、
    /// 体原样。
    #[test]
    fn rewrite_body_fast_path_still_writes_fallbacks() {
        let off = store::ForwardFlags {
            spoof_identity: false,
            spoof_device_id: false,
            normalize_device_fp: false,
            billing_cch: false,
            fill_client_headers: false,
            merge_beta: false,
            system_shape: false,
            orig_header_case: false,
            thinking_signature_retry: false,
            thinking_modified_retry: false,
            redacted_thinking_retry: false,
            simulate_cc: false,
            fill_metadata: false,
            rate_limit_retry: false,
            cache_scope_global: false,
            cache_ttl_1h: false,
            eager_tool_streaming: false,
            nonstream_as_sse: false,
            strip_extra_fields: false,
            tool_name_mimic: false,
            inject_thinking: false,
            flatten_tool_schemas: false,
            strip_empty_text: false,
            hoist_system_role: false,
            reject_openai_shape: false,
            reject_session_conflict: false,
            reject_probes: false,
            reject_probes_strict: false,
            reject_refusals: false,
            reject_empty_replies: false,
            api_telemetry: false,
            keepalive_telemetry: false,
            fable_refusal_fallback: true,
            opus_refusal_fallback: false,
        };
        let body = Bytes::from_static(
            br#"{"model":"claude-fable-5-1","max_tokens":32000,"messages":[{"role":"user","content":"hi"}]}"#,
        );
        let plan = crate::proxy::cc_profile_for("claude-fable-5-1").fallbacks.unwrap();
        let shape = |fallbacks: Option<&str>| {
            crate::proxy::rewrite_body(
                &body,
                &test_cred(),
                "fp",
                off,
                None,
                None,
                None,
                false,
                None,
                false,
                false,
                None,
                None,
                crate::proxy::CcRequestKind::Main,
                fallbacks,
            )
        };
        // 不补：快路径，体原样。
        assert_eq!(shape(None), body);
        // 补：体里必须有官方那份数组，且按官方键序落在 max_tokens 之后。
        let out: serde_json::Value = serde_json::from_slice(&shape(Some(plan))).unwrap();
        assert_eq!(out["fallbacks"], serde_json::json!([{"model": "claude-opus-5"}]));
        let keys: Vec<&str> = out.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, ["model", "max_tokens", "fallbacks", "messages"]);
    }

    #[test]
    fn ensure_fallbacks_inserts_at_the_official_position_and_keeps_client_arrays() {
        let plan = config::OPUS_REFUSAL_FALLBACKS;
        let mut v = serde_json::json!({
            "model": "claude-opus-5", "messages": [], "max_tokens": 8, "thinking": {"type": "adaptive"},
            "context_management": {"edits": []}, "output_config": {"effort": "high"}, "stream": true
        });
        assert!(crate::proxy::ensure_fallbacks(&mut v, plan));
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            vec![
                "model",
                "messages",
                "max_tokens",
                "thinking",
                "context_management",
                "fallbacks",
                "output_config",
                "stream"
            ],
            "{v}"
        );
        assert_eq!(
            v["fallbacks"],
            serde_json::json!([{"model": "claude-opus-4-8"}, {"model": "claude-opus-4-6"}])
        );
        // 2.1.258 的字符串形态：换成数组、位置不动。
        let mut v = serde_json::json!({"model": "claude-fable-5-1", "fallbacks": "default", "messages": []});
        assert!(crate::proxy::ensure_fallbacks(&mut v, r#"[{"model":"claude-opus-5"}]"#));
        assert_eq!(v["fallbacks"], serde_json::json!([{"model": "claude-opus-5"}]));
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(keys, vec!["model", "fallbacks", "messages"]);
        // 客户端自己带的数组：原样不动。
        let mut v = serde_json::json!({"model": "claude-opus-5", "fallbacks": [{"model": "claude-sonnet-5"}], "messages": []});
        assert!(!crate::proxy::ensure_fallbacks(&mut v, plan));
        assert_eq!(v["fallbacks"], serde_json::json!([{"model": "claude-sonnet-5"}]));
        // 经 rewrite_body 走一遍模拟路径：fable 有字面量就补，位置按 profile 键序归位。
        let body = Bytes::from(r#"{"model":"claude-fable-5-1","messages":[{"role":"user","content":"hi"}],"max_tokens":16}"#.to_string());
        let sim = sim_for(std::str::from_utf8(&body).unwrap());
        let out = crate::proxy::rewrite_body(
            &body,
            &test_cred(),
            "fp",
            all_on(),
            Some(&sim),
            None,
            None,
            false,
            None,
            true,
            true,
            None,
            None,
            crate::proxy::CcRequestKind::Main,
            Some(r#"[{"model":"claude-opus-5"}]"#),
        );
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["fallbacks"], serde_json::json!([{"model": "claude-opus-5"}]), "{v}");
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        let idx = |k: &str| {
            keys.iter().position(|x| *x == k).unwrap_or_else(|| panic!("{k} 缺失: {keys:?}"))
        };
        assert!(idx("fallbacks") > idx("max_tokens"), "{keys:?}");
        assert!(
            idx("fallbacks") < idx("diagnostics"),
            "官方序里 fallbacks 在 diagnostics 之前: {keys:?}"
        );
    }

    /// 注入的工具声明**逐 profile 一份**，且是官方主线程恒带的 11 个真工具，不是四个。
    ///
    /// 依据：2.1.258 四族、2.1.260 opus / fable、2.1.270 sonnet 的主线程抓包没有一条只带
    /// 四个工具，全部样本共有的 13 个去掉 `ToolSearch` + `DeferredToolPlaceholder` 那一对
    /// 延迟加载机制就是这 11 个。`cap/2.1.260-2/00025`（opus）与 `cap/2.1.260/00018`
    /// （fable）这 11 个的 schema 无一相同——opus 全带 `eager_input_streaming`、fable 全不带，
    /// 此外 opus 的 Bash 多了 `Foreground sleep is blocked` 那句。
    #[test]
    fn core_tool_stubs_are_profile_specific() {
        let opus = crate::proxy::cc_tools_core(config::cc_profile(config::CcProfileKind::MainOpus));
        let fable =
            crate::proxy::cc_tools_core(config::cc_profile(config::CcProfileKind::MainFable));
        let names = |t: &[serde_json::Value]| -> Vec<String> {
            t.iter().map(|x| x["name"].as_str().unwrap_or("?").to_string()).collect()
        };
        // **顺序也是抓包的一部分**：所有主线程抓包里这 11 个的相对次序都是官方声明序，
        // 不是字母序（`Write` 排在 `Workflow` 之后）。资产按字母序或按手写顺序排都会得到
        // 一个官方不产生的排列，而这种错不会有任何运行期症状。
        assert_eq!(
            names(opus),
            [
                "Agent",
                "AskUserQuestion",
                "Bash",
                "Edit",
                "ListAgents",
                "Read",
                "ReportFindings",
                "ScheduleWakeup",
                "Skill",
                "Workflow",
                "Write",
            ],
            "官方主线程恒带的 11 个真工具与其次序"
        );
        assert_eq!(names(fable), names(opus), "两族的工具集相同，差的是描述");
        // 延迟加载那一对与环境相关的三个故意不注。
        for absent in [
            "ToolSearch",
            "DeferredToolPlaceholder",
            "Artifact",
            "SendFeedback",
            "ShareOnboardingGuide",
        ] {
            assert!(!names(opus).iter().any(|n| n == absent), "{absent} 不该注入");
        }
        for (a, b) in opus.iter().zip(fable.iter()) {
            assert_ne!(a, b, "{} 两族的 schema 不该相同", a["name"]);
        }
        // `eager_input_streaming` 原样保留：opus 每个都带（`cap/2.1.260-2/00025`），fable
        // 一个都不带（`cap/2.1.260/00018`）。加了或剥了都会偏离抓包。
        assert!(opus.iter().all(|t| t["eager_input_streaming"] == true), "opus 全带");
        assert!(fable.iter().all(|t| t.get("eager_input_streaming").is_none()), "fable 全不带");
        // opus 那份里那句 fable 没有的话，是这两份资产真的分开了的最短证据。
        let bash = opus.iter().find(|t| t["name"] == "Bash").unwrap();
        assert!(
            bash["description"].as_str().unwrap().contains("Foreground `sleep` is blocked"),
            "opus 的 Bash 描述取自 cap/2.1.260-2/00025"
        );
        let fable_bash = fable.iter().find(|t| t["name"] == "Bash").unwrap();
        assert!(!fable_bash["description"].as_str().unwrap().contains("Foreground `sleep`"));

        // 没有 2.1.260 样本的两族退回 opus 那份（版本对得上优先于族别对得上）。
        for kind in [config::CcProfileKind::MainSonnet, config::CcProfileKind::MainHaiku] {
            assert_eq!(
                crate::proxy::cc_tools_core(config::cc_profile(kind)),
                opus,
                "{kind:?} 退回 opus"
            );
        }
    }

    /// [`cc_tools_to_inject`] 是注入与流水共用的那一份判据：注进去的名单与流水拿去对
    /// 回复 tool_use 的名单必须是同一份，否则「模型调了注入工具」会被记错对象。
    #[test]
    fn cc_tools_to_inject_names_exactly_what_gets_injected() {
        let profile = config::cc_profile(config::CcProfileKind::MainOpus);
        let all: Vec<&str> = crate::proxy::cc_tools_core(profile)
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        let body = |tools: &str| -> serde_json::Value {
            serde_json::from_str(&format!(r#"{{"model":"claude-opus-5","messages":[]{tools}}}"#))
                .unwrap()
        };
        // 没有 tools 键：官方无工具 helper 的形态，一个都不注。
        assert!(super::cc_tools_to_inject(&body(""), profile).is_empty());
        // 空数组：全部 11 个。
        assert_eq!(super::cc_tools_to_inject(&body(r#","tools":[]"#), profile), all);
        // 已带部分官方名：只补缺的，顺序仍是官方声明序。
        let partial = body(r#","tools":[{"name":"Skill"},{"name":"Bash"},{"name":"TaskCreate"}]"#);
        let expect: Vec<&str> =
            all.iter().copied().filter(|n| !["Skill", "Bash"].contains(n)).collect();
        assert_eq!(super::cc_tools_to_inject(&partial, profile), expect);
        // 11 个全声明了：不注。
        let full = body(&format!(
            r#","tools":[{}]"#,
            all.iter().map(|n| format!(r#"{{"name":"{n}"}}"#)).collect::<Vec<_>>().join(",")
        ));
        assert!(super::cc_tools_to_inject(&full, profile).is_empty());
        // 只有第三方名：全部 11 个，与真正注进去的一致。
        let mut v = body(r#","tools":[{"name":"exec"},{"name":"read_file"}]"#);
        let planned = super::cc_tools_to_inject(&v, profile);
        assert_eq!(planned, all);
        assert!(super::inject_cc_tools(&mut v, profile));
        let injected: Vec<&str> = v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .filter(|n| config::CC_TOOL_NAMES.contains(n))
            .collect();
        assert_eq!(injected, planned, "注进去的名单必须就是判据给出的那份");
        // 客户端自己的工具仍在后面，一个没丢。
        assert_eq!(v["tools"].as_array().unwrap().len(), all.len() + 2);
    }

    /// 同名替换保留客户端显式写的 `eager_input_streaming`：opus 资产带 true，客户端 Read 写了
    /// false → 出站 Read 是官方对象但 eager 为 false；fable 资产没有这个键，客户端 Read 写了
    /// true → 出站 Read 带 true；没写的一律等于资产。整条模拟路径走完（注入 → 补 eager → 混淆）
    /// 结论不变——补 eager 那步对已有键不动。
    #[test]
    fn same_named_client_tools_keep_their_explicit_eager_setting() {
        for (kind, model, client_value) in [
            (config::CcProfileKind::MainOpus, "claude-opus-5", false),
            (config::CcProfileKind::MainFable, "claude-fable-5-1", true),
        ] {
            let profile = config::cc_profile(kind);
            let asset = crate::proxy::cc_tools_core(profile);
            let asset_read = asset.iter().find(|t| t["name"] == "Read").unwrap();
            let asset_bash = asset.iter().find(|t| t["name"] == "Bash").unwrap();
            let mut v = serde_json::json!({
                "model": model, "messages": [],
                "tools": [
                    {"name": "Read", "description": "mine", "input_schema": {"type": "object"}, "eager_input_streaming": client_value},
                    {"name": "Bash", "description": "mine", "input_schema": {"type": "object"}}
                ]
            });
            assert!(super::inject_cc_tools(&mut v, profile));
            let tools = v["tools"].as_array().unwrap();
            let read = tools.iter().find(|t| t["name"] == "Read").unwrap();
            let bash = tools.iter().find(|t| t["name"] == "Bash").unwrap();
            let mut expect_read = asset_read.clone();
            expect_read["eager_input_streaming"] = serde_json::json!(client_value);
            assert_eq!(
                serde_json::to_string(read).unwrap(),
                serde_json::to_string(&expect_read).unwrap(),
                "{model}: 官方对象 + 客户端显式 eager"
            );
            assert_eq!(
                serde_json::to_string(bash).unwrap(),
                serde_json::to_string(asset_bash).unwrap(),
                "{model}: 没写的等于资产"
            );

            // 整条模拟路径：补 eager 那步不覆盖已有键，混淆不动白名单里的官方名。
            let body: Bytes = serde_json::json!({
                "model": model, "max_tokens": 1024,
                "messages": [{"role": "user", "content": "hi"}],
                "tools": [{"name": "Read", "description": "mine", "input_schema": {"type": "object"}, "eager_input_streaming": client_value}],
                "stream": true
            })
            .to_string()
            .into();
            let sim = sim_for(std::str::from_utf8(&body).unwrap());
            let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
            let out: serde_json::Value = serde_json::from_slice(&out).unwrap();
            let read =
                out["tools"].as_array().unwrap().iter().find(|t| t["name"] == "Read").unwrap();
            assert_eq!(read["eager_input_streaming"], client_value, "{model}: 出站保留显式值");
        }
    }

    /// 客户端已带部分官方名的「半抄」克隆：缺的补到头部，同名的一律整条换成官方声明（参数
    /// 表面一致与否都换，不一致的只多一行日志），老版本多出来的不删。依据是现网一条
    /// Go-http-client：15 个官方名（含 2.1.258 才有的 TaskCreate 等）、缺 2.1.260 恒带的四个，
    /// 原先一个都不补。
    #[test]
    fn partial_cc_clones_get_the_missing_tools_and_official_replacements() {
        let profile = config::cc_profile(config::CcProfileKind::MainOpus);
        let official = crate::proxy::cc_tools_core(profile);
        let official_read = official.iter().find(|t| t["name"] == "Read").unwrap();
        let official_bash = official.iter().find(|t| t["name"] == "Bash").unwrap();
        // Read：抄了参数表面（同一组 properties / required），自己写的描述 → 换。
        let mut client_read = official_read.clone();
        client_read["description"] = serde_json::json!("reads a file, my own wording");
        client_read.as_object_mut().unwrap().remove("eager_input_streaming");
        // Bash：多要一个必填 `cwd`，参数表面与官方不一致 → 照样换，只是多一行日志。
        let client_bash = serde_json::json!({
            "name": "Bash", "description": "run",
            "input_schema": {"type": "object", "properties": {"command": {"type": "string"}, "cwd": {"type": "string"}}, "required": ["command", "cwd"]}
        });
        let mut v = serde_json::json!({
            "model": "claude-opus-5", "messages": [],
            "tools": [{"name": "my_tool", "input_schema": {"type": "object"}}, client_read, client_bash, {"name": "TaskCreate", "input_schema": {"type": "object"}}]
        });
        let planned = super::cc_tools_to_inject(&v, profile);
        assert!(!planned.contains(&"Read") && !planned.contains(&"Bash"), "{planned:?}");
        assert_eq!(planned.len(), 9, "11 个里客户端已有 Read / Bash 两个");
        assert!(super::inject_cc_tools(&mut v, profile));
        let tools = v["tools"].as_array().unwrap();
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        // 头部是完整的 11 条、按官方声明序（客户端的 Read / Bash 被挪进这一段）；客户端其余
        // 工具紧随其后、相对次序不变。
        let all: Vec<&str> = official.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert_eq!(&names[..11], all.as_slice());
        assert_eq!(&names[11..], ["my_tool", "TaskCreate"]);
        let read = tools.iter().find(|t| t["name"] == "Read").unwrap();
        assert_eq!(read, official_read, "参数表面一致的 Read 整条换成官方声明");
        let bash = tools.iter().find(|t| t["name"] == "Bash").unwrap();
        assert_eq!(bash, official_bash, "参数表面不一致的 Bash 同样换成官方声明");
        assert!(!super::same_schema_surface(&client_bash, official_bash), "日志判据认得出它不一致");
        assert!(names.contains(&"TaskCreate"), "老版本多出来的不删");
    }

    /// 客户端把 11 个全声明了，但次序不是官方的、其中一条键序不同（内容全同）：出站仍要是
    /// 按官方序排列的 11 条逐字节官方声明。`Value` 相等忽略键序，按它跳过替换会把客户端键序
    /// 原样发出去；只缺几条时把缺的插头部则会把次序排乱。
    #[test]
    fn official_tools_are_emitted_in_asset_order_and_byte_exact() {
        let profile = config::cc_profile(config::CcProfileKind::MainOpus);
        let official = crate::proxy::cc_tools_core(profile);
        let expected = serde_json::to_string(official).unwrap();
        // 倒序声明，并把第一条（Write）的键序打乱：input_schema 提到 name 之前。
        let mut declared: Vec<serde_json::Value> = official.iter().rev().cloned().collect();
        let scrambled = {
            let src = declared[0].as_object().unwrap();
            let mut m = serde_json::Map::new();
            m.insert("input_schema".into(), src["input_schema"].clone());
            for (k, val) in src.iter().filter(|(k, _)| *k != "input_schema") {
                m.insert(k.clone(), val.clone());
            }
            serde_json::Value::Object(m)
        };
        assert_eq!(scrambled, declared[0], "Value 相等看不出键序不同——这正是要防的");
        assert_ne!(
            serde_json::to_string(&scrambled).unwrap(),
            serde_json::to_string(&declared[0]).unwrap()
        );
        declared[0] = scrambled;
        declared.push(serde_json::json!({"name": "my_tool", "input_schema": {"type": "object"}}));
        let mut v =
            serde_json::json!({"model": "claude-opus-5", "messages": [], "tools": declared});
        assert!(super::cc_tools_to_inject(&v, profile).is_empty(), "一个都不缺");
        assert!(super::inject_cc_tools(&mut v, profile), "次序与键序都要改");
        let tools = v["tools"].as_array().unwrap();
        assert_eq!(
            serde_json::to_string(&tools[..11]).unwrap(),
            expected,
            "11 条逐字节等于资产、按资产序"
        );
        assert_eq!(tools[11]["name"], "my_tool");
        // 已经是官方形态的再过一遍什么都不动。
        assert!(!super::inject_cc_tools(&mut v, profile));
    }

    /// Windows 那种**扁平** `metadata.user_id` 同样要认，额度探测复用它的**原文**。
    ///
    /// 只解内嵌 JSON 的话，扁平串解析失败 → 退回「device 为空 + 凭证账号」：主请求有设备、
    /// 握手/eval/启动遥测/额度探测却没有。而把它重拼成 JSON 又会造出「同一会话一条扁平、
    /// 一条 JSON」——`spoof_identity` 那边特意保住了扁平形态，这里不能给拆了。
    #[test]
    fn outbound_identity_handles_the_windows_flat_form() {
        const SID: &str = "9f8e7d6c-0000-1111-2222-333344445555";
        let cred = test_cred();
        let body = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","max_tokens":64000,"messages":[{{"role":"user","content":"hi"}}],"system":[{{"type":"text","text":"{}"}}],"metadata":{{"user_id":"user_winDev1_account_oldacct_session_{SID}"}}}}"#,
            config::CC_SYSTEM_IDENTITY
        ));

        // 默认配置：`spoof_identity` 换掉 device 与 account，**仍以扁平串回写**。
        let sent = rewrite_body(&body, &cred, "fp", all_on(), None, None);
        let ident = crate::proxy::outbound_identity(&sent, &cred);
        let raw = ident.raw_user_id.clone().expect("出站体里有 user_id");
        assert!(raw.starts_with("user_"), "出站仍是扁平串: {raw}");
        assert!(raw.ends_with(&format!("_session_{SID}")), "session 段保留: {raw}");
        assert_eq!(ident.device_id, cred.spoof_device_id("fp").unwrap(), "device 段解出来了");
        assert_eq!(ident.account_uuid, ACCOUNT_UUID, "account 段也解出来了");
        assert!(!ident.device_id.is_empty(), "不能像只解 JSON 那样退回空 device");

        // `spoof_device_id=false`：device 段保留客户端的，握手跟着。
        let sent = rewrite_body(
            &body,
            &cred,
            "fp",
            store::ForwardFlags { spoof_device_id: false, ..all_on() },
            None,
            None,
        );
        let keep = crate::proxy::outbound_identity(&sent, &cred);
        assert_eq!(keep.device_id, "winDev1");

        // 额度探测复用原文，连编码形态一起——不会变成 JSON。
        let probe = crate::proxy::with_outbound_identity(
            crate::proxy::probe_body(crate::proxy::QUOTA_PROBE_MODEL),
            &keep,
        );
        let v: serde_json::Value = serde_json::from_slice(&probe).unwrap();
        let probe_uid = v["metadata"]["user_id"].as_str().unwrap();
        assert_eq!(Some(probe_uid), keep.raw_user_id.as_deref(), "逐字节同一串");
        assert!(probe_uid.starts_with("user_winDev1_account_"), "还是扁平串: {probe_uid}");
    }

    /// 关掉 `spoof_identity` 之后，客户端自己带的 `metadata.user_id` **必须原样留着**。
    ///
    /// 剥这一步原先只看 `sim.is_some()`，而重建那步要 `flags.spoof_identity`：开关一关，
    /// 身份就被删掉且没人补回来——头上还有会话 id、体里什么都没有。那既违背这个开关的
    /// 语义（「别改身份」被执行成了「把身份删了」），也违背客户端数据透传契约。
    #[test]
    fn identity_spoofing_off_keeps_the_client_metadata() {
        const USER_ID: &str = r#"{\"device_id\":\"dev-1\",\"account_uuid\":\"acct-1\",\"session_id\":\"d0c1fb05-9b19-4576-9465-e2b8a206dabf\"}"#;
        let body = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","max_tokens":64000,"messages":[{{"role":"user","content":"hi"}}],"metadata":{{"user_id":"{USER_ID}"}}}}"#
        ));
        let sim = detect_for(&body, all_on()).expect("非 CC 形态该走模拟");

        let off = store::ForwardFlags { spoof_identity: false, ..all_on() };
        let out = rewrite_body(&body, &test_cred(), "fp", off, Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let kept = v["metadata"]["user_id"].as_str().expect("身份不该被删掉");
        assert!(kept.contains("dev-1"), "device_id 原样留着: {kept}");
        assert!(kept.contains("acct-1"), "account_uuid 原样留着: {kept}");

        // 开关开着时照旧重建成该凭证自洽的那份（原有行为不变）。
        let on = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&on).unwrap();
        let rebuilt = v["metadata"]["user_id"].as_str().unwrap();
        assert!(rebuilt.contains(ACCOUNT_UUID), "开着就换成凭证自己的: {rebuilt}");
        assert!(!rebuilt.contains("acct-1"));
    }

    /// [`crate::proxy::sync_metadata_session`] 把体里的会话段对齐到出站那个，**保持原格式**：
    /// 内嵌 JSON 定点替换（字段序与其余内容逐字节不变）、扁平串重拼；本来没有会话段的
    /// 就按各自格式补一段——头上有合法会话 id、体里没有，是官方绝不产生的组合。
    #[test]
    fn syncing_the_metadata_session_keeps_the_original_shape() {
        const SID: &str = "d0c1fb05-9b19-4576-9465-e2b8a206dabf";
        let user_id =
            |v: &serde_json::Value| v["metadata"]["user_id"].as_str().unwrap().to_string();

        // 内嵌 JSON：只有 session_id 那段变了，字段顺序与其余内容原样。
        let mut v = serde_json::json!({
            "metadata": { "user_id": r#"{"device_id":"dd","account_uuid":"aa","session_id":"sess-9"}"# }
        });
        assert!(crate::proxy::sync_metadata_session(&mut v, SID));
        assert_eq!(
            user_id(&v),
            format!(r#"{{"device_id":"dd","account_uuid":"aa","session_id":"{SID}"}}"#)
        );
        // 已经同值 → 不动。
        assert!(!crate::proxy::sync_metadata_session(&mut v, SID));
        // 值只差首尾空白也**要**改：逐字节比，不 trim——否则头上写的是干净的 uuid，体里留着
        // 带空格的那份，两处不再逐字相同。
        let mut v = serde_json::json!({
            "metadata": { "user_id": format!(r#"{{"device_id":"dd","account_uuid":"aa","session_id":" {SID} "}}"#) }
        });
        assert!(crate::proxy::sync_metadata_session(&mut v, SID), "带空白的同值也得改写");
        assert_eq!(
            user_id(&v),
            format!(r#"{{"device_id":"dd","account_uuid":"aa","session_id":"{SID}"}}"#)
        );

        // 扁平串：device 与 account 段原样，只换 session 段，仍以扁平串回写。
        let mut v = serde_json::json!({
            "metadata": { "user_id": "user_deadbeef_account_acct-1_session_sess-9" }
        });
        assert!(crate::proxy::sync_metadata_session(&mut v, SID));
        assert_eq!(user_id(&v), format!("user_deadbeef_account_acct-1_session_{SID}"));
        assert!(!crate::proxy::sync_metadata_session(&mut v, SID));
        // 扁平串的 session 段带尾部空白同样要改写。
        let mut v = serde_json::json!({
            "metadata": { "user_id": format!("user_deadbeef_account_acct-1_session_{SID} ") }
        });
        assert!(crate::proxy::sync_metadata_session(&mut v, SID));
        assert_eq!(user_id(&v), format!("user_deadbeef_account_acct-1_session_{SID}"));

        // 内嵌 JSON 没有会话段 → 追加到末尾（官方键序 device → account → session），
        // 其余内容逐字节不变。补过之后再同步一次是幂等的。
        let mut v = serde_json::json!({
            "metadata": { "user_id": r#"{"device_id":"dd","account_uuid":"aa"}"# }
        });
        assert!(crate::proxy::sync_metadata_session(&mut v, SID));
        assert_eq!(
            user_id(&v),
            format!(r#"{{"device_id":"dd","account_uuid":"aa","session_id":"{SID}"}}"#)
        );
        assert!(!crate::proxy::sync_metadata_session(&mut v, SID));
        // 空对象也能补，不多出前导逗号。
        let mut v = serde_json::json!({ "metadata": { "user_id": "{}" } });
        assert!(crate::proxy::sync_metadata_session(&mut v, SID));
        assert_eq!(user_id(&v), format!(r#"{{"session_id":"{SID}"}}"#));
        // 扁平串缺 session 段 → 追加整段，仍是扁平串。
        let mut v = serde_json::json!({
            "metadata": { "user_id": "user_deadbeef_account_acct-1" }
        });
        assert!(crate::proxy::sync_metadata_session(&mut v, SID));
        assert_eq!(user_id(&v), format!("user_deadbeef_account_acct-1_session_{SID}"));
        assert!(!crate::proxy::sync_metadata_session(&mut v, SID));
        // 两种格式都认不出 → 不动。
        let mut v = serde_json::json!({ "metadata": { "user_id": "opaque-user-42" } });
        assert!(!crate::proxy::sync_metadata_session(&mut v, SID));
        assert_eq!(user_id(&v), "opaque-user-42");
        // 压根没有 metadata.user_id → 不动（那条交给 `ensure_cc_metadata`）。
        let mut v = serde_json::json!({ "model": "claude-opus-5" });
        assert!(!crate::proxy::sync_metadata_session(&mut v, SID));
        assert!(v.get("metadata").is_none());
    }

    /// 回归 2026-08-07 的拒绝日志：客户端的顶层顺序是
    /// `model, system, messages, max_tokens, stream, tools, metadata, output_config`，即使
    /// system 和工具名都已整形，这个顺序仍把第三方客户端指纹原样带了出去。
    #[test]
    fn simulated_request_reorders_existing_top_level_keys() {
        let body = Bytes::from(
            r#"{"model":"claude-sonnet-5","system":"third party","messages":[],"max_tokens":65536,"stream":true,"tools":[{"name":"skill_manage"}],"metadata":{},"output_config":{"effort":"high"}}"#
                .to_string(),
        );
        let parsed_body = parsed(&body);
        let sim = detect_for(&body, all_on()).expect("该请求应走模拟路径");
        let map = build_tool_name_map(parsed_body.as_ref()).unwrap();
        let out = crate::proxy::rewrite_body(
            &body,
            &test_cred(),
            "fp",
            all_on(),
            Some(&sim),
            None,
            None,
            false,
            Some(&map),
            true,
            true,
            None,
            None,
            crate::proxy::CcRequestKind::Main,
            None,
        );
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(
            keys,
            [
                "model",
                "messages",
                "system",
                "tools",
                "metadata",
                "max_tokens",
                "thinking",
                "context_management",
                "output_config",
                // 官方主线程每条都带（首轮值为 null），见 [`crate::proxy::ensure_diagnostics`]。
                "diagnostics",
                "stream",
            ],
            "模拟后顶层键序必须与官方抓包一致: {}",
            String::from_utf8_lossy(&out)
        );
        // 模拟路径注入了官方主线程的 11 个工具，它们排在前面。
        let tool_names: Vec<&str> = v["tools"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()))
            .collect();
        assert!(tool_names.contains(&"Bash"), "模拟路径应注入 CC 核心工具: {tool_names:?}");
        assert!(
            tool_names.iter().any(|n| n.starts_with("mcp__luban__")),
            "客户端自有工具应改成 MCP 形态: {tool_names:?}"
        );
    }

    /// 补 `thinking` 的形态按 profile：opus/sonnet 是 `{adaptive, display:"updates"}`
    /// （2.1.260 起 opus 也带 display，`cap/2.1.260-2/00025`），haiku 是
    /// `{budget_tokens, type:enabled, display:"updates"}`（key 序 budget 在前，
    /// `cap/2.1.260/00020`）。给 opus-5 / sonnet-5 发 `budget_tokens` 会直接 400。
    #[test]
    fn injects_thinking_shape_per_model_family() {
        let run = |body: &str| -> serde_json::Value {
            let b = Bytes::from(body.to_string());
            let sim = sim_for(body);
            let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
            serde_json::from_slice(&out).unwrap()
        };
        let opus = run(
            r#"{"model":"claude-opus-5","max_tokens":64000,"messages":[{"role":"user","content":"hi"}]}"#,
        );
        assert_eq!(
            opus["thinking"],
            serde_json::json!({"type": "adaptive", "display": "updates"}),
            "{opus}"
        );
        let sonnet = run(
            r#"{"model":"claude-sonnet-5","max_tokens":64000,"messages":[{"role":"user","content":"hi"}]}"#,
        );
        assert_eq!(
            sonnet["thinking"],
            serde_json::json!({"type": "adaptive", "display": "updates"}),
            "{sonnet}"
        );
        let haiku = run(
            r#"{"model":"claude-haiku-4-5-20251001","max_tokens":32000,"messages":[{"role":"user","content":"hi"}]}"#,
        );
        let s = serde_json::to_string(&haiku).unwrap();
        assert!(
            s.contains(
                r#""thinking":{"budget_tokens":31999,"type":"enabled","display":"updates"}"#
            ),
            "haiku 的 thinking 逐字节对齐官方: {s}"
        );
    }

    /// 模拟路径要补 `context_management`：`cap/raw` 八份抓包逐字节相同，而声明它的
    /// `context-management-2025-06-27` 已在两份 seed 里，不补就是「头上声明了、体里没有」。
    ///
    /// **但只在客户端自己开了 thinking 时补**——`clear_thinking` 依赖它，没开硬补上游回
    /// `` `clear_thinking_20251015` strategy requires `thinking` to be enabled or adaptive ``。
    /// 抓包八份全开着 thinking，这层依赖看不出来，v0.2.51 即因此让普通请求 400。
    #[test]
    fn simulated_body_carries_official_context_management() {
        const OFFICIAL: &str =
            r#""context_management":{"edits":[{"type":"clear_thinking_20251015","keep":"all"}]}"#;
        // 开着 thinking 的来访（官方 opus/sonnet/fable 那族的形态）。
        let thinking_body = concat!(
            r#"{"model":"claude-opus-5","max_tokens":1024,"#,
            r#""messages":[{"role":"user","content":"hi"}],"#,
            r#""thinking":{"type":"adaptive"},"stream":true}"#
        );
        let body = Bytes::from(thinking_body.to_string());
        let sim = sim_for(thinking_body);
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
        let text = String::from_utf8(out.to_vec()).unwrap();
        assert!(text.contains(OFFICIAL), "取值要与官方逐字节相同: {text}");

        // 官方位置：`thinking` 之后、`stream` 之前。
        let at = text.find(OFFICIAL).unwrap();
        assert!(at > text.find(r#""thinking""#).unwrap(), "该排在 thinking 之后: {text}");
        assert!(at < text.find(r#""stream""#).unwrap(), "该排在 stream 之前: {text}");
        assert!(at > text.find(r#""metadata""#).unwrap(), "该排在 metadata 之后: {text}");

        // 头上那份声明确实在，否则补了体就是反向的自相矛盾。
        assert!(
            sim.profile.beta.contains("context-management-2025-06-27"),
            "seed 里该有对应的 beta"
        );

        // haiku 那族的 `{"type":"enabled","budget_tokens":N}` 同样算开着。
        let haiku = concat!(
            r#"{"model":"claude-haiku-4-5-20251001","max_tokens":32000,"#,
            r#""messages":[{"role":"user","content":"hi"}],"#,
            r#""thinking":{"budget_tokens":31999,"type":"enabled"}}"#
        );
        let b = Bytes::from(haiku.to_string());
        let sim = sim_for(haiku);
        let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
        assert!(String::from_utf8(out.to_vec()).unwrap().contains(OFFICIAL), "enabled 也该补");

        // 没开 thinking 的几种写法都不补——补了上游直接 400。
        // max_tokens 低于阈值时也不补（探测级请求不值得加 thinking）。
        for body in [
            // max_tokens 太小，不注入 thinking
            r#"{"model":"claude-opus-5","max_tokens":16,"messages":[{"role":"user","content":"hi"}]}"#.to_string(),
            // thinking 显式 disabled
            concat!(
                r#"{"model":"claude-opus-5","max_tokens":16,"#,
                r#""messages":[{"role":"user","content":"hi"}],"thinking":{"type":"disabled"}}"#
            )
            .to_string(),
            // thinking 显式 null
            concat!(
                r#"{"model":"claude-opus-5","max_tokens":16,"#,
                r#""messages":[{"role":"user","content":"hi"}],"thinking":null}"#
            )
            .to_string(),
        ] {
            let b = Bytes::from(body.clone());
            let sim = sim_for(&body);
            let out = rewrite_body(&b, &test_cred(), "fp", all_on(), Some(&sim), None);
            let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
            assert!(v.get("context_management").is_none(), "没开 thinking 却补了: {body}");
        }
    }

    /// 客户端自己带了 `context_management` 就一个字节都不动——那是它自己的编辑策略。
    /// CC 形态的来访（非模拟路径）则根本不补。
    #[test]
    fn context_management_respects_client_and_skips_non_simulated() {
        let mine = concat!(
            r#"{"model":"claude-opus-5","max_tokens":1024,"#,
            r#""messages":[{"role":"user","content":"hi"}],"#,
            r#""context_management":{"edits":[]},"stream":true}"#
        );
        let body = Bytes::from(mine.to_string());
        let sim = sim_for(mine);
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(
            v["context_management"]["edits"].as_array().unwrap().len(),
            0,
            "客户端的被改写了"
        );

        // 非模拟路径不补：那条路是尽量原样透传，来访本来就是 CC 形态、自己会带。
        let cc = Bytes::from(API_SHAPE_BODY);
        let out = rewrite_body(&cc, &test_cred(), "fp", all_on(), None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert!(v.get("context_management").is_none(), "非模拟路径不该补: {v}");
    }

    /// 模拟路径补 `metadata.user_id`：键序与 CC 一致，session_id 与请求头同值且逐设备稳定；
    /// 客户端自己带了 user_id 就不新造（交给 spoof_identity 原格式改写）。
    #[test]
    fn injects_cc_metadata_only_when_absent() {
        let body = Bytes::from(PLAIN_BODY.to_string());
        let sim = sim_for(PLAIN_BODY);
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let user_id = v["metadata"]["user_id"].as_str().unwrap();
        let inner: serde_json::Value = serde_json::from_str(user_id).unwrap();

        assert_eq!(
            inner.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["device_id", "account_uuid", "session_id"],
            "键序应与 CC 一致: {user_id}"
        );
        assert_eq!(inner["account_uuid"], ACCOUNT_UUID);
        assert_eq!(inner["device_id"], test_cred().spoof_device_id("fp").unwrap());
        assert_eq!(inner["session_id"], sim.session_id, "两处 session_id 必须同值");
        assert_eq!(sim.session_id, sim_for(PLAIN_BODY).session_id, "同设备同账号应恒定");

        // 客户端自己带了 user_id 但 UA 不是 claude-cli → 走模拟，原有 user_id 被剥掉，
        // ensure_cc_metadata 用 sim.session_id 重建，确保头体自洽。
        let with_meta = Bytes::from(
            r#"{"model":"claude-opus-5","messages":[],"metadata":{"user_id":"user_aa_account_bb_session_cc"}}"#
                .to_string(),
        );
        assert!(detect_for(&with_meta, all_on()).is_some(), "非 CC UA 带 user_id 也走模拟");
        let sim2 = detect_for(&with_meta, all_on()).unwrap();
        let out2 = rewrite_body(&with_meta, &test_cred(), "fp", all_on(), Some(&sim2), None);
        let v2: serde_json::Value = serde_json::from_slice(&out2).unwrap();
        let uid2 = v2["metadata"]["user_id"].as_str().unwrap();
        let inner2: serde_json::Value = serde_json::from_str(uid2).unwrap();
        assert_eq!(
            inner2["session_id"].as_str().unwrap(),
            sim2.session_id,
            "重建后 session_id 应与 sim 一致"
        );

        // 反面：真 CC 客户端（UA 带 claude-cli/ **且** system 是 CC 形态）带了 user_id →
        // 不走模拟，spoof_identity 原格式改写。只有 UA 没有形态的那种见
        // [`cc_client_needs_both_ua_and_cc_shape`]。
        // 扁平串里 device 段与 session 段都得是官方格式（64 位 hex / uuid），否则 detect 会把它
        // 当成身份写错的非官方客户端送去模拟，测不到透传那条路。
        const FLAT_DEV: &str = "832cb7e697190bc475b926c7994ef183a0f8a58e29818f182e11f924e1ea2870";
        const FLAT_SESS: &str = "d0c1fb05-9b19-4576-9465-e2b8a206dabf";
        let with_meta = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","system":[{{"type":"text","text":"{}"}},{}],"messages":[],"metadata":{{"user_id":"user_{FLAT_DEV}_account_bb_session_{FLAT_SESS}"}}}}"#,
            config::CC_SYSTEM_IDENTITY,
            base_block()
        ));
        let mut cc_ua = crate::proxy::HeaderMap::new();
        cc_ua.insert(
            header::USER_AGENT,
            HeaderValue::from_static("claude-cli/2.1.226 (external, cli)"),
        );
        assert!(detect_with(&with_meta, &cc_ua, all_on()).is_none(), "真 CC 客户端不走模拟");
        let out3 = rewrite_body(&with_meta, &test_cred(), "fp", all_on(), None, None);
        let v3: serde_json::Value = serde_json::from_slice(&out3).unwrap();
        assert_eq!(
            v3["metadata"]["user_id"],
            format!(
                "user_{}_account_{ACCOUNT_UUID}_session_{FLAT_SESS}",
                test_cred().spoof_device_id("fp").unwrap()
            ),
            "扁平串形态应原格式改写，而不是被换成 CC 的 JSON 形态"
        );
    }

    /// 出站 UA 进设备指纹：**同一台设备只会有一个客户端版本**，换版本就是换设备。
    ///
    /// 复盘依据见 [`crate::proxy::device_fingerprint`]：归一化把同平台的客户端收敛成一个 device_id，
    /// 各自的 UA 却原样透传，上游看到的是「同一台设备同一秒里既是 2.1.141 的 sdk-cli
    /// 又是 2.1.263 的 VSCode 扩展」——官方客户端不可能产生的形态。
    #[test]
    fn device_fingerprint_separates_client_versions() {
        const UA_OLD: &str = "claude-cli/2.1.141 (external, sdk-cli)";
        const UA_NEW: &str = "claude-cli/2.1.263 (external, claude-vscode, agent-sdk/0.3.263)";
        let h = platform_headers(None);

        let old = crate::proxy::device_fingerprint(None, &h, UA_OLD);
        let new = crate::proxy::device_fingerprint(None, &h, UA_NEW);
        assert_ne!(old, new, "同平台、不同客户端版本必须落在两台设备上");
        assert_ne!(
            test_cred().spoof_device_id(&old),
            test_cred().spoof_device_id(&new),
            "指纹不同，派生出的伪装 device_id 也必须不同"
        );
        // 同一版本恒定：升级才换设备，逐请求不会漂。
        assert_eq!(old, crate::proxy::device_fingerprint(None, &h, UA_OLD));

        // 归一化开着（`client_device_id = None`）时，同平台 + 同版本的多个客户端仍收敛成
        // 一台设备——这是本次改动**没有**动的那一半。
        assert_eq!(
            crate::proxy::device_fingerprint(None, &h, UA_OLD),
            crate::proxy::device_fingerprint(None, &platform_headers(None), UA_OLD),
        );
        // 归一化关着时照旧按客户端设备分开。
        assert_ne!(
            crate::proxy::device_fingerprint(Some("dev-a"), &h, UA_OLD),
            crate::proxy::device_fingerprint(Some("dev-b"), &h, UA_OLD),
        );
        // 平台仍然参与：同版本、不同系统不是一台设备。
        let mut win = crate::proxy::HeaderMap::new();
        win.insert("x-stainless-arch", HeaderValue::from_static("x64"));
        win.insert("x-stainless-os", HeaderValue::from_static("Windows"));
        assert_ne!(old, crate::proxy::device_fingerprint(None, &win, UA_OLD));
    }

    /// 指纹里那段 UA 取的是**出站**那份：模拟路径整套换头（UA 恒为
    /// [`config::CC_USER_AGENT`]），所以被模拟的第三方客户端落在「官方客户端」那台设备上，
    /// 而不是各自自报的 UA 上——否则指纹与真正发出去的版本又对不上了。
    #[test]
    fn device_fingerprint_follows_the_outbound_ua() {
        assert_eq!(crate::proxy::outbound_ua("python-httpx/0.27.0", true), config::CC_USER_AGENT);
        assert_eq!(crate::proxy::outbound_ua("Go-http-client/2.0", true), config::CC_USER_AGENT);
        assert_eq!(
            crate::proxy::outbound_ua("claude-cli/2.1.141 (external, sdk-cli)", false),
            "claude-cli/2.1.141 (external, sdk-cli)",
            "非模拟路径原样转发来访那份，指纹也跟着它"
        );

        let h = platform_headers(None);
        assert_eq!(
            crate::proxy::device_fingerprint(
                None,
                &h,
                crate::proxy::outbound_ua("python-httpx/0.27.0", true)
            ),
            crate::proxy::device_fingerprint(
                None,
                &h,
                crate::proxy::outbound_ua("Go-http-client/2.0", true)
            ),
            "两个第三方 UA 都被重塑成同一个官方客户端，就是同一台设备"
        );
    }

    /// `spoof_device_id` 关掉时只换 account 段，来访自带的 `device_id` 原样保留。
    ///
    /// **判据取自真实抓包对**：`cap/raw/00002`（API-key 模式经 luban）与 `00006`（订阅模式
    /// 直连）是同机、同客户端、同模型、相隔 28 秒的两条请求，两者的 `device_id` **完全相同**
    /// （`832cb7e6…`），只有 `account_uuid` 不同（空串 ↔ 真 uuid）。故「补 account、留 device」
    /// 正是官方两种模式之间真实存在的那一处差别，见 [`store::ForwardFlags::spoof_device_id`]。
    #[test]
    fn keeps_client_device_id_when_spoof_device_off() {
        const CLIENT_DEVICE: &str =
            "832cb7e697190bc475b926c7994ef183a0f8a58e29818f182e11f924e1ea2870";
        let off = store::ForwardFlags { spoof_device_id: false, ..all_on() };

        // 格式一：CC 内嵌 JSON（键序与官方一致）。
        let body = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"metadata":{{"user_id":"{{\"device_id\":\"{CLIENT_DEVICE}\",\"account_uuid\":\"\",\"session_id\":\"ssss\"}}"}}}}"#
        ));
        let out = rewrite_body(&body, &test_cred(), "fp", off, None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let inner: serde_json::Value =
            serde_json::from_str(v["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(inner["device_id"], CLIENT_DEVICE, "关掉后 device_id 该原样保留");
        assert_eq!(inner["account_uuid"], ACCOUNT_UUID, "account 段照样要补——那才是两模式的差别");
        assert_eq!(inner["session_id"], "ssss", "session 段一如既往不动");

        // 开着时（默认）仍换成派生值：本开关不改变既有行为。
        let on = rewrite_body(&body, &test_cred(), "fp", all_on(), None, None);
        let v_on: serde_json::Value = serde_json::from_slice(&on).unwrap();
        let inner_on: serde_json::Value =
            serde_json::from_str(v_on["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(inner_on["device_id"], test_cred().spoof_device_id("fp").unwrap());

        // 格式二：扁平串——device 段同样保留，仍以扁平串回写。
        let flat = Bytes::from(
            r#"{"model":"claude-opus-5","messages":[],"metadata":{"user_id":"user_aa_account_bb_session_cc"}}"#
                .to_string(),
        );
        let out = rewrite_body(&flat, &test_cred(), "fp", off, None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["metadata"]["user_id"], format!("user_aa_account_{ACCOUNT_UUID}_session_cc"));

        // 模拟路径不受本开关影响：那条路来访压根没有 device_id，只能派生——否则产出的是
        // 一份没有 device_id 的 metadata，官方从不发那种形态。
        let bare = Bytes::from(PLAIN_BODY.to_string());
        let sim = detect_for(&bare, off).expect("裸请求仍应走模拟");
        let out = rewrite_body(&bare, &test_cred(), "fp", off, Some(&sim), None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let inner: serde_json::Value =
            serde_json::from_str(v["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(
            inner["device_id"],
            test_cred().spoof_device_id("fp").unwrap(),
            "模拟路径必须派生，不受开关影响"
        );
    }

    /// CC 形态但不带 `metadata.user_id` 的来访（第三方 CC 兼容客户端）：照样补一份官方身份，
    /// **且头体两处的 session_id 逐字节相同**。
    ///
    /// 判据逐条取自 `cap/raw/00006`（claude-cli/2.1.220 直连，opus-5）的原始报文：
    /// ```text
    /// "metadata":{"user_id":"{\"device_id\":\"832cb7…2870\",\"account_uuid\":\"edded6bb-…\",\"session_id\":\"bc201916-d0bc-4b4e-adba-caf41fb58746\"}"}
    /// X-Claude-Code-Session-Id: bc201916-d0bc-4b4e-adba-caf41fb58746
    /// ```
    /// 即：内层是紧凑 JSON 字符串、键序 device_id→account_uuid→session_id、device_id 是
    /// 64 位小写 hex、session_id 是 uuid 且与那个头**同值**。`00009`（sonnet-5）同形。
    /// 老版本 API-key 模式的 CC：system 是 `[身份句(带断点), 基座, 其余…]`，有身份句、没 billing
    /// header（现网 `claude-cli/2.1.238`，req_ujomarOOPtXL38jx 的形态）。补前缀只该插一块
    /// billing header，客户端的身份句连同它的 `cache_control` 留在第二块——原先会再插一句
    /// 身份句，出站 `[billing, 身份, 身份(带断点), …]`。
    #[test]
    fn prefix_injection_keeps_the_client_identity_block() {
        let body = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"{}","cache_control":{{"type":"ephemeral"}}}},{},{{"type":"text","text":"tail"}}]}}"#,
            config::CC_SYSTEM_IDENTITY,
            base_block()
        ));
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();
        assert!(
            sys[0]["text"].as_str().unwrap().starts_with("x-anthropic-billing-header:"),
            "{sys:?}"
        );
        assert_eq!(sys[1]["text"], config::CC_SYSTEM_IDENTITY);
        assert_eq!(sys[1]["cache_control"]["type"], "ephemeral", "客户端身份句的断点原样保留");
        let identities = sys
            .iter()
            .filter(|b| {
                b["text"].as_str().is_some_and(|t| t.contains(config::CC_SYSTEM_IDENTITY_PREFIX))
            })
            .count();
        assert_eq!(identities, 1, "身份句只能有一句: {sys:?}");
        assert_eq!(sys.len(), 4, "原 3 块 + billing header");

        // 字符串形态的 system 同理：已含身份句就只在前面加 billing header。
        let body = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"system":"{}\n\nrest"}}"#,
            config::CC_SYSTEM_IDENTITY
        ));
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();
        assert_eq!(sys.len(), 2, "{sys:?}");
        assert!(sys[0]["text"].as_str().unwrap().starts_with("x-anthropic-billing-header:"));
        assert!(sys[1]["text"].as_str().unwrap().starts_with(config::CC_SYSTEM_IDENTITY));
    }

    /// 封顶必须排在补前缀之后：客户端 5 块、没 billing header 的来访，补前缀后是 6 块，超过
    /// [`MAX_SYSTEM_BLOCKS`]。原先封顶在前、补前缀在后，这 6 块就原样出站，上游
    /// 按第三方应用计费——现网 2.1.238 那条出站是 7 块。
    #[test]
    fn system_block_cap_runs_after_prefix_injection() {
        let body = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"{}","cache_control":{{"type":"ephemeral"}}}},{},{{"type":"text","text":"env"}},{{"type":"text","text":"memory"}},{{"type":"text","text":"tail"}}]}}"#,
            config::CC_SYSTEM_IDENTITY,
            base_block()
        ));
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), None, None);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let sys = v["system"].as_array().unwrap();
        assert!(
            sys.len() <= crate::proxy::simulation::MAX_SYSTEM_BLOCKS,
            "封顶没管住补前缀后的块数: {}",
            sys.len()
        );
        assert!(sys[0]["text"].as_str().unwrap().starts_with("x-anthropic-billing-header:"));
        assert_eq!(sys[1]["text"], config::CC_SYSTEM_IDENTITY);
        let all: String = sys.iter().filter_map(|b| b["text"].as_str()).collect();
        assert!(all.contains("memory") && all.contains("tail"), "并块不能丢内容: {sys:?}");
    }

    #[test]
    fn cc_shaped_without_metadata_gets_aligned_identity() {
        // CC 形态 + 真 CC 客户端：system 里有那句身份声明且 UA 是 claude-cli，
        // detect 返回 None（不模拟），走 bare_session 路径补 metadata。
        let body = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"{}"}},{}]}}"#,
            config::CC_SYSTEM_IDENTITY,
            base_block()
        ));
        let mut cc_ua = crate::proxy::HeaderMap::new();
        cc_ua.insert(
            header::USER_AGENT,
            HeaderValue::from_static("claude-cli/2.1.226 (external, cli)"),
        );
        assert!(detect_with(&body, &cc_ua, all_on()).is_none(), "真 CC 客户端不该走模拟");
        assert!(
            !crate::proxy::body_has_user_id(parsed(&body).as_ref()),
            "这条来访本来就没有 metadata.user_id"
        );

        // 来访没带会话 id 头 → 派生一个，头体同步补。
        let client = crate::proxy::HeaderMap::new();
        let sid = crate::proxy::bare_session_id(
            &client,
            all_on(),
            None,
            true,
            crate::proxy::body_has_user_id(parsed(&body).as_ref()),
            &test_cred(),
            "fp",
        )
        .expect("CC 形态 + 无 metadata 应补身份");
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), None, Some(sid.as_str()));
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        let user_id = v["metadata"]["user_id"].as_str().expect("应补出 metadata.user_id");
        let inner: serde_json::Value = serde_json::from_str(user_id).unwrap();

        assert_eq!(
            inner.as_object().unwrap().keys().map(String::as_str).collect::<Vec<_>>(),
            vec!["device_id", "account_uuid", "session_id"],
            "键序应与 00006 一致: {user_id}"
        );
        assert!(!user_id.contains(": "), "内层须是紧凑 JSON（无空白），同 00006");
        let device_id = inner["device_id"].as_str().unwrap();
        assert_eq!(device_id.len(), 64, "device_id 同 00006 是 64 位 hex");
        assert!(
            device_id.chars().all(|c| c.is_ascii_digit() || ('a'..='f').contains(&c)),
            "device_id 须是小写 hex: {device_id}"
        );
        let session_id = inner["session_id"].as_str().unwrap();
        let seg: Vec<usize> = session_id.split('-').map(str::len).collect();
        assert_eq!(seg, vec![8, 4, 4, 4, 12], "session_id 同 00006 是 uuid 形态: {session_id}");

        // 头体同值——00006 里这两处逐字节相同，这正是本条路径最容易做错的地方。
        let headers =
            build_forward_headers(&client, "sk-ant-oat01-REAL", all_on(), None, Some(&sid));
        assert_eq!(
            headers.get("x-claude-code-session-id").unwrap().to_str().unwrap(),
            session_id,
            "头与 metadata 里的 session_id 必须逐字节相同"
        );

        // 来访自己带了那个头 → 沿用它（按账号钉住，[`crate::proxy::account_session_id`]），不按设备
        // 另派生；头体落的是同一个值。
        const CLIENT_SID: &str = "bc201916-d0bc-4b4e-adba-caf41fb58746";
        let mut with_sid = crate::proxy::HeaderMap::new();
        with_sid.insert(
            crate::proxy::HeaderName::from_static("x-claude-code-session-id"),
            HeaderValue::from_static(CLIENT_SID),
        );
        let sid2 = crate::proxy::bare_session_id(
            &with_sid,
            all_on(),
            None,
            true,
            false,
            &test_cred(),
            "fp",
        )
        .unwrap();
        assert_eq!(
            sid2,
            crate::proxy::account_session_id(&test_cred(), CLIENT_SID).unwrap(),
            "应沿用来访自己的会话 id（按账号钉住）"
        );
        assert_ne!(sid2, sid, "来访带了会话 id 就不按设备派生");
        let out2 = rewrite_body(&body, &test_cred(), "fp", all_on(), None, Some(sid2.as_str()));
        let v2: serde_json::Value = serde_json::from_slice(&out2).unwrap();
        let inner2: serde_json::Value =
            serde_json::from_str(v2["metadata"]["user_id"].as_str().unwrap()).unwrap();
        assert_eq!(inner2["session_id"], sid2, "体里要用同一个值");
        let headers2 = build_forward_headers(&with_sid, "tok", all_on(), None, Some(&sid2));
        assert_eq!(
            headers2.get("x-claude-code-session-id").unwrap().to_str().unwrap(),
            sid2,
            "头上那个来访原值要被钉住后的值顶掉，与体同值"
        );
    }

    /// 补出来的 metadata 必须与官方报文**逐字节同形**（只有取值不同）。
    ///
    /// 金标准逐字取自 `cap/raw/00006_101505.964.req.raw`（claude-cli/2.1.220 直连 opus-5）
    /// 的请求体原文，位置在 `tools` 之后、`max_tokens` 之前：
    /// ```text
    /// …,"metadata":{"user_id":"{\"device_id\":\"832cb7e6…2870\",\"account_uuid\":\"edded6bb-2521-4a68-94cb-241bb4d96bb9\",\"session_id\":\"bc201916-d0bc-4b4e-adba-caf41fb58746\"}"},"max_tokens":64000,…
    /// ```
    /// 抓包不入库（`cap/` 未跟踪），故把这串固化在这里——与基座字节数、beta 串同一做法。
    /// 逐字节比对是为了钉住**转义写法**：内层是「字符串里的 JSON」，序列化器只要把
    /// `\"` 写成别的形式（或插进任何空白），出去的就不是官方那串了。
    #[test]
    fn injected_metadata_matches_raw_capture_bytes() {
        let body = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","messages":[],"system":[{{"type":"text","text":"{}"}}],"max_tokens":64000}}"#,
            config::CC_SYSTEM_IDENTITY
        ));
        let sid = "bc201916-d0bc-4b4e-adba-caf41fb58746";
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), None, Some(sid));
        let text = String::from_utf8(out.to_vec()).unwrap();

        let expected = format!(
            r#""metadata":{{"user_id":"{{\"device_id\":\"{}\",\"account_uuid\":\"{ACCOUNT_UUID}\",\"session_id\":\"{sid}\"}}"}}"#,
            test_cred().spoof_device_id("fp").unwrap()
        );
        assert!(text.contains(&expected), "与 00006 的 metadata 形态不符\n实际: {text}");
        // 位置也照抓包：metadata 在 max_tokens 之前。
        assert!(
            text.find(r#""metadata""#) < text.find(r#""max_tokens""#),
            "metadata 应落在 max_tokens 之前（同 00006 的 key 序）: {text}"
        );
    }

    /// 裸客户端的日志设备标识：只在真伪装过时才有值，且带 `sim:` 前缀以免被当成真实设备。
    #[test]
    fn logs_simulated_device_only_when_spoofed() {
        let sim = sim_for(PLAIN_BODY);
        let expect = format!("sim:{}", test_cred().spoof_device_id("fp").unwrap());
        let id =
            crate::proxy::sim_device_id(Some(&sim), None, all_on(), &test_cred(), "fp").unwrap();
        assert_eq!(id, expect);

        // CC 形态补身份那条路（sim 为 None、bare_session 有值）同样把这个 id 发了出去，
        // 日志要记它——否则这段流量在库里只留下 `-`，无从聚合。
        let bare =
            crate::proxy::sim_device_id(None, Some("sess"), all_on(), &test_cred(), "fp").unwrap();
        assert_eq!(bare, expect, "两条补身份的路径记的是同一个 id");

        // 两条路都没走（来访是 CC 形态且自带 metadata）→ 出站体里根本没有这个 id，不该记。
        assert!(crate::proxy::sim_device_id(None, None, all_on(), &test_cred(), "fp").is_none());
        // spoof_identity 关着时同理：ensure_cc_metadata 不会写 metadata。
        let no_spoof = store::ForwardFlags { spoof_identity: false, ..all_on() };
        assert!(
            crate::proxy::sim_device_id(Some(&sim), None, no_spoof, &test_cred(), "fp").is_none()
        );
        // 凭证没有 account_uuid 就派生不出来，退回 `-`。
        let no_uuid = crate::credentials::Credential { account_uuid: None, ..test_cred() };
        assert!(crate::proxy::sim_device_id(Some(&sim), None, all_on(), &no_uuid, "fp").is_none());
    }

    /// 日志用的 UA 取值：缺失/空串取 `-`，过长按 char 截断（不能按字节切，会劈开多字节 UTF-8）。
    #[test]
    fn client_ua_falls_back_and_truncates() {
        let ua = |v: Option<&str>| {
            let mut h = crate::proxy::HeaderMap::new();
            if let Some(v) = v {
                h.insert(crate::proxy::header::USER_AGENT, HeaderValue::from_str(v).unwrap());
            }
            crate::proxy::ua_of(&h)
        };
        assert_eq!(ua(None), "-", "没有该头");
        assert_eq!(ua(Some("   ")), "-", "空白等于没带");
        assert_eq!(ua(Some(config::CC_USER_AGENT)), config::CC_USER_AGENT, "正常那串原样保留");
        let long = "a".repeat(300);
        assert_eq!(ua(Some(&long)).len(), 120, "超长的截到 120");
        // 非 ASCII 的头值 `to_str()` 直接失败，落回 `-`——所以库里存的 UA 恒为可见 ASCII。
        let cjk = crate::proxy::HeaderValue::from_bytes("中文客户端".as_bytes()).unwrap();
        let mut h = crate::proxy::HeaderMap::new();
        h.insert(crate::proxy::header::USER_AGENT, cjk);
        assert_eq!(crate::proxy::ua_of(&h), "-");
    }

    /// 版本串解析：段数不齐按 0 补齐，预发布后缀按主版本算，非数字段作废。
    #[test]
    fn parses_version_strings() {
        let v = crate::proxy::parse_version;
        assert_eq!(v("2.1.220"), Some((2, 1, 220)));
        assert_eq!(v("2.1"), Some((2, 1, 0)), "缺的段补 0");
        assert_eq!(v("2"), Some((2, 0, 0)));
        assert_eq!(v(" 2.1.220 "), Some((2, 1, 220)), "首尾空白不算数");
        assert_eq!(v("2.1.220-beta.1"), Some((2, 1, 220)), "预发布按主版本算，不判成更旧");
        assert_eq!(v("1.2.3.4"), Some((1, 2, 3)), "第四段忽略");
        assert_eq!(v(""), None);
        assert_eq!(v("v2.1.220"), None, "带前缀的不猜，交给调用方按「读不出」放行");
        assert_eq!(v("2.x.1"), None, "写了但不是数字的段整串作废");
        // 数值比较，不是字典序：字符串比的话 "2.1.9" 会大于 "2.1.220"。
        assert!(v("2.1.9") < v("2.1.220"));
    }

    /// UA 里的 CC 版本：认 `claude-cli/<版本>`，后面跟什么都不影响；别的客户端读不出版本。
    #[test]
    fn reads_the_cc_version_from_the_user_agent() {
        let v = crate::proxy::cc_cli_version;
        assert_eq!(v(config::CC_USER_AGENT), Some((2, 1, 260)), "官方那串");
        assert_eq!(v("claude-cli/2.1.251"), Some((2, 1, 251)), "光秃秃一串也认");
        assert_eq!(v("claude-cli/1.0 (external, cli)"), Some((1, 0, 0)));
        assert_eq!(v("python-httpx/0.27.0"), None, "非 CC 客户端没有版本可比");
        assert_eq!(v("claude-cli/"), None, "有前缀没版本");
        assert_eq!(v("claude-cli/next (external, cli)"), None, "版本位不是数字");
    }

    /// 自报版本高于官方最新发布版的 UA 不算官方客户端；等于或更低的照认。
    #[test]
    fn a_cc_version_newer_than_the_latest_release_is_not_trusted() {
        let t = |ua: &str| crate::proxy::trusted_cc_version_against(ua, (2, 1, 260));
        assert_eq!(t("claude-cli/2.1.260 (external, cli)"), Some((2, 1, 260)), "正好最新版");
        assert_eq!(t("claude-cli/2.1.226 (external, cli)"), Some((2, 1, 226)), "旧版照认");
        assert_eq!(t("claude-cli/2.5.0 (external, cli)"), None, "官方没发过 2.5.0");
        assert_eq!(t("claude-cli/2.1.261 (external, cli)"), None, "哪怕只高一个补丁号");
        assert_eq!(t("claude-cli/3.0.0 (external, cli)"), None);
        assert_eq!(t("python-httpx/0.27.0"), None, "非 CC 客户端本来就读不出");
    }

    /// 没学到 `latest` 时上限退回 [`config::CC_LATEST_KNOWN_RELEASE`]，且学到的值不会把上限
    /// 拉到它之下。模拟版本 [`config::CC_VERSION_BASE`] 更旧，自然也在上限之内。
    #[test]
    fn known_latest_release_is_at_least_the_baked_in_version() {
        let v = |s: &str| crate::proxy::parse_version(s).unwrap();
        let latest = crate::proxy::known_latest_release();
        assert!(latest >= v(config::CC_LATEST_KNOWN_RELEASE));
        assert!(latest >= v(config::CC_VERSION_BASE));
        // 抓包证实的 2.1.270 来访：没学到 `latest` 也得认，不能落成「读不出版本」。
        assert_eq!(
            crate::proxy::trusted_cc_version("claude-cli/2.1.270 (external, cli)"),
            Some((2, 1, 270))
        );
    }

    /// 最低版本闸的三态：低于门槛才拒，等于/高于放行；闸没配、UA 不是 CC、版本读不出来
    /// 全都放行——这道闸只用来逼旧版 CC 升级，不该把别的客户端一起挡在门外。
    #[test]
    fn rejects_only_cc_clients_below_the_minimum() {
        let gate = |ua: &str, min: Option<&str>| crate::proxy::below_min_client_version(ua, min);
        let old = "claude-cli/2.0.30 (external, cli)";

        let (got, want) = gate(old, Some("2.1.220")).expect("旧版该被拦下");
        assert_eq!(got, "2.0.30", "拦下时要报出自报版本，日志与提示都靠它");
        assert_eq!(want, "2.1.220", "提示里给的是配置原样，不是解析后的三元组");

        assert!(gate(config::CC_USER_AGENT, Some("2.1.220")).is_none(), "正好等于门槛要放行");
        assert!(gate("claude-cli/3.0.0 (external, cli)", Some("2.1.220")).is_none(), "更新的放行");
        assert!(gate(old, Some("2.1")).is_some(), "门槛写两段即 2.1.0，2.0.30 更旧——照拦");
        assert!(gate(old, None).is_none(), "闸没配");
        assert!(gate(old, Some("   ")).is_none(), "空串等于没配");
        assert!(gate(old, Some("最新版")).is_none(), "门槛不是版本号 → 当没配，不能全拒");
        assert!(gate("python-httpx/0.27.0", Some("2.1.220")).is_none(), "非 CC 客户端不受这道闸管");
        assert!(gate("-", Some("2.1.220")).is_none(), "没带 UA 的（ua_of 落 `-`）照旧放行");
    }

    /// 上一条里「2.1 门槛拦下 2.0.30」的反面：同一个门槛不能把 2.1.0 之后的版本也拦了。
    #[test]
    fn a_two_segment_minimum_means_dot_zero() {
        assert!(crate::proxy::below_min_client_version("claude-cli/2.1.0", Some("2.1")).is_none());
        assert!(
            crate::proxy::below_min_client_version("claude-cli/2.1.220", Some("2.1")).is_none()
        );
        assert!(
            crate::proxy::below_min_client_version("claude-cli/2.0.999", Some("2.1")).is_some()
        );
    }

    /// 出站 URL 上那个 `?beta=true`：官方 `cap/raw` 八份抓包的请求行全带，Anthropic 公开 API
    /// 里却没有这个参数——它是 CC 客户端自己的标记，故只在模拟路径上补。
    #[test]
    fn appends_official_beta_query() {
        let base = "https://api.anthropic.com/v1/messages";
        assert_eq!(
            crate::proxy::ensure_beta_query(base),
            format!("{base}?beta=true"),
            "没有查询串就加 ?"
        );
        assert_eq!(
            crate::proxy::ensure_beta_query(&format!("{base}?foo=1")),
            format!("{base}?foo=1&beta=true"),
            "已有查询串就接 &"
        );

        // 客户端自己写了 beta= 的一律不动——包括它显式关掉的情形。
        for already in ["?beta=true", "?beta=false", "?foo=1&beta=true", "?beta=true&foo=1"] {
            let url = format!("{base}{already}");
            assert_eq!(crate::proxy::ensure_beta_query(&url), url, "客户端自己的 beta= 被改写了");
        }
        // `betas=`/`xbeta=` 不是 `beta=`，不该被当成已有。
        assert!(
            crate::proxy::ensure_beta_query(&format!("{base}?betas=1")).ends_with("&beta=true")
        );
        assert!(
            crate::proxy::ensure_beta_query(&format!("{base}?xbeta=1")).ends_with("&beta=true")
        );
    }

    // ---------- 非流式改流式 + SSE 聚合 ----------

    /// `stream` 的判定口径：只有布尔 `true` 算流式。字符串 `"true"`、数字、缺失都不是——
    /// 上游那边它们同样回整段 JSON，判据跟着响应形态走才不会错配。
    #[test]
    fn stream_requested_only_counts_boolean_true() {
        let case =
            |body: &str| crate::proxy::stream_requested(&serde_json::from_str(body).unwrap());
        assert!(case(r#"{"stream":true}"#));
        assert!(!case(r#"{"stream":false}"#));
        assert!(!case(r#"{"model":"claude-opus-5"}"#), "字段缺失 = 非流式");
        assert!(!case(r#"{"stream":"true"}"#), "字符串不算");
        assert!(!case(r#"{"stream":1}"#), "数字不算");
    }

    /// 流式化把 `stream` 置成 `true`，且落在官方 key 序该在的位置（队尾）：
    /// 来访带了就原位改值，没带就追加——两条路都与官方线序一致。
    #[test]
    fn forces_stream_true_and_keeps_key_order() {
        let keys = |bytes: &Bytes| {
            let v: serde_json::Value = serde_json::from_slice(bytes).unwrap();
            v.as_object().unwrap().keys().cloned().collect::<Vec<_>>()
        };
        // 只开流式化这一项，确保观察到的差异只来自它。
        let only_stream = store::ForwardFlags {
            simulate_cc: false,
            spoof_identity: false,
            system_shape: false,
            billing_cch: false,
            ..all_on()
        };
        let call = |body: &str| {
            crate::proxy::rewrite_body(
                &Bytes::from(body.to_string()),
                &test_cred(),
                "fp",
                only_stream,
                None,
                None,
                None,
                true,
                None,
                true,
                true,
                None,
                None,
                crate::proxy::CcRequestKind::Main,
                None,
            )
        };

        // 1) 来访压根没带 `stream`：追加到末尾（官方线序里它就是最后一个）。
        let out = call(r#"{"model":"claude-opus-5","messages":[],"max_tokens":64}"#);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["stream"], serde_json::json!(true));
        assert_eq!(keys(&out), vec!["model", "messages", "max_tokens", "stream"]);

        // 2) 来访带了 `stream:false`：原位改值，位置不动。
        let out = call(r#"{"model":"claude-opus-5","stream":false,"max_tokens":64}"#);
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["stream"], serde_json::json!(true));
        assert_eq!(keys(&out), vec!["model", "stream", "max_tokens"], "已有字段不该挪位置");

        // 3) 开关关着：一个字节都不动（哪怕 body 是非流式的）。
        let untouched = crate::proxy::rewrite_body(
            &Bytes::from(r#"{"model":"claude-opus-5","stream":false}"#.to_string()),
            &test_cred(),
            "fp",
            only_stream,
            None,
            None,
            None,
            false,
            None,
            true,
            true,
            None,
            None,
            crate::proxy::CcRequestKind::Main,
            None,
        );
        assert_eq!(
            untouched,
            Bytes::from(r#"{"model":"claude-opus-5","stream":false}"#.to_string())
        );
    }
}
