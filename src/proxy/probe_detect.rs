//! 探针 / 探活请求识别与本地应答：不透传到上游，就地回一条最小的正常回复。

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;

use super::learned_rules::SSE_CONTENT_TYPE;
use super::simulation::{
    cc_identity_blocks, field_is_empty, is_official_classifier_request, is_official_helper_request,
    is_official_thread_continuation, is_official_title_request,
};
use super::{error_response, is_quota_probe_shaped, request_max_tokens};

/// [`probe_signature`] 命中的哪一条判据。日志与错误消息按类写清楚，运维一眼能看出拦的是什么。
///
/// 身份格式不对（device 不是 64 位 hex、session 不是 uuid）**不在**这里：那是「抄错了」而不是
/// 探针，交给模拟路径重建身份（[`cc_identity_well_formed`](crate::proxy::cc_identity_well_formed) 决定它进不了透传），不拒。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ProbeKind {
    /// 单句 ping：无 tools、恰好 1 条消息、`max_tokens` 在 `2..=16`（带不带 system 都算）。
    Ping,
    /// 1 token 的探活：无 system、无 tools、恰好 1 条用户消息、`max_tokens=1`，且不可能是官方
    /// 那两种 1 token 形态——UA 不是可信的 Claude Code，或可信却既没带 device_id 也不是额度
    /// 探测。官方的 cache 预热与额度探测都来自可信 UA 且带身份；下游中转的健康检查
    /// （`Go-http-client` 每 5 分钟一条）正是这个形态，此前走模拟被补上 10KB 基座发了出去。
    OneTokenPing,
    /// 凭空冒出的一次性对话：有 system、无 tools、恰好 1 条消息、不是官方那两种无 tools
    /// 形态、且设备从没见过。
    ThrowawayConversation,
    /// system 里 CC 身份句出现在**不止一块**里。官方只写一次。
    DuplicateIdentity,
    /// 短开场（严格模式，[`store::ForwardFlags::reject_probes_strict`](crate::store::ForwardFlags::reject_probes_strict)）：无 system、无 tools、
    /// 恰好 1 条用户消息、正文不超过 [`PROBE_SHORT_OPENER_BYTES`] 字节、`max_tokens != 1`。
    /// 测活脚本发的正是「hi」「ping」「test」。
    ShortOpener,
}

/// [`ProbeKind::ShortOpener`] 的正文上限，按 UTF-8 **字节**算：英文 32 个字符，中文约十个字。
/// 「hi」「ping」「test」「你好」「测试」都在几个字节内；按字符算会把三十个汉字的一整句话也
/// 算进去，那已是正常的单轮对话。
const PROBE_SHORT_OPENER_BYTES: usize = 32;

impl ProbeKind {
    /// 进日志的短名。
    pub(crate) fn tag(self) -> &'static str {
        match self {
            Self::Ping => "ping",
            Self::OneTokenPing => "one-token-ping",
            Self::ThrowawayConversation => "throwaway-conversation",
            Self::DuplicateIdentity => "duplicate-identity",
            Self::ShortOpener => "short-opener",
        }
    }

    /// 进日志的原因说明：拦的是哪一条判据。0.3.101 起探针回的是一条正常的 200
    /// （[`probe_reply`]），这段话不再进响应体——探活看到的是「健康」，看不出被拦过。
    pub(super) fn message(self) -> &'static str {
        match self {
            Self::Ping => {
                "automated probe / health-check requests are not forwarded: single-message ping with max_tokens in 2..=16"
            }
            Self::OneTokenPing => {
                "automated probe / health-check requests are not forwarded: single-message max_tokens=1 request with no system prompt and no tools that cannot be an official Claude Code prewarm or quota probe"
            }
            Self::ShortOpener => {
                "automated probe / health-check requests are not forwarded: a single one-line user message with no system prompt and no tools"
            }
            Self::ThrowawayConversation => {
                "automated probe / health-check requests are not forwarded: first request from an unknown device is a tool-less single-message conversation"
            }
            Self::DuplicateIdentity => {
                "automated probe / health-check requests are not forwarded: the Claude Code identity sentence appears in more than one system block"
            }
        }
    }
}

