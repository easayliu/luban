use axum::body::Bytes;
use axum::http::{HeaderMap, header};
use futures_util::StreamExt;
use rand::RngExt;

use crate::config;
use crate::store;

use super::ban::parse_upstream_error;
use super::learned_rules::{DeprecatedFieldMemory, LEARNED_KIND_DEPRECATED, SHAPE_MEMORY_CAP};
use super::session_link::{
    CachePrefix, CcRequestKind, CcSessionKey, CcSessionLink, REPLY_TEXT_FP_INIT, ReplyFp,
    ThreadDecision, ThreadMsg, cache_prefix_stable, reply_text_fp, thread_decision,
};
use super::simulation::{
    MAX_CACHE_BREAKPOINTS, Simulation, billing_header_text, cap_system_blocks, cc_profile_for,
    cc_profile_kind_for, is_cc_shaped, relocate_long_client_system, simulate_system,
};
use super::thinking::{preserve_thinking_encoding, strip_empty_thinking_blocks};
use super::{CacheSlot, cache_slots, count_cache_control, ensure_cc_metadata, insert_top_level};

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

/// 体里有没有 `ttl:"1h"` 的缓存断点：只看 API 认 `cache_control` 的那几处——顶层、`system[]`、
/// `tools[]`、`messages[].content[]` 各块，以及 `tool_result.content[]` 里的块。按字节扫
/// `"ttl":"1h"` 会把工具入参（`tool_use.input.ttl`）或 schema 里的同名字段当成断点，平白补上
/// `extended-cache-ttl`，而出站体里一个 1h 断点都没有。
pub(crate) fn has_cache_ttl_1h(v: &serde_json::Value) -> bool {
    let is_1h = |x: &serde_json::Value| {
        x.get("cache_control").and_then(|c| c.get("ttl")).and_then(|t| t.as_str()) == Some("1h")
    };
    fn blocks(c: Option<&serde_json::Value>) -> impl Iterator<Item = &serde_json::Value> {
        c.and_then(|c| c.as_array()).into_iter().flatten()
    }
    is_1h(v)
        || blocks(v.get("system")).any(is_1h)
        || blocks(v.get("tools")).any(is_1h)
        || blocks(v.get("messages")).any(|m| {
            blocks(m.get("content")).any(|b| {
                is_1h(b)
                    || (b.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                        && blocks(b.get("content")).any(is_1h))
            })
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
///    取值另有两项独立策略：真实 CC 来访按 `cch_real_recompute`、模拟请求按 `cch_sim_compute`，
///    见 [`finalize_cch`]。
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
///
/// 第二项是**改写后的那份 `Value`**，`Some` 即「它与返回的出站字节同构」。留着它是给
/// [`crate::proxy::shape_summary_of`] 用的：取证的形态摘要要的正是出站体，此前它拿着字节
/// 又从头解析了一遍——那是整条请求里第二次解析同一份几 MB 的 JSON，实测 1.8MB 的会话要
/// 6.7ms，而摘要本身的走查加 sha256 只要 0.6ms。这里顺手交出去，那 6.7ms 整个消失。
///
/// `None` 有三种来源，共同点是**出站字节与入参 `body` 逐字节相同**（没改写、不是 JSON、
/// 或序列化失败原样退回）：调用方此时拿来访那份已解析好的 body 算摘要即可，同样不必重新
/// 解析，见 [`Upstream::shape_outbound`]。
// 参数多是有意的：这些全是「一次改写要知道的上下文」，打包成结构体只会多一层间接，
// 而调用点只有 `Upstream::shape` 一处。
#[allow(clippy::too_many_arguments)]
pub(super) fn rewrite_body_out(
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
) -> (Bytes, Option<serde_json::Value>) {
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
        && !real_cch_stale(body, sim, flags)
    {
        return (body.clone(), None);
    }
    // 补 metadata 用的 session_id：模拟模式取 Simulation 那份，CC 形态来访取 `bare_session`
    // （见 [`Upstream::bare_session`]）。两者都与出站头上的 `X-Claude-Code-Session-Id` 同值。
    let meta_session = sim.map(|s| s.session_id.as_str()).or(bare_session);
    let mut v: serde_json::Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return (body.clone(), None),
    };
    // 空壳 `role:"system"` 消息：一个内容块都没有的那种，上游恒 400
    // （`messages.N: system content must contain at least one block`）。放在提升之前，
    // 两条路都要过它——见 [`drop_empty_system_messages`] 里为什么不受 `hoist_system_role`
    // 与 CC 形态那道豁免管。
    let empty_system_dropped = drop_empty_system_messages(&mut v);
    // role:"system" 提升：litellm 等第三方客户端把 system 放在 messages 里，
    // 上游对开头那段恒 400、老模型对中途的也 400，提前挪到顶层 system 字段。必须在
    // simulate_system 之前——后者和 align_system_shape 都只读顶层 system。
    // CC 形态的请求跳过：CC 在 messages 里合法使用 role:"system"（如 deferred tools），
    // 强行提升会破坏形态。严格检查开着时也跳过，见 [`hoists_system_role`]。
    let system_hoisted =
        hoists_system_role(&flags, is_cc_shaped(&v)) && hoist_system_role_messages(&mut v);
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
    // `output_config.effort`：2.1.277 起 opus / fable / sonnet 主线程恒带（模拟路径三族都按 `high`），
    // 按 profile 补（[`ensure_output_config`]）；haiku 与辅助 profile 官方不带，`effort` 为 `None`。
    let effort_filled = sim.is_some_and(|s| ensure_output_config(&mut v, s.profile));
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
    // messages 标了断点也是未命中，只会把裸算换成更贵的写入；会话第一轮同样不补。按
    // 请求类别分谱系：同一会话里主线程与辅助请求交替出现，不能互相当对方的「上一轮」。
    let cc_msg_shape = shape
        && sim.is_none()
        && cc_inbound
        && cc_kind.allows_system_prefix()
        && session_out.is_some_and(|sid| {
            cache_prefix_stable(
                CcSessionKey { cred_id: cred.id, session_id: sid },
                cc_kind,
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
        && ensure_cc_system_prefix(&mut v, client_version);
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
        // 主线程（含工具续轮与 thread 续轮）的 `cc_prompt_id` 后面还跟着 `cc_turn_origin`
        // （2.1.277 起）与会话里的第几轮（2.1.285 起，`cap/2.1.285/00113`、`00115`）；子代理与
        // helper 只有 `cc_prompt_id`（`00120`、`00125`）。按来访自报的版本给，版本读不出不补。
        let ver = client_version.and_then(parse_version);
        let main = cc_kind == CcRequestKind::Main;
        let turn = TurnFields {
            origin: main && ver.is_some_and(|v| v >= (2, 1, 277)),
            index: main && ver.is_some_and(|v| v >= (2, 1, 285)),
        };
        let billing = append_billing_link(&mut v, l, turn);
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
    // 模拟路径把官方主线程恒带的 14 个真工具（[`cc_tools_core`]）对齐进工具列表：客户端没
    // 声明的补上，声明了的同名工具换成官方那条，其余原样。上游判第三方的信号之一是
    // 「自称 CC 但没有 CC 工具」，光加 mcp__ 前缀不够——零个 CC 工具等于自证不是 CC；而只注
    // 四个也不是官方形态：2.1.258 / 2.1.260 / 2.1.270 的主线程抓包最少 13 个工具。注入的
    // 工具在白名单内，混淆不会动它们。
    //
    // **只给主线程 profile 注**：官方的标题生成、安全分类、无工具 helper 与额度探测本来就
    // 一个工具都不发（`tools: []` 或整个字段都没有），给它们塞 Bash 是把一条辅助请求装成
    // 了主线程。判据是 profile，不是「有没有 tools 字段」。
    let cc_tools_injected = sim.is_some_and(|s| {
        s.profile.has_billing_header()
            && inject_cc_tools(
                &mut v,
                s.profile,
                s.fill_absent_tools,
                ToolAlignWho { cred_id: cred.id, cred: &cred.label, session: &s.session_id },
            )
    });
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
    // message thread 排在所有改写之后：判「能不能接上上一轮」比的是**最终出站**的历史
    // （工具名已混淆、断点已落位），切增量也得切这一份。见 [`apply_sim_thread`]。
    let threaded =
        flags.sim_message_threads && sim.is_some_and(|s| apply_sim_thread(&mut v, s, cred.id));
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
        threaded,
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
        && !effort_filled
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
        && !threaded
        // 排在最后：前面任何一项为真都不会走到这里，只有「看似没改」的体才多算一次哈希。
        && !real_cch_stale(body, sim, flags)
    {
        return (body.clone(), None);
    }
    match serde_json::to_vec(&v) {
        Ok(bytes) => {
            // cch 排在最后：它对**最终出站字节**（含 `cch=00000;` 占位符）算 xxHash64。
            // 一切改写与 `preserve_thinking_encoding` 都落定之后再原地回填，才与官方出口层
            // 的时序一致（见 [`apply_cch`]）。
            let mut out = preserve_thinking_encoding(body, bytes);
            // 回填的值也写回 `v` 的 billing header：调用方拿 `Some(v)` 算形态摘要，
            // 这份 Value 必须与出站字节同构，否则 cch 那一块的 sha 会对不上。
            // 模拟请求与真实来访各走各的策略开关；`ours` 即 cch 是 luban 自己落的占位符。
            let compute =
                if sim.is_some() { flags.cch_sim_compute } else { flags.cch_real_recompute };
            if let Some(cch) = finalize_cch(&mut out, compute, sim.is_some() || cch_added) {
                sync_value_cch(&mut v, &cch);
            }
            (Bytes::from(out), Some(v))
        }
        // 序列化失败等于「这份 Value 与将要发出去的字节对不上」，此时必须把它丢掉：
        // 形态摘要的调用方认的是「Some 即与出站字节同构」，交一份对不上的回去比不交更糟。
        Err(_) => (body.clone(), None),
    }
}

/// 模拟路径的 message thread（`message-threads-2026-08-12`），规则见
/// [`super::session_link::ThreadState`]：
///
/// - 会话里这段对话的第一条，或历史接不上上一轮（客户端改了历史、重新生成、自己裁剪了上下文，
///   换了模型 / effort / system / tools，上一条失败或被取消）：`thread: {type: create}`，完整上下文。
///   `diagnostics` 照旧由 [`ensure_diagnostics`] 写会话上一条回复——与官方切模型后那条 `create`
///   同形（`cap/auto-2.1.285-20260930/00243`）。
/// - 接得上：`thread: {type: continue, previous_message_id}`，`messages` 只留新增的那几条，
///   `system` 只剩 billing header 一块（去掉断点，官方那块只有 `type` / `text`），去掉 `tools`，
///   `diagnostics.previous_message_id` 改成同一个 id（`00033`）。billing header 的 `cc_version`
///   后缀此前已按完整历史算好，与官方「续轮沿用首轮后缀」一致。
///
/// 只给官方会发 `thread` 的主线程：2.1.285 的 opus / sonnet / haiku 各代与 fable-5 都发，
/// **fable-5-1 一条都不发**（auto、非 auto、`-p` 都是，`00383`、`00554`，`cap/2.1.285/00039`），
/// 见 [`super::simulation::sim_uses_threads`]。来访指定了 `tool_choice` 的只 `create`：续轮不带
/// `tools`，`tool_choice` 没有落脚处。
///
/// 这一轮的结论（等回程提交的那份）挂到 `sim` 上，由 `ReqLog` 取走（[`Simulation::take_thread`]）。
fn apply_sim_thread(v: &mut serde_json::Value, sim: &Simulation, cred_id: i64) -> bool {
    if !super::simulation::sim_is_main_thread(sim) {
        return false;
    }
    let model = v.get("model").and_then(|m| m.as_str()).unwrap_or_default();
    let threads = super::simulation::sim_uses_threads(sim, model);
    let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) else { return false };
    if msgs.is_empty() {
        return false;
    }
    // 指纹在插 `<total_tokens>` 提醒**之前**算：客户端下一轮带回来的历史里没有这条提醒（它只进了
    // 上游线程），把它算进去，下一轮的前缀就永远对不上。
    let fps: Vec<ThreadMsg> = msgs.iter().map(thread_msg_of).collect();
    let regular_prompt = crate::telemetry::last_is_new_prompt_body(v);
    let key = CcSessionKey { cred_id, session_id: &sim.session_id };
    let (mut decision, pending, tokens_left) =
        thread_decision(key, thread_shape_of(v), &fps, regular_prompt);
    let has_tool_choice = v.get("tool_choice").is_some_and(|c| !c.is_null());
    if !threads || (has_tool_choice && matches!(decision, ThreadDecision::Continue { .. })) {
        decision = ThreadDecision::Create;
    }
    let pending = match &decision {
        ThreadDecision::Create => pending.into_create(),
        ThreadDecision::Continue { .. } => pending,
    };
    let Some(obj) = v.as_object_mut() else { return false };
    match decision {
        ThreadDecision::Create if !threads => {}
        ThreadDecision::Create => {
            obj.insert("thread".into(), serde_json::json!({ "type": "create" }));
        }
        ThreadDecision::Continue { from, previous_message_id } => {
            if let Some(serde_json::Value::Array(m)) = obj.get_mut("messages") {
                m.drain(..from);
            }
            let billing = obj
                .get("system")
                .and_then(|s| s.as_array())
                .and_then(|a| a.first())
                .and_then(|b| b.get("text"))
                .and_then(|t| t.as_str())
                .filter(|t| t.starts_with("x-anthropic-billing-header:"))
                .map(str::to_string);
            match billing {
                Some(text) => {
                    obj.insert(
                        "system".into(),
                        serde_json::json!([{ "type": "text", "text": text }]),
                    );
                }
                None => {
                    obj.remove("system");
                }
            }
            obj.remove("tools");
            obj.insert(
                "thread".into(),
                serde_json::json!({ "type": "continue", "previous_message_id": previous_message_id }),
            );
            obj.insert(
                "diagnostics".into(),
                serde_json::json!({ "previous_message_id": previous_message_id }),
            );
        }
    }
    insert_total_tokens_reminder(
        v,
        tokens_left,
        regular_prompt,
        super::simulation::sim_has_beta(sim, config::CC_BETA_MID_CONVERSATION_SYSTEM),
    );
    if threads {
        cap_thread_breakpoints(v);
    }
    align_cc_top_level_order(v, sim.profile.body_key_order);
    sim.set_thread(pending);
    true
}

/// 带 `thread` 时一条请求最多 3 个缓存断点：第 4 个名额留给上游自己标在对话末尾的那个，多了整条
/// 拒（`thread: a maximum of 3 blocks with cache_control may be provided when `thread` is set`）。
/// 官方 `create` 恒为 3 个（基座、其余、末条消息），`continue` 恒为 1 个
/// （`cap/auto-2.1.285-20260930` 里 25 条 `create`、55 条 `continue` 逐条数过）。
const MAX_THREAD_CACHE_BREAKPOINTS: usize = 3;

/// 把断点裁到 [`MAX_THREAD_CACHE_BREAKPOINTS`] 以内。前面那几步按 [`MAX_CACHE_BREAKPOINTS`]
/// 分配预算：来访自带 system 时 [`super::simulate_system`] 在第五块（客户端那段）上也标一个，
/// 加上基座、其余与末条消息正好 4 个；来访自己在工具定义、历史消息上标的也会凑满。按「摘了最
/// 不心疼」的顺序摘：
///
/// 1. `tools` 上的：缓存前缀按 tools → system → messages 排，system 上有断点就已经盖住了它；
/// 2. 顶层的（自动缓存）：它标的正是对话末尾，与上游预留的那个重复；
/// 3. `messages` 里除最后一个之外的，从前往后；
/// 4. `system` 里的，**从后往前**：官方只标基座与其余两块，先摘第五块那个；
/// 5. 仍超（只剩末条消息上那个）才摘它。
///
/// 计数与摘除只看 API 认 `cache_control` 的位置（[`cache_slots`]）：工具 schema 里叫
/// `cache_control` 的参数、`tool_use.input` 里的同名字段都是业务数据，算进去会把它们当断点
/// 摘掉，schema 从此 `required` 了一个不存在的属性。
///
/// 返回摘掉了几个。
fn cap_thread_breakpoints(v: &mut serde_json::Value) -> usize {
    let slots = cache_slots(v);
    let excess = slots.len().saturating_sub(MAX_THREAD_CACHE_BREAKPOINTS);
    if excess == 0 {
        return 0;
    }
    let of = |pick: fn(&CacheSlot) -> bool| slots.iter().copied().filter(pick);
    let mut msgs: Vec<CacheSlot> = of(|s| matches!(s, CacheSlot::Message(..))).collect();
    let last_msg = msgs.pop();
    let order = of(|s| matches!(s, CacheSlot::Tool(_)))
        .chain(of(|s| matches!(s, CacheSlot::Top)))
        .chain(msgs)
        .chain(of(|s| matches!(s, CacheSlot::System(_))).collect::<Vec<_>>().into_iter().rev())
        .chain(last_msg);
    let doomed: Vec<CacheSlot> = order.take(excess).collect();
    for slot in &doomed {
        if let Some(o) = slot.resolve(v) {
            o.shift_remove("cache_control");
        }
    }
    doomed.len()
}

/// 官方主线程每条请求末尾的 `<total_tokens>N tokens left</total_tokens>` 提醒（数怎么算见
/// [`super::session_link::TotalTokens`]），只跟在 user 消息（新输入或工具结果）后面。落法随
/// `mid-conversation-system` beta 分两种：
///
/// - **带那项 beta**（opus / sonnet / fable）：追加一条独立的 `role: system` 消息，末条消息的
///   缓存断点挪到它身上（`cap/auto-2.1.285-20260930/00033`、`00036`、`00405`）；
/// - **不带**（haiku）：写成 `<system-reminder>`——工具续轮拼在最后一个 `tool_result` 正文的
///   末尾（`00412`），新输入则作为一个文本块插在用户那句话前面（`00411`）。
///
/// 官方首轮那条会把它与环境说明、日期并进同一条 system 消息（`00032`、`00349`），那是首轮附件
/// 整体的形态，这里只补单独这一条。
fn insert_total_tokens_reminder(
    v: &mut serde_json::Value,
    tokens_left: u64,
    regular_prompt: bool,
    mid_conversation_system: bool,
) {
    let text = format!("<total_tokens>{tokens_left} tokens left</total_tokens>");
    let Some(msgs) = v.get_mut("messages").and_then(|m| m.as_array_mut()) else { return };
    let Some(last) = msgs.last_mut() else { return };
    if last.get("role").and_then(|r| r.as_str()) != Some("user") {
        return;
    }
    if mid_conversation_system {
        let mut block = serde_json::json!({ "type": "text", "text": text });
        if let Some(cc) = take_last_cache_control(last) {
            block["cache_control"] = cc;
        }
        msgs.push(serde_json::json!({ "role": "system", "content": [block] }));
        return;
    }
    let wrapped = format!("<system-reminder>\n{text}\n</system-reminder>");
    let content = last.get_mut("content");
    let Some(content) = content else { return };
    if let serde_json::Value::String(s) = content {
        let s = std::mem::take(s);
        *content = serde_json::json!([{ "type": "text", "text": s }]);
    }
    let Some(blocks) = content.as_array_mut() else { return };
    let ty = |b: &serde_json::Value| b.get("type").and_then(|t| t.as_str()).map(str::to_string);
    if regular_prompt {
        let at = blocks.iter().rposition(|b| ty(b).as_deref() == Some("text")).unwrap_or(0);
        blocks.insert(at, serde_json::json!({ "type": "text", "text": format!("{wrapped}\n") }));
    } else if let Some(tr) =
        blocks.iter_mut().rev().find(|b| ty(b).as_deref() == Some("tool_result"))
    {
        match tr.get_mut("content") {
            Some(serde_json::Value::String(s)) => {
                s.push_str("\n\n");
                s.push_str(&wrapped);
            }
            Some(serde_json::Value::Array(parts)) => {
                parts.push(serde_json::json!({ "type": "text", "text": wrapped }));
            }
            _ => {
                tr["content"] = serde_json::Value::String(wrapped);
            }
        }
    }
}

/// 摘下一条消息里最后一个缓存断点并交出来（没有就 `None`）。官方的断点在末条消息上，追加一条
/// system 提醒之后末条换成了它，断点跟着挪过去，总数不变。
fn take_last_cache_control(m: &mut serde_json::Value) -> Option<serde_json::Value> {
    let blocks = m.get_mut("content")?.as_array_mut()?;
    blocks.iter_mut().rev().find_map(|b| b.as_object_mut().and_then(|o| o.remove("cache_control")))
}

/// 出站一条消息的线程指纹，见 [`ThreadMsg`]。
pub(super) fn thread_msg_of(m: &serde_json::Value) -> ThreadMsg {
    let assistant = m.get("role").and_then(|r| r.as_str()) == Some("assistant");
    let tool_use_ids = if assistant {
        m.get("content")
            .and_then(|c| c.as_array())
            .into_iter()
            .flatten()
            .filter(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"))
            .filter_map(|b| b.get("id").and_then(|i| i.as_str()).map(str::to_string))
            .collect()
    } else {
        Vec::new()
    };
    // 回复指纹与回程嗅探器同一个算法（[`ReplyFp`]），按块序喂；字符串形态的 content 即一个
    // text 块。只有 assistant 要比，user 留缺省值。
    let mut reply = ReplyFp::default();
    if assistant {
        match m.get("content") {
            Some(serde_json::Value::String(s)) if !s.is_empty() => {
                reply.text(reply_text_fp(REPLY_TEXT_FP_INIT, s), &[]);
            }
            Some(serde_json::Value::Array(blocks)) => {
                for b in blocks {
                    let field = |k: &str| b.get(k).and_then(|t| t.as_str()).unwrap_or_default();
                    match b.get("type").and_then(|t| t.as_str()) {
                        Some("text") => {
                            let citations = b
                                .get("citations")
                                .and_then(|c| c.as_array())
                                .map_or(&[][..], |c| &c[..]);
                            if !field("text").is_empty() || !citations.is_empty() {
                                reply.text(
                                    reply_text_fp(REPLY_TEXT_FP_INIT, field("text")),
                                    citations,
                                );
                            }
                        }
                        Some("tool_use") => reply.tool_use(
                            field("id"),
                            field("name"),
                            b.get("input").unwrap_or(&serde_json::Value::Null),
                        ),
                        Some("thinking") => reply
                            .thinking(false, reply_text_fp(REPLY_TEXT_FP_INIT, field("thinking"))),
                        Some("redacted_thinking") => {
                            reply.thinking(true, reply_text_fp(REPLY_TEXT_FP_INIT, field("data")))
                        }
                        Some("fallback") => {}
                        _ => reply.block(b),
                    }
                }
            }
            _ => {}
        }
    }
    ThreadMsg { fp: message_fingerprint(m), assistant, tool_use_ids, reply }
}

/// 一条消息的线程指纹：去掉 `cache_control`，且字符串形态的 `content` 与等价的单个 `text` 块
/// 同指纹——[`align_message_shape`] 给末条消息补断点时会把字符串改成块数组，同一条消息在上一轮
/// 是末条（块数组）、这一轮不是（仍是字符串），不能因此判成历史变了。
fn message_fingerprint(m: &serde_json::Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let Some(obj) = m.as_object() else { return fingerprint_without_cache_control(m) };
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for (k, x) in obj {
        if k == "cache_control" {
            continue;
        }
        k.hash(&mut h);
        match (k.as_str(), x) {
            ("content", serde_json::Value::String(s)) => hash_without_cache_control(
                &serde_json::json!([{ "type": "text", "text": s }]),
                &mut h,
            ),
            _ => hash_without_cache_control(x, &mut h),
        }
    }
    h.finish()
}

/// 线程形态指纹：模型、system 正文（billing header 那块除外——`cch` 与会话链字段每条都变）、
/// `tools`、`thinking`、`output_config`。全部去掉 `cache_control` 再算，见
/// [`super::session_link::ThreadPending`] 的 `shape`。
fn thread_shape_of(v: &serde_json::Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    v.get("model").and_then(|m| m.as_str()).unwrap_or_default().hash(&mut h);
    match v.get("system") {
        Some(serde_json::Value::Array(blocks)) => {
            for b in blocks {
                let billing = b
                    .get("text")
                    .and_then(|t| t.as_str())
                    .is_some_and(|t| t.starts_with("x-anthropic-billing-header:"));
                if !billing {
                    hash_without_cache_control(b, &mut h);
                }
            }
        }
        Some(other) => hash_without_cache_control(other, &mut h),
        None => {}
    }
    for key in ["tools", "thinking", "output_config"] {
        key.hash(&mut h);
        if let Some(x) = v.get(key) {
            hash_without_cache_control(x, &mut h);
        }
    }
    h.finish()
}

fn fingerprint_without_cache_control(v: &serde_json::Value) -> u64 {
    use std::hash::Hasher;
    let mut h = std::collections::hash_map::DefaultHasher::new();
    hash_without_cache_control(v, &mut h);
    h.finish()
}

/// 按结构哈希一个 JSON 值，跳过所有 `cache_control` 键：断点每轮都挪到最后一条消息上，同一条
/// 消息这轮有、下轮没有，不能因此判成历史变了。
fn hash_without_cache_control<H: std::hash::Hasher>(v: &serde_json::Value, h: &mut H) {
    use std::hash::Hash;
    match v {
        serde_json::Value::Null => 0u8.hash(h),
        serde_json::Value::Bool(b) => {
            1u8.hash(h);
            b.hash(h);
        }
        serde_json::Value::Number(n) => {
            2u8.hash(h);
            n.to_string().hash(h);
        }
        serde_json::Value::String(s) => {
            3u8.hash(h);
            s.hash(h);
        }
        serde_json::Value::Array(a) => {
            4u8.hash(h);
            a.len().hash(h);
            for x in a {
                hash_without_cache_control(x, h);
            }
        }
        serde_json::Value::Object(o) => {
            5u8.hash(h);
            for (k, x) in o {
                if k == "cache_control" {
                    continue;
                }
                k.hash(h);
                hash_without_cache_control(x, h);
            }
            6u8.hash(h);
        }
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

/// **模拟路径**的设备指纹：平台段与 UA 段都取**实际发出去的那套头**（[`config::CC_SIM_HEADERS`]
/// 里的 `x-stainless-arch` / `x-stainless-os` 与 [`config::CC_USER_AGENT`]），不看来访自己带的。
///
/// 模拟路径整套换头，来访的 `x-stainless-*` 与 UA 一个都不会发出去；此前指纹却照抄来访的
/// arch/os，于是 Windows 上的裸客户端与 Mac 上的裸客户端派生出两台设备，而上游收到的两条
/// 请求平台头完全一样（都是 arm64 + MacOS）——「两台设备、同一套平台头」正是
/// [`device_fingerprint`] 要堵的那类矛盾，只是方向反了。没带平台头的来访（curl、不走 SDK 的
/// 中转）此前落在 `"||…"` 上，与带头的又是另一台。现在同一账号经模拟路径的全部请求都是
/// **同一台设备**，与它们在上游呈现的那套头一致；arm64 mac 上的来访指纹与原来逐字相同，
/// 别的平台与没带头的并到它上面来。
///
/// `client_device_id` 仍然参与（归一化关着时）：来访自带设备 id 却被判成非官方客户端的那种，
/// 各自还是各自的设备。
pub(super) fn sim_device_fingerprint(client_device_id: Option<&str>) -> String {
    let sim = |k: &str| config::CC_SIM_HEADERS.iter().find(|(n, _)| *n == k).map_or("", |(_, v)| v);
    format!(
        "{}|{}|{}|{}",
        client_device_id.unwrap_or(""),
        sim("x-stainless-arch"),
        sim("x-stainless-os"),
        config::CC_USER_AGENT,
    )
}

/// 模拟路径上**来访没带会话 id** 时用来做会话绑定的键（占了槽位后会话 id 按槽位派生，见
/// [`crate::credentials::derive_session_id`]；没占槽位的模拟请求也用它派生会话 id）：**缓存前缀**
/// （`tools` 整段加 `system` 各块正文，不含 billing header 那一块，口径同 [`cache_prefix_of`]）
/// 再加**对话起点**（第一条 `role:"user"` 消息的文本），一起 sha256，取前 16 字节的小写 hex。
///
/// 为什么按前缀而不是按设备：此前来访没带会话 id 就按「账号 + 设备指纹」派生一个恒定值，
/// 同一账号经模拟路径的所有裸请求在上游看来是**一台设备上一条会话打了全部请求**，不同应用、
/// 不同工作区、不同对话混在同一个会话 id 下，消息历史互不为前缀。官方一条会话里 tools 与
/// system 是稳定的、消息只增不改，所以前缀相同**且对话起点相同**的请求才是同一条会话。
///
/// 为什么还要对话起点：同一应用同一工作区里开的几个对话 tools 与 system 完全一样，只按前缀
/// 它们是一条会话——几个对话只占一个会话名额（上限形同虚设）、粘在同一个号上、在上游共用一个
/// `X-Claude-Code-Session-Id` 却各发各的历史。第一条用户消息是一条对话里最稳定的东西：之后
/// 每轮都原样带着它、只在末尾追加。取它的**整段 content**——文本、图片、文档、tool_result
/// 都算（两个「描述这张图片」的对话差的正是图片），只去掉 `cache_control`（客户端逐轮挪断点）；
/// 字符串正文与单个 text 块等价，键序不影响。
///
/// 代价：客户端每轮都在改 system 或改首条消息的（压缩历史、把环境信息重写进首条），每轮或
/// 每次压缩后一个新会话——那种客户端本来也命不中缓存，见 [`ensure_cc_message_breakpoint`]
/// 的记述。没有 `tools`、`system`，首条也不是用户消息的请求键恒定——同一账号下这类请求仍是
/// 一条会话，与原来一样。**这是近似**：两条请求连首条 content 都逐字相同时，协议里没有任何
/// 信息能分出它们是不是两个对话，后台的会话数与上限都按这个口径算。返回 32 个 hex 字符；
/// 入库前还要经 [`session_binding_key`] 套上命名空间与口径版本。
pub(super) fn sim_session_key(v: &serde_json::Value) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"luban-prefix\0");
    if let Some(tools) = v.get("tools") {
        h.update(tools.to_string().as_bytes());
    }
    h.update([0u8]);
    match v.get("system") {
        Some(serde_json::Value::String(s)) => {
            h.update(s.as_bytes());
            h.update([0u8]);
        }
        Some(serde_json::Value::Array(blocks)) => {
            for t in blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .filter(|t| !t.starts_with("x-anthropic-billing-header:"))
            {
                h.update(t.as_bytes());
                h.update([0u8]);
            }
        }
        _ => {}
    }
    // 对话起点：第一条用户消息的文本。`role:"system"` 之类夹在前面的（litellm 那种，后面会被
    // 提升进顶层 system）跳过——它们是前缀的一部分，不是对话的起点。
    h.update(b"\0first-user\0");
    let first_user = v
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|m| m.iter().find(|m| m.get("role").and_then(|r| r.as_str()) == Some("user")));
    // 整段 content 都算，不只文本：两个「描述这张图片」的对话差的正是图片。字符串正文与单个
    // text 块等价（客户端第二轮常把首条从字符串改成块数组）；每块去掉 `cache_control` 后按
    // 键排序序列化（[`canonical_json`]），键序与断点都不影响键。
    let blocks: Vec<serde_json::Value> = match first_user.and_then(|m| m.get("content")) {
        Some(serde_json::Value::String(s)) => {
            vec![serde_json::json!({"type": "text", "text": s})]
        }
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .map(|b| {
                let mut b = b.clone();
                if let Some(o) = b.as_object_mut() {
                    o.remove("cache_control");
                }
                b
            })
            .collect(),
        _ => Vec::new(),
    };
    for b in &blocks {
        h.update(canonical_json(b).as_bytes());
        h.update([0u8]);
    }
    crate::credentials::hex_lower(&h.finalize()[..16])
}

/// 会话绑定键（`session_bindings.session_key`）的命名空间加口径版本：`lb:v2:`。
///
/// **为什么要有版本位**：这张表的键算法已经改过一次——v0.3.126 是「账号 + 设备指纹」，现在
/// 是「来访自带的会话 id，没带才按缓存前缀 + 对话起点」（[`sim_session_key`]）。两版的键长得
/// 一模一样（都是 32 个 hex），库里的存量行在新口径下含义已经不同，却没有任何办法分辨、也
/// 没法按前缀批量清。带上版本之后旧行一眼可辨，启动时按前缀清掉（见 `store` 里建表之后的
/// 那条迁移），以后再改口径 bump 到 v3 即可，不必动表结构。
pub(crate) const SESSION_KEY_VERSION: &str = "lb:v2:";

/// 这条请求落在 `session_bindings` 上的键：`lb:v2:<来源>:<值>`。
///
/// 来源段是**明文**，两种取值：
/// - `sid` —— 来访自己带了会话 id（头或 `metadata.user_id`），值就是那个 uuid。键要跨账号
///   稳定，故取来访原值，而不是出站那个按账号钉住的（`session_id::account_session_id`）。
/// - `pfx` —— 来访没带，值是 [`sim_session_key`] 那 32 个 hex。
///
/// 加这一段之前，两种来源只能靠「是不是 32 个 hex」去猜（后台列表里原来就是这么判的），
/// 而 uuid 去掉横线也是 32 个 hex，猜法本身站不住。
///
/// 为什么只放这一维、不把模型和线程类型也拼进去：这个键的用途是**粘住账号、占会话名额**，
/// 不是缓存分区。把模型拼进键，同一条对话换个模型就成了两个键——占两份名额，还可能粘到另一
/// 个号上，于是同一段对话历史出现在两个组织下，正是 `ban/luban-ban-13` 那类形态。
///
/// 线程类型（`x-claude-code-request-class`）更是**抓包直接判了死刑**：`cap/2.1.277` 里 47 条
/// `/v1/messages`，11 条 `main`、26 条 `subagent`、10 条 `auxiliary`，`X-Claude-Code-Session-Id`
/// 与 `metadata.user_id` 里的 `session_id` **全是同一个 uuid**（`7fe47444-…`，`00049` 起那批
/// 子代理只是多带 `x-claude-code-agent-id: a842a8d67aeec7a12` 与 `agent-type: custom`）。
/// 官方口径是「子代理不另起会话，只是父会话里的一条支线」，`agent-id` 是支线号、不是会话身份。
/// 把 class 或 agent-id 拼进键就会把官方本来一条的会话劈成三条：占三份名额，还可能粘到三个
/// 号上——同一段历史同时出现在三个组织下，比不劈更像机器人。
///
/// **`sid` 优先于 `pfx` 的理由也在这里**：子代理的 `tools` 与 `system` 与主线程不同，光按
/// 缓存前缀算必然是两个键；来访带了会话 id 就一切以它为准，主线程与子代理自动并回一条，与
/// 上面那 47 条的形态一致。反过来，**裸客户端自己实现的子代理又不带会话 id** 时协议里没有
/// 任何父子线索，只能按前缀各算各的——这是 `pfx` 分支已知的近似（见 [`sim_session_key`]）。
///
/// 模型与线程类型要看就记在绑定行上（`session_bindings.last_model`）给后台列，不进键。
pub(super) fn session_binding_key(incoming_session_id: Option<&str>, prefix_key: &str) -> String {
    match incoming_session_id {
        Some(sid) => format!("{SESSION_KEY_VERSION}sid:{sid}"),
        None => format!("{SESSION_KEY_VERSION}pfx:{prefix_key}"),
    }
}

/// 键按字典序排好的紧凑 JSON：`preserve_order` 开着时 `to_string` 按客户端发来的键序输出，
/// 同一内容两种键序会算出两个不同的键。只给 [`sim_session_key`] 用。
fn canonical_json(v: &serde_json::Value) -> String {
    fn sort(v: &serde_json::Value) -> serde_json::Value {
        match v {
            serde_json::Value::Object(o) => {
                let mut keys: Vec<&String> = o.keys().collect();
                keys.sort();
                serde_json::Value::Object(
                    keys.into_iter().map(|k| (k.clone(), sort(&o[k]))).collect(),
                )
            }
            serde_json::Value::Array(a) => serde_json::Value::Array(a.iter().map(sort).collect()),
            other => other.clone(),
        }
    }
    sort(v).to_string()
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

/// 真 CC 主线程的 billing header 在 `cc_prompt_id` 之后还要补哪几项，见 [`append_billing_link`]。
#[derive(Debug, Clone, Copy, Default)]
struct TurnFields {
    /// `cc_turn_origin=human`（2.1.277 起）。
    origin: bool,
    /// `cc_prompt_index=N; cc_turn_index=N;`（2.1.285 起）。
    index: bool,
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
///
/// `turn` 决定 `cc_prompt_id` 之后再补不补 `cc_turn_origin=human` 与
/// `cc_prompt_index` / `cc_turn_index`，见 [`TurnFields`]。只在这条已经有 `cc_prompt_id`（客户端
/// 自己的或刚补的）时补：官方这几项与它同条件出现。`cc_turn_origin` 恒写 `human`——后台任务
/// 通知那种轮次（`task_notification`，`cap/2.1.285/00149`）代理分辨不出。
fn append_billing_link(v: &mut serde_json::Value, link: &CcSessionLink, turn: TurnFields) -> bool {
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
    if s.contains("cc_prompt_id=") {
        if turn.origin && !s.contains("cc_turn_origin=") {
            s.push_str(" cc_turn_origin=human;");
            changed = true;
        }
        if turn.index && link.prompt_index > 0 && !s.contains("cc_prompt_index=") {
            let n = link.prompt_index;
            s.push_str(&format!(" cc_prompt_index={n}; cc_turn_index={n};"));
            changed = true;
        }
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
/// beta 串没有 `advisor-tool`，[`merge_beta_for`] 按老世代处理不补，体里却写了。
///
/// 那个前提也是**唯一**的门槛：本函数不再自己按模型族判一遍。哪一族在哪一版发这项 beta
/// 是 [`merge_beta_for`] 的事（它按来访自报的版本查 [`config::cc_profile_at`]），在这里再判
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

/// 模拟路径下按 profile 补顶层 `output_config`：2.1.277 起 opus / fable / sonnet 主线程每条都是
/// `{"effort":"high"}`（`cap/2.1.277/00023`、`00031`、`00357`，首轮与工具续轮都带；opus-5-5 与
/// sonnet-5-5 的官方默认是 `medium`，`cap/2.1.280/00021`、`cap/2.1.285/00045`，模拟路径照旧按
/// `high` 发），haiku
/// 主线程与全部辅助请求不带（[`config::CcProfile::effort`] 为 `None`，这里什么都不做）。
///
/// 客户端自己带了 `output_config`（不论写的是 `effort` 还是 `format`）就不动——那是它自己的
/// 输出策略，替它改是越权。位置按官方线序落在 `context_management` 之后、`thread` /
/// `diagnostics` / `stream` 之前（[`config::CC_BODY_ORDER_MAIN_2_1_270`]）。
///
/// **`effort` 要配 `effort-2025-11-24` beta**：三个带它的 profile 的 beta 串里都有这一项，
/// haiku 的没有——这也是 haiku 那行 `effort: None` 的另一层理由，别只看抓包里有没有字段。
pub(super) fn ensure_output_config(v: &mut serde_json::Value, profile: &config::CcProfile) -> bool {
    let Some(effort) = profile.effort else { return false };
    let Some(obj) = v.as_object() else { return false };
    if obj.contains_key("output_config") {
        return false;
    }
    insert_top_level(
        v,
        "output_config",
        serde_json::json!({ "effort": effort }),
        &[
            "context_management",
            "thinking",
            "max_tokens",
            "metadata",
            "tools",
            "system",
            "messages",
            "model",
        ],
    );
    true
}

/// `cch` 先写进 billing header 的**占位符**：五个 `0`。真值不在这里取，而是等整条 body
/// 的全部改写都落定、序列化成最终出站字节之后，由 [`apply_cch`] 原地算出来替换掉。
///
/// **为什么是占位符而不是在这里算**：`cch` 是官方 Bun HTTP 出口层对**最终出站 body**
/// （含 `cch=00000` 占位符本身）算的 xxHash64 取低 20 位（见 [`compute_cch`]）。它依赖
/// 工具注入、工具名混淆、message thread 等所有后续改写的结果，而这个函数在改写链的中段
/// （[`ensure_billing_cch`] / [`simulated_billing_header_text`]）就被调用，那时 body 还没定型，
/// 算不出正确值。于是这里只落占位符，把真正的计算推到全部改写之后。
///
/// **不会打爆 prompt cache**：`cap/2.1.260-2` 的 00057 → 00059 是同一会话连续两条，
/// `system[0]` 的 cch 与 `cc_prompt_id` 都变了，00059 的 `cache_creation_input_tokens` 仍只有
/// 81（首条 8482），前缀照样命中。上游不把 billing header 那一块算进缓存键——否则官方客户端
/// 自己也一次都缓存不上。
pub(super) fn cch_value() -> String {
    CCH_PLACEHOLDER.to_string()
}

/// [`cch_value`] 落进 billing header 的占位值。真值由 [`apply_cch`] 在出站字节里（以
/// `x-anthropic-billing-header:` 为锚，见 [`billing_cch_region`]）算出后回填——不靠这五个 `0`
/// 本身定位，所以 billing header 里即便已是别的值（真实 CC 自带的真值）也照样会被重算。
const CCH_PLACEHOLDER: &str = "00000";

/// `cch` 的 xxHash64 种子，逆向自 `claude-cli/2.1.289` 的 Bun HTTP 出口层（原生段里
/// 以 `movk` 序列加载的 `0x4d659218e32a3268`，紧接 strip 预处理之后作为 `XXH64` init 的
/// seed）。跨版本一致：用 2.1.289 的运行时去算 2.1.258–2.1.285 九组抓包里**他机他版本**
/// 客户端自己写的 cch，263 条全中。
pub(super) const CCH_SEED: u64 = 0x4d65_9218_e32a_3268;

/// 给出站字节里 billing header 的 `cch` 填上（或重算成）真值：把那一段**先归零**成
/// `cch=00000;`、对整条 body 做 [`compute_cch`]、再把结果写回那 5 位。回填成功返回那个 cch，
/// body 里没有 billing header 的 cch 时返回 `None`（没改）。
///
/// **只认 billing header 那一个 cch，按 JSON 结构定位**：257/263 抓包的 `messages` 排在
/// `system` 之前，用户正文里若含 `cch=…;` 甚至整条 `x-anthropic-billing-header:` 都会更早出现
/// ——所以不能按内容全局搜。[`billing_cch_region`] 顺着顶层对象定位到 `system[0].text` 这个
/// 字段本身（用户正文里的引号在序列化后是 `\"`，冒充不了键结构），再在字段边界内完整校验
/// `cch=<5 hex>;` 才认，绝不越界覆盖收尾引号或 JSON 结构。
///
/// **占位符与自带真值一视同仁**：luban 自己补的是 `cch=00000;` 占位符，而真实 CC 订阅端
/// 来访自带的是它对**自己那份** body 算好的真值；一旦 luban 改写了 body（身份伪装、工具注入/
/// 混淆等，凡走到这里就意味着 body 变过），那份自带真值就对不上**改写后**的字节了。所以这里
/// 不管原来是零还是真值，统一先归零、按最终字节重算、回填——与官方「对含 `00000` 占位符的
/// body 算 xxHash64」同序，结果就是这份出站 body 该有的 cch。
pub(super) fn apply_cch(bytes: &mut [u8]) -> Option<String> {
    let (start, end) = billing_cch_region(bytes)?;
    // 先把这 5 位归零成占位态，再对整条 body 算——官方就是对含 `00000` 的 body 哈希的。
    bytes[start..end].copy_from_slice(b"00000");
    let cch = compute_cch(bytes);
    bytes[start..end].copy_from_slice(cch.as_bytes());
    Some(cch)
}

/// 真实来访的 billing header `cch` 与这份 body **对不上**（且 `cch_real_recompute` 开着）。
///
/// [`rewrite_body_out`] 自己没改任何东西时会原样放行，但进来的 `body` 未必还是客户端发的那份：
/// 主动剥 prefill、弃用字段剥除、thinking 降级 / 剥 prefill 的重试，都在改写之前先动过 body。
/// 官方客户端自带的 cch 恒等于对它自己那份 body 的 [`compute_cch`]（`cap/` 263 条全中），
/// 所以「对不上」就说明前置改写动过 body（或者来访写的本就不是真值），这时不能早退，
/// 得走完整路径让 [`finalize_cch`] 按出站字节重算。没有 billing cch 时返回 `false`。
fn real_cch_stale(body: &[u8], sim: Option<&Simulation>, flags: store::ForwardFlags) -> bool {
    if sim.is_some() || !flags.cch_real_recompute {
        return false;
    }
    let Some((start, end)) = billing_cch_region(body) else {
        return false;
    };
    let mut zeroed = body.to_vec();
    zeroed[start..end].copy_from_slice(CCH_PLACEHOLDER.as_bytes());
    compute_cch(&zeroed).as_bytes() != &body[start..end]
}

/// 出站字节定型后按策略处理 billing header 的 `cch`，改了就返回填进去的值，没动返回 `None`。
///
/// - `compute`（真实来访看 `cch_real_recompute`、模拟看 `cch_sim_compute`）：按 [`apply_cch`]
///   算真值回填，来访自带的与 luban 补的占位一并重算。
/// - 不 `compute` 且 `ours`（cch 是 luban 落的占位符：模拟请求，或替真实来访补的那条）：填一个
///   随机的 5 位小写 hex。不能留着 `00000`——那是跨账号恒定值，上游一按它聚类就把所有账号
///   串成一串。
/// - 不 `compute` 且不是 `ours`：来访自带的 `cch` 原样保留。
fn finalize_cch(bytes: &mut [u8], compute: bool, ours: bool) -> Option<String> {
    if compute {
        return apply_cch(bytes);
    }
    if !ours {
        return None;
    }
    let (start, end) = billing_cch_region(bytes)?;
    let n: u32 = rand::rng().random_range(0..0x10_0000);
    let cch = format!("{n:05x}");
    bytes[start..end].copy_from_slice(cch.as_bytes());
    Some(cch)
}

/// 在出站字节里定位 billing header 的 `cch` **值**那 5 个字节，返回 `[start, end)`。
///
/// **按 JSON 结构定位、不做全局内容搜索**：先用 [`system0_text_range`] 顺着顶层对象找到
/// `system[0].text` 这个字符串值的区间——用户正文里即便原样引用了 `x-anthropic-billing-header:`
/// 甚至整条 header，它在序列化后处于别的字符串值里、引号是 `\"`，冒充不了真正的键结构，不会
/// 被误认。拿到该字段区间后，才在**区间之内**找 `cch=`，并且**完整校验** `cch=<5 位小写 hex>;`
/// 才认（`i + 10 <= text.len()` 保证那个 `;` 也落在字段内，5 位值绝不会越过收尾引号去覆盖
/// JSON 结构）。字段不是 billing header、或没有合法的 cch 段，返回 `None`。
fn billing_cch_region(bytes: &[u8]) -> Option<(usize, usize)> {
    let (ts, te) = system0_text_range(bytes)?;
    let text = &bytes[ts..te];
    if !text.starts_with(b"x-anthropic-billing-header:") {
        return None;
    }
    let mut i = 0;
    while i + 10 <= text.len() {
        if &text[i..i + 4] == b"cch="
            && text[i + 9] == b';'
            && text[i + 4..i + 9].iter().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
        {
            return Some((ts + i + 4, ts + i + 9));
        }
        i += 1;
    }
    None
}

/// 顺着序列化好的请求体字节，按 JSON 结构找到**顶层** `system` 数组第 0 个元素的 `text`
/// 字符串**值**（两个引号之间的内容）的字节区间 `[start, end)`，找不到返回 `None`。
///
/// 全程区分「真·结构引号」与「字符串内容里被转义的 `\"`」：扫到的 `"system"` / `"text"` 必须
/// 是顶层对象 / 元素对象里真正的键，用户正文里出现的同名串其引号在序列化后是 `\"`、不会被当成
/// 键。这样 cch 的定位就只认 body 结构里那个 billing header 字段，与正文内容无关。
fn system0_text_range(bytes: &[u8]) -> Option<(usize, usize)> {
    let n = bytes.len();
    let mut i = skip_ws(bytes, 0);
    if i >= n || bytes[i] != b'{' {
        return None;
    }
    i += 1;
    // 扫顶层对象的成员，找键 `system`。
    loop {
        i = skip_ws(bytes, i);
        if i >= n || bytes[i] != b'"' {
            return None;
        }
        let (ks, ke, after) = read_json_string(bytes, i)?;
        i = skip_ws(bytes, after);
        if i >= n || bytes[i] != b':' {
            return None;
        }
        i = skip_ws(bytes, i + 1);
        if &bytes[ks..ke] == b"system" {
            // 期待数组、首元素对象，在其中找键 `text`。
            if i >= n || bytes[i] != b'[' {
                return None;
            }
            i = skip_ws(bytes, i + 1);
            if i >= n || bytes[i] != b'{' {
                return None;
            }
            i = skip_ws(bytes, i + 1);
            loop {
                if i >= n || bytes[i] != b'"' {
                    return None;
                }
                let (ks2, ke2, after2) = read_json_string(bytes, i)?;
                i = skip_ws(bytes, after2);
                if i >= n || bytes[i] != b':' {
                    return None;
                }
                i = skip_ws(bytes, i + 1);
                if &bytes[ks2..ke2] == b"text" {
                    if i >= n || bytes[i] != b'"' {
                        return None;
                    }
                    let (vs, ve, _) = read_json_string(bytes, i)?;
                    return Some((vs, ve));
                }
                i = skip_json_value(bytes, i)?;
                i = skip_ws(bytes, i);
                if i < n && bytes[i] == b',' {
                    i = skip_ws(bytes, i + 1);
                    continue;
                }
                return None; // 首元素里没有 text
            }
        }
        i = skip_json_value(bytes, i)?;
        i = skip_ws(bytes, i);
        if i < n && bytes[i] == b',' {
            i += 1;
            continue;
        }
        return None;
    }
}

/// 跳过从 `i` 起的 JSON 空白，返回第一个非空白字节的下标（或 `bytes.len()`）。
fn skip_ws(bytes: &[u8], mut i: usize) -> usize {
    while i < bytes.len() && matches!(bytes[i], b' ' | b'\t' | b'\n' | b'\r') {
        i += 1;
    }
    i
}

/// `bytes[i]` 是开引号，读完这个 JSON 字符串。返回（内容起、内容止、收尾引号之后的下标）。
/// 处理 `\` 转义（含 `\"`），不解码——只按边界扫。
fn read_json_string(bytes: &[u8], i: usize) -> Option<(usize, usize, usize)> {
    if bytes.get(i) != Some(&b'"') {
        return None;
    }
    let start = i + 1;
    let mut j = start;
    let mut esc = false;
    while j < bytes.len() {
        let c = bytes[j];
        if esc {
            esc = false;
        } else if c == b'\\' {
            esc = true;
        } else if c == b'"' {
            return Some((start, j, j + 1));
        }
        j += 1;
    }
    None
}

/// 跳过从 `i` 起的一个 JSON 值（字符串 / 对象 / 数组 / 数字 / true / false / null），返回其后
/// 的下标。对象与数组按 `{}` `[]` 深度配平，字符串内的括号与引号不计。
fn skip_json_value(bytes: &[u8], i: usize) -> Option<usize> {
    let n = bytes.len();
    match bytes.get(i)? {
        b'"' => read_json_string(bytes, i).map(|(_, _, after)| after),
        b'{' | b'[' => {
            let mut depth = 0i32;
            let mut in_str = false;
            let mut esc = false;
            let mut j = i;
            while j < n {
                let c = bytes[j];
                j += 1;
                if in_str {
                    if esc {
                        esc = false;
                    } else if c == b'\\' {
                        esc = true;
                    } else if c == b'"' {
                        in_str = false;
                    }
                } else {
                    match c {
                        b'"' => in_str = true,
                        b'{' | b'[' => depth += 1,
                        b'}' | b']' => {
                            depth -= 1;
                            if depth == 0 {
                                return Some(j);
                            }
                        }
                        _ => {}
                    }
                }
            }
            None
        }
        // 数字 / true / false / null：读到分隔符为止。
        _ => {
            let mut j = i;
            while j < n && !matches!(bytes[j], b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                j += 1;
            }
            (j > i).then_some(j)
        }
    }
}

/// 把回填到出站字节里的 cch 真值同步写回 `v` 的 billing header（`system[0]` 文本里的
/// `cch=<原值>;` → `cch=<真值>;`，原值可能是 `00000` 占位符或自带真值）。保证返回的 `Value`
/// 与出站字节逐字节同构——调用方拿它算形态摘要。`system[0]` 不是 billing header 时不动。
fn sync_value_cch(v: &mut serde_json::Value, cch: &str) {
    let Some(b) = v.get_mut("system").and_then(|s| s.as_array_mut()).and_then(|a| a.first_mut())
    else {
        return;
    };
    let Some(text) = b.get("text").and_then(|t| t.as_str()) else {
        return;
    };
    if !text.starts_with("x-anthropic-billing-header:") {
        return;
    }
    // 只替换 billing header 里的 `cch=<5 hex>;`（首个即是，billing 串里只有这一个）。
    let replaced = replace_billing_cch(text, cch);
    b["text"] = serde_json::Value::String(replaced);
}

/// 把 billing header 文本里 `cch=<5 位小写 hex>;` 的那 5 位换成 `cch`，只换第一处。
/// 没有匹配就原样返回。
fn replace_billing_cch(text: &str, cch: &str) -> String {
    let bytes = text.as_bytes();
    let mut i = 0;
    while i + 10 <= bytes.len() {
        if &bytes[i..i + 4] == b"cch="
            && bytes[i + 9] == b';'
            && bytes[i + 4..i + 9].iter().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
        {
            return format!("{}cch={cch};{}", &text[..i], &text[i + 10..]);
        }
        i += 1;
    }
    text.to_string()
}

/// 按官方算法算 billing header 的 `cch`：对**规范化后**的整条出站 body 做 xxHash64
/// （种子 [`CCH_SEED`]），取结果低 20 位，输出 5 位小写 hex。
///
/// 规范化（[`normalize_for_cch`]）复刻官方出口层在哈希前做的那遍预处理：清空所有
/// `"model"` 的字符串值、剥掉 `"max_tokens"` / `"fallbacks"` / `"fallback_credit_token"`
/// 三个字段。
pub(super) fn compute_cch(body: &[u8]) -> String {
    let norm = normalize_for_cch(body);
    let h = xxhash_rust::xxh64::xxh64(&norm, CCH_SEED);
    format!("{:05x}", h & 0xf_ffff)
}

/// `cch` 哈希前的 body 规范化，逐字节复刻官方出口层那段预处理（逆向自 2.1.289，原生函数
/// `0x1018bf7bc` 做字段定位与逗号消解、`0x1018b7700` 串流喂 xxHash）：
///
/// 1. 把**每一处** `"model":"<字符串>"` 的值清空成 `"model":""`（`"model":{…}` 这种值不是
///    字符串的不动——工具 schema 里的 `model` 参数就是对象，保留）。抓包里主线程体顶层的
///    `model` 与 advisor 工具声明里的 `model` 都是字符串、都被清空，故全局处理。
/// 2. 删掉 `"max_tokens":<数字>`、`"fallbacks":[<数组>]`、`"fallback_credit_token":"<串>"`
///    三个字段连同一个相邻逗号。详见 [`strip_cch_fields`]。
///
/// 这几个字段是官方故意排除在 cch 之外的——`max_tokens` / `fallbacks` /
/// `fallback_credit_token` 逐请求会变（上游下发的额度与回退名单），`model` 的值在重试回退时
/// 会被改写，算进去就会让同一轮的 cch 不稳定。
fn normalize_for_cch(body: &[u8]) -> Vec<u8> {
    strip_cch_fields(&empty_model_values(body))
}

/// 把 body 里每一处 `"model":"<字符串>"` 的值清空成 `"model":""`，其余字节原样。
/// `"model":` 后面不是 `"`（如 `"model":{…}`）的不动。
fn empty_model_values(body: &[u8]) -> Vec<u8> {
    const KEY: &[u8] = b"\"model\":\"";
    let mut out = Vec::with_capacity(body.len());
    let mut i = 0;
    while i < body.len() {
        if body[i..].starts_with(KEY) {
            // 写入 `"model":"`，跳过原值内容到配对收尾引号，留下一对空引号。
            out.extend_from_slice(KEY);
            let mut j = i + KEY.len();
            let mut esc = false;
            while j < body.len() {
                let c = body[j];
                if esc {
                    esc = false;
                } else if c == b'\\' {
                    esc = true;
                } else if c == b'"' {
                    break;
                }
                j += 1;
            }
            // `j` 停在收尾引号（或 body 末尾）。写入该引号，i 跳到其后。
            if j < body.len() {
                out.push(b'"');
                i = j + 1;
            } else {
                i = j;
            }
        } else {
            out.push(body[i]);
            i += 1;
        }
    }
    out
}

/// 单趟从左到右扫一遍 body，删掉遇到的每一处被剥字段连同一个相邻逗号。
///
/// **匹配模式含值的开头定界符**（逆向自原生：strip 函数搜的就是 `"fallbacks":[` 与
/// `"fallback_credit_token":"` 这种带 `[` / `"` 的整串）：所以只有 `"fallbacks":[数组]`、
/// `"fallback_credit_token":"串"` 这种形态会被剥；`"fallbacks":"default"`（值是字符串而非
/// 数组，重试回退那一路会这么下发）不匹配 `"fallbacks":[`，原样留在哈希里。`"max_tokens":`
/// 则要求冒号后紧跟数字，否则不当作匹配。
///
/// **逗号消解**：先看值后面是不是 `,`，是就连那个后逗号一起删；否则看字段前面是不是一个
/// **还没被前一个剥除区间吃掉的** `,`，是就把它也删掉（`prev_end` 记上一个区间的终点，拦住
/// 相邻两个被剥字段去重复吃它们中间那一个逗号）。两个被剥字段贴在一起、且后面紧跟 `}` 时，
/// 中间那个逗号归前一个字段当后逗号吃掉，末尾就会剩下一个 `,}`——这正是官方出口层的产物
/// （`cap` 里多字段相邻的体据此 263/263 对齐）。
fn strip_cch_fields(body: &[u8]) -> Vec<u8> {
    // (匹配前缀含定界符, 值形态)。顺序不影响结果：扫描按位置推进，命中哪个算哪个。
    const SPECS: &[(&[u8], CchFieldVal)] = &[
        (b"\"max_tokens\":", CchFieldVal::Number),
        (b"\"fallbacks\":[", CchFieldVal::Array),
        (b"\"fallback_credit_token\":\"", CchFieldVal::String),
    ];
    let mut out: Vec<u8> = Vec::with_capacity(body.len());
    let mut i = 0;
    // 上一个被剥区间的终点（body 下标）。初值 0：body[−1] 不存在，首个字段也就吃不到前逗号。
    let mut prev_end = 0usize;
    while i < body.len() {
        let hit = SPECS.iter().find(|(key, _)| body[i..].starts_with(key));
        let Some(&(key, val)) = hit else {
            out.push(body[i]);
            i += 1;
            continue;
        };
        let value_start = i + key.len();
        let Some(value_end) = cch_value_end(body, value_start, val) else {
            // 定界符对上了但值不成形（如 `"max_tokens":` 后面不是数字）：不当匹配，照常逐字节走。
            out.push(body[i]);
            i += 1;
            continue;
        };
        let end = if value_end < body.len() && body[value_end] == b',' {
            // 值后面是逗号：连后逗号一起删。
            value_end + 1
        } else {
            // 否则：前面那个逗号若还没被上一个区间吃掉，就把已写进 out 的它删掉。
            if i > prev_end && body[i - 1] == b',' {
                out.pop();
            }
            value_end
        };
        prev_end = end;
        i = end;
    }
    out
}

/// [`strip_cch_fields`] 里被剥字段的值形态，决定从定界符之后怎么找到值的末尾。
#[derive(Clone, Copy)]
enum CchFieldVal {
    /// 十进制数字串，如 `"max_tokens":128000`；一个数字都没有则返回 `None`（不算匹配）。
    Number,
    /// JSON 数组，开头的 `[` 已在匹配前缀里，从其后按括号深度（初值 1）配平到收尾 `]`。
    Array,
    /// JSON 字符串，开头的 `"` 已在匹配前缀里，从其后读到配对收尾 `"`（`\` 转义的不算）。
    String,
}

/// 从 `start`（定界符之后的第一个字节）出发，按值形态找到值的末尾（返回末尾的下一位）。
/// `Number` 在没有任何数字时返回 `None`。
fn cch_value_end(body: &[u8], start: usize, val: CchFieldVal) -> Option<usize> {
    let mut i = start;
    match val {
        CchFieldVal::Number => {
            while i < body.len() && body[i].is_ascii_digit() {
                i += 1;
            }
            (i > start).then_some(i)
        }
        CchFieldVal::String => {
            // 开引号已在匹配前缀里，这里从引号内第一个字节读到配对收尾引号。
            let mut esc = false;
            while i < body.len() {
                let c = body[i];
                i += 1;
                if esc {
                    esc = false;
                } else if c == b'\\' {
                    esc = true;
                } else if c == b'"' {
                    break;
                }
            }
            Some(i)
        }
        CchFieldVal::Array => {
            // 开头的 `[` 已在匹配前缀里，深度从 1 起，配平到收尾 `]`；字符串内的括号不计。
            let mut depth = 1i32;
            let mut in_str = false;
            let mut esc = false;
            while i < body.len() {
                let c = body[i];
                i += 1;
                if esc {
                    esc = false;
                } else if in_str {
                    if c == b'\\' {
                        esc = true;
                    } else if c == b'"' {
                        in_str = false;
                    }
                } else if c == b'"' {
                    in_str = true;
                } else if c == b'[' {
                    depth += 1;
                } else if c == b']' {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                }
            }
            Some(i)
        }
    }
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

/// 补消息级缓存断点时算作「最后一条」的消息：从尾部往前跳过指令式 system
/// （[`is_system_directive`]）。
///
/// 指令式那条 `content` 是空数组，挂不上断点；以前它被当空壳丢掉，前一条自然成了末条，
/// 现在原样留着，不跳过的话 [`align_message_shape`] / [`ensure_cc_message_breakpoint`] 摸到
/// 空数组就直接返回，前面整段历史失去自动缓存。断点落在它前一条上，缓存前缀覆盖的内容与
/// 丢掉它时一样；指令本身留在原位，不挪。
fn last_cacheable_message_mut(msgs: &mut [serde_json::Value]) -> Option<&mut serde_json::Value> {
    msgs.iter_mut().rev().find(|m| !is_system_directive(m))
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
        .and_then(|a| last_cacheable_message_mut(a))
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
    if msgs.is_empty() || cache_slots(v).iter().any(|s| matches!(s, CacheSlot::Message(..))) {
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
        .and_then(|a| last_cacheable_message_mut(a))
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
/// 所有假名共用的命名空间前缀。生成处（[`build_tool_name_map`]）与还原处
/// （[`ToolNameMap::restore`]）必须是同一个串：还原靠它一趟扫出「可能是假名的位置」，
/// 两边一旦漂开，回程就一个假名都还原不了，而症状要到客户端拿着假名发下一轮才暴露。
const FAKE_TOOL_NS: &str = "mcp__luban__";

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
        let stem = format!("{FAKE_TOOL_NS}{prefix}{head}{i:02}");
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

/// 模拟路径下注入的官方主线程工具声明：14 条，与 `cap/2.1.285` 里 11 个模型的主线程
/// （`00030` opus-5-5 ↔ `00039` fable-5-1 ↔ `00045` sonnet-5-5 ↔ `00051` haiku-4-5 ……）逐字节相同。
///
/// **为什么要注入**：上游判第三方的信号之一是「自称 CC 但没有 CC 工具」。光把客户端自有
/// 工具名加 `mcp__` 前缀不够——那只是消去负面信号（被 blocklist 的名字），而正面信号
/// （存在 CC 官方工具声明）仍然缺失。注入之后请求的工具组合是「CC 内建 + MCP 扩展」，
/// 与真实 CC 接 MCP server 的形态一致（`cap/2.1.258-api/00006`：内建在前，`mcp__*` 在尾）。
///
/// **为什么是 14 个而不是 4 个**：2.1.285 四族主线程抓包（`cap/2.1.285/00030` / `00039` /
/// `00045` / `00051`）每条都带 19 或 20 个工具，其中四族共有的内建工具 15 个：下面这 14 个
/// 真工具，加上 `ToolSearch`；其余是延迟池里的 `DeferredToolPlaceholder` 占位、用户自己的
/// MCP 工具（`mcp__claude_ai_Claude_Docs__*`）与 opus 独有的服务端工具 `advisor`
/// （`type: advisor_20260301`，没有 `input_schema`）。`TaskStop` 等其余内建工具在延迟清单里，
/// 不在正文声明中。只带 Bash/Edit/Read/Write 是一个
/// 官方不产生的组合，与「零个工具」一样是自证。
/// 那一对**故意不注**：`ToolSearch` 被模型调起来时客户端拿到一个自己没声明的
/// tool_use 且没法执行，而 `DeferredToolPlaceholder` 只是它的占位；两者都不是「工具」。第四块
/// 模板里依赖它的那段指令也一并去掉了（[`config::CC_SYSTEM_REST`]）。`advisor` 是服务端在
/// 模型之外再跑一个模型，要另计费、也改变行为，不替客户端拨这个开关。
///
/// **顺序也是抓包的一部分**：按官方声明序 `Agent → Artifact → AskUserQuestion → Bash → Edit →
/// ListAgents → Read → ReportFindings → ScheduleWakeup → SendFeedback → ShareOnboardingGuide →
/// Skill → Workflow → Write`（官方在 `Skill` 与 `Workflow` 之间还有 `ToolSearch`、`Workflow`
/// 与 `Write` 之间还有 MCP 工具与占位），不是字母序。
///
/// **`eager_input_streaming`**：2.1.277 起四族的 OAuth 主线程每个内建工具都带
/// `eager_input_streaming: true`——fable 也带（2.1.260 时一个都不带），2.1.285 仍如此。资产原样保留，
/// 不另加也不剥。客户端改名后的 `mcp__luban__*` 带不带这个键**没有 OAuth 样本**
/// （`2.1.258-api` 的 `mcp__ide__*` 不带，但那是 API-key 模式；2.1.277 订阅端的 MCP 工具在
/// 延迟池里带 `eager_input_streaming: true` 加 `defer_loading: true`，与正文声明不是一回事），
/// 这里不猜。
///
/// **模型会不会调这些工具**：概率低。客户端的 system prompt 会指名自己的工具
/// （被混淆成 `mcp__luban__*`），模型优先响应 system 的指令。万一调了，客户端收到一个
/// 自己没声明的 tool_use，按协议返回错误 tool_result 即可，不影响会话继续。
///
/// **代价**：资产约 63KB（Artifact 一条就 33KB），每条模拟主线程请求都带，首轮进 prompt cache
/// 之前按输入 token 计费；同一会话后续轮次命中缓存。
static CC_TOOLS_CORE: std::sync::LazyLock<Vec<serde_json::Value>> =
    std::sync::LazyLock::new(|| {
        serde_json::from_str(include_str!("../assets/cc_tools_core.json"))
            .expect("cc_tools_core.json must be a valid JSON array of tool objects")
    });

/// 注入用的工具声明（2.1.285，`cap/2.1.285/00030` 的 20 条里去掉 `ToolSearch` /
/// `DeferredToolPlaceholder` 那一对、用户自己的 `mcp__*` 与服务端工具 `advisor` 之后的 14 条）。
///
/// 2.1.285 各族主线程的内建工具声明**逐字节相同**（`cap/2.1.285` 的 11 个模型，含
/// `eager_input_streaming: true`），故只有一份；2.1.277 / 2.1.280 的 `Artifact`、`Bash` 措辞与
/// 这份不同。2.1.260 时 fable 那份不带 eager、措辞也不同，要单独一份，现在不用了。`profile` 仍收着，将来某族再分家时不必
/// 改调用点。
pub(super) fn cc_tools_core(_profile: &config::CcProfile) -> &'static [serde_json::Value] {
    &CC_TOOLS_CORE
}

/// [`inject_cc_tools`] 会往这条请求里**补**哪几个工具名（客户端没声明的那些）；**不改体**。
///
/// 只给注入那一步用（[`inject_cc_tools`]，此时 `tool_choice` 已归一）。流水**不**拿它预判：
/// 那边读的是来访原文，`tool_choice: "required"` 这类方言还没归一，会算错——流水改按实际出站
/// 体对来访算（[`injected_tools_of`]）。
///
/// **不带工具的来访**（[`declares_no_tools`]：没有 `tools` 键、`tools: null`、`tools: []` 三种
/// 一视同仁）只在 `fill_absent`（开关 `fill_absent_tools`，默认开）开着时按全缺算，且来访的
/// `tool_choice` 是 `any` / 指定工具时不补（[`forces_tool_use`]）。理由：模拟路径只发主线程
/// profile，而官方主线程一条不带工具的样本都没有（`cap/2.1.280` 主线程恒为 19 / 20 个），「主线程
/// 的 beta 与 system、零个工具」是官方不产生的组合。已知代价：这类来访多半是没有工具循环的纯
/// 聊天客户端，模型调了注入的工具时它拿到的是一个处理不了的 `tool_use`（流水 `rewrites` 列打
/// `injected_tool_called`；补了工具的请求本身打 `tools_filled`；算调用率时分子要数**两个标签都有**
/// 的——单数 `injected_tool_called` 还混着自带工具、只被补缺的客户端）；每个新
/// 会话首轮还要按写入价多付约两万 token 的工具声明。
///
/// 返回空的情形：不带工具且开关关着或来访强制调用工具、`tools` 是数组以外的怪值、该 profile
/// 的每个工具名客户端都已声明。
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
    fill_absent: bool,
) -> Vec<&'static str> {
    if declares_no_tools(v) && (!fill_absent || forces_tool_use(v)) {
        return Vec::new();
    }
    let declared: Vec<&str> = match v.get("tools") {
        None | Some(serde_json::Value::Null) => Vec::new(),
        Some(serde_json::Value::Array(tools)) => {
            tools.iter().filter_map(|t| t.get("name")?.as_str()).collect()
        }
        Some(_) => return Vec::new(),
    };
    cc_tools_core(profile)
        .iter()
        .filter_map(|stub| stub.get("name")?.as_str())
        .filter(|name| !declared.contains(name))
        .collect()
}

/// 模拟路径**实际**注进出站体、来访自己没声明的官方工具名：出站 `tools` 里属于该 profile
/// 官方资产（[`cc_tools_core`]）的名字，减去来访 `tools` 里已有的名字。
///
/// 流水按它记 `injected_tool_called` / `tools_filled`，不再拿来访体预判
/// （[`cc_tools_to_inject`]）：来访的 `tool_choice: "required"` / `"any"` / OpenAI 的
/// `{"type":"function"}` 要先被 [`normalize_tool_choice`] 归一成 `any` / `tool`，注入那一步才
/// 据此不补；读来访原文会把这类请求误记成「补了」。官方工具名不参与假名混淆，出站里认得出。
pub(super) fn injected_tools_of(
    inbound: &serde_json::Value,
    outbound: &serde_json::Value,
    profile: &config::CcProfile,
) -> Vec<&'static str> {
    let names = |v: &serde_json::Value| -> Vec<String> {
        v.get("tools")
            .and_then(|t| t.as_array())
            .map(|a| a.iter().filter_map(|t| t.get("name")?.as_str().map(str::to_owned)).collect())
            .unwrap_or_default()
    };
    let (declared, sent) = (names(inbound), names(outbound));
    cc_tools_core(profile)
        .iter()
        .filter_map(|stub| stub.get("name")?.as_str())
        .filter(|n| sent.iter().any(|s| s == n) && !declared.iter().any(|d| d == n))
        .collect()
}

/// 来访一个工具都没声明：没有 `tools` 键、`tools: null` 或 `tools: []`。三种在上游眼里都是
/// 「没有工具」，开关 `fill_absent_tools` 对它们一视同仁（[`cc_tools_to_inject`]）。
pub(super) fn declares_no_tools(v: &serde_json::Value) -> bool {
    match v.get("tools") {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::Array(a)) => a.is_empty(),
        Some(_) => false,
    }
}

/// 来访的 `tool_choice` 要求模型**必须**调工具（`any` 或指定某个工具）。配上「一个工具都没
/// 声明」这条请求本来就不成立（上游 400）；替它补官方工具等于逼模型去调 Bash 之类，把客户端
/// 自己的错误换成一次它接不住的工具调用。`auto` / `none` 不算。
fn forces_tool_use(v: &serde_json::Value) -> bool {
    matches!(
        v.get("tool_choice").and_then(|c| c.get("type")).and_then(|t| t.as_str()),
        Some("any" | "tool")
    )
}

/// 这条请求是谁发的——**只用于日志**，让「某个客户端的 Bash 被换成官方声明了」对得回是谁。
///
/// 模拟路径上能拿到的身份就这两项：服务它的凭证，与 luban 给它派生的会话 id（流水里
/// `cc_session` 记的是同一个串，据此能把那一行日志翻回具体哪条请求）。来访 UA 在这儿拿不到
/// ——出站头里那个已经是伪装成官方 CC 的那份了。
#[derive(Clone, Copy)]
pub(super) struct ToolAlignWho<'a> {
    pub(super) cred_id: i64,
    pub(super) cred: &'a str,
    pub(super) session: &'a str,
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

/// 把该 profile 的 14 个官方主线程工具对齐进 `tools`：出站列表**以这 14 条按官方声明序开头**，
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
/// Agent 前面，14 条的相对次序就不是官方的了。官方的内建工具是一段固定次序，MCP 工具跟在
/// 最后（`cap/2.1.258-api/00006`：`Write` 之后才是 `mcp__ide__*`），这里照这个形态排。
///
/// **同名声明里显式写的 `eager_input_streaming` 保留**：那是客户端的设置（true 或 false 都是），
/// 换成官方对象时不能顺手抹掉（fable 资产没有这个键）或改成资产的值（opus 资产是 true）。
/// 与 [`fill_eager_tools`]「已有值不覆盖」是同一条约定，只是这里发生在替换那一步。其余字段
/// 一律取资产的。
///
/// **换不换不看 JSON 值相等**：`Value` 的相等忽略对象键序，客户端一条内容全同、键序不同的
/// 声明会被当成「已经是官方的」跳过，出站就不是逐字节的官方声明了。故 14 条一律以资产对象
/// 落位，「有没有变」按紧凑序列化的字节比——这只影响日志计数与 [`rewrite_body`] 那条
/// 「什么都没改就原样透传」的快路。
fn inject_cc_tools(
    v: &mut serde_json::Value,
    profile: &config::CcProfile,
    fill_absent: bool,
    who: ToolAlignWho<'_>,
) -> bool {
    // 不带工具、且开关关着或来访强制调工具：整条不动。闸必须落在这里而不只在
    // [`cc_tools_to_inject`] 里——下面的对齐不看缺哪几个，只要有 `tools` 数组就把 14 条排到开头，
    // `tools: []` 会绕过那道判据被补满。
    if declares_no_tools(v) && (!fill_absent || forces_tool_use(v)) {
        return false;
    }
    let missing = cc_tools_to_inject(v, profile, fill_absent);
    // 没带 `tools` 键或是 `null`、又要补：先按官方键序放一个空数组（`system` 之后，没有
    // `system` 就跟 `messages`；`null` 原位换掉），下面照「全缺」对齐。补不出东西时不动。
    if !missing.is_empty() && v.get("tools").is_none_or(|t| t.is_null()) {
        insert_top_level(v, "tools", serde_json::json!([]), &["model", "messages", "system"]);
    }
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

    // **warn 而不是 info**：这一行标的是「这条请求换完之后可能执行不了」——模型按官方 schema
    // 拼的入参，客户端那条声明不一定认。混在下面那条计数 info 里会被当成例行输出刷过去。
    if !surface_differs.is_empty() {
        // 模型名到这儿才读：`tools` 的可变借用刚随上一行结束，而这条日志本来就是少数派，
        // 不必为它在每条模拟请求上都拷一个串。
        let model = v.get("model").and_then(|m| m.as_str()).unwrap_or("-");
        tracing::warn!(
            cred_id = who.cred_id, cred = %who.cred, session = %who.session, %model,
            tools = %surface_differs.join(","),
            "replaced same-named client tools whose parameter surface differs from the official one; \
             the model will fill the official schema, which this client may not accept"
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
///
/// **指令式写法不算空壳**（[`is_system_directive`]）：`content: []` 带消息级 `output_config`，
/// 2.1.285 官方就这么发（`{"role":"system","output_config":{"effort":"high"},"content":[]}`），
/// 上游明说它「放在任何位置都收」。删掉它就是把客户端中途调的 effort 一并删了。
pub(super) fn drop_empty_system_messages(v: &mut serde_json::Value) -> bool {
    let Some(msgs) = v.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return false;
    };
    let total = msgs.len();
    let mut dropped: Vec<String> = Vec::new();
    for (i, msg) in msgs.iter().enumerate() {
        if is_empty_system_shell(msg) {
            dropped.push(format!("{i}/{total}"));
        }
    }
    if dropped.is_empty() {
        return false;
    }
    msgs.retain(|msg| !is_empty_system_shell(msg));
    tracing::info!(
        count = dropped.len(),
        at = %dropped.join(", "),
        "dropped empty role:\"system\" messages: upstream rejects a system message with no content blocks"
    );
    true
}

/// 出站时会被 [`drop_empty_system_messages`] 丢掉的那种 `role:"system"` 空壳：`content` 缺失、
/// `null`、空串、空数组，或整条只有空 `text` 块；[`is_system_directive`] 除外。
pub(super) fn is_empty_system_shell(msg: &serde_json::Value) -> bool {
    if msg.get("role").and_then(|r| r.as_str()) != Some("system") || is_system_directive(msg) {
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
}

/// 指令式 `role:"system"`：`content` 恰为空数组、且带消息级 `output_config`。
///
/// 上游原话（2026-10-02 线上 400）：`the directive-only form (content: [] with output_config)
/// is accepted at any position`。口径逐字照搬——空串、缺 content 的不算，上游没说收。
pub(super) fn is_system_directive(msg: &serde_json::Value) -> bool {
    msg.get("role").and_then(|r| r.as_str()) == Some("system")
        && msg.get("content").and_then(|c| c.as_array()).is_some_and(|a| a.is_empty())
        && msg.get("output_config").is_some()
}

/// 对话中途 `role:"system"` 摆错位置时上游那句 400 的原话，`{}` 处是消息下标。
///
/// 2026-10-02 线上实测（`claude-opus-5-5`）：
/// ```text
/// messages.1: role 'system' must precede an 'assistant' message or end the array; the
/// directive-only form (content: [] with output_config) is accepted at any position
/// ```
fn misplaced_system_message(index: usize) -> String {
    format!(
        "messages.{index}: role 'system' must precede an 'assistant' message or end the array; \
         the directive-only form (content: [] with output_config) is accepted at any position"
    )
}

/// 对话中途的 `role:"system"` 摆错了位置 → 上游那句原话（见 [`misplaced_system_message`]）。
///
/// 规则是上游报错自己写明的：中途的 system 必须紧挨在 assistant 之前，或者是数组最后一条；
/// 指令式写法（[`is_system_directive`]）放哪儿都行。官方抓包（2.1.260–2.1.285，七百余条中途
/// system）与之吻合，并补了一条：**连续几条 system 算一段**——`user → system →
/// system(clear_at) → assistant` 官方常发、上游收，所以判的是「这一段之后的第一条非 system」。
///
/// **只拒确定无疑的**：段后紧跟的是 `user` 才算错（`tool` 之类别的 role 交给上游去说）；段里
/// 夹着指令式 system 时不拒——「普通 system → 指令 → user」上游收不收没有实测，宁可放过去
/// 让上游判。出站会被丢掉的空壳（[`is_empty_system_shell`]）不算数，它们到不了上游。
///
/// 首条 user/assistant 之前的不归这里（[`first_turn_index`]）：上游回的是另一句，由
/// [`find_openai_marker`] 或提升（[`hoist_system_role_messages`]）处理。
pub(super) fn misplaced_system_role(body: Option<&serde_json::Value>) -> Option<String> {
    let msgs = body?.get("messages")?.as_array()?;
    let role = |m: &serde_json::Value| m.get("role").and_then(|r| r.as_str()).map(str::to_owned);
    let mut i = first_turn_index(msgs);
    while i < msgs.len() {
        if role(&msgs[i]).as_deref() != Some("system") {
            i += 1;
            continue;
        }
        // 这一段连续 system 是 [i, end)。
        let end = (i..msgs.len())
            .find(|&j| role(&msgs[j]).as_deref() != Some("system"))
            .unwrap_or(msgs.len());
        let run = &msgs[i..end];
        let followed_by_user = end < msgs.len() && role(&msgs[end]).as_deref() == Some("user");
        if followed_by_user && !run.iter().any(is_system_directive) {
            // 点名段里第一条会真正送到上游的；整段全是空壳则出站后这段不存在。
            if let Some(k) = run.iter().position(|m| !is_empty_system_shell(m)) {
                return Some(misplaced_system_message(i + k));
            }
        }
        i = end;
    }
    None
}

/// `messages` 里首条 user/assistant 的下标（一条都没有时为长度）。
///
/// 在它之前的 `role:"system"` 是 OpenAI 那种开头系统提示词，上游恒 400（`messages.0: use the
/// top-level 'system' parameter for the initial system prompt`）；在它之后的上游是认的
/// （2026-09-30 实测：新模型 200，老模型回 `role 'system' is not supported on this model`）。
/// 判定（[`find_openai_marker`]）与形态记忆（`learned_rules::role_values`）共用这一条，
/// 口径不许分叉。
pub(super) fn first_turn_index(msgs: &[serde_json::Value]) -> usize {
    msgs.iter()
        .position(|m| matches!(m.get("role").and_then(|r| r.as_str()), Some("user" | "assistant")))
        .unwrap_or(msgs.len())
}

/// 出站前 [`hoist_system_role_messages`] 会不会跑：`hoist_system_role` 开着、`reject_openai_shape`
/// **关着**、且来访不是 CC 形态。
///
/// 严格检查开着时提升整个不跑：开头那段 system 在入口就被 [`find_openai_marker`] 拒了，能走到
/// 这里的只剩对话中途的，那是上游认的原生 system 消息（带着 `clear_at`、消息级 `output_config`
/// 这类只在原位才有意义的字段，作用范围也从它所在的位置起算），挪到顶层就改了语义。
///
/// 改写（[`rewrite_body_out`]）与入口处的形态记忆豁免（`known_shape_rejection` 的
/// `system_hoisted`）都从这里取，口径不许分叉；后者另外还要叠上 billable——非计费路径
/// （`count_tokens` 等）出站根本不改写。
pub(super) fn hoists_system_role(flags: &store::ForwardFlags, cc_shaped: bool) -> bool {
    flags.hoist_system_role && !flags.reject_openai_shape && !cc_shaped
}

/// 把 `messages` 里 `role:"system"` 的消息提升到顶层 `system` 字段。
///
/// litellm 等第三方客户端采用 OpenAI 格式，把 system 指令放在 `messages` 数组里
/// （`{"role":"system","content":"..."}`）。上游对开头那段恒 400（见 [`first_turn_index`]），
/// 跟在 user 之后的新模型认、老模型不认（`role 'system' is not supported on this model`）。
///
/// **对话中途的也一并提升**：挪到开头会改变它的位置，但那是修补路径本来的取舍——留在原位，
/// 老模型上就是一条修得好却没修的 400，还会被 [`remember_shape_rejection`] 学成规则，
/// 之后同模型带 system 的请求全在本地拒掉。
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
    /// 回程还原：假名 → 真名。**一趟扫完**，靠所有假名共用的 [`FAKE_TOOL_NS`] 定位。
    ///
    /// **按字节而不是按 `str` 做**：回程是流式的，一个 chunk 可以在任意字节处切断，
    /// `String::from_utf8` 会在半个多字节字符上失败。假名全是 ASCII，字节级替换在 UTF-8 上
    /// 安全（ASCII 不会出现在多字节序列内部）。
    ///
    /// **为什么不是「每个假名扫一遍」**：那是这条路原先的写法，代价随工具数线性翻倍，而且
    /// 每一遍都重新分配一个 `Vec`、还逐字节 `push`。回程的每一个 SSE 块都要过它——实测 2000
    /// 个事件的一次回答，5 个工具要 7.8ms，20 个要 30ms，50 个要 40ms，全部落在流式转发路径
    /// 上，直接变成回复的整体延迟。所有假名都以 `mcp__luban__` 开头（[`build_tool_name_map`]
    /// 唯一的生成处），所以「可能是假名的位置」一趟就能扫出来，命中才逐个比对候选。
    ///
    /// 语义与逐个替换完全一致：`reverse` 按假名长度倒序，同一位置上先命中的就是最长的那个。
    /// 还原后的内容不再被回扫，故也不再依赖「真名里不含假名」这个前提。
    pub(super) fn restore(&self, buf: &[u8]) -> Vec<u8> {
        let ns = FAKE_TOOL_NS.as_bytes();
        let mut out = Vec::with_capacity(buf.len());
        let mut i = 0;
        while let Some(rel) = find_sub(&buf[i..], ns) {
            let at = i + rel;
            out.extend_from_slice(&buf[i..at]);
            match self.reverse.iter().find(|(fake, _)| buf[at..].starts_with(fake.as_bytes())) {
                Some((fake, real)) => {
                    out.extend_from_slice(real.as_bytes());
                    i = at + fake.len();
                }
                // 命名空间前缀出现了，后面却不是我们发出去的任何一个假名（客户端自己就有
                // `mcp__luban__*` 工具，或正文里恰好提到）：原样留着往下找。跳过整个前缀是
                // 安全的——`mcp__luban__` 没有既是前缀又是后缀的真边界，故两处出现不可能重叠。
                None => {
                    out.extend_from_slice(ns);
                    i = at + ns.len();
                }
            }
        }
        out.extend_from_slice(&buf[i..]);
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

/// `haystack` 里第一次出现 `needle` 的位置。
///
/// 先按首字节筛（`position` 在 `&[u8]` 上是一条紧循环，编译器还能向量化），命中再整段比对。
/// 回程每个流块都要过一遍，逐字节推进的写法在这里是实打实的热点。
fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    let last = haystack.len() - needle.len();
    let first = needle[0];
    let mut i = 0;
    while i <= last {
        let off = haystack[i..=last].iter().position(|&b| b == first)?;
        let at = i + off;
        if haystack[at..].starts_with(needle) {
            return Some(at);
        }
        i = at + 1;
    }
    None
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
///
/// 只走 API 认 `cache_control` 的位置（[`cache_slots`]）：工具 schema 里叫 `cache_control` 的
/// 参数定义、`tool_use.input` 里的同名字段是业务数据，补上 `ttl` 就改了来访的 schema / 入参。
fn fill_cache_ttl(v: &mut serde_json::Value) -> bool {
    let mut changed = false;
    for slot in cache_slots(v) {
        let Some(cc) = slot
            .resolve(v)
            .and_then(|o| o.get_mut("cache_control"))
            .and_then(|c| c.as_object_mut())
        else {
            continue;
        };
        if cc.get("ttl").and_then(|t| t.as_str()) == Some("1h") {
            continue;
        }
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
fn ensure_cc_system_prefix(v: &mut serde_json::Value, version: Option<&str>) -> bool {
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
    let mut prefix = vec![text_block_bare(&billing_header_text(v, version))];
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
mod tests;
