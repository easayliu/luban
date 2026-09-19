//! 探针 / 探活请求识别与本地应答：不透传到上游，就地回一条最小的正常回复。

use axum::body::Body;
use axum::http::{StatusCode, header};
use axum::response::Response;

use super::learned_rules::SSE_CONTENT_TYPE;
use super::simulation::{
    cc_identity_blocks, field_is_empty, is_official_classifier_request, is_official_helper_request,
    is_official_thread_continuation, is_official_title_request,
};
use super::{REWRITE_PROBE_REPLY, error_response, is_quota_probe_shaped, request_max_tokens};

/// [`probe_signature`] 命中的哪一条判据。日志与错误消息按类写清楚，运维一眼能看出拦的是什么。
///
/// 身份格式不对（device 不是 64 位 hex、session 不是 uuid）**不在**这里：那是「抄错了」而不是
/// 探针，交给模拟路径重建身份（[`cc_identity_well_formed`] 决定它进不了透传），不拒。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ProbeKind {
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
    /// 短开场（严格模式，[`store::ForwardFlags::reject_probes_strict`]）：无 system、无 tools、
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
    pub(super) fn tag(self) -> &'static str {
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
/// 第三类**不在这里拒**：身份写错是「抄错了」，不是探针形态。它由 [`cc_identity_well_formed`]
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
///   严格模式（`strict`，[`store::ForwardFlags::reject_probes_strict`]）下 ping 不再要求无
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
///   （[`store::CredentialStore::device_is_known`]）。官方无 tools 的请求只有四种：预热
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
pub(super) fn probe_signature(
    body: Option<&serde_json::Value>,
    device_id: Option<&str>,
    beta: &[String],
    from_cc_client: bool,
    strict: bool,
    device_known: impl FnOnce() -> bool,
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
        && !device_known()
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
pub(super) const PROBE_REPLY_TEXT: &str = "OK";

/// 「这条是 luban 就地答的、没到上游」的标记头，值是流水里的那个标签（[`REWRITE_PROBE_REPLY`]）。
/// 上游不会回这个头，所以它在抓包、下游面板与自己的日志里都是确定的证据；放在头上而不是体里，
/// 是因为探活只看状态码与正文，头不影响它把这个号判成健康的。
pub(super) const LOCAL_REPLY_HEADER: &str = "x-luban-local";
/// 本地作答那条命中的是哪一条探针判据（[`ProbeKind::tag`]），与日志行里的 `kind` 同一个值。
pub(super) const PROBE_KIND_HEADER: &str = "x-luban-probe-kind";
/// 本地作答那条 Message 的 id 前缀：`msg_luban` + 随机串。官方的是 `msg_01…`，这里一眼能认出
/// 来——下游面板与流水多半只记 id，正文未必留得下。
pub(super) const PROBE_REPLY_ID_PREFIX: &str = "msg_luban";

/// 探针 / 探活请求的本地回复：**200 + 一条最小的正常回复**，不到上游。
///
/// 按来访要的形态给：要流式就把这条 Message 展成 SSE（[`message_to_sse`]，与回放学到的拒答
/// 同一套），否则整段 JSON。`model` 原样回来访声明的那个（没写就留空——那种请求上游本会回
/// 400，这里不替它编一个）；`usage` 记名义上的 1 进 1 出，流水里的花费记 0（见
/// [`REWRITE_PROBE_REPLY`]）；`stop_reason` 按来访要的输出上限给——[`ProbeKind::OneTokenPing`]
/// 那条只要 1 个 token，真上游回的必是 `max_tokens`，其余是 `end_turn`。为什么不回 403，见调用处 2.3a3 的说明。
///
/// **标出来是 luban 答的**：响应头 [`LOCAL_REPLY_HEADER`] + [`PROBE_KIND_HEADER`]，Message id 用
/// [`PROBE_REPLY_ID_PREFIX`] 前缀（流式那条在 `message_start` 里同样带着）。三处都在正文的语义
/// 之外——探活读到的仍是一条正常回复，而抓包、下游面板与自己的流水里这条认得出来、不会被当成
/// 上游真答过一次。
pub(super) fn probe_reply(kind: ProbeKind, model: Option<&str>, wants_stream: bool) -> Response {
    use rand::RngExt;
    use rand::distr::Alphanumeric;
    let tail: String = rand::rng().sample_iter(Alphanumeric).take(16).map(char::from).collect();
    // 来访只要 1 个 token 时上游必然是被截断的，回 `max_tokens`；其余照常收在 `end_turn`。
    let stop_reason = match kind {
        ProbeKind::OneTokenPing => "max_tokens",
        _ => "end_turn",
    };
    let msg = serde_json::json!({
        "id": format!("{PROBE_REPLY_ID_PREFIX}{tail}"),
        "type": "message",
        "role": "assistant",
        "model": model.unwrap_or_default(),
        "content": [{ "type": "text", "text": PROBE_REPLY_TEXT }],
        "stop_reason": stop_reason,
        "stop_sequence": serde_json::Value::Null,
        "usage": {
            "input_tokens": 1,
            "output_tokens": 1,
            "cache_creation_input_tokens": 0,
            "cache_read_input_tokens": 0}});
    // 展不成 SSE 只可能是上面这段字面量被改坏了（没有 `content` 数组）；真走到那一步宁可回
    // 整段 JSON，也不回一段拼不齐的流。
    let (content_type, body) = match wants_stream.then(|| message_to_sse(&msg)).flatten() {
        Some(sse) => (SSE_CONTENT_TYPE, sse.into_bytes()),
        None => ("application/json", serde_json::to_vec(&msg).unwrap_or_default()),
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(LOCAL_REPLY_HEADER, REWRITE_PROBE_REPLY)
        .header(PROBE_KIND_HEADER, kind.tag())
        .body(Body::from(body))
        .unwrap_or_else(|_| {
            error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "api_error",
                "failed to build the local probe reply",
            )
        })
}

/// 把一条整段 Message 展成 `/v1/messages` 的 SSE 事件流（[`SseAggregator`] 的逆操作）：
/// `message_start`（骨架，`content` 清空、停止字段置空）→ 每个内容块的
/// `content_block_start` / 一条整块的 `content_block_delta` / `content_block_stop` →
/// `message_delta`（`stop_reason` / `stop_sequence` / `stop_details` + `usage`）→ `message_stop`。
///
/// 只用于回放学到的拒答，而学进来的都是**输出前**被拒的（`content` 为空），块那一段基本走
/// 不到；仍按官方 delta 种类写全（text / thinking / tool_use 入参），不认识的块类型整块放在
/// `content_block_start` 里——与聚合器「未知块原样收下」对称。不是对象、没有 `content` 数组的
/// 返回 `None`。
pub(super) fn message_to_sse(msg: &serde_json::Value) -> Option<String> {
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
mod tests {
    use crate::proxy::{StatusCode, config};

    /// 探针命中时本地回的那条：200 + 一条最小的正常回复，按来访要的形态给（非流式整段 JSON、
    /// 流式展成 SSE 且能原样聚合回来），model 原样回来访声明的那个、没写就留空，正文里不带
    /// 任何「被拦了」的痕迹——探活看到的必须是「健康」。是 luban 就地答的这件事标在头上
    /// （`x-luban-local` / `x-luban-probe-kind`）与 Message id 的 `msg_luban` 前缀里。
    #[tokio::test]
    async fn probe_reply_is_a_minimal_200_in_the_shape_the_request_asks_for() {
        async fn parts(
            resp: crate::proxy::Response,
        ) -> (StatusCode, axum::http::HeaderMap, String) {
            let status = resp.status();
            let headers = resp.headers().clone();
            let bytes = axum::body::to_bytes(resp.into_body(), 256 * 1024).await.unwrap();
            (status, headers, String::from_utf8(bytes.to_vec()).unwrap())
        }

        // 非流式：整段 Message，字段齐全、状态 200。
        let (status, headers, body) = parts(crate::proxy::probe_reply(
            crate::proxy::ProbeKind::Ping,
            Some("claude-opus-5"),
            false,
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get("content-type").unwrap(), "application/json");
        // 标记：这条是 luban 就地答的、命中的是哪条判据。
        assert_eq!(
            headers.get(crate::proxy::LOCAL_REPLY_HEADER).unwrap(),
            crate::proxy::REWRITE_PROBE_REPLY
        );
        assert_eq!(headers.get(crate::proxy::PROBE_KIND_HEADER).unwrap(), "ping");
        let msg: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(msg["type"], "message");
        assert_eq!(msg["role"], "assistant");
        assert_eq!(msg["model"], "claude-opus-5");
        assert_eq!(msg["stop_reason"], "end_turn");
        assert_eq!(msg["content"][0]["type"], "text");
        assert_eq!(msg["content"][0]["text"], crate::proxy::PROBE_REPLY_TEXT);
        assert_eq!(msg["usage"]["output_tokens"], 1);
        assert!(
            msg["id"].as_str().is_some_and(|id| id
                .starts_with(crate::proxy::PROBE_REPLY_ID_PREFIX)
                && id.len() > 16),
            "id 要一眼认得出是本地答的：{body}"
        );
        // 正文里除了 id 的前缀不留别的痕迹：探活读到的必须是一条正常回复。
        let without_id = body.replace(msg["id"].as_str().unwrap(), "");
        for word in ["probe", "health", "forward", "luban", "403"] {
            assert!(!without_id.contains(word), "本地回复不该露出被拦的痕迹：{word} in {body}");
        }
        // 两条的 id 不同：探活一条接一条，同一个 id 在下游那侧会被当成同一条回复。
        let (_, _, again) = parts(crate::proxy::probe_reply(
            crate::proxy::ProbeKind::Ping,
            Some("claude-opus-5"),
            false,
        ))
        .await;
        let msg2: serde_json::Value = serde_json::from_str(&again).unwrap();
        assert_ne!(msg["id"], msg2["id"]);

        // 流式：展成 SSE，再聚合回来是同一条 Message。
        let (status, headers, body) = parts(crate::proxy::probe_reply(
            crate::proxy::ProbeKind::ShortOpener,
            Some("claude-opus-5"),
            true,
        ))
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(headers.get("content-type").unwrap(), crate::proxy::SSE_CONTENT_TYPE);
        assert_eq!(
            headers.get(crate::proxy::LOCAL_REPLY_HEADER).unwrap(),
            crate::proxy::REWRITE_PROBE_REPLY
        );
        assert_eq!(headers.get(crate::proxy::PROBE_KIND_HEADER).unwrap(), "short-opener");
        assert!(body.starts_with("event: message_start\n"), "{body}");
        assert!(
            body.ends_with("event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n"),
            "{body}"
        );
        let mut agg = crate::proxy::SseAggregator::default();
        agg.feed(body.as_bytes());
        let crate::proxy::Aggregated::Message(back) = agg.finish() else {
            panic!("本地回复的 SSE 应能聚合")
        };
        assert_eq!(back["content"][0]["text"], crate::proxy::PROBE_REPLY_TEXT);
        assert_eq!(back["stop_reason"], "end_turn");

        // 来访没写 model：留空，不替它编一个。
        let (_, _, body) = parts(crate::proxy::probe_reply(
            crate::proxy::ProbeKind::DuplicateIdentity,
            None,
            false,
        ))
        .await;
        let msg: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(msg["model"], "");

        // 1 token 探活：来访只要 1 个 token，真上游回的必是截断，stop_reason 给 max_tokens；
        // 流式那条 message_delta 里同样是它。
        let (_, headers, body) = parts(crate::proxy::probe_reply(
            crate::proxy::ProbeKind::OneTokenPing,
            Some("claude-opus-4-8"),
            false,
        ))
        .await;
        assert_eq!(headers.get(crate::proxy::PROBE_KIND_HEADER).unwrap(), "one-token-ping");
        let msg: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert_eq!(msg["stop_reason"], "max_tokens");
        assert_eq!(msg["usage"]["output_tokens"], 1);
        let (_, _, body) = parts(crate::proxy::probe_reply(
            crate::proxy::ProbeKind::OneTokenPing,
            Some("claude-opus-4-8"),
            true,
        ))
        .await;
        let mut agg = crate::proxy::SseAggregator::default();
        agg.feed(body.as_bytes());
        let crate::proxy::Aggregated::Message(back) = agg.finish() else {
            panic!("本地回复的 SSE 应能聚合")
        };
        assert_eq!(back["stop_reason"], "max_tokens");
    }

    /// 探针判定（[`probe_signature`]）：封号复盘里的三类探活形态都命中，官方 CC 的三种
    /// 无 tools 请求（预热 / haiku 工具调用 / 补全建议）与带 tools 的主请求都不命中；老设备的
    /// 「一句话对话」不算；下游那个去掉基座、带空 tools 与 thinking 的客户端不算。
    #[test]
    fn probe_signature_matches_probes_and_spares_official_shapes() {
        use crate::proxy::ProbeKind::*;
        const DEV: &str = "4fef933b15e89f7060000573496ce0eab6e9f0d1cf43e31dd4c7dc1c6801cfb5";
        let identity = config::CC_SYSTEM_IDENTITY;
        // 官方两块：billing header + 身份句。
        let cc_sys = format!(
            r#"[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.abcdef"}},{{"type":"text","text":"{identity}"}}]"#
        );
        // ban.log 里探针那份 3 块：billing header + 身份句 + 身份句（第三块与身份句等长）。
        let probe_sys = format!(
            r#"[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.220.abcdef"}},{{"type":"text","text":"{identity}"}},{{"type":"text","text":"{identity}"}}]"#
        );
        let one_msg = r#"[{"role":"user","content":"hi"}]"#;
        let sig_beta = |body: &str, cc: bool, dev: Option<&str>, known: bool, beta: &[&str]| {
            let v: serde_json::Value = serde_json::from_str(body).unwrap();
            let beta: Vec<String> = beta.iter().map(|b| b.to_string()).collect();
            crate::proxy::probe_signature(Some(&v), dev, &beta, cc, false, || known)
        };
        let strict = |body: &str| {
            let v: serde_json::Value = serde_json::from_str(body).unwrap();
            crate::proxy::probe_signature(Some(&v), None, &[], false, true, || false)
        };
        let sig = |body: &str, cc: bool, dev: Option<&str>, known: bool| {
            sig_beta(body, cc, dev, known, &[])
        };

        // ---- ban.log 三类探针，按复盘里的形态复现 ----
        // A) 每 15 分钟一条的 haiku ping：3 块 system、max_tokens=2、老设备。
        //    身份句重复先于 ping 命中；把第三块换成别的文字后就是 ping。
        let ping3 = format!(
            r#"{{"model":"claude-haiku-4-5-20251001","system":{probe_sys},"messages":{one_msg},"max_tokens":2,"metadata":{{}}}}"#
        );
        assert_eq!(sig(&ping3, true, Some(DEV), true), Some(DuplicateIdentity));
        let ping = format!(
            r#"{{"model":"claude-haiku-4-5-20251001","system":{cc_sys},"messages":{one_msg},"max_tokens":2,"metadata":{{}}}}"#
        );
        assert_eq!(sig(&ping, true, Some(DEV), true), Some(Ping), "老设备也算：判据在 max_tokens");
        // B) 封前 21 条：3 块 system、无 tools/thinking、temperature=1、max_tokens=1024、新设备。
        let burst3 = format!(
            r#"{{"model":"claude-sonnet-5","system":{probe_sys},"messages":{one_msg},"max_tokens":1024,"temperature":1,"stream":true,"metadata":{{}}}}"#
        );
        assert_eq!(sig(&burst3, true, Some(DEV), false), Some(DuplicateIdentity));
        assert_eq!(
            sig(&burst3, true, Some(DEV), true),
            Some(DuplicateIdentity),
            "已知设备发同形态：身份句重复这条补上"
        );
        let burst = format!(
            r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":{one_msg},"max_tokens":1024,"temperature":1,"stream":true,"metadata":{{}}}}"#
        );
        assert_eq!(sig(&burst, true, Some(DEV), false), Some(ThrowawayConversation));
        assert_eq!(sig(&burst, true, Some(DEV), true), None, "老设备发同样形态不算");
        // C) channel-test 身份**不在这里拒**：它的形态不命中三条（max_tokens=256、设备恒定），
        //    由 detect 送进模拟重建身份，见 [`cc_identity_well_formed`] 的测试。
        let channel_test = format!(
            r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":{one_msg},"max_tokens":256,"stream":true,"thinking":{{"type":"adaptive"}},"metadata":{{}}}}"#
        );
        assert_eq!(sig(&channel_test, true, Some("channel-test"), true), None);

        // ---- 不限 UA（0.3.99 起）：非 CC UA 同样判——Go-http-client 的探活此前全走模拟 ----
        assert_eq!(sig(&ping3, false, Some(DEV), false), Some(DuplicateIdentity));
        assert_eq!(sig(&burst, false, Some(DEV), false), Some(ThrowawayConversation));
        // 不带 system 的 ping 也算：官方没有「无 system、max_tokens 2..16」的请求。
        let bare_ping =
            format!(r#"{{"model":"claude-sonnet-4-6","messages":{one_msg},"max_tokens":16}}"#);
        assert_eq!(sig(&bare_ping, false, None, false), Some(Ping));
        assert_eq!(sig(&bare_ping, true, None, false), Some(Ping));
        // 不带 system、max_tokens 正常的单句请求不算一次性会话：第三方小应用太常见。
        let bare_chat =
            format!(r#"{{"model":"claude-sonnet-4-6","messages":{one_msg},"max_tokens":1024}}"#);
        assert_eq!(sig(&bare_chat, false, Some(DEV), false), None);
        // 默认判据：带 tools 的不算 ping（复盘里 mt=16 带 4 个 tools 的那批照常放行）。
        let tooled_ping = format!(
            r#"{{"model":"claude-sonnet-4-6","messages":{one_msg},"max_tokens":16,"tools":[{{"name":"t","input_schema":{{"type":"object"}}}}]}}"#
        );
        assert_eq!(sig(&tooled_ping, false, None, false), None);

        // ---- 严格模式（默认关）：多收两刀 ----
        // 带 tools 的 ping 也算：16 个 token 装不下一次 tool_use。
        assert_eq!(strict(&tooled_ping), Some(Ping));
        // 短开场：无 system、无 tools、一句「hi」，max_tokens 随便多大。
        for mt in [50, 1024, 32000] {
            let opener =
                format!(r#"{{"model":"claude-opus-5","messages":{one_msg},"max_tokens":{mt}}}"#);
            assert_eq!(strict(&opener), Some(ShortOpener), "max_tokens {mt}");
            assert_eq!(sig(&opener, false, None, false), None, "默认判据不拦 max_tokens {mt}");
        }
        // 文本块形态、带首尾空白也算；不超过 32 字节。中文短语同样算，一整句话不算。
        let blocks = r#"{"model":"claude-opus-5","messages":[{"role":"user","content":[{"type":"text","text":"  ping  "}]}],"max_tokens":1024}"#;
        assert_eq!(strict(blocks), Some(ShortOpener));
        let zh_opener = r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"你好，测试一下"}],"max_tokens":1024}"#;
        assert_eq!(strict(zh_opener), Some(ShortOpener), "7 个汉字 21 字节");
        // 正文长了就是正常单轮对话，不算（30 个汉字 90 字节，按字符算会误伤）。
        let real = format!(
            r#"{{"model":"claude-opus-5","messages":[{{"role":"user","content":"{}"}}],"max_tokens":1024}}"#,
            "请把下面这段话翻译成英文，并保留原有的段落结构与专有名词。"
        );
        assert_eq!(strict(&real), None);
        // 带 system、带 tools、带附件、max_tokens=1（预热）、多条消息：都不是短开场。
        let with_sys = format!(
            r#"{{"model":"claude-opus-5","system":"be brief","messages":{one_msg},"max_tokens":1024}}"#
        );
        assert_eq!(strict(&with_sys), None);
        let with_tools = format!(
            r#"{{"model":"claude-opus-5","messages":{one_msg},"max_tokens":1024,"tools":[{{"name":"t","input_schema":{{"type":"object"}}}}]}}"#
        );
        assert_eq!(strict(&with_tools), None);
        let with_image = r#"{"model":"claude-opus-5","messages":[{"role":"user","content":[{"type":"text","text":"hi"},{"type":"image","source":{}}]}],"max_tokens":1024}"#;
        assert_eq!(strict(with_image), None);
        // 无 system、无身份、UA 不可信的 1 token 请求在严格模式下命中的是 1 token 探活（排在
        // 短开场之前）；短开场自己不认 max_tokens=1。
        let prewarm = format!(r#"{{"model":"claude-opus-5","messages":{one_msg},"max_tokens":1}}"#);
        assert_eq!(strict(&prewarm), Some(OneTokenPing));

        // ---- 1 token 探活：ban 导出里 Go-http-client 每 5 分钟一条的健康检查 ----
        // 非 CC UA：无 system、无 tools、一条消息、max_tokens=1 → 命中，带不带 device_id 都算
        // （Go 客户端抄了 metadata.user_id 的也有），流式与否无关，模型无关。
        let one_token = |model: &str, extra: &str| {
            format!(r#"{{"model":"{model}","messages":{one_msg},"max_tokens":1{extra}}}"#)
        };
        assert_eq!(sig(&one_token("claude-opus-4-8", ""), false, None, false), Some(OneTokenPing));
        assert_eq!(
            sig(&one_token("claude-opus-4-8", r#","stream":true"#), false, Some(DEV), true),
            Some(OneTokenPing),
            "带 device_id、老设备、流式：UA 不可信就算"
        );
        assert_eq!(
            sig(&one_token("claude-haiku-4-5-20251001", ""), false, None, false),
            Some(OneTokenPing),
            "haiku 也算：官方额度探测来自可信 UA"
        );
        // 非 CC UA 抄了官方额度探测的正文（haiku + `quota`）：仍算——不是可信 UA 发的就不是官方探测。
        let quota_probe = r#"{"model":"claude-haiku-4-5-20251001","max_tokens":1,"messages":[{"role":"user","content":"quota"}]}"#;
        assert_eq!(sig(quota_probe, false, None, false), Some(OneTokenPing));
        // 可信 UA：带 device_id 的是桌面端预热，不碰（由模拟路径按形态放行）；额度探测形态
        // 不碰；什么身份都没有的 1 token 请求算——模拟路径本来也按第三方处理它。
        assert_eq!(sig(&one_token("claude-opus-5", ""), true, Some(DEV), false), None);
        assert_eq!(sig(quota_probe, true, None, false), None, "官方额度探测不带身份也放");
        assert_eq!(sig(&one_token("claude-opus-5", ""), true, None, false), Some(OneTokenPing));
        // 带 system 的 1 token 请求不算：桌面端另一种预热带 `[billing, 身份句]` 两块，第三方带
        // 长 system 的由模拟接管；`system: null` 按没有算，加个空 tools 也绕不过。
        let sys_prewarm = format!(
            r#"{{"model":"claude-opus-5","system":{cc_sys},"messages":{one_msg},"max_tokens":1}}"#
        );
        assert_eq!(sig(&sys_prewarm, false, None, false), None);
        assert_eq!(
            sig(&one_token("claude-opus-5", r#","system":"be brief""#), false, None, false),
            None
        );
        assert_eq!(
            sig(&one_token("claude-opus-5", r#","system":null,"tools":[]"#), false, None, false),
            Some(OneTokenPing)
        );
        // 带 tools 或多条消息的不算。
        assert_eq!(
            sig(
                &one_token(
                    "claude-opus-5",
                    r#","tools":[{"name":"t","input_schema":{"type":"object"}}]"#
                ),
                false,
                None,
                false
            ),
            None
        );
        let two_turn_one_token = r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"hello"},{"role":"user","content":"hi"}],"max_tokens":1}"#;
        assert_eq!(sig(two_turn_one_token, false, None, false), None);
        let two_msgs = r#"{"model":"claude-opus-5","messages":[{"role":"user","content":"hi"},{"role":"assistant","content":"hello"},{"role":"user","content":"hi"}],"max_tokens":1024}"#;
        assert_eq!(strict(two_msgs), None);

        // ---- 加空字段绕不过 ----
        let padded = format!(
            r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":{one_msg},"max_tokens":1024,"tools":[],"thinking":null,"stop_sequences":[],"metadata":{{}}}}"#
        );
        assert_eq!(sig(&padded, true, Some(DEV), false), Some(ThrowawayConversation));
        let padded_ping = format!(
            r#"{{"model":"claude-haiku-4-5-20251001","system":{cc_sys},"messages":{one_msg},"max_tokens":8,"tools":null,"metadata":{{}}}}"#
        );
        assert_eq!(sig(&padded_ping, true, Some(DEV), true), Some(Ping));
        // thinking 给个非对象也不算「带了 thinking」。
        let fake_thinking = format!(
            r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":{one_msg},"max_tokens":1024,"thinking":"adaptive","metadata":{{}}}}"#
        );
        assert_eq!(sig(&fake_thinking, true, Some(DEV), false), Some(ThrowawayConversation));
        // thinking 是对象但 max_tokens 不到官方工具调用那档 → 仍算。
        let small_thinking = format!(
            r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":{one_msg},"max_tokens":1024,"thinking":{{"type":"adaptive"}},"metadata":{{}}}}"#
        );
        assert_eq!(sig(&small_thinking, true, Some(DEV), false), Some(ThrowawayConversation));

        // ---- 官方三种无 tools 请求都不命中（形态取自 cap/ 抓包） ----
        let prewarm = format!(
            r#"{{"model":"claude-haiku-4-5-20251001","max_tokens":1,"messages":{one_msg},"metadata":{{}}}}"#
        );
        assert_eq!(sig(&prewarm, true, Some(DEV), false), None, "预热不算");
        // 2.1.187 Claude Desktop 的预热带 [billing, identity] 两块 system（ban.log）。
        let prewarm_with_sys = format!(
            r#"{{"model":"claude-haiku-4-5-20251001","messages":{one_msg},"system":{cc_sys},"max_tokens":1,"metadata":{{}}}}"#
        );
        assert_eq!(sig(&prewarm_with_sys, true, Some(DEV), false), None, "带 system 的预热不算");
        // 只有 Helper / 安全分类的 body 取值、system 是普通 CC 两块、也没带各自的 beta：
        // 这不是官方那两种请求，是抄了几个字段的探针，**要**命中。完整形态的放行见下面。
        let haiku_body_only = format!(
            r#"{{"model":"claude-haiku-4-5-20251001","messages":{one_msg},"system":{cc_sys},"tools":[],"metadata":{{}},"max_tokens":32000,"thinking":{{"type":"disabled"}},"temperature":1,"stream":true}}"#
        );
        assert_eq!(sig(&haiku_body_only, true, Some(DEV), false), Some(ThrowawayConversation));
        let classifier_body_only = format!(
            r#"{{"model":"claude-sonnet-5","max_tokens":64,"system":{cc_sys},"messages":{one_msg},"stop_sequences":["\\n"],"thinking":{{"type":"disabled"}},"metadata":{{}}}}"#
        );
        assert_eq!(sig(&classifier_body_only, true, Some(DEV), false), Some(ThrowawayConversation));
        let main = format!(
            r#"{{"model":"claude-opus-5","system":{cc_sys},"messages":{one_msg},"tools":[{{"name":"Bash"}}],"max_tokens":32000,"metadata":{{}}}}"#
        );
        assert_eq!(sig(&main, true, Some(DEV), false), None, "带 tools 的主请求不算");
        // 多轮对话不算，哪怕其余都像。
        let multi = format!(
            r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":[{{"role":"user","content":"a"}},{{"role":"assistant","content":"b"}},{{"role":"user","content":"c"}}],"max_tokens":8,"metadata":{{}}}}"#
        );
        assert_eq!(sig(&multi, true, Some(DEV), false), None);

        // ---- ban.log 里 claude-cli/2.1.165 那批：opus-5、去掉基座、tools: []、thinking 对象、
        //      max_tokens 10240，每台新设备同样四道题。上一版的宽豁免放过了它；按官方 Helper
        //      的取值逐项对之后（模型不是 haiku、max_tokens 不是 32000）它就是一次性对话。
        let rig = format!(
            r#"{{"model":"claude-opus-5","system":{cc_sys},"messages":{one_msg},"max_tokens":10240,"stream":true,"tools":[],"output_config":{{}},"thinking":{{"type":"adaptive"}},"metadata":{{}}}}"#
        );
        assert_eq!(sig(&rig, true, Some(DEV), false), Some(ThrowawayConversation));

        // ---- 豁免只认官方取值：下面每一条都是「抄了个字段名」的绕法，全部仍命中 ----
        for (label, body) in [
            (
                "thinking:{} + 4096",
                format!(
                    r#"{{"model":"claude-haiku-4-5-20251001","system":{cc_sys},"messages":{one_msg},"max_tokens":4096,"tools":[],"thinking":{{}},"stream":true,"metadata":{{}}}}"#
                ),
            ),
            (
                "thinking:{} + stop_sequences",
                format!(
                    r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":{one_msg},"max_tokens":64,"thinking":{{}},"stop_sequences":["x"],"metadata":{{}}}}"#
                ),
            ),
            (
                "helper 取值但模型是 opus",
                format!(
                    r#"{{"model":"claude-opus-5","system":{cc_sys},"messages":{one_msg},"max_tokens":32000,"tools":[],"thinking":{{"type":"disabled"}},"stream":true,"metadata":{{}}}}"#
                ),
            ),
            (
                "helper 取值但 max_tokens 31999",
                format!(
                    r#"{{"model":"claude-haiku-4-5-20251001","system":{cc_sys},"messages":{one_msg},"max_tokens":31999,"tools":[],"thinking":{{"type":"disabled"}},"stream":true,"metadata":{{}}}}"#
                ),
            ),
            (
                "helper 取值但 tools 缺失",
                format!(
                    r#"{{"model":"claude-haiku-4-5-20251001","system":{cc_sys},"messages":{one_msg},"max_tokens":32000,"thinking":{{"type":"disabled"}},"stream":true,"metadata":{{}}}}"#
                ),
            ),
            (
                "helper 取值但非流式",
                format!(
                    r#"{{"model":"claude-haiku-4-5-20251001","system":{cc_sys},"messages":{one_msg},"max_tokens":32000,"tools":[],"thinking":{{"type":"disabled"}},"metadata":{{}}}}"#
                ),
            ),
            (
                "classifier 取值但 max_tokens 63",
                format!(
                    r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":{one_msg},"max_tokens":63,"thinking":{{"type":"disabled"}},"stop_sequences":["</x>"],"metadata":{{}}}}"#
                ),
            ),
            (
                "classifier 取值但 max_tokens 65",
                format!(
                    r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":{one_msg},"max_tokens":65,"thinking":{{"type":"disabled"}},"stop_sequences":["</x>"],"metadata":{{}}}}"#
                ),
            ),
            (
                "classifier 取值但 stop_sequences 里是空串",
                format!(
                    r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":{one_msg},"max_tokens":64,"thinking":{{"type":"disabled"}},"stop_sequences":[""],"metadata":{{}}}}"#
                ),
            ),
            (
                "classifier 取值但流式",
                format!(
                    r#"{{"model":"claude-sonnet-5","system":{cc_sys},"messages":{one_msg},"max_tokens":64,"thinking":{{"type":"disabled"}},"stop_sequences":["</x>"],"stream":true,"metadata":{{}}}}"#
                ),
            ),
        ] {
            assert_eq!(sig(&body, true, Some(DEV), false), Some(ThrowawayConversation), "{label}");
        }
        // 只抄 Helper 的 body 五个字段、system 却是普通 CC 两块（billing + CC 身份句、没有 SDK
        // 身份句也没有长提示词）：不是 Helper 也不是标题生成，照样是一次性对话。
        let helper_body_only = format!(
            r#"{{"model":"claude-haiku-4-5-20251001","messages":{one_msg},"system":{cc_sys},"tools":[],"metadata":{{}},"max_tokens":32000,"thinking":{{"type":"disabled"}},"temperature":1,"stream":true}}"#
        );
        assert_eq!(sig(&helper_body_only, true, Some(DEV), false), Some(ThrowawayConversation));

        // ---- 官方三种无 tools 请求抄全了才免：这是形态判据的边界，形态上它们就是官方请求 ----
        let sub_billing = "x-anthropic-billing-header: cc_version=2.1.260.d95; cc_entrypoint=cli; cch=ca354; cc_is_subagent=true; cc_prompt_id=bd224f00-5a91-4f15-b92f-0f47c663eae8;";
        let sdk = config::CC_SDK_AGENT_IDENTITY;
        let helper_exact = format!(
            r#"{{"model":"claude-haiku-4-5-20251001","messages":{one_msg},"system":[{{"type":"text","text":"{sub_billing}"}},{{"type":"text","text":"{sdk}"}}],"tools":[],"metadata":{{}},"max_tokens":32000,"thinking":{{"type":"disabled"}},"temperature":1,"stream":true}}"#
        );
        let helper_betas: Vec<&str> =
            config::cc_profile(config::CcProfileKind::HelperSubagentHaiku)
                .beta
                .split(',')
                .collect();
        assert_eq!(
            sig_beta(&helper_exact, true, Some(DEV), false, &helper_betas),
            None,
            "官方 Helper 放行"
        );
        let long = "x".repeat(3059);
        let title_exact = format!(
            r#"{{"model":"claude-haiku-4-5-20251001","messages":{one_msg},"system":[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.260.ced; cch=00000"}},{{"type":"text","text":"{identity}"}},{{"type":"text","text":"{long}"}}],"tools":[],"metadata":{{}},"max_tokens":32000,"thinking":{{"type":"disabled"}},"temperature":1,"output_config":{{}},"stream":true}}"#
        );
        assert_eq!(
            sig_beta(&title_exact, true, Some(DEV), false, &[config::CC_BETA_STRUCTURED_OUTPUTS]),
            None,
            "官方标题生成放行（它与主请求并发，新设备上可能先到）"
        );
        let session_ctx = r"  ## Session Context\n\n- **User identity**: `e@x`";
        let classifier_exact = format!(
            r#"{{"model":"claude-sonnet-5","max_tokens":64,"system":[{{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.260.3de; cch=00000"}},{{"type":"text","text":"{long}"}},{{"type":"text","text":"{session_ctx}"}}],"messages":{one_msg},"stop_sequences":["</severity>"],"thinking":{{"type":"disabled"}},"metadata":{{}}}}"#
        );
        assert_eq!(
            sig_beta(
                &classifier_exact,
                true,
                Some(DEV),
                false,
                &[config::CC_BETA_AUTO_MODE_CLASSIFIER]
            ),
            None,
            "官方安全分类放行"
        );

        // ---- 三种官方请求各少一样，都不放 ----
        for (label, body, beta) in [
            ("Helper 缺官方 beta", helper_exact.clone(), vec![]),
            ("Helper 只带了部分官方 beta", helper_exact.clone(), helper_betas[..3].to_vec()),
            (
                "Helper 第二块是 CC 身份句而不是 SDK 身份句",
                helper_exact.replace(sdk, identity),
                helper_betas.clone(),
            ),
            (
                "Helper thinking.type=enabled",
                helper_exact.replace(r#""type":"disabled""#, r#""type":"enabled""#),
                helper_betas.clone(),
            ),
            (
                "Helper 的 billing header 没有子代理标记",
                helper_exact.replace("cc_is_subagent=true; ", ""),
                helper_betas.clone(),
            ),
            ("标题生成缺 structured-outputs beta", title_exact.clone(), vec![]),
            (
                "标题生成的提示词不够长",
                title_exact.replace(&long, "short"),
                vec![config::CC_BETA_STRUCTURED_OUTPUTS],
            ),
            ("安全分类缺 auto-mode-classifier beta", classifier_exact.clone(), vec![]),
            (
                "安全分类 thinking.type=adaptive",
                classifier_exact.replace(r#""type":"disabled""#, r#""type":"adaptive""#),
                vec![config::CC_BETA_AUTO_MODE_CLASSIFIER],
            ),
            (
                "安全分类第一块不是 billing header",
                classifier_exact.replace("x-anthropic-billing-header: ", ""),
                vec![config::CC_BETA_AUTO_MODE_CLASSIFIER],
            ),
            (
                "安全分类只有两块 system",
                classifier_exact
                    .replace(&format!(r#",{{"type":"text","text":"{session_ctx}"}}"#), ""),
                vec![config::CC_BETA_AUTO_MODE_CLASSIFIER],
            ),
            (
                "安全分类第三块不是 Session Context",
                classifier_exact.replace(session_ctx, "something else"),
                vec![config::CC_BETA_AUTO_MODE_CLASSIFIER],
            ),
            (
                "安全分类中间块不够长",
                classifier_exact.replace(&long, "short"),
                vec![config::CC_BETA_AUTO_MODE_CLASSIFIER],
            ),
        ] {
            assert_eq!(
                sig_beta(&body, true, Some(DEV), false, &beta),
                Some(ThrowawayConversation),
                "{label}"
            );
        }

        // 字符串形态的 system 里身份句只算一块。
        let as_string = format!(
            r#"{{"model":"claude-opus-5","system":"{identity} {identity}","messages":{one_msg},"tools":[{{"name":"Bash"}}],"max_tokens":32000}}"#
        );
        assert_eq!(sig(&as_string, true, Some(DEV), true), None);
    }
}