/// 探针 / 探活请求的强特征；命中返回是哪一条（[`ProbeKind`]），没命中 `None`。
///
/// 背景是一次真实封号（`ban.log`）。被封的号一天里收到三类探活，全都自报 CC 的 UA、
/// 带官方那两块 system（billing header + 身份句），却在别处露馅：
///
/// | 类 | 形态 | 露馅处 |
/// |---|---|---|
/// | 每 15 分钟一条的 haiku | 3 块 system `[70, 57, 57]`、无 `tools`、1 条消息、`max_tokens=2`、36 token | 官方从不在带 system 的请求上给个位数的 `max_tokens`（预热那条 `max_tokens=1` **没有** system） |
/// | 封前 8 分钟的 21 条 | 同样 3 块 system、无 `tools`/`thinking`、1 条消息、`temperature=1`、每条一个新 device_id + 新 session_id | 官方一台新机器的第一条是预热（无 system）或主请求（带 tools），从不是「没有 tools 的一句话对话」 |
/// | `sdk-cli` 每 20 分钟一条 | `device_id`=`channel-test`、`session_id`=`channel-test-claude-code` | 官方 device_id 恒为 64 位 hex、session_id 恒为 uuid（`cap/` 37 份样本无一例外） |
///
/// 第三类**不在这里拒**：身份写错是「抄错了」，不是探针形态。它由 [`cc_identity_well_formed`](crate::proxy::cc_identity_well_formed)
/// 挡在透传门外、走模拟——模拟路径本来就会剥掉来访的 `metadata.user_id`，用凭证 + 平台指纹
/// 重建一份合法身份，那个 `channel-test` 到不了上游。代价是这类探活换了身合法的皮照常放行；
/// 它 `max_tokens=256`、设备恒定，也不命中下面三条。这是有意的取舍：只拒探针，不拒抄错的。
///
/// 三条判据，**任一命中即算**，**不限 UA**（0.3.99 起；此前只对自报 CC 的 UA 生效，非 CC 的
/// 探活走模拟被装成官方形态发了出去，见 `ban/luban-ban-37/38/42`）：
///
/// - **单句 ping**（[`ProbeKind::Ping`]）：没有 tools、恰好 1 条消息、`max_tokens` 在 `2..=16`，
///   不要求带 system。下界排除官方 cache 预热（`max_tokens=1`；2.1.187 Claude Desktop 的预热带
///   `[billing, identity]` 两块 system，见 `ban.log`），上界远低于官方带 system 的最小值 64
///   （补全建议）；官方也没有任何一种「无 system、`max_tokens` 在 2 到 16」的请求。
///   严格模式（`strict`，[`store::ForwardFlags::reject_probes_strict`](crate::store::ForwardFlags::reject_probes_strict)）下 ping 不再要求无
///   tools，并多一条**短开场**（[`ProbeKind::ShortOpener`]），见函数体内的说明；
/// - **1 token 探活**（[`ProbeKind::OneTokenPing`]）：无 system、无 tools、恰好 1 条消息、
///   `max_tokens=1`，且**不可能是官方的**：`from_cc_client` 为假（UA 不是可信的 Claude Code），
///   或 UA 可信却没带 device_id 且不是额度探测形态（[`is_quota_probe_shaped`]）。上一条把 1
///   排除是为了放过官方的 cache 预热与额度探测，但那两种都来自可信 UA 且带身份——非 CC UA
///   的 1 token 请求不可能是它们。ban 导出里这一形态 138 条（`Go-http-client` / `python-httpx`，
///   一个号每 5 分钟一条、跨两天），此前全部走模拟、被补成三块 system 加 10622 字节基座发了
///   出去：一条「带完整基座、不带 tools、只要 1 个 token」的请求官方从不产生。UA 可信且带
///   device_id 的 1 token 请求这里不碰，它是桌面端预热，由模拟路径按形态放行；
/// - **凭空冒出的一次性对话**（[`ProbeKind::ThrowawayConversation`]）：有 system、没有
///   tools、恰好 1 条消息、`max_tokens != 1`、不是官方那三种无 tools 请求、且设备从没见过
///   （[`store::CredentialStore::device_is_known`](crate::store::CredentialStore::device_is_known)）。官方无 tools 的请求只有四种：预热
///   （`max_tokens=1`）、Helper（[`is_official_helper_request`]）、标题生成
///   （[`is_official_title_request`]）、安全分类（[`is_official_classifier_request`]）；后三种
///   按 system 结构 + beta 头 + body 取值**逐项**对，不是「带了某个字段就放」。标题生成必须在
///   豁免里：它在用户第一句话后与主请求**并发**发出，一台全新设备上若它先到，设备还没绑定，
///   不豁免就会把一条官方请求当成一次性对话拒掉。老设备发同样的形态不算：那可能是某个真
///   客户端的边角请求；
/// - **身份句重复**（[`ProbeKind::DuplicateIdentity`]）：system 里含 CC 身份句的块不止一个。
///   官方只写一次；`ban.log` 那份 `[70, 57, 57]` 的第三块与身份句等长，多半就是它又抄了一遍。
///   这条不看设备，正好补上「已知设备发同形态」时第三条够不着的口子。
///
/// **「没有 tools」按值算，不按键算**：`tools` 缺失、`null`、`[]` 都是没有；`thinking` 为
/// `null`、`stop_sequences` 缺失/`null`/`[]` 同样按没有算。否则加一个空字段就绕过去了。
///
/// **边界要说清**：形态判据拦得住现在这批和它们的简单变种。把官方 haiku 工具调用的形态
/// 逐字抄全（`tools: []` + `thinking` 对象 + `max_tokens: 32000`）的探针，形态上就是一条官方
/// 请求，这里分不出来——那时剩下的信号只有行为（身份轮换、节奏），不在本函数范围内。
///
/// 只看形态与身份、一条就判，不做任何计数——所以不存在「合法流量大了会误伤」的问题。
pub(crate) async fn probe_signature(
    body: Option<&serde_json::Value>,
    device_id: Option<&str>,
    beta: &[String],
    from_cc_client: bool,
    strict: bool,
    // 只在走到「一次性对话」那条判据时才 await，别的判据命中或不适用时不查库。
    device_known: impl std::future::Future<Output = bool>,
) -> Option<ProbeKind> {
    let v = body?;
    // 身份句重复：不依赖下面「无 tools / 单条消息」的前提，先判。
    if cc_identity_blocks(v) > 1 {
        return Some(ProbeKind::DuplicateIdentity);
    }
    // 其余几条都要求恰好一条（用户）消息；除严格模式的 ping 外还要求没有 tools。
    let has_system = v.get("system").is_some_and(|s| !s.is_null());
    let no_tools = field_is_empty(v.get("tools"));
    let single_message = v
        .get("messages")
        .and_then(|m| m.as_array())
        .is_some_and(|m| m.len() == 1 && m[0].get("role").and_then(|r| r.as_str()) == Some("user"));
    if !single_message {
        return None;
    }
    let max_tokens = request_max_tokens(Some(v));
    // 严格模式（[`store::ForwardFlags::reject_probes_strict`]，默认关）多收两刀，覆盖复盘里
    // 默认判据放过去的 Go-http-client 探活：
    // - ping 不再要求无 tools：`max_tokens` 不超过 16 装不下一次 tool_use 调用，带着工具只给
    //   16 个 token 只能是测活（复盘里 4 个 tools、max_tokens 16 那批）；
    // - 短开场：无 system、无 tools、一条不超过 32 字节的用户消息、`max_tokens != 1`（复盘里
    //   max_tokens 50 / 1024 / 32000、正文一句「hi」那三批）。官方没有任何一种无 system 的请求
    //   （额度探测是 max_tokens 1，已排除）。代价是真人用裸聊天客户端经中转站发的第一句
    //   「你好」也会被拒，故默认关。
    if strict {
        if max_tokens.is_some_and(|m| (2..=16).contains(&m)) {
            return Some(ProbeKind::Ping);
        }
        if !has_system
            && no_tools
            && max_tokens != Some(1)
            && single_user_text_bytes(v).is_some_and(|n| n <= PROBE_SHORT_OPENER_BYTES)
        {
            return Some(ProbeKind::ShortOpener);
        }
    }
    if !no_tools {
        return None;
    }
    // ping **不要求带 system**：官方没有任何一种「无 system、max_tokens 在 2 到 16」的请求
    // （额度探测恒为 `max_tokens=1`），而不带身份的下游（Go-http-client）的探活恰恰多半不带
    // system——封号复盘里它们每 3 到 6 秒一批、四五个模型一起问一句，此前因为 UA 不是
    // claude-cli 一律不判、全走模拟发了出去。
    //
    // **有意不按 UA、身份或正文长短收窄**（复审曾提出「普通 SDK 也可能合法用 max_tokens: 8」）：
    // 这里的下游是中转站与 Claude Code 客户端，四份封号导出里这一形态每个号 160 到 210 条、
    // 无一条是分类业务；而放行的代价不只是多一条请求——它会被模拟成带基座的官方形态发出去，
    // 正是「一台设备只问一句话」的封号判据。这类探活就不该到上游，误伤的分类请求收到的是
    // 一条写明原因的 403。
    if max_tokens.is_some_and(|m| (2..=16).contains(&m)) {
        return Some(ProbeKind::Ping);
    }
    // 1 token 探活。`has_system` 按「键存在且非 null」算：桌面端预热有一种带一块几百字节应用块的
    // 形态，这里不碰它；官方两种无 system 的 1 token 形态（额度探测、无 system 的桌面端预热）
    // 都来自可信 UA 且带身份，所以 UA 不可信的一律算，UA 可信的只在「没带 device_id 且不是额度
    // 探测」时算——那样一条什么身份都没有的 1 token 请求，模拟路径本来也按第三方处理。
    if max_tokens == Some(1)
        && !has_system
        && (!from_cc_client || (device_id.is_none() && !is_quota_probe_shaped(v)))
    {
        return Some(ProbeKind::OneTokenPing);
    }
    // 一次性会话那条仍要求带 system：不带 system 只问一句的第三方小应用太常见，按它拒会误伤。
    if !has_system {
        return None;
    }
    // 官方那三种无 tools 请求的**完整**样子，见 [`is_official_helper_request`]、
    // [`is_official_title_request`]、[`is_official_classifier_request`]：system 结构、beta 头、
    // body 取值逐项对，不是「带了某个字段就放」，也不是只对 body 那几个字段。
    // 第四种是 2.1.277 起 message-threads 的续轮（[`is_official_thread_continuation`]，六项逐项
    // 对）：只发新增的那一条消息、`system` 只剩 billing header、不带 `tools`（`cap/2.1.277/00051`
    // 那 25 条子代理续轮全是这个样子）——正是「有 system、没 tools、一条消息」，设备第一次见到时
    // 会撞上这一刀。
    if max_tokens != Some(1)
        && !is_official_helper_request(v, beta)
        && !is_official_title_request(v, beta)
        && !is_official_classifier_request(v, beta)
        && !is_official_thread_continuation(v, beta)
        && device_id.is_some()
        && !device_known.await
    {
        return Some(ProbeKind::ThrowawayConversation);
    }
    None
}

