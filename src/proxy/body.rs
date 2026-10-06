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

mod billing;
mod cache;
mod cch;
mod fallbacks;
mod identity;
mod inspect;
mod params;
mod shape;
mod thread;
mod tool_names;
mod tools;
mod version;

pub(super) use billing::*;
pub(crate) use cache::*;
pub(super) use cch::*;
pub(super) use fallbacks::*;
pub(crate) use identity::*;
pub(crate) use inspect::*;
pub(super) use params::*;
pub(super) use shape::*;
pub(super) use thread::*;
pub(super) use tool_names::*;
pub(super) use tools::*;
pub(crate) use version::*;

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
                s.trim_tools,
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

#[cfg(test)]
mod tests;
