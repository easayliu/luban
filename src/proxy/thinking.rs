//! `thinking` / `redacted_thinking` 块的降级重试、原始字节编码保持与取证定位。

use axum::body::Bytes;

use super::ban::parse_upstream_error;
use super::digest::turn_label;
use super::logging::{ReqLog, UsageSniffer};
use super::rate_limit::RateLimitInfo;
use super::upstream::{Upstream, error_chain, resp_shape, strip_assistant_prefill};

/// 上游验不过历史思考块（签名、被改过的编码、`redacted_thinking` 的密文）之后的兜底：把历史
/// thinking 降级成 text、`redacted_thinking` 整块删掉，用**同一个凭证**重发一次。
///
/// `reason` 只进日志，写的是上游这次点名的是哪一样（三条调用路各自传自己的那句）。
///
/// 成功则返回重试那次的上游响应，并把 `rl` 改按它记账（交给调用方 [`stream_upstream`]）；
/// 任何一步不成都返回 `None`、`rl` 不动，由调用方继续透传最初那条 400——这条兜底路径在设计上
/// 不会让结果变差，最坏就是白花一次往返。
///
/// **代价是每轮一次**：客户端自己的会话记录里那些原始 thinking 块并不会因为这次重试而改写，
/// 于是这条会话的后续每一轮都会先撞一次 400 再降级重发，直到会话结束。会话能继续跑，但上游
/// 请求数翻倍。真正的解法是别让会话中途换号（见 `store::CredentialStore::select_for_device`
/// 的软绑定：名额到点就还，但设备回来仍优先回原号），这里只是兜底。
pub(super) async fn retry_demoted_thinking(
    upstream: &Upstream<'_>,
    cred: &crate::credentials::Credential,
    device_fp: &str,
    client_body: &Bytes,
    rl: &mut ReqLog,
    reason: &str,
) -> Option<wreq::Response> {
    let Some(demoted) = demote_thinking_blocks(client_body) else {
        tracing::warn!(
            cred_id = cred.id,
            cred = %cred.label,
            reason,
            "upstream rejected a historical thinking block, but the body has no thinking block to demote, passing through as is"
        );
        return None;
    };
    tracing::warn!(
        cred_id = cred.id,
        cred = %cred.label,
        reason,
        "upstream rejected a historical thinking block (it was most likely issued to another credential, or the turn holding it was rewritten): demoted historical thinking to text, retrying once with the same credential"
    );

    // 实际发出去的那份留一手：重试成了的话，请求日志与遥测的请求侧都要换成它。
    let retried = upstream.shape(&demoted, cred, device_fp);
    let up = match upstream.send(retried.clone()).await {
        Ok(up) => up,
        Err(e) => {
            tracing::warn!(error = %error_chain(&e), "the retry after demoting thinking could not be sent, passing the original 400 through");
            return None;
        }
    };
    let status = up.status();
    if !status.is_success() {
        // 最常见的是末轮为 `tool_result` 的工具续跑：上游另外要求「最后一条 assistant
        // 消息必须以 thinking 块开头」，降级完照样被拒，只是换了条错误信息。
        tracing::warn!(
            cred_id = cred.id,
            cred = %cred.label,
            status = status.as_u16(),
            "the retry after demoting thinking was rejected too, passing the original 400 through"
        );
        return None;
    }

    // 重试成功：这条请求日志改按重试那次记账——状态码、用量、限流都以它为准，TTFT 重新
    // 计时。`started` 不动，故 total_ms 含两次往返，那正是客户端实际等到的时间。
    let (is_stream, encoding) = resp_shape(&up);
    rl.status = status.as_u16();
    rl.ttft_ms = None;
    rl.sniffer = UsageSniffer::new(is_stream, encoding.is_some());
    rl.ratelimit = RateLimitInfo::from_headers(up.headers());
    rl.note_retry("demoted_thinking", up.headers(), &retried);
    Some(up)
}

/// 上游那条 400 是不是「thinking 块签名验不过」，形如
/// `messages.1.content.0: Invalid \`signature\` in \`thinking\` block`。
///
/// 只按 message 文本判、不卡 `error.type`：这条错误上游归在 `invalid_request_error` 名下，
/// 跟一大堆真正的请求形态错误同类，靠类型分不出来；而 `signature` 与 `thinking` 同时出现在
/// 一句错误里只有这一种情况。
pub(super) fn is_thinking_signature_error(body: &[u8]) -> bool {
    let (_, message) = parse_upstream_error(body);
    let hay = message.to_lowercase();
    hay.contains("signature") && hay.contains("thinking")
}

/// 上游那条 400 是不是「thinking 块被修改过」，形如
/// `messages.N.content.M: \`thinking\` or \`redacted_thinking\` blocks in the latest
///  assistant message cannot be modified.`
///
/// 成因：客户端或代理的 JSON 序列化/反序列化改变了 thinking 块的编码。
/// 处理方式与签名错误一样——降级重试。
pub(super) fn is_thinking_modified_error(body: &[u8]) -> bool {
    let (_, message) = parse_upstream_error(body);
    let hay = message.to_lowercase();
    hay.contains("cannot be modified") && hay.contains("thinking")
}

/// 上游那条 400 是不是「thinking 块没有 thinking 内容」，形如
/// `messages.N.content.M: each thinking block must contain thinking`。
///
/// [`strip_empty_thinking_blocks`] 本该在出站前把这种块剥干净；还能撞上，说明要么剥除
/// 条件没覆盖到（比如整条 content 只有空块而被刻意保留），要么触发条件根本不是「空」
/// 而是别的（签名缺失？）。所以命中时把**客户端原始请求体**整体打出来供复现，见调用处。
pub(super) fn is_empty_thinking_error(body: &[u8]) -> bool {
    let (_, message) = parse_upstream_error(body);
    message.to_lowercase().contains("must contain thinking")
}