/// 唯一那条用户消息的正文 UTF-8 字节数（`content` 是字串，或全部 text 块拼起来，各自去掉首尾
/// 空白）；没有文本块、或带了非文本块（图片、文档）返回 `None`——带附件的不是「一句话」，
/// 短开场判据不认。
fn single_user_text_bytes(v: &serde_json::Value) -> Option<usize> {
    let msgs = v.get("messages")?.as_array()?;
    let content = msgs.first()?.get("content")?;
    match content {
        serde_json::Value::String(s) => Some(s.trim().len()),
        serde_json::Value::Array(blocks) => {
            let mut n = 0;
            for b in blocks {
                if b.get("type").and_then(|t| t.as_str()) != Some("text") {
                    return None;
                }
                n += b.get("text").and_then(|t| t.as_str())?.trim().len();
            }
            Some(n)
        }
        _ => None,
    }
}

/// 探针命中时本地回给客户端的正文（[`probe_reply`]）。探活脚本只看状态码与「有没有回话」，
/// 一个字就够；严格模式下被短开场判据拦住的真人看到的也是它，下一句正常长度的话照常放行。
pub(crate) const PROBE_REPLY_TEXT: &str = "OK";

/// 上游 Message id 与 `request-id` 用的字母表（base58：没有 `0`、`O`、`I`、`l`）。
const BASE58: &[u8] = b"123456789ABCDEFGHJKLMNPQRSTUVWXYZabcdefghijkmnopqrstuvwxyz";

/// 照上游的规则造一个 id：`<前缀>_0` + 23 位 base58，编的是「毫秒时间戳 << 80 | 80 位随机数」。
///
/// 规则是从流水里 401 条上游 `request-id` 对着请求时刻解出来的：`req_011CfyP3gz2FBSzQgQpVDqsa`
/// 去掉 `req_0` 后按 base58 解成整数，右移 80 位就是上游收到请求那一刻的 Unix 毫秒（与流水时间
/// 只差请求耗时）。所以今天的 id 都是 `011Cfy…` 开头、随时间递增——纯随机的后段在一串真 id 里
/// 一眼就能挑出来。`Message` id 同一个规则，时刻是开始出字那一刻（比 `request-id` 晚几十到两百
/// 毫秒，见 [`probe_reply`]）。
fn upstream_like_id(prefix: &str, unix_ms: u64) -> String {
    use rand::RngExt;
    let mut n = (u128::from(unix_ms) << 80) | (rand::rng().random::<u128>() & ((1 << 80) - 1));
    let mut digits = [BASE58[0]; 23];
    for d in digits.iter_mut().rev() {
        *d = BASE58[(n % 58) as usize];
        n /= 58;
    }
    format!("{prefix}_0{}", String::from_utf8_lossy(&digits))
}