/// 上游那条 400 是不是「`redacted_thinking` 块的密文验不过」，形如
/// `messages.5.content.48: Invalid \`data\` in \`redacted_thinking\` block`。
///
/// 与 [`is_thinking_signature_error`] 是同一类事（上游验不过历史思考块里那段它自己签发的
/// 载荷），只是被点名的是 `redacted_thinking` 的密文而不是 `thinking` 的签名，处理方式也一样
/// ——降级重试，[`demote_thinking_blocks`] 对 `redacted_thinking` 正是整块删。
///
/// 两条判据不重叠：这句里没有 `signature`，签名那句里没有 `redacted_thinking`。
pub(super) fn is_redacted_thinking_data_error(body: &[u8]) -> bool {
    let (_, message) = parse_upstream_error(body);
    let hay = message.to_lowercase();
    hay.contains("redacted_thinking") && hay.contains("data")
}

/// 上游拒绝 prefill 时，剥掉末尾 assistant 轮后用同一个凭证重试一次。
///
/// 模式与 [`retry_demoted_thinking`] 一致：对原始客户端 body 改写后走 `upstream.shape()` →
/// `upstream.send()`，成功则替换请求日志里的状态/用量/限流信息并返回上游响应；
/// 失败或重试仍被拒则返回 `None`，调用侧透传最初那条 400。
pub(super) async fn retry_without_prefill(
    upstream: &Upstream<'_>,
    cred: &crate::credentials::Credential,
    device_fp: &str,
    client_body: &Bytes,
    rl: &mut ReqLog,
) -> Option<wreq::Response> {
    let Some(stripped) = strip_assistant_prefill(client_body) else {
        tracing::warn!(
            cred_id = cred.id,
            cred = %cred.label,
            "upstream says prefill not supported, but no trailing assistant message found; passing through as is"
        );
        return None;
    };
    tracing::warn!(
        cred_id = cred.id,
        cred = %cred.label,
        "upstream says this model does not support assistant message prefill: stripped trailing assistant message(s), retrying once"
    );

    // 同 [`retry_demoted_thinking`]：留下实际发出去的那份。
    let retried = upstream.shape(&stripped, cred, device_fp);
    let up = match upstream.send(retried.clone()).await {
        Ok(up) => up,
        Err(e) => {
            tracing::warn!(
                error = %error_chain(&e),
                "the retry after stripping prefill could not be sent, passing the original 400 through"
            );
            return None;
        }
    };
    let status = up.status();
    if !status.is_success() {
        tracing::warn!(
            cred_id = cred.id,
            cred = %cred.label,
            status = status.as_u16(),
            "the retry after stripping prefill was rejected too, passing the original 400 through"
        );
        return None;
    }

    let (is_stream, encoding) = resp_shape(&up);
    rl.status = status.as_u16();
    rl.ttft_ms = None;
    rl.sniffer = UsageSniffer::new(is_stream, encoding.is_some());
    rl.ratelimit = RateLimitInfo::from_headers(up.headers());
    rl.note_retry("no_prefill", up.headers(), &retried);
    Some(up)
}

/// 把 assistant 轮里的 `thinking` 块降级成 `text` 块：推理原文原样搬进 text（外面裹一层
/// `<previous_thinking>`，让模型分得清那不是它当时说给用户的话），带不过去的签名丢掉。
/// `redacted_thinking` 只有一段密文 `data`、没有可搬的内容，直接删。
///
/// **为什么是降级而不是整块删**：删掉模型就丢了自己上一轮的推理链，续跑时容易从头再想一遍
/// 甚至改主意——用户看到的是「它突然忘了刚才在干嘛」。搬成 text 则历史完整，只是从「想过的」
/// 变成「说过的」。
///
/// 返回 `None` 表示没有可降级的块，那这条 400 另有原因，不值得再花一次往返。
///
/// **救不了工具续跑轮**：请求末尾是 `tool_result` 时，上游另外要求「最后一条 assistant 消息
/// 必须以 thinking 块开头」，降级完照样被拒。这种情况下重试白跑一次，随后原样透传最初那条
/// 400——不会更差，但也确实救不回来。
pub(super) fn demote_thinking_blocks(body: &Bytes) -> Option<Bytes> {
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let msgs = v.get_mut("messages")?.as_array_mut()?;
    let mut changed = false;
    for msg in msgs.iter_mut() {
        let Some(obj) = msg.as_object_mut() else { continue };
        if obj.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        // `content` 是字符串形态的 assistant 轮压根没有 thinking 块，跳过即可。
        let Some(content) = obj.get_mut("content").and_then(|c| c.as_array_mut()) else { continue };
        let mut next = Vec::with_capacity(content.len());
        let mut touched = false;
        for blk in content.iter() {
            match blk.get("type").and_then(|t| t.as_str()) {
                Some("thinking") => {
                    touched = true;
                    let text = blk.get("thinking").and_then(|t| t.as_str()).unwrap_or_default();
                    if !text.trim().is_empty() {
                        next.push(previous_thinking_block(text));
                    }
                }
                Some("redacted_thinking") => touched = true,
                _ => next.push(blk.clone()),
            }
        }
        // 降级后空掉的 assistant 轮（整轮只有 thinking）是上游必拒的形态——`content` 不能是
        // 空数组。这种轮次原样留着：反正整条请求本来就要重试，少改一处也比发一个铁定被拒的
        // body 强。
        if !touched || next.is_empty() {
            continue;
        }
        *content = next;
        changed = true;
    }
    if !changed {
        return None;
    }
    serde_json::to_vec(&v).ok().map(Bytes::from)
}

/// 由一段历史推理原文构造替代它的 text 块，key 序与官方内容块一致：`type` → `text`。
fn previous_thinking_block(thinking: &str) -> serde_json::Value {
    let mut blk = serde_json::Map::new();
    blk.insert("type".into(), "text".into());
    blk.insert(
        "text".into(),
        format!("<previous_thinking>\n{thinking}\n</previous_thinking>").into(),
    );
    serde_json::Value::Object(blk)
}

// ---------------------------------------------------------------------------
// thinking 块原始编码保持
// ---------------------------------------------------------------------------