/// 照 Cloudflare 的规则造 `cf-ray` 的十六进制段：高 36 位是「Unix 时间（10 毫秒为单位）减
/// 135 000 000 000」，低 28 位随机。1806 份抓包（`cap/auto-2.1.29*`，`Date` 头对 `cf-ray`）全部
/// 落在这条式子上，误差在 `Date` 那一秒的精度以内。
fn cf_ray_hex(unix_ms: u64) -> String {
    use rand::RngExt;
    let ticks = (unix_ms / 10).saturating_sub(135_000_000_000);
    let low = rand::rng().random::<u64>() & ((1 << 28) - 1);
    format!("{:016x}", (ticks << 28) | low)
}

/// `n` 位小写十六进制随机串（`traceresponse` 的两段）。
fn random_hex(n: usize) -> String {
    use rand::RngExt;
    let mut rng = rand::rng();
    (0..n).map(|_| char::from(b"0123456789abcdef"[rng.random_range(0..16)])).collect()
}

/// 官方 `/model` 预热在一个模型族上的回包取值：上游回的 `model`（别名解析成带日期的全名）、
/// `input_tokens`，以及旧一代回的那一个字。
struct PrewarmReply {
    prefix: &'static str,
    /// 来访写的正是这个别名时上游回的全名；`None` 即原样回来访写的那个。
    full_name: Option<&'static str>,
    input_tokens: u64,
    /// 旧一代（opus-4-6 及更早）：预热回一个可见的字，`usage` 里也没有 `output_tokens_details`。
    /// 新一代为 `None`：上游回空正文、有 `output_tokens_details`，正文用 [`PROBE_REPLY_TEXT`]——
    /// 与改写成预热那条路给空正文补的同一句。
    old_gen_text: Option<&'static str>,
}

/// 各模型族的预热回包，逐个对过 `cap/auto-2.1.296-20261013-prewarm` 与
/// `cap/auto-2.1.29{3,6}-20261010-model` 里的真实回包。按前缀匹配，**具体的排在前面**：
/// `claude-opus-5-5` 必须先于 `claude-opus-5`。
const PREWARM_REPLIES: &[PrewarmReply] = &[
    PrewarmReply {
        prefix: "claude-opus-5-5",
        full_name: None,
        input_tokens: 35,
        old_gen_text: None,
    },
    PrewarmReply {
        prefix: "claude-sonnet-5-5",
        full_name: None,
        input_tokens: 35,
        old_gen_text: None,
    },
    PrewarmReply {
        prefix: "claude-haiku-5-5",
        full_name: None,
        input_tokens: 35,
        old_gen_text: None,
    },
    PrewarmReply {
        prefix: "claude-fable-5-1",
        full_name: None,
        input_tokens: 35,
        old_gen_text: None,
    },
    PrewarmReply { prefix: "claude-opus-5", full_name: None, input_tokens: 33, old_gen_text: None },
    PrewarmReply {
        prefix: "claude-sonnet-5",
        full_name: None,
        input_tokens: 33,
        old_gen_text: None,
    },
    PrewarmReply {
        prefix: "claude-fable-5",
        full_name: None,
        input_tokens: 33,
        old_gen_text: None,
    },
    PrewarmReply {
        prefix: "claude-opus-4-8",
        full_name: None,
        input_tokens: 33,
        old_gen_text: None,
    },
    PrewarmReply {
        prefix: "claude-opus-4-7",
        full_name: None,
        input_tokens: 38,
        old_gen_text: None,
    },
    PrewarmReply {
        prefix: "claude-opus-4-6",
        full_name: None,
        input_tokens: 23,
        old_gen_text: Some("Hi"),
    },
    PrewarmReply {
        prefix: "claude-sonnet-4-6",
        full_name: None,
        input_tokens: 23,
        old_gen_text: Some("Hi"),
    },
    PrewarmReply {
        prefix: "claude-opus-4-5",
        full_name: Some("claude-opus-4-5-20251101"),
        input_tokens: 22,
        old_gen_text: Some("Hi"),
    },
    PrewarmReply {
        prefix: "claude-sonnet-4-5",
        full_name: Some("claude-sonnet-4-5-20250929"),
        input_tokens: 22,
        old_gen_text: Some("Hello"),
    },
    PrewarmReply {
        prefix: "claude-haiku-4-5",
        full_name: Some("claude-haiku-4-5-20251001"),
        input_tokens: 22,
        old_gen_text: Some("#"),
    },
];

/// 这个模型的预热回包取值：`(上游回的 model, input_tokens, 旧一代回的那个字)`。表里没有的模型按
/// 最新一代给（33、空正文那一类），`model` 原样回。
fn prewarm_reply_of(model: &str) -> (&str, u64, Option<&'static str>) {
    match PREWARM_REPLIES.iter().find(|r| model.starts_with(r.prefix)) {
        Some(r) => {
            let full = r.full_name.filter(|_| model == r.prefix).unwrap_or(model);
            (full, r.input_tokens, r.old_gen_text)
        }
        None => (model, 33, None),
    }
}

/// 各账号最近一次上游回包 `cf-ray` 里的机房代码（`a49a3231df555df6-HKG` 的 `HKG`），由转发回程与
/// 连通性测试记下（[`note_upstream_cf_ray`]）。机房跟着号的出口走（同一份抓包经香港出口是 `HKG`、
/// 经美国是 `IAD`），按号记，本地应答才能取这把 Key 真会分到的那些号的机房（[`cf_colo_for`]）。
/// 进程内的，重启后要等号再到过一次上游才有。
static UPSTREAM_CF_COLO: parking_lot::Mutex<std::collections::BTreeMap<i64, String>> =
    parking_lot::Mutex::new(std::collections::BTreeMap::new());

/// 记下 `cred_id` 这个号上游回包 `cf-ray` 的机房代码，见 [`UPSTREAM_CF_COLO`]。
pub(super) fn note_upstream_cf_ray(cred_id: i64, headers: &axum::http::HeaderMap) {
    let Some(colo) = headers
        .get("cf-ray")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.rsplit_once('-'))
        .map(|(_, colo)| colo)
        .filter(|c| (3..=4).contains(&c.len()) && c.bytes().all(|b| b.is_ascii_uppercase()))
    else {
        return;
    };
    let mut map = UPSTREAM_CF_COLO.lock();
    if map.get(&cred_id).map(String::as_str) != Some(colo) {
        map.insert(cred_id, colo.to_string());
    }
}

/// 这把 Key 此刻能分到的那些号（`cred_ids`，见 `store::CredentialStore::usable_credential_ids`）
/// 最常见的机房：本地应答没落到具体哪个号上，取多数那个与真到上游的回包对上的机会最大；一样多取
/// id 小的那个号的（`cred_ids` 按 id 升序给，打平时同一把 Key 前后几次取到的是同一个，不来回变）。这些号都还没到过上游时为 `None`，本地应答就不带 `cf-ray`。
pub(super) fn cf_colo_for(cred_ids: &[i64]) -> Option<String> {
    let map = UPSTREAM_CF_COLO.lock();
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for colo in cred_ids.iter().filter_map(|id| map.get(id)) {
        match counts.iter_mut().find(|(c, _)| *c == colo.as_str()) {
            Some((_, n)) => *n += 1,
            None => counts.push((colo.as_str(), 1)),
        }
    }
    // `max_by_key` 取最后一个最大值；倒着找才是「一样多取先出现的」。
    counts.iter().rev().max_by_key(|(_, n)| *n).map(|(c, _)| (*c).to_string())
}

/// 探针回包的 `stop_reason`：按**来访**要的输出上限给。来访只要 1 个 token 才是 `max_tokens`；其余
/// （短开场常见的 50、1024、32000）回一个字就停，是 `end_turn`——回 `max_tokens` 会让客户端以为回答被
/// 截断、提示或自动续写。本地应答与改写成预热（出站 `max_tokens:1`，上游必回 `max_tokens`）两条路
/// 都按这一处改，下游看到的一致。
pub(super) fn probe_stop_reason(req_max_tokens: Option<i64>) -> &'static str {
    if req_max_tokens == Some(1) { "max_tokens" } else { "end_turn" }
}