/// `serde_json` 的反序列化会把 JSON 字符串里的 `\uXXXX` 解码成 UTF-8 字符；再用
/// `to_vec` 序列化回去时只保留 serde 自己的转义策略——结果与原始 JSON 在**字节层面**
/// 不同，即使**逻辑值**完全一致。Anthropic 上游会按字节比对 thinking 块，只要字节变了
/// 就 400。
///
/// 此函数在 [`rewrite_body`] 的最终序列化之后调用：把 `rewritten` 里每个
/// thinking / redacted_thinking 块替换回 `original` 里的原始字节。
pub(super) fn preserve_thinking_encoding(original: &[u8], rewritten: Vec<u8>) -> Vec<u8> {
    let Ok(orig_str) = std::str::from_utf8(original) else { return rewritten };
    // 两个都要找：`"redacted_thinking"` 里没有 `"thinking"` 这个子串（前面那个引号被
    // `redacted_` 占着）。只找后者的话，一份只有 `redacted_thinking` 块、顶层又没有
    // `thinking` 字段的体（第三方中转常见）会整段跳过不还原——而 base64 的标准字母表里正有
    // `/`，`\/` 又是合法 JSON 转义，转义过的体一经 serde 往返就换了字节，上游必拒。
    // 判据与 [`thinking_block_byte_ranges`] 里那道块级过滤保持同一口径。
    if !(orig_str.contains("\"thinking\"") || orig_str.contains("\"redacted_thinking\"")) {
        return rewritten;
    }
    let orig_blocks = thinking_block_byte_ranges(orig_str);
    if orig_blocks.is_empty() {
        return rewritten;
    }
    let rw_blocks = {
        let Ok(s) = std::str::from_utf8(&rewritten) else { return rewritten };
        thinking_block_byte_ranges(s)
    };
    // 改写过程中整条消息可能被删掉（[`drop_empty_system_messages`]、
    // [`hoist_system_role_messages`]），消息里的块也可能被剥掉（[`strip_empty_text_blocks`]、
    // [`strip_empty_thinking_blocks`]）——下标一移，按 `(msg, blk)` 配对就会把 A 块的原始字节
    // 盖到 B 块上，历史与签名一起错乱。所以带 `signature` / `data` 的按那个值配（base64，
    // 改写前后逐字相同），只有两侧的 `(msg, blk)` 完全对齐时才退回按位置配。
    let aligned = orig_blocks.len() == rw_blocks.len()
        && orig_blocks.iter().zip(&rw_blocks).all(|(o, r)| o.msg == r.msg && o.blk == r.blk);
    let unique = |list: &[ThinkingBlockRef], key: &str| {
        list.iter().filter(|b| b.key.as_deref() == Some(key)).count() == 1
    };
    let mut subs: Vec<(std::ops::Range<usize>, &[u8])> = Vec::new();
    for ob in &orig_blocks {
        let rb = match ob.key.as_deref() {
            // 两侧各只出现一次才算认得出是同一个块；重复的（客户端把同一块贴了两遍）退回位置。
            Some(key) if unique(&orig_blocks, key) && unique(&rw_blocks, key) => {
                rw_blocks.iter().find(|r| r.key.as_deref() == Some(key))
            }
            // 没有签名的块只在完全对齐时按位置配。错位时宁可不还原：它没有签名，上游无从
            // 按字节校验，重新编码一遍无害，而配错了就是把历史改乱。
            _ if aligned => rw_blocks.iter().find(|r| r.msg == ob.msg && r.blk == ob.blk),
            _ => None,
        };
        if let Some(rb) = rb {
            let orig_slice = &original[ob.span.clone()];
            let rw_slice = &rewritten[rb.span.clone()];
            if orig_slice != rw_slice {
                subs.push((rb.span.clone(), orig_slice));
            }
        }
    }
    if subs.is_empty() {
        return rewritten;
    }
    subs.sort_by(|a, b| b.0.start.cmp(&a.0.start));
    let mut out = rewritten;
    for (range, orig) in subs {
        let mut v = Vec::with_capacity(out.len() - range.len() + orig.len());
        v.extend_from_slice(&out[..range.start]);
        v.extend_from_slice(orig);
        v.extend_from_slice(&out[range.end..]);
        out = v;
    }
    out
}

struct ThinkingBlockRef {
    msg: usize,
    blk: usize,
    span: std::ops::Range<usize>,
    /// 这个块的**稳定身份**：`thinking` 的 `signature`、`redacted_thinking` 的 `data`。
    /// 两者都是 base64（`[A-Za-z0-9+/=]`），而这里存的是**解析之后**的值——原文把 `/` 写成
    /// `\/` 也好、原样也好，解出来都是同一串，所以拿它配对既不怕下标移位、也不怕两侧转义
    /// 策略不同，见 [`preserve_thinking_encoding`]。客户端自己拼的块可能两样都没有，
    /// 那时是 `None`。
    key: Option<String>,
}

/// 用 `RawValue` 零拷贝反序列化找到 `original` 里每个 thinking / redacted_thinking
/// 块在字节层面的精确位置。
fn thinking_block_byte_ranges(json: &str) -> Vec<ThinkingBlockRef> {
    use serde::Deserialize;
    #[derive(Deserialize)]
    struct B<'a> {
        #[serde(borrow, default)]
        messages: Vec<M<'a>>,
    }
    #[derive(Deserialize)]
    struct M<'a> {
        role: Option<&'a str>,
        #[serde(borrow)]
        content: Option<&'a serde_json::value::RawValue>,
    }
    let Ok(body) = serde_json::from_str::<B<'_>>(json) else { return vec![] };
    let base = json.as_ptr() as usize;
    let mut out = Vec::new();
    for (mi, m) in body.messages.iter().enumerate() {
        if m.role != Some("assistant") {
            continue;
        }
        let Some(raw_content) = m.content else { continue };
        let content_str = raw_content.get();
        // content 是字符串形态时跳过
        if !content_str.starts_with('[') {
            continue;
        }
        let Ok(blocks) = serde_json::from_str::<Vec<&serde_json::value::RawValue>>(content_str)
        else {
            continue;
        };
        for (bi, raw) in blocks.iter().enumerate() {
            let s = raw.get();
            if !(s.contains("\"thinking\"") || s.contains("\"redacted_thinking\"")) {
                continue;
            }
            if let Ok(v) = serde_json::from_str::<serde_json::Value>(s) {
                match v.get("type").and_then(|t| t.as_str()) {
                    Some("thinking") | Some("redacted_thinking") => {}
                    _ => continue,
                }
            } else {
                continue;
            }
            let start = s.as_ptr() as usize - base;
            let key = serde_json::from_str::<serde_json::Value>(s).ok().and_then(|v| {
                ["signature", "data"]
                    .iter()
                    .find_map(|k| v.get(*k).and_then(|x| x.as_str()).map(str::to_string))
            });
            out.push(ThinkingBlockRef { msg: mi, blk: bi, span: start..start + s.len(), key });
        }
    }
    out
}

/// 从上游 400 的 message 里解析 `messages.<i>.content.<j>` 这个坐标。
///
/// 官方这类错误恒以它开头（`messages.5.content.48: Invalid …`）；不是这个形态就返回 `None`，
/// 调用方照常透传，只是少一份定位信息。
pub(super) fn error_block_path(message: &str) -> Option<(usize, usize)> {
    let (msg, rest) = message.strip_prefix("messages.")?.split_once(".content.")?;
    let blk: &str = rest.split(|c: char| !c.is_ascii_digit()).next()?;
    Some((msg.parse().ok()?, blk.parse().ok()?))
}

/// 被上游点名的那个思考块在**入站**（客户端原件）与**出站**（luban 实际发出去的那份）两侧的
/// 对照，见 [`trace_thinking_block`]。
pub(super) struct ThinkingBlockTrace {
    /// 出站体里的坐标，也就是上游报错里那个（原样回显，供核对解析对没对）。
    pub(super) outbound_at: String,
    /// 入站体里同一个块的坐标；`none` 表示**入站体里根本没有这段载荷**。
    pub(super) inbound_at: String,
    /// 载荷（`redacted_thinking` 的 `data` / `thinking` 的 `signature`）的字节数。
    pub(super) payload_len: usize,
    /// 那一轮 assistant 消息在两侧是否逐字节相同。`None` 表示有一侧取不到那条消息。
    pub(super) turn_identical: Option<bool>,
    /// 那一轮的块型序列（`tool_use` 连名字一起，见 [`block_label`]），两侧各一份。
    pub(super) inbound_turn: String,
    pub(super) outbound_turn: String,
}

/// 拿上游点名的坐标，在出站体里取到那个思考块，再**用载荷本身当身份**回到入站体里找同一个块。
///
/// 载荷是 base64（`data` / `signature` 都是），JSON 转义策略碰不到它，改写前后逐字相同，
/// 所以这条比对不受「消息或块被删掉导致下标前移」的影响——这正是要它的原因：坐标对不上也
/// 照样找得到。
///
/// 打出来的几项各回答一个问题：
/// - `inbound_at=none`：入站体里没有这段载荷 = **luban 把它改坏了**，这是唯一一种 luban 的锅；
/// - `inbound_at` 与 `outbound_at` 不同：块还在，但下标前移过（有消息或块被删）；
/// - `turn_identical=false`：那一轮除这个块之外还被改过——工具名混淆（[`apply_tool_names`]）
///   会改历史里的 `tool_use.name`，两份 `*_turn` 一比就能看出改的是哪一块；
/// - 三项全对上：luban 原样转发，坏在客户端发来的那份或上游自己那边。
///
/// 不打任何正文，也不打载荷本身（几 KB 密文，没有信息量）。
pub(super) fn trace_thinking_block(
    inbound: &[u8],
    outbound: &[u8],
    mi: usize,
    bi: usize,
) -> Option<ThinkingBlockTrace> {
    let out_str = std::str::from_utf8(outbound).ok()?;
    let out_blocks = thinking_block_byte_ranges(out_str);
    let ob = out_blocks.iter().find(|b| b.msg == mi && b.blk == bi)?;
    let key = ob.key.clone();
    let payload_len = key.as_deref().map(str::len).unwrap_or(0);

    let in_blocks =
        std::str::from_utf8(inbound).map(thinking_block_byte_ranges).unwrap_or_default();
    let ib = key.as_deref().and_then(|k| in_blocks.iter().find(|b| b.key.as_deref() == Some(k)));

    let turn_of = |body: &[u8], idx: usize| -> Option<serde_json::Value> {
        serde_json::from_slice::<serde_json::Value>(body)
            .ok()?
            .get("messages")?
            .as_array()?
            .get(idx)
            .cloned()
    };
    let out_turn = turn_of(outbound, mi);
    let in_turn = ib.and_then(|b| turn_of(inbound, b.msg));
    // 逐字节比那一轮：两侧都已是 `serde_json::Value`，同一套序列化下的字节差异就是真差异
    // （`preserve_order` 保着键序）。序列化失败当作「比不出来」，不牵连整份对照。
    let turn_identical = match (&in_turn, &out_turn) {
        (Some(a), Some(b)) => match (serde_json::to_vec(a), serde_json::to_vec(b)) {
            (Ok(a), Ok(b)) => Some(a == b),
            _ => None,
        },
        _ => None,
    };

    Some(ThinkingBlockTrace {
        outbound_at: format!("messages.{mi}.content.{bi}"),
        inbound_at: ib
            .map(|b| format!("messages.{}.content.{}", b.msg, b.blk))
            .unwrap_or_else(|| "none".into()),
        payload_len,
        turn_identical,
        inbound_turn: in_turn.as_ref().map(turn_label).unwrap_or_else(|| "-".into()),
        outbound_turn: out_turn.as_ref().map(turn_label).unwrap_or_else(|| "-".into()),
    })
}