/// 上游每条回包都带、与账号无关的那几个头，取值与先后照抓包（`cap/auto-2.1.296-20261013-prewarm`）：
/// `date` 打头，然后是安全头与 Cloudflare 那几项，`request-id`、`cf-ray` 按时刻造（见
/// [`upstream_like_id`]、[`cf_ray_hex`]；机房由调用方按 [`cf_colo_for`] 给，没有就不带 `cf-ray`），
/// `traceresponse` 随机。限流头
/// （`anthropic-ratelimit-unified-*`）与 `anthropic-organization-id` / `anthropic-workspace-id`
/// **不造**：那是某个账号此刻的真实额度与归属，本地应答没落到任何账号上，编出来的额度会被下游
/// 当真拿去调度。`date` 自己写上，hyper 就不再在队尾补一个。
fn upstream_like_headers(
    builder: axum::http::response::Builder,
    now: chrono::DateTime<chrono::Utc>,
    colo: Option<&str>,
) -> axum::http::response::Builder {
    let unix_ms = u64::try_from(now.timestamp_millis()).unwrap_or_default();
    let builder = builder
        .header(header::DATE, now.format("%a, %d %b %Y %H:%M:%S GMT").to_string())
        .header("strict-transport-security", "max-age=31536000; includeSubDomains; preload")
        .header("request-id", upstream_like_id("req", unix_ms))
        .header("traceresponse", format!("00-{}-{}-01", random_hex(32), random_hex(16)))
        .header("vary", "Accept-Encoding");
    let builder = match colo {
        Some(colo) => builder.header("cf-ray", format!("{}-{colo}", cf_ray_hex(unix_ms))),
        None => builder,
    };
    builder
        .header("server", "cloudflare")
        .header("cf-cache-status", "DYNAMIC")
        .header("x-robots-tag", "none")
        .header("content-security-policy", "default-src 'none'; frame-ancestors 'none'")
}

/// 探针 / 探活请求的本地回复：**200 + 一条与改写成预热后上游真回的那条同形态的 Message**，
/// 不到上游。
///
/// 正文照官方 `/model` 预热的真实回包逐字段造：键序、`container` / `stop_details` / `diagnostics` 置空、`context_management`，`model`、`input_tokens`、
/// `usage` 的子字段与正文按模型族取（[`prewarm_reply_of`]）：新一代回 [`PROBE_REPLY_TEXT`]（改写成
/// 预热那条路给空正文补的同一句），旧一代回上游真会回的那个字。`id` 按时刻造（[`upstream_like_id`]，
/// 比 `request-id` 晚一个随机的出字耗时）。所以「拒绝探针」开着（本地答）还是关着（改写成预热发
/// 上游），下游看到的是同一种回复。来访没写 `model` 就留空（那种请求上游本会回 400，这里不替它编
/// 一个）。`stop_reason` 按来访要的输出上限给（[`probe_stop_reason`]）。要流式就展成 SSE
/// （[`message_to_sse`]），否则整段 JSON。头照上游的样子带（[`upstream_like_headers`]，机房 `colo`
/// 见 [`cf_colo_for`]）。为什么不回 403，见调用处 2.1a 的说明。
///
/// 回给下游的东西里**没有任何 luban 的标记**：命中的是哪条判据、是 luban 就地答的，都只记在自己的
/// 流水里（`probe_reply:<判据>` 标签，花费记 0）。
pub(crate) fn probe_reply(
    model: Option<&str>,
    req_max_tokens: Option<i64>,
    wants_stream: bool,
    colo: Option<&str>,
) -> Response {
    use rand::RngExt;
    let now = chrono::Utc::now();
    // 上游开始出字比收到请求晚几十到两百毫秒（同一请求的 `msg_` 与 `req_` 解出来差 67～199）。
    let msg_ms = u64::try_from(now.timestamp_millis()).unwrap_or_default()
        + rand::rng().random_range(60..=200);
    let (model, input_tokens, old_gen_text) = prewarm_reply_of(model.unwrap_or_default());
    let mut usage = serde_json::json!({
        "input_tokens": input_tokens,
        "cache_creation_input_tokens": 0,
        "cache_read_input_tokens": 0,
        "cache_creation": { "ephemeral_5m_input_tokens": 0, "ephemeral_1h_input_tokens": 0 },
        "output_tokens": 1});
    if old_gen_text.is_none() {
        usage["output_tokens_details"] = serde_json::json!({ "thinking_tokens": 0 });
    }
    usage["service_tier"] = "standard".into();
    usage["inference_geo"] = "not_available".into();
    let msg = serde_json::json!({
        "model": model,
        "id": upstream_like_id("msg", msg_ms),
        "type": "message",
        "role": "assistant",
        "content": [{ "type": "text", "text": old_gen_text.unwrap_or(PROBE_REPLY_TEXT) }],
        "container": serde_json::Value::Null,
        "stop_reason": probe_stop_reason(req_max_tokens),
        "stop_sequence": serde_json::Value::Null,
        "stop_details": serde_json::Value::Null,
        "usage": usage,
        "diagnostics": serde_json::Value::Null,
        "context_management": { "applied_edits": [] }});
    // 展不成 SSE 只可能是上面这段字面量被改坏了（没有 `content` 数组）；真走到那一步宁可回
    // 整段 JSON，也不回一段拼不齐的流。
    let (content_type, body) = match wants_stream.then(|| message_to_sse(&msg)).flatten() {
        Some(sse) => (SSE_CONTENT_TYPE, sse.into_bytes()),
        None => ("application/json", serde_json::to_vec(&msg).unwrap_or_default()),
    };
    upstream_like_headers(Response::builder().status(StatusCode::OK), now, colo)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                "failed to build the local probe reply",
            )
        })
}