/// 剥除 `messages` 历史里 `thinking` 为空**且没有 `signature`** 的 `thinking` 块。
///
/// 上游对没有内容的 thinking 块回 400（`each thinking block must contain thinking`）。
/// 但**带签名的空块是合法的**：上游不带 `display` 时本来就只回空文本 + 签名（`cap/2.1.258`
/// 00025/00031、`cap/2.1.258-api` 00023/00025），官方 CC 下一轮原样回传，上游 200
/// （`cap/2.1.260` 00021/00025/00028/00029/00031，签名 776～2592 字节）。实跑日志里一条
/// 669 消息的 opus 请求被剥掉 33 块，**全部带签名**——那些都是不该动的。
///
/// 所以剥除只针对无签名的空块：那是第三方客户端自己拼出来的历史，上游必拒。带签名的
/// 原样放行，与 CC 直连形态一致，也不触发上游对 thinking 块的「不可修改」校验。
///
/// 与 [`strip_empty_text_blocks`] 对称：只剥目标块，留下其余内容块；若整个 `content`
/// 只有这种块则保留原样（空 `content` 数组是另一种 400）。
pub(super) fn strip_empty_thinking_blocks(v: &mut serde_json::Value) -> bool {
    let Some(msgs) = v.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return false;
    };
    let total = msgs.len();
    let mut changed = false;
    // 每个被剥块的形态：`msg=<第几条>/<总数> keys=[..] sig_len=none thinking=<missing|empty>`。
    // 按现在的判据 sig_len 恒为 none；保留字段是为了万一判据再变时日志形态不用改。
    let mut stripped: Vec<String> = Vec::new();
    let mut kept_all_empty: Vec<String> = Vec::new();
    for (mi, msg) in msgs.iter_mut().enumerate() {
        if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let Some(content) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            continue;
        };
        let is_empty_thinking = |blk: &serde_json::Value| {
            blk.get("type").and_then(|t| t.as_str()) == Some("thinking")
                && blk.get("thinking").and_then(|t| t.as_str()).is_none_or(|t| t.is_empty())
                && blk.get("signature").and_then(|s| s.as_str()).is_none_or(|s| s.is_empty())
        };
        let non_empty_count = content.iter().filter(|blk| !is_empty_thinking(blk)).count();
        if non_empty_count == content.len() {
            continue;
        }
        let shapes = content
            .iter()
            .filter(|blk| is_empty_thinking(blk))
            .map(|blk| format!("msg={}/{} {}", mi, total, empty_thinking_shape(blk)));
        if non_empty_count == 0 {
            kept_all_empty.extend(shapes);
            continue;
        }
        stripped.extend(shapes);
        content.retain(|blk| !is_empty_thinking(blk));
        changed = true;
    }
    if changed {
        tracing::info!(
            model = v.get("model").and_then(|m| m.as_str()).unwrap_or("-"),
            count = stripped.len(),
            blocks = %stripped.join("; "),
            "stripped unsigned empty thinking blocks from messages"
        );
    }
    if !kept_all_empty.is_empty() {
        tracing::warn!(
            model = v.get("model").and_then(|m| m.as_str()).unwrap_or("-"),
            count = kept_all_empty.len(),
            blocks = %kept_all_empty.join("; "),
            "kept unsigned empty thinking blocks: stripping would leave the message with empty content"
        );
    }
    changed
}

/// 一个空 thinking 块的形态摘要，供 [`strip_empty_thinking_blocks`] 与
/// [`block_label`] 打日志用：有哪些 key、签名多长、`thinking` 是缺失还是空串。
/// 不打签名本身（几 KB 的 base64，没有信息量），也不打任何正文。
pub(super) fn empty_thinking_shape(blk: &serde_json::Value) -> String {
    let keys: Vec<&str> =
        blk.as_object().map(|o| o.keys().map(String::as_str).collect()).unwrap_or_default();
    let sig_len = blk
        .get("signature")
        .and_then(|s| s.as_str())
        .map(|s| s.len().to_string())
        .unwrap_or_else(|| "none".into());
    let thinking = match blk.get("thinking") {
        None => "missing",
        Some(serde_json::Value::String(s)) if s.is_empty() => "empty",
        Some(serde_json::Value::String(_)) => "non-empty",
        Some(_) => "non-string",
    };
    format!("keys=[{}] sig_len={sig_len} thinking={thinking}", keys.join(","))
}

#[cfg(test)]
mod tests {
    use crate::proxy::Bytes;
    use crate::proxy::digest::block_label;
    use crate::proxy::test_support::{all_on, rewrite_body, test_cred};

    /// 删掉空壳 system 消息之后，thinking 块的原始字节仍要落回**它自己**那一块。
    ///
    /// [`preserve_thinking_encoding`] 原先按 `(消息下标, 块下标)` 配对；一旦有整条消息被删
    /// （空壳 system、role:"system" 提升）或消息内的块被剥（空 text / 无签名空 thinking），
    /// 下标就会前移，A 块的原始字节会被盖到 B 块上——历史与签名一起错乱，上游按签名校验必拒。
    /// 现在带 `signature` / `data` 的按那个值配，与下标无关。
    #[test]
    fn thinking_bytes_follow_their_own_block_when_messages_are_dropped() {
        // 两条空壳 system 夹在两轮之间；两轮 thinking 的正文都带 \u003c 转义（serde 重新
        // 序列化会解码成字面量 `<`，正是这套字节还原存在的理由）。
        const A: &str = r#"{"type":"thinking","thinking":"A\u003cx\u003e","signature":"sigA=="}"#;
        const B: &str = r#"{"type":"thinking","thinking":"B\u003cy\u003e","signature":"sigB=="}"#;
        let body = Bytes::from(format!(
            r#"{{"model":"claude-opus-5","system":[{{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude."}}],"messages":[{{"role":"system","content":[]}},{{"role":"user","content":"hi"}},{{"role":"assistant","content":[{A},{{"type":"text","text":"a"}}]}},{{"role":"system","content":[]}},{{"role":"user","content":"more"}},{{"role":"assistant","content":[{B},{{"type":"text","text":"b"}}]}}]}}"#
        ));
        let out = rewrite_body(&body, &test_cred(), "fp", all_on(), None, None);
        let s = String::from_utf8(out.to_vec()).unwrap();
        assert!(!s.contains(r#""role":"system""#), "空壳该被丢掉: {s}");
        assert!(s.contains(A), "A 轮的原始字节该原样落回 A 块: {s}");
        assert!(s.contains(B), "B 轮的原始字节该原样落回 B 块: {s}");
        assert_eq!(s.matches("sigA==").count(), 1, "A 的签名不该被复制到第二轮: {s}");
        assert_eq!(s.matches("sigB==").count(), 1, "B 的签名该还在: {s}");
        assert_eq!(s.matches(r"A\u003cx").count(), 1, "A 的正文只该出现一次: {s}");
    }

    // ---------- thinking 签名兜底 ----------

    /// 只认「signature + thinking 同现」这一种 400，别的 `invalid_request_error` 一律不碰——
    /// 误判的代价是给每个普通请求错误都白搭一次上游往返。
    #[test]
    fn detects_only_the_thinking_signature_400() {
        let hit = br#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.1.content.0: Invalid `signature` in `thinking` block"}}"#;
        assert!(crate::proxy::is_thinking_signature_error(hit));

        for miss in [
            // 普通请求形态错误。
            &br#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: must be greater than 0"}}"#[..],
            // 提到了 thinking 但不是签名问题（工具续跑那条）——降级救不了它，不该触发。
            &br#"{"type":"error","error":{"type":"invalid_request_error","message":"a final `assistant` message must start with a thinking block"}}"#[..],
            // 非 JSON 的拦截页：整段当 message 扫，同样不该命中。
            &b"<html>403 Forbidden</html>"[..],
        ] {
            assert!(!crate::proxy::is_thinking_signature_error(miss), "不该命中: {}", String::from_utf8_lossy(miss));
        }
    }

    /// `redacted_thinking` 的密文那条 400 自成一档：与签名、被改、空块三条判据互不误触。
    #[test]
    fn detects_only_the_redacted_thinking_data_400() {
        let hit = br#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.5.content.48: Invalid `data` in `redacted_thinking` block"}}"#;
        assert!(crate::proxy::is_redacted_thinking_data_error(hit));
        // 三条老判据都不该认领它，否则日志与重试原因会张冠李戴。
        assert!(!crate::proxy::is_thinking_signature_error(hit));
        assert!(!crate::proxy::is_thinking_modified_error(hit));
        assert!(!crate::proxy::is_empty_thinking_error(hit));

        for miss in [
            // 签名那条：有 thinking 没 redacted_thinking。
            &br#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.1.content.0: Invalid `signature` in `thinking` block"}}"#[..],
            // 被改那条：提到了 redacted_thinking，但没提 data，且另有专属判据。
            &br#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.1.content.0: `thinking` or `redacted_thinking` blocks in the latest assistant message cannot be modified."}}"#[..],
            // 普通请求形态错误。
            &br#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: must be greater than 0"}}"#[..],
            &b"<html>403 Forbidden</html>"[..],
        ] {
            assert!(
                !crate::proxy::is_redacted_thinking_data_error(miss),
                "不该命中: {}",
                String::from_utf8_lossy(miss)
            );
        }
    }

    /// 坐标解析：官方那句以 `messages.<i>.content.<j>:` 开头，别的形态一律给 `None`。
    #[test]
    fn parses_the_error_block_path() {
        assert_eq!(
            crate::proxy::error_block_path(
                "messages.5.content.48: Invalid `data` in `redacted_thinking` block"
            ),
            Some((5, 48))
        );
        assert_eq!(crate::proxy::error_block_path("messages.0.content.0: whatever"), Some((0, 0)));
        for miss in [
            "max_tokens: must be greater than 0",
            "messages.5: system content must contain at least one block",
            "messages.x.content.1: nope",
            "messages.5.content.x: nope",
            "",
        ] {
            assert_eq!(crate::proxy::error_block_path(miss), None, "不该解析出坐标: {miss}");
        }
    }

    // ---------- 被拒的 redacted_thinking 块：入站 / 出站对照 ----------

    /// 构造一份带两轮 assistant 的体：第 2 轮（下标 `2`）末块是 `redacted_thinking`。
    fn traceable_body(data: &str, tool: &str, lead_system: bool) -> Vec<u8> {
        let lead = if lead_system {
            r#"{"role":"system","content":[{"type":"text","text":"x"}]},"#
        } else {
            ""
        };
        format!(
            concat!(
                r#"{{"model":"claude-sonnet-5","messages":["#,
                r#"{{"role":"user","content":[{{"type":"text","text":"hi"}}]}},"#,
                "{lead}",
                r#"{{"role":"assistant","content":["#,
                r#"{{"type":"thinking","thinking":"t","signature":"SIG"}},"#,
                r#"{{"type":"tool_use","id":"tu1","name":"{tool}","input":{{}}}},"#,
                r#"{{"type":"redacted_thinking","data":"{data}"}}]}}]}}"#
            ),
            lead = lead,
            tool = tool,
            data = data
        )
        .into_bytes()
    }

    /// luban 一个字节没动：坐标两侧相同，那一轮也逐字节相同——这就是「坏在客户端发来的那份」。
    #[test]
    fn traces_an_untouched_redacted_block() {
        let body = traceable_body("ENCRYPTED", "Bash", false);
        let t = crate::proxy::trace_thinking_block(&body, &body, 1, 2).expect("该坐标上有思考块");
        assert_eq!(t.outbound_at, "messages.1.content.2");
        assert_eq!(t.inbound_at, "messages.1.content.2");
        assert_eq!(t.payload_len, "ENCRYPTED".len());
        assert_eq!(t.turn_identical, Some(true));
        assert_eq!(t.inbound_turn, t.outbound_turn);
        assert!(t.outbound_turn.contains("redacted_thinking(data_len=9)"), "{}", t.outbound_turn);
    }

    /// 出站少了一条消息（丢空壳 / 提升 role:"system"）：下标前移，但按密文仍找得到同一个块。
    #[test]
    fn traces_a_shifted_redacted_block() {
        let inbound = traceable_body("ENCRYPTED", "Bash", true);
        let outbound = traceable_body("ENCRYPTED", "Bash", false);
        let t = crate::proxy::trace_thinking_block(&inbound, &outbound, 1, 2)
            .expect("该坐标上有思考块");
        assert_eq!(t.outbound_at, "messages.1.content.2");
        assert_eq!(t.inbound_at, "messages.2.content.2", "按密文配对，不受下标前移影响");
        assert_eq!(t.turn_identical, Some(true), "那一轮本身没被改");
    }

    /// 出站那段密文入站体里根本没有：只有这一种情形是 luban 把它改坏了。
    #[test]
    fn traces_a_corrupted_redacted_block() {
        let inbound = traceable_body("ENCRYPTED", "Bash", false);
        let outbound = traceable_body("CORRUPTED", "Bash", false);
        let t = crate::proxy::trace_thinking_block(&inbound, &outbound, 1, 2)
            .expect("该坐标上有思考块");
        assert_eq!(t.inbound_at, "none");
        assert_eq!(t.turn_identical, None, "入站那一轮都定位不到，无从比对");
    }

    /// 密文原样、但那一轮里的 `tool_use.name` 被改过（工具名混淆）：两份 turn 摘要一比即见。
    #[test]
    fn traces_a_rewritten_turn_around_the_redacted_block() {
        let inbound = traceable_body("ENCRYPTED", "Bash", false);
        let outbound = traceable_body("ENCRYPTED", "mcp__luban__abcBas00", false);
        let t = crate::proxy::trace_thinking_block(&inbound, &outbound, 1, 2)
            .expect("该坐标上有思考块");
        assert_eq!(t.inbound_at, "messages.1.content.2", "块本身没动");
        assert_eq!(t.turn_identical, Some(false));
        assert!(t.inbound_turn.contains("tool_use(Bash)"), "{}", t.inbound_turn);
        assert!(t.outbound_turn.contains("tool_use(mcp__luban__abcBas00)"), "{}", t.outbound_turn);
    }

    /// 坐标上没有思考块（判据与形态对不上）：给 `None`，调用方打原文而不是编一份对照。
    #[test]
    fn traces_nothing_when_the_named_block_is_not_a_thinking_block() {
        let body = traceable_body("ENCRYPTED", "Bash", false);
        assert!(crate::proxy::trace_thinking_block(&body, &body, 1, 1).is_none(), "那是 tool_use");
        assert!(crate::proxy::trace_thinking_block(&body, &body, 9, 0).is_none(), "越界");
    }

    /// thinking 原文搬进 text、redacted_thinking 直接删，其余块与 key 序原样不动。
    #[test]
    fn demotes_thinking_to_text() {
        let raw = concat!(
            r#"{"model":"claude-opus-5","messages":["#,
            r#"{"role":"user","content":[{"type":"text","text":"hi"}]},"#,
            r#"{"role":"assistant","content":["#,
            r#"{"type":"thinking","thinking":"想了想","signature":"AAAA"},"#,
            r#"{"type":"redacted_thinking","data":"ZZZZ"},"#,
            r#"{"type":"text","text":"答案"}]}]}"#
        );
        let out = crate::proxy::demote_thinking_blocks(&Bytes::from(raw)).expect("应有可降级的块");
        let s = String::from_utf8(out.to_vec()).unwrap();

        assert!(!s.contains("\"thinking\""), "thinking 块应已消失: {s}");
        assert!(!s.contains("AAAA"), "签名应已丢弃: {s}");
        assert!(!s.contains("ZZZZ"), "redacted_thinking 应整块删掉: {s}");
        assert!(
            s.contains("<previous_thinking>\\n想了想\\n</previous_thinking>"),
            "推理原文应搬进 text: {s}"
        );
        assert!(s.contains(r#"{"type":"text","text":"答案"}"#), "原有 text 块应原样保留: {s}");
        // 降级块自己也照官方内容块的 type→text 键序写。
        assert!(
            s.contains(r#"{"type":"text","text":"<previous_thinking>"#),
            "降级块 key 被重排: {s}"
        );
    }

    /// user 轮不碰（它本来就没有 thinking 块，扫到也不该动），没得降级时返回 None——
    /// 避免为一条另有原因的 400 白发一次重试。
    #[test]
    fn skips_when_nothing_to_demote() {
        let raw = r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
        assert!(crate::proxy::demote_thinking_blocks(&Bytes::from(raw)).is_none());
        // 非 JSON、以及没有 messages 的请求体都不该 panic。
        assert!(crate::proxy::demote_thinking_blocks(&Bytes::from_static(b"not json")).is_none());
        assert!(
            crate::proxy::demote_thinking_blocks(&Bytes::from_static(br#"{"model":"x"}"#))
                .is_none()
        );
    }

    /// 整轮只有 thinking 的 assistant 消息原样留着：降级完 `content` 会是空数组，
    /// 那是上游必拒的形态，发出去反而把「多一次往返」变成「多一次注定失败的往返」。
    #[test]
    fn keeps_assistant_turn_that_would_become_empty() {
        let raw = concat!(
            r#"{"messages":[{"role":"assistant","content":["#,
            r#"{"type":"thinking","thinking":"  ","signature":"AAAA"}]},"#,
            r#"{"role":"assistant","content":[{"type":"thinking","thinking":"实打实","signature":"BBBB"},"#,
            r#"{"type":"text","text":"答案"}]}]}"#
        );
        let out = crate::proxy::demote_thinking_blocks(&Bytes::from(raw)).expect("第二轮可降级");
        let s = String::from_utf8(out.to_vec()).unwrap();
        assert!(s.contains("AAAA"), "空 thinking 那轮应原样留着: {s}");
        assert!(!s.contains("BBBB"), "第二轮仍应降级: {s}");
    }

    // ---------- thinking 块编码保持 ----------

    #[test]
    fn preserves_thinking_encoding_unicode_escape() {
        // 原始 body：thinking 内容含 < / >（如 Python json.dumps 产出）。
        let original = b"{\"model\":\"x\",\"messages\":[{\"role\":\"user\",\"content\":\"hi\"},{\"role\":\"assistant\",\"content\":[{\"type\":\"thinking\",\"thinking\":\"hello \\u003cworld\\u003e\",\"signature\":\"sig==\"},{\"type\":\"text\",\"text\":\"ok\"}]},{\"role\":\"user\",\"content\":\"bye\"}],\"stream\":true}";

        let orig_str = std::str::from_utf8(original.as_ref()).unwrap();
        assert!(orig_str.contains(r"\u003c"), "original should contain \\u003c escape: {orig_str}");

        // serde 反序列化把 < 解码成 <，重新序列化变成 literal <world>。
        let mut v: serde_json::Value = serde_json::from_slice(original.as_ref()).unwrap();
        v["stream"] = serde_json::Value::Bool(false);
        let rewritten = serde_json::to_vec(&v).unwrap();
        let rw_str = std::str::from_utf8(&rewritten).unwrap();
        assert!(
            rw_str.contains("hello <world>") && !rw_str.contains(r"\u003c"),
            "serde should decode \\u003c to literal <: {rw_str}"
        );

        let fixed = crate::proxy::preserve_thinking_encoding(original, rewritten);
        let fixed_str = std::str::from_utf8(&fixed).unwrap();

        // thinking 块应保留原始的 < 编码——不是解码后的 <。
        assert!(
            fixed_str.contains(r"\u003c") && fixed_str.contains(r"\u003e"),
            "thinking content should preserve original \\u003c encoding: {fixed_str}"
        );
        // 非 thinking 内容的改动（stream: false）应保留。
        assert!(
            fixed_str.contains(r#""stream":false"#),
            "non-thinking modifications should be preserved: {fixed_str}"
        );
    }

    #[test]
    fn preserve_thinking_noop_when_no_thinking() {
        let body = br#"{"model":"x","messages":[{"role":"user","content":"hi"}]}"#;
        let rewritten = body.to_vec();
        let result = crate::proxy::preserve_thinking_encoding(body, rewritten.clone());
        assert_eq!(result, rewritten);
    }

    /// 只有 `redacted_thinking` 块、顶层没有 `thinking` 字段的体：入口判据以前只找
    /// `"thinking"`，这类体整段跳过不还原。而 base64 里有 `/`、`\/` 又是合法 JSON 转义
    /// （PHP 那类编码器默认就这么写），serde 往返会把它还原成 `/`——逻辑值没变、字节变了，
    /// 上游按字节校验必拒，且降级重试也救不回下一轮。
    #[test]
    fn preserve_thinking_restores_escaped_slashes_in_a_redacted_only_body() {
        let original = br#"{"model":"claude-sonnet-5","messages":[{"role":"assistant","content":[{"type":"redacted_thinking","data":"ab\/cd+ef\/gh"}]}]}"#;
        let mut v: serde_json::Value = serde_json::from_slice(original.as_ref()).unwrap();
        v["stream"] = true.into(); // 任意一处真实改写，逼出一次重新序列化
        let rewritten = serde_json::to_vec(&v).unwrap();
        assert!(
            !String::from_utf8(rewritten.clone()).unwrap().contains(r"ab\/cd"),
            "前提：serde 会把 \\/ 写回成 /"
        );

        let fixed = crate::proxy::preserve_thinking_encoding(original, rewritten);
        let s = String::from_utf8(fixed).unwrap();
        assert!(
            s.contains(r#"{"type":"redacted_thinking","data":"ab\/cd+ef\/gh"}"#),
            "整块原始字节应还原（含 \\/ 转义）: {s}"
        );
        assert!(s.contains(r#""stream":true"#), "还原只针对思考块，改写本身不该被回滚: {s}");
    }

    #[test]
    fn preserve_thinking_handles_redacted() {
        let original = br#"{"messages":[{"role":"assistant","content":[{"type":"redacted_thinking","data":"abc+123"},{"type":"text","text":"ok"}]}]}"#;
        let mut v: serde_json::Value = serde_json::from_slice(original.as_ref()).unwrap();
        v["stream"] = true.into();
        let rewritten = serde_json::to_vec(&v).unwrap();

        let fixed = crate::proxy::preserve_thinking_encoding(original, rewritten);
        let s = std::str::from_utf8(&fixed).unwrap();
        assert!(
            s.contains(r#""data":"abc+123""#),
            "redacted_thinking data should preserve original encoding: {s}"
        );
    }

    // ---------- 空 thinking 块剥除 ----------

    #[test]
    fn strips_empty_thinking_blocks_and_keeps_all_empty_message() {
        let mut v = serde_json::json!({
            "model": "claude-fable-5-1",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "", "signature": "abc"},
                    {"type": "text", "text": "hello"}
                ]},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": ""}
                ]},
                {"role": "user", "content": [
                    {"type": "thinking", "thinking": ""}
                ]}
            ]
        });
        assert!(!crate::proxy::strip_empty_thinking_blocks(&mut v), "带签名的空块合法，整体无改动");
        let first = v["messages"][0]["content"].as_array().unwrap();
        assert_eq!(first.len(), 2, "带签名的空 thinking 块原样放行（cap/2.1.260 官方回传形态）");
        assert_eq!(v["messages"][1]["content"].as_array().unwrap().len(), 1, "全空的消息原样保留");
        assert_eq!(v["messages"][2]["content"].as_array().unwrap().len(), 1, "非 assistant 不动");
    }

    #[test]
    fn strips_only_unsigned_empty_thinking_blocks() {
        let mut v = serde_json::json!({
            "model": "claude-opus-5",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": ""},
                    {"type": "thinking", "thinking": "", "signature": ""},
                    {"type": "thinking", "thinking": "", "signature": "signed"},
                    {"type": "thinking", "thinking": "real", "signature": ""},
                    {"type": "text", "text": "hello"}
                ]}
            ]
        });
        assert!(crate::proxy::strip_empty_thinking_blocks(&mut v));
        let kept: Vec<String> =
            v["messages"][0]["content"].as_array().unwrap().iter().map(block_label).collect();
        assert_eq!(
            kept,
            ["thinking(len=0,sig_len=6)", "thinking(len=4,sig_len=0)", "text(len=5)"],
            "无签名空块剥掉（含 signature 为空串的），带签名或有内容的留下"
        );
    }

    #[test]
    fn empty_thinking_shape_reports_keys_signature_and_kind() {
        let signed = serde_json::json!({"type": "thinking", "thinking": "", "signature": "abcd"});
        assert_eq!(
            crate::proxy::empty_thinking_shape(&signed),
            "keys=[type,thinking,signature] sig_len=4 thinking=empty"
        );
        let bare = serde_json::json!({"type": "thinking"});
        assert_eq!(
            crate::proxy::empty_thinking_shape(&bare),
            "keys=[type] sig_len=none thinking=missing"
        );
    }

    #[test]
    fn detects_empty_thinking_error() {
        let body = br#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.2.content.0: each thinking block must contain thinking"}}"#;
        assert!(crate::proxy::is_empty_thinking_error(body));
        let other = br#"{"type":"error","error":{"type":"invalid_request_error","message":"Invalid signature in thinking block"}}"#;
        assert!(!crate::proxy::is_empty_thinking_error(other));
    }
}