/// 把一条整段 Message 展成 `/v1/messages` 的 SSE 事件流（[`SseAggregator`](crate::proxy::SseAggregator) 的逆操作）：
/// `message_start`（骨架，`content` 清空、停止字段置空）→ 每个内容块的
/// `content_block_start` / 一条整块的 `content_block_delta` / `content_block_stop` →
/// `message_delta`（`stop_reason` / `stop_sequence` / `stop_details` + `usage`）→ `message_stop`。
///
/// 只用于回放学到的拒答，而学进来的都是**输出前**被拒的（`content` 为空），块那一段基本走
/// 不到；仍按官方 delta 种类写全（text / thinking / tool_use 入参），不认识的块类型整块放在
/// `content_block_start` 里——与聚合器「未知块原样收下」对称。不是对象、没有 `content` 数组的
/// 返回 `None`。
pub(crate) fn message_to_sse(msg: &serde_json::Value) -> Option<String> {
    use serde_json::{Value, json};
    let obj = msg.as_object()?;
    let content = obj.get("content")?.as_array()?;
    let mut out = String::new();
    let mut event = |name: &str, data: Value| {
        out.push_str("event: ");
        out.push_str(name);
        out.push_str("\ndata: ");
        out.push_str(&data.to_string());
        out.push_str("\n\n");
    };
    let mut skeleton = obj.clone();
    skeleton.insert("content".into(), json!([]));
    for k in ["stop_reason", "stop_sequence", "stop_details"] {
        if skeleton.contains_key(k) {
            skeleton.insert(k.into(), Value::Null);
        }
    }
    event("message_start", json!({"type": "message_start", "message": Value::Object(skeleton)}));
    for (index, block) in content.iter().enumerate() {
        let Some(b) = block.as_object() else { continue };
        let ty = b.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let mut start = b.clone();
        let mut deltas: Vec<Value> = Vec::new();
        match ty {
            "text" => {
                if let Some(t) = b.get("text").and_then(|t| t.as_str()) {
                    start.insert("text".into(), json!(""));
                    deltas.push(json!({"type": "text_delta", "text": t}));
                }
            }
            "thinking" => {
                if let Some(t) = b.get("thinking").and_then(|t| t.as_str()) {
                    start.insert("thinking".into(), json!(""));
                    deltas.push(json!({"type": "thinking_delta", "thinking": t}));
                }
                if let Some(sig) = b.get("signature").and_then(|t| t.as_str()) {
                    start.insert("signature".into(), json!(""));
                    deltas.push(json!({"type": "signature_delta", "signature": sig}));
                }
            }
            "tool_use" | "server_tool_use" | "mcp_tool_use" => {
                if let Some(input) = b.get("input") {
                    start.insert("input".into(), json!({}));
                    deltas.push(
                        json!({"type": "input_json_delta", "partial_json": input.to_string()}),
                    );
                }
            }
            _ => {}
        }
        event(
            "content_block_start",
            json!({"type": "content_block_start", "index": index, "content_block": Value::Object(start)}),
        );
        for delta in deltas {
            event(
                "content_block_delta",
                json!({"type": "content_block_delta", "index": index, "delta": delta}),
            );
        }
        event("content_block_stop", json!({"type": "content_block_stop", "index": index}));
    }
    let mut delta = serde_json::Map::new();
    for k in ["stop_reason", "stop_sequence", "stop_details"] {
        if let Some(v) = obj.get(k) {
            delta.insert(k.into(), v.clone());
        }
    }
    let usage = obj.get("usage").cloned().unwrap_or(Value::Null);
    event(
        "message_delta",
        json!({"type": "message_delta", "delta": Value::Object(delta), "usage": usage}),
    );
    event("message_stop", json!({"type": "message_stop"}));
    Some(out)
}

#[cfg(test)]
mod tests;
