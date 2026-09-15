//! `thinking` / `redacted_thinking` 块的降级重试、原始字节编码保持与取证定位。

use axum::body::Bytes;

use super::ban::parse_upstream_error;
use super::digest::{block_label, turn_label};
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

/// 上游那条 400 是不是「历史思考块验不过」的任意一种，是的话给出是哪一种（只进日志）。
///
/// 三条各有自己的兜底重试——开关不同、降级后能不能救回来也不同，见 [`handle`] 里并排的
/// 那三段。但**取证问的是同一个问题**：那个块是 luban 改坏的，还是客户端发来就是坏的。
/// [`trace_thinking_block`] 本身也不分块型（`thinking` 按 `signature` 配对、
/// `redacted_thinking` 按 `data` 配对，两者都是 base64、改写前后逐字相同），所以三条共用
/// 同一段入站/出站对照，这个函数只负责认出「是这一类」并记下是哪一条。
///
/// 顺序即优先级，与那三段重试的先后一致：三条判据在实测形态上互不重叠（见测试里的互斥断言），
/// 但「被改」那句里 `thinking` 与 `redacted_thinking` 同现，上游哪天改措辞添上 `signature`
/// 或 `data` 就会有两条同时认领；排在前面的先认领，日志里的 `kind` 与实际走的那条兜底就不会
/// 张冠李戴。
pub(super) fn thinking_block_error_kind(body: &[u8]) -> Option<&'static str> {
    if is_thinking_signature_error(body) {
        Some("signature")
    } else if is_thinking_modified_error(body) {
        Some("modified")
    } else if is_redacted_thinking_data_error(body) {
        Some("redacted_data")
    } else {
        None
    }
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
    /// 入站体里同一个块的坐标，或没配上的理由（三者结论完全不同，见 [`InboundMatch`]）：
    /// `none` 表示**入站体里根本没有这段载荷**，`ambiguous(xN)` 表示同一段载荷有 N 处、
    /// 认不出是哪一个，`unkeyed` 表示这个块压根没有可当身份的载荷。
    pub(super) inbound_at: String,
    /// 载荷（`redacted_thinking` 的 `data` / `thinking` 的 `signature`）的字节数。
    pub(super) payload_len: usize,
    /// 那一轮 assistant 消息在两侧是否逐字节相同。`None` 表示有一侧取不到那条消息。
    pub(super) turn_identical: Option<bool>,
    /// 那一轮的块型序列（`tool_use` 连名字一起，见 [`block_label`]），两侧各一份。
    pub(super) inbound_turn: String,
    pub(super) outbound_turn: String,
}

/// 上游点名的那个坐标在一份体里落到什么上：`msgs=N role=R blocks=M at=<块标签>`。
///
/// [`trace_thinking_block`] 给 `None` 时（坐标上不是思考块）唯一能打的东西。那种情形下光有
/// 一句「定位不到」等于什么都没说，而这三项各回答一个问题：`msgs` 与 `role` 说坐标本身对不对得上
/// （上游那句「latest assistant message」指的是哪一条）；`blocks` 说那条消息在出站体里还剩几块，
/// 与入站一比就知道 luban 有没有剥掉过块；`at` 说那个位置现在装的是什么——对「cannot be modified」
/// 这条 400，上游记得那里是思考块而出站体里不是，本身就是答案。
pub(super) fn block_site(body: &[u8], mi: usize, bi: usize) -> String {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(body) else {
        return "<unparsable>".into();
    };
    let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) else {
        return "<no messages>".into();
    };
    let Some(msg) = msgs.get(mi) else { return format!("msgs={} <no messages.{mi}>", msgs.len()) };
    let n = msgs.len();
    let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("?");
    let Some(blocks) = msg.get("content").and_then(|c| c.as_array()) else {
        return format!("msgs={n} role={role} content=<not an array>");
    };
    match blocks.get(bi) {
        Some(b) => format!("msgs={n} role={role} blocks={} at={}", blocks.len(), block_label(b)),
        None => format!("msgs={n} role={role} blocks={} at=<out of range>", blocks.len()),
    }
}

/// 最后一条 assistant 消息在入站与出站两份体里的对照，见 [`latest_assistant_diff`]。
pub(super) struct LatestAssistant {
    /// 两侧各自那条消息的下标（`none` = 这份体里没有 assistant 消息）。
    pub(super) inbound_at: String,
    pub(super) outbound_at: String,
    /// 那一轮的**逻辑值**是否相同（两侧各自 `serde_json::Value` 再同一套序列化后比）。
    /// 抓的是结构性改写：块被剥掉、`tool_use.name` 被混淆。`None` 表示有一侧取不到。
    ///
    /// **不能单凭它判无罪**：Value 往返会抹平空白与转义，客户端发 `"\u0061"`、luban 出站写
    /// `"a"`，逻辑值一样而字节已经变了——而上游对思考块校验的正是字节。故另有下面那项。
    pub(super) turn_same: Option<bool>,
    /// 那一轮里**全部思考块的原始字节**是否逐字相同（见 [`thinking_bytes_of_turn`]）。
    /// 这一项才对得上上游的判据。`None` 表示有一侧不是合法 UTF-8 或取不到那一轮。
    pub(super) thinking_bytes_same: Option<bool>,
    /// 两侧各自的块型序列（[`turn_label`]，过长截断）。
    pub(super) inbound_turn: String,
    pub(super) outbound_turn: String,
}

/// 轮摘要进日志的长度上限。块标签本身不含正文，但一串几十块拼起来仍能刷屏。
const TURN_LABEL_CAP: usize = 400;

fn cap_label(s: String) -> String {
    match s.char_indices().nth(TURN_LABEL_CAP) {
        Some((at, _)) => format!("{}…(+{}B)", &s[..at], s.len() - at),
        None => s,
    }
}

/// 末尾那**一串连续的** assistant 消息的下标区间（左闭右开）。
///
/// 上游把相邻同角色的消息并成一轮（Messages API 的既定行为），所以它说的「latest assistant
/// message」是这一整串，不是数组里最后那一条。只看最后一条会把
/// `assistant(thinking) → assistant(tool_use)` 判成「末轮没有思考块」——而那一轮明明有，
/// [`demote_thinking_blocks`] 也确实动得到它。
fn latest_assistant_run(msgs: &[serde_json::Value]) -> Option<std::ops::Range<usize>> {
    let is_assistant =
        |m: &serde_json::Value| m.get("role").and_then(|r| r.as_str()) == Some("assistant");
    let end = msgs.iter().rposition(is_assistant)? + 1;
    let mut start = end - 1;
    while start > 0 && is_assistant(&msgs[start - 1]) {
        start -= 1;
    }
    Some(start..end)
}

/// 一串消息在摘要里的写法：块按上游合并后的顺序连起来，`assistant:blk,blk,…`。
/// 与 [`turn_label`] 同一套块标签，只是跨越整串。
fn run_label(msgs: &[serde_json::Value], run: std::ops::Range<usize>) -> String {
    let mut blocks: Vec<String> = Vec::new();
    for m in &msgs[run] {
        match m.get("content") {
            Some(serde_json::Value::Array(bs)) => blocks.extend(bs.iter().map(block_label)),
            Some(serde_json::Value::String(t)) => blocks.push(format!("text(len={})", t.len())),
            _ => blocks.push("?".into()),
        }
    }
    cap_label(format!("assistant:{}", blocks.join(",")))
}

/// 两份体各自末尾那串 assistant 消息的对照。
///
/// 「`thinking` blocks in the latest assistant message cannot be modified」这条 400 明说了是
/// **最后一条 assistant 消息**（按上游的合并口径即末尾那一串，见 [`latest_assistant_run`]），
/// 而它随手给的那个坐标未必对得上 luban 实际发出去的那份体——现网见过
/// `messages.65.content.13` 落在一份 551 条消息的体上、`messages.223.content.21` 落在一份
/// 1403 条的体上，两侧都越界（见 [`block_site`] 打出来的两侧落点）。坐标靠不住就别靠坐标：
/// 两侧各自找那一串自己比。
///
/// 两项判断分开给，因为它们回答的不是同一个问题，合成一个就必有一头失真：
/// - `turn_same=false`：那一串的**结构**被改过（块被剥、`tool_use.name` 被混淆），两份
///   `*_turn` 一比即见改的是哪一块；
/// - `thinking_bytes_same=false`：那一串里思考块的**原始字节**变了——上游校验的就是这个，
///   这一项为假才是这条 400 板上钉钉的成因。
///
/// 只有两项**都**为真才能说 luban 原样转发了那一轮，这条 400 的成因在 luban 之外（最常见的
/// 是那些块由另一个凭证签发，见 [`retry_demoted_thinking`] 的措辞）。
///
/// 为什么不直接比整轮的原始字节：[`rewrite_body`] 出站恒是紧凑序列化，客户端只要发过缩进
/// JSON，整轮的字节就必然不同——那会让 `false` 成为常态，把一条「luban 什么实质都没改」
/// 的请求指认成改过。而排版差异不在上游的判据里，思考块的字节在（`preserve_thinking_encoding`
/// 专门把它们还原回原样），所以字节这一项只取思考块。
pub(super) fn latest_assistant_diff(inbound: &[u8], outbound: &[u8]) -> LatestAssistant {
    let parse = |body: &[u8]| -> Option<(serde_json::Value, std::ops::Range<usize>)> {
        let v: serde_json::Value = serde_json::from_slice(body).ok()?;
        let run = latest_assistant_run(v.get("messages")?.as_array()?)?;
        Some((v, run))
    };
    let ib = parse(inbound);
    let ob = parse(outbound);
    type Parsed = Option<(serde_json::Value, std::ops::Range<usize>)>;
    let msgs = |v: &serde_json::Value| -> Vec<serde_json::Value> {
        v.get("messages").and_then(|m| m.as_array()).cloned().unwrap_or_default()
    };
    let at = |x: &Parsed| match x {
        None => "none".to_string(),
        // 一条就写一条，连着好几条才写区间——绝大多数请求是前者，别让日志凭空多个减号。
        Some((_, r)) if r.len() == 1 => format!("messages.{}", r.start),
        Some((_, r)) => format!("messages.{}-{}", r.start, r.end - 1),
    };
    let label = |x: &Parsed| match x {
        None => "-".to_string(),
        Some((v, r)) => run_label(&msgs(v), r.clone()),
    };
    // 结构比：两侧各自把那一串序列化成数组再比。这一步会抹平空白与转义，故只作数于
    // 「结构变了」这一头，判无罪要连下面那项一起看。串长不同（luban 拆了或并了消息）
    // 直接就是两个不同的数组，同样落到 false。
    let turn_same = match (&ib, &ob) {
        (Some((iv, ir)), Some((ov, or))) => {
            let (im, om) = (msgs(iv), msgs(ov));
            match (serde_json::to_vec(&im[ir.clone()]), serde_json::to_vec(&om[or.clone()])) {
                (Ok(a), Ok(b)) => Some(a == b),
                _ => None,
            }
        }
        _ => None,
    };
    // 字节比：只取那一串里的思考块，那才是上游按字节校验的东西。
    let thinking_bytes_same = match (
        std::str::from_utf8(inbound).ok().zip(ib.as_ref()),
        std::str::from_utf8(outbound).ok().zip(ob.as_ref()),
    ) {
        (Some((i, (_, ir))), Some((o, (_, or)))) => {
            Some(thinking_bytes_of_run(i, ir.clone()) == thinking_bytes_of_run(o, or.clone()))
        }
        _ => None,
    };
    LatestAssistant {
        inbound_at: at(&ib),
        outbound_at: at(&ob),
        turn_same,
        thinking_bytes_same,
        inbound_turn: label(&ib),
        outbound_turn: label(&ob),
    }
}

/// 末尾那串 assistant 消息里有没有 `thinking` / `redacted_thinking` 块。
///
/// [`is_thinking_modified_error`] 那条 400 点名的是**最后一条 assistant 消息**（按上游的合并
/// 口径即末尾那一串，见 [`latest_assistant_run`]），而 [`retry_demoted_thinking`] 的全部动作
/// 就是把思考块降级成 text、把 `redacted_thinking` 删掉。那一串里一个思考块都没有时，降级
/// 改不到它一个字节，重发出去的还是同一条被拒的形态——这一发上游往返是**注定白费的**，
/// 且它每轮复发（历史里那个缺口不会自己长回来）。
///
/// 现网形态：`assistant:tool_use(Edit)` 单块一串——上游当初连着 `tool_use` 一起签发的那个
/// thinking 块被客户端或它上游的中转丢掉了，于是每一轮都先撞一次 400、再白跑一次重试。
///
/// **按串不按条**：`assistant(thinking) → assistant(tool_use)` 在上游眼里是一轮，思考块在
/// 前一条上，降级动得到它，这种要照常重试。存疑一律算「有」——多跑一次重试只是回到改这道
/// 闸之前，而少跑一次就是把一条本可救回的会话判死。
///
/// **必须传出站体**（`sent`），不能传客户端原件。哪几条消息挨在一起是 [`rewrite_body`] 之后
/// 才定下来的：[`hoist_system_role_messages`] 与 [`drop_empty_system_messages`] 会把
/// `messages` 里的 `role:"system"` 整条摘走，于是
/// `assistant(thinking) → system → assistant(tool_use)` 出站时变成两条挨着的 assistant、
/// 被上游并成一轮。拿原件判，那条 system 还夹在中间，串就只剩最后一条、看着没有思考块，
/// 于是跳过一次**本该跑**的重试。出站体是上游真正看到并拒掉的那一份，不必去复刻改写规则。
///
/// 降级本身仍作用在客户端原件上（[`retry_demoted_thinking`] 拿 `client_body` 重走一遍
/// `shape`），这不矛盾：它对**每一条** assistant 消息一视同仁地降级，出站串里那些块无论
/// 原先隔着什么，源头都在原件里，降级都动得到。
pub(super) fn latest_assistant_has_thinking(outbound: &[u8]) -> bool {
    let Ok(v) = serde_json::from_slice::<serde_json::Value>(outbound) else { return false };
    let Some(msgs) = v.get("messages").and_then(|m| m.as_array()) else { return false };
    let Some(run) = latest_assistant_run(msgs) else { return false };
    msgs[run].iter().any(|m| {
        m.get("content").and_then(|c| c.as_array()).is_some_and(|bs| {
            bs.iter().any(|b| {
                matches!(
                    b.get("type").and_then(|t| t.as_str()),
                    Some("thinking") | Some("redacted_thinking")
                )
            })
        })
    })
}

/// 一串消息里全部思考块的**原始字节**，按出现顺序。
///
/// 取原文切片而不是解析后的值：上游对 `signature` / `data` 是按字节校验的，而
/// `serde_json` 往返会换掉转义策略（`\/` 写回 `/`、`\u0061` 写回 `a`），逻辑值没变、字节变了，
/// 正是 [`preserve_thinking_encoding`] 要防的那一种。
fn thinking_bytes_of_run(json: &str, run: std::ops::Range<usize>) -> Vec<&str> {
    thinking_block_byte_ranges(json)
        .into_iter()
        .filter(|b| run.contains(&b.msg))
        .map(|b| &json[b.span])
        .collect()
}

/// 入站体里那个对应块的定位结果。没配上的三种理由分开记：它们指向的结论完全相反，
/// 混成一个 `none` 就会把「认不出来」说成「luban 改坏了」，而那行日志的全部价值正在于此。
#[derive(Clone, Copy)]
enum InboundMatch<'a> {
    Found(&'a ThinkingBlockRef),
    /// 有载荷可配，入站体里一处都没有 —— 唯一一种 luban 的锅。
    Missing,
    /// 同一段载荷两侧不止一处，坐标也对不上：认不出是哪一个，附上入站那侧的处数。
    Ambiguous(usize),
    /// 这个块既没有 `signature` 也没有 `data`，没有可当身份的载荷。
    Unkeyed,
}

/// 在入站体里找出站那个块的对应物，口径与 [`preserve_thinking_encoding`] 一致：
/// **两侧各只出现一次**才认按载荷配得上。
///
/// 客户端把同一块贴了两遍（中转站转发的历史里常见）时，`find` 会一律配到第一处——出站问的
/// 是第二处，配回来的却是第一轮，`turn_identical` 于是恒为 `false`，日志报「luban 改了那一轮」，
/// 正好是这段取证要排除的那个误判。
///
/// 不唯一时还留一条能答的路，但**坐标本身不算数**：`coord_turn_identical` 说的是「入站与出站
/// 在同一条消息下标上那一轮逐字节相同」，相同才认这个坐标。
///
/// 只比坐标不够。前面丢过消息时（[`drop_empty_system_messages`]、[`hoist_system_role_messages`]）
/// 整串下标会前移，出站 `(mi, bi)` 上那个块可能来自入站的另一条消息，而入站同一坐标上恰好是
/// 另一处同款载荷——载荷对得上、坐标也对得上，配出来却是两条不同的轮次，`turn_identical` 于是
/// 又成了那个凭空的 `false`。轮次逐字节相同则不然：那一轮一致，它的第 `bi` 块自然是同一个块，
/// 报出来的三项都成立（贴了两遍但 luban 没挪动过任何东西，是重复里最常见的一种，这条能答就答）。
///
/// 轮次对不上就认 `Ambiguous`：那时「luban 改了那一轮」与「配错了同款载荷」长得一模一样，
/// 分不出来就不下结论。代价是真被改写过的那一轮在载荷重复时失去这条判断——宁可少一条，
/// 也不能给一个反过来的。
fn match_inbound_block<'a>(
    in_blocks: &'a [ThinkingBlockRef],
    out_blocks: &[ThinkingBlockRef],
    key: Option<&str>,
    (mi, bi): (usize, usize),
    coord_turn_identical: impl FnOnce() -> bool,
) -> InboundMatch<'a> {
    let Some(key) = key else { return InboundMatch::Unkeyed };
    let hits =
        |list: &[ThinkingBlockRef]| list.iter().filter(|b| b.key.as_deref() == Some(key)).count();
    let n_in = hits(in_blocks);
    if n_in == 0 {
        return InboundMatch::Missing;
    }
    // 出站那侧也要唯一：两处出站块共用一段载荷、入站只剩一处时（中间那轮被删），按载荷配
    // 同样会把另一轮的坐标报上来。
    if n_in == 1 && hits(out_blocks) == 1 {
        let found = in_blocks.iter().find(|b| b.key.as_deref() == Some(key));
        return found.map_or(InboundMatch::Missing, InboundMatch::Found);
    }
    // 载荷不唯一：先看那一轮立不立得住，立得住才去取坐标上那个块（闭包只在这条路上求值，
    // 唯一那条不多解一遍入站体）。
    let at_coord = coord_turn_identical()
        .then(|| {
            in_blocks.iter().find(|b| b.msg == mi && b.blk == bi && b.key.as_deref() == Some(key))
        })
        .flatten();
    at_coord.map_or(InboundMatch::Ambiguous(n_in), InboundMatch::Found)
}

/// 拿上游点名的坐标，在出站体里取到那个思考块，再**用载荷本身当身份**回到入站体里找同一个块。
///
/// 载荷是 base64（`data` / `signature` 都是），JSON 转义策略碰不到它，改写前后逐字相同，
/// 所以这条比对不受「消息或块被删掉导致下标前移」的影响——这正是要它的原因：坐标对不上也
/// 照样找得到。
///
/// 打出来的几项各回答一个问题：
/// - `inbound_at=none`：入站体里没有这段载荷 = **luban 把它改坏了**，这是唯一一种 luban 的锅
///   （`ambiguous(xN)` 与 `unkeyed` 是「认不出来」，不是这一档，见 [`InboundMatch`]）；
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

    let turn_of = |body: &[u8], idx: usize| -> Option<serde_json::Value> {
        serde_json::from_slice::<serde_json::Value>(body)
            .ok()?
            .get("messages")?
            .as_array()?
            .get(idx)
            .cloned()
    };
    let out_turn = turn_of(outbound, mi);
    // 载荷重复时给坐标背书的那一轮，见 [`match_inbound_block`]。取不到或序列化不了都算
    // 不成立——这条只用来**放行**一个坐标，存疑一律不放。
    let coord_turn_identical = || match (turn_of(inbound, mi), &out_turn) {
        (Some(a), Some(b)) => match (serde_json::to_vec(&a), serde_json::to_vec(b)) {
            (Ok(a), Ok(b)) => a == b,
            _ => false,
        },
        _ => false,
    };
    let matched = match_inbound_block(
        &in_blocks,
        &out_blocks,
        key.as_deref(),
        (mi, bi),
        coord_turn_identical,
    );
    let ib = match matched {
        InboundMatch::Found(b) => Some(b),
        _ => None,
    };
    let in_turn = ib.and_then(|b| turn_of(inbound, b.msg));
    // 比那一轮：两侧都已是 `serde_json::Value`，同一套序列化下的差异就是结构上的真差异
    // （`preserve_order` 保着键序）。序列化失败当作「比不出来」，不牵连整份对照。
    //
    // **这是逻辑值比较，不是字节比较**：Value 往返会抹平空白与转义。被点名的那个块无须
    // 担心——它是按 `key`（`signature` / `data`）配上的，配上即等于那段载荷逐字相同；这里
    // 比的是那一轮里**除它之外**还有没有被动过（工具名混淆那类）。要整轮思考块的字节级
    // 判断，见 [`latest_assistant_diff`] 的 `thinking_bytes_same`。
    let turn_identical = match (&in_turn, &out_turn) {
        (Some(a), Some(b)) => match (serde_json::to_vec(a), serde_json::to_vec(b)) {
            (Ok(a), Ok(b)) => Some(a == b),
            _ => None,
        },
        _ => None,
    };

    Some(ThinkingBlockTrace {
        outbound_at: format!("messages.{mi}.content.{bi}"),
        inbound_at: match matched {
            InboundMatch::Found(b) => format!("messages.{}.content.{}", b.msg, b.blk),
            InboundMatch::Missing => "none".into(),
            InboundMatch::Ambiguous(n) => format!("ambiguous(x{n})"),
            InboundMatch::Unkeyed => "unkeyed".into(),
        },
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

    /// 三条 400 共用同一段取证，但各记各的 `kind`；不属这一类的 400 一概不认领——认领了就是
    /// 给每个普通请求错误白打一行对照日志。
    #[test]
    fn classifies_all_three_thinking_400s() {
        for (msg, kind) in [
            ("messages.1.content.0: Invalid `signature` in `thinking` block", "signature"),
            (
                "messages.43.content.110: `thinking` or `redacted_thinking` blocks in the latest assistant message cannot be modified.",
                "modified",
            ),
            ("messages.5.content.48: Invalid `data` in `redacted_thinking` block", "redacted_data"),
        ] {
            let body = format!(
                r#"{{"type":"error","error":{{"type":"invalid_request_error","message":"{msg}"}}}}"#
            );
            assert_eq!(
                crate::proxy::thinking_block_error_kind(body.as_bytes()),
                Some(kind),
                "该归到 {kind}: {msg}"
            );
        }

        for miss in [
            // 空 thinking 块那条：另有专属的「整体 dump 入站体」路径，不该在这里再打一行。
            &br#"{"type":"error","error":{"type":"invalid_request_error","message":"messages.1.content.0: each thinking block must contain thinking"}}"#[..],
            // 提到了 thinking，但说的是末轮形态，不是某个块验不过。
            &br#"{"type":"error","error":{"type":"invalid_request_error","message":"a final `assistant` message must start with a thinking block"}}"#[..],
            &br#"{"type":"error","error":{"type":"invalid_request_error","message":"max_tokens: must be greater than 0"}}"#[..],
            &b"<html>403 Forbidden</html>"[..],
        ] {
            assert_eq!(
                crate::proxy::thinking_block_error_kind(miss),
                None,
                "不该认领: {}",
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

    /// 两轮 assistant 带着**同一段**密文（客户端把同一块贴了两遍，经中转站转发的历史里常见）。
    /// 两轮的 `tool_use.name` 不同，配错了轮次 `turn_identical` 立刻变 false。
    ///
    /// `leads` 是前面垫几条会被 luban 丢掉的消息（空壳 `role:"system"`）：垫 n 条再与垫 0 条
    /// 的那份对照，就是「出站整串下标前移 n 位」。
    fn duplicate_payload_body(leads: usize) -> Vec<u8> {
        let lead = r#"{"role":"system","content":[{"type":"text","text":"x"}]},"#.repeat(leads);
        format!(
            concat!(
                r#"{{"model":"claude-sonnet-5","messages":["#,
                r#"{{"role":"user","content":[{{"type":"text","text":"hi"}}]}},"#,
                "{lead}",
                r#"{{"role":"assistant","content":["#,
                r#"{{"type":"tool_use","id":"tu1","name":"Bash","input":{{}}}},"#,
                r#"{{"type":"redacted_thinking","data":"DUP"}}]}},"#,
                r#"{{"role":"user","content":[{{"type":"text","text":"more"}}]}},"#,
                r#"{{"role":"assistant","content":["#,
                r#"{{"type":"tool_use","id":"tu2","name":"Read","input":{{}}}},"#,
                r#"{{"type":"redacted_thinking","data":"DUP"}}]}}]}}"#
            ),
            lead = lead
        )
        .into_bytes()
    }

    /// 同一段密文出现两处、但坐标对得上：按坐标 + 载荷双证认下第二轮那个。
    ///
    /// 这条守的是按载荷配对那步少了唯一性检查的回归——`find` 一律给第一处，于是出站问的是
    /// 第 3 条消息、配回来的是第 1 条，`turn_identical` 变 false，日志报「luban 改了那一轮」，
    /// 而 luban 一个字节都没动。
    #[test]
    fn traces_the_right_one_of_two_identical_payloads() {
        let body = duplicate_payload_body(0);
        let t = crate::proxy::trace_thinking_block(&body, &body, 3, 1).expect("该坐标上有思考块");
        assert_eq!(t.outbound_at, "messages.3.content.1");
        assert_eq!(t.inbound_at, "messages.3.content.1", "不该配到第一处那个同款密文");
        assert_eq!(t.turn_identical, Some(true), "luban 什么都没改");
        assert!(t.outbound_turn.contains("tool_use(Read)"), "{}", t.outbound_turn);
    }

    /// 同一段密文出现两处，坐标又因为前移对不上：认不出是哪一个，落 `ambiguous`、不下结论。
    /// 宁可少一条判断也不能给一个错的——`turn_identical=false` 会被读成「luban 改了那一轮」。
    #[test]
    fn refuses_to_guess_between_two_identical_payloads() {
        let inbound = duplicate_payload_body(1);
        let outbound = duplicate_payload_body(0);
        let t = crate::proxy::trace_thinking_block(&inbound, &outbound, 3, 1)
            .expect("该坐标上有思考块");
        assert_eq!(t.inbound_at, "ambiguous(x2)");
        assert_eq!(t.turn_identical, None, "认不出对应块，就没有可比的那一轮");
        assert_eq!(t.inbound_turn, "-");
        assert!(t.outbound_turn.contains("tool_use(Read)"), "{}", t.outbound_turn);
    }

    /// 载荷重复、坐标也对得上，但坐标上装的是**另一个**同款块：仍要认 `ambiguous`。
    ///
    /// 丢掉两条空壳 `role:"system"` 后整串前移两位，出站 `messages.3.content.1` 是 Read 那轮的
    /// 密文，入站同一坐标上恰好是 Bash 那轮的同款密文——载荷对得上、坐标也对得上，配出来却是
    /// 两条不同的轮次。只认「坐标 + 载荷」双证的话这里会报 `turn_identical=false`，等于凭空
    /// 指认 luban 改了那一轮。
    #[test]
    fn refuses_a_coordinate_that_lands_on_the_other_duplicate() {
        let inbound = duplicate_payload_body(2);
        let outbound = duplicate_payload_body(0);
        // 前提先钉住：入站那个坐标上确实有一个同款载荷的块，否则这条用例是空转的。
        let decoy = crate::proxy::trace_thinking_block(&inbound, &inbound, 3, 1)
            .expect("入站同一坐标上也有一个同款密文块");
        assert!(decoy.outbound_turn.contains("tool_use(Bash)"), "{}", decoy.outbound_turn);

        let t = crate::proxy::trace_thinking_block(&inbound, &outbound, 3, 1)
            .expect("该坐标上有思考块");
        assert!(
            t.outbound_turn.contains("tool_use(Read)"),
            "出站问的是 Read 那轮: {}",
            t.outbound_turn
        );
        assert_eq!(t.inbound_at, "ambiguous(x2)", "坐标撞上了另一个同款块，不能认");
        assert_eq!(t.turn_identical, None);
        assert_eq!(t.inbound_turn, "-");
    }

    /// 坐标落点：定位不到思考块时唯一能打的东西，三项各答一个问题。
    #[test]
    fn block_site_reports_where_the_coordinate_lands() {
        let body = traceable_body("ENCRYPTED", "Bash", false);
        assert_eq!(
            crate::proxy::block_site(&body, 1, 2),
            "msgs=2 role=assistant blocks=3 at=redacted_thinking(data_len=9)"
        );
        // 上游点名的位置上不是思考块——「cannot be modified」那条 400 的现网形态。
        assert_eq!(
            crate::proxy::block_site(&body, 1, 1),
            "msgs=2 role=assistant blocks=3 at=tool_use(Bash)"
        );
        // 出站体里那条消息被剥短了：`blocks=` 两侧一比就看得出来。
        assert_eq!(
            crate::proxy::block_site(&body, 1, 9),
            "msgs=2 role=assistant blocks=3 at=<out of range>"
        );
        assert_eq!(
            crate::proxy::block_site(&body, 0, 0),
            "msgs=2 role=user blocks=1 at=text(len=2)"
        );
        assert_eq!(crate::proxy::block_site(&body, 7, 0), "msgs=2 <no messages.7>");
        assert_eq!(crate::proxy::block_site(b"not json", 0, 0), "<unparsable>");
        assert_eq!(crate::proxy::block_site(br#"{"model":"x"}"#, 0, 0), "<no messages>");
    }

    // ---------- 最后一条 assistant 消息的对照 ----------

    /// luban 一个字节没动：两侧同一条消息、逐字节相同。这是「坐标靠不住」时唯一还能作数的判断。
    #[test]
    fn latest_assistant_diff_sees_an_untouched_turn() {
        let body = traceable_body("ENCRYPTED", "Bash", false);
        let d = crate::proxy::latest_assistant_diff(&body, &body);
        assert_eq!(d.inbound_at, "messages.1");
        assert_eq!(d.outbound_at, "messages.1");
        assert_eq!(d.turn_same, Some(true));
        assert_eq!(d.thinking_bytes_same, Some(true));
        assert!(d.outbound_turn.starts_with("assistant:thinking("), "{}", d.outbound_turn);
    }

    /// 出站少了一条消息（丢空壳 / 提升 role:"system"）：下标不同，但那一轮本身没变。
    /// 下标一动就判为改过的话，每条被丢过空壳的请求都会被诬告一次。
    #[test]
    fn latest_assistant_diff_is_not_fooled_by_an_index_shift() {
        let inbound = traceable_body("ENCRYPTED", "Bash", true);
        let outbound = traceable_body("ENCRYPTED", "Bash", false);
        let d = crate::proxy::latest_assistant_diff(&inbound, &outbound);
        assert_eq!(d.inbound_at, "messages.2");
        assert_eq!(d.outbound_at, "messages.1");
        assert_eq!(d.turn_same, Some(true), "挪了位置不等于改了内容");
        assert_eq!(d.thinking_bytes_same, Some(true), "块的字节也没动");
    }

    /// 那一轮真被改过（工具名混淆）：`turn_same=false`，两份摘要一比就看出改的是哪一块。
    /// 而思考块的字节没动——两项分开给才说得清「改的是结构，不是上游校验的那部分」。
    #[test]
    fn latest_assistant_diff_catches_a_rewritten_turn() {
        let inbound = traceable_body("ENCRYPTED", "Bash", false);
        let outbound = traceable_body("ENCRYPTED", "mcp__luban__abcBas00", false);
        let d = crate::proxy::latest_assistant_diff(&inbound, &outbound);
        assert_eq!(d.turn_same, Some(false), "结构变了");
        assert_eq!(d.thinking_bytes_same, Some(true), "改的是工具名，思考块的字节没动");
        assert!(d.inbound_turn.contains("tool_use(Bash)"), "{}", d.inbound_turn);
        assert!(d.outbound_turn.contains("tool_use(mcp__luban__abcBas00)"), "{}", d.outbound_turn);
    }

    /// 一条 assistant 消息都没有：给 `none`，不下结论。
    #[test]
    fn latest_assistant_diff_reports_none_without_an_assistant_turn() {
        let body = br#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#;
        let d = crate::proxy::latest_assistant_diff(body, body);
        assert_eq!(d.inbound_at, "none");
        assert_eq!(d.outbound_at, "none");
        assert_eq!(d.turn_same, None);
        assert_eq!(d.thinking_bytes_same, None);
        assert_eq!(d.inbound_turn, "-");
    }

    /// 末轮是连续两条 assistant（上游并成一轮）：区间写成 `messages.A-B`，块按合并后的顺序
    /// 连起来，而 luban 改的是**靠前**那一条。只比数组里最后那一条会给出 `turn_same=true`
    /// 的伪无罪——改的那条压根没进比较。
    #[test]
    fn latest_assistant_diff_covers_the_whole_merged_run() {
        let run = |name: &str| {
            format!(
                concat!(
                    r#"{{"messages":[{{"role":"user","content":[{{"type":"text","text":"hi"}}]}},"#,
                    r#"{{"role":"assistant","content":[{{"type":"tool_use","id":"t1","name":"{name}","input":{{}}}}]}},"#,
                    r#"{{"role":"assistant","content":[{{"type":"text","text":"done"}}]}}]}}"#
                ),
                name = name
            )
        };
        let inbound = run("Bash");
        let outbound = run("mcp__luban__abcBas00");
        let d = crate::proxy::latest_assistant_diff(inbound.as_bytes(), outbound.as_bytes());
        assert_eq!(d.inbound_at, "messages.1-2", "整串都算这一轮");
        assert_eq!(d.turn_same, Some(false), "改的是串里靠前那条，不能算没改");
        assert!(d.inbound_turn.contains("tool_use(Bash)"), "{}", d.inbound_turn);
        assert!(
            d.inbound_turn.contains("text(len=4)"),
            "块要按合并后的顺序连起来: {}",
            d.inbound_turn
        );
    }

    /// 思考块在串里靠前那条上、被 luban 改了字节：同样要抓到。
    #[test]
    fn latest_assistant_diff_checks_thinking_bytes_across_the_run() {
        let run = |sig: &str| {
            format!(
                concat!(
                    r#"{{"messages":[{{"role":"assistant","content":[{{"type":"thinking","thinking":"t","signature":"{sig}"}}]}},"#,
                    r#"{{"role":"assistant","content":[{{"type":"text","text":"done"}}]}}]}}"#
                ),
                sig = sig
            )
        };
        let d = crate::proxy::latest_assistant_diff(
            run(r"ab\/cd==").as_bytes(),
            run("ab/cd==").as_bytes(),
        );
        assert_eq!(d.turn_same, Some(true), "逻辑值相同");
        assert_eq!(d.thinking_bytes_same, Some(false), "字节变了，且那个块不在串的最后一条上");
    }

    /// 逻辑值相同、字节不同：客户端把签名里的 `/` 写成 `\/`（PHP 那类编码器的默认），
    /// luban 出站写回 `/`。`turn_same` 看不出来（Value 往返抹平转义），而上游对签名是按字节
    /// 校验的——只认结构那一项就会把这条 400 的真凶判成无罪。
    #[test]
    fn latest_assistant_diff_catches_a_reencoded_signature() {
        let turn = |sig: &str| {
            format!(
                concat!(
                    r#"{{"messages":[{{"role":"user","content":[{{"type":"text","text":"hi"}}]}},"#,
                    r#"{{"role":"assistant","content":[{{"type":"thinking","thinking":"t","signature":"{sig}"}}]}}]}}"#
                ),
                sig = sig
            )
        };
        let inbound = turn(r"ab\/cd==");
        let outbound = turn("ab/cd==");
        let d = crate::proxy::latest_assistant_diff(inbound.as_bytes(), outbound.as_bytes());
        assert_eq!(d.turn_same, Some(true), "逻辑值确实相同，这一项看不出问题");
        assert_eq!(d.thinking_bytes_same, Some(false), "字节变了，上游校验的正是它");
    }

    /// 反过来：整轮排版变了（客户端发缩进 JSON，出站恒紧凑），但思考块的字节被
    /// `preserve_thinking_encoding` 原样还原。字节这一项只取思考块，正是为了不把这种
    /// 「什么实质都没改」的请求指认成改过。
    #[test]
    fn latest_assistant_diff_ignores_reformatting_around_the_blocks() {
        let inbound = concat!(
            "{\n  \"messages\": [\n    {\"role\": \"user\", \"content\": [{\"type\": \"text\", \"text\": \"hi\"}]},\n",
            "    {\"role\": \"assistant\", \"content\": [{\"type\":\"thinking\",\"thinking\":\"t\",\"signature\":\"SIG\"}]}\n  ]\n}"
        );
        let outbound = concat!(
            r#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]},"#,
            r#"{"role":"assistant","content":[{"type":"thinking","thinking":"t","signature":"SIG"}]}]}"#
        );
        assert_ne!(inbound, outbound, "两份原始字节本来就不同");
        let d = crate::proxy::latest_assistant_diff(inbound.as_bytes(), outbound.as_bytes());
        assert_eq!(d.turn_same, Some(true));
        assert_eq!(d.thinking_bytes_same, Some(true), "块本身逐字相同，排版不算改");
    }

    /// 最后一条 assistant 消息里有没有可降级的思考块——「被改过」那条 400 要不要花一次
    /// 上游往返去重试，全看这个。
    #[test]
    fn latest_assistant_has_thinking_gates_the_pointless_retry() {
        // 现网形态：末轮只有一个 tool_use，思考块被客户端丢了。降级改不到它，重试白跑。
        let no_thinking = br#"{"messages":[
            {"role":"user","content":[{"type":"text","text":"hi"}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{}}]}]}"#;
        assert!(!crate::proxy::latest_assistant_has_thinking(no_thinking));

        // 末轮带思考块：降级动得到它，该重试。
        let with_thinking = br#"{"messages":[
            {"role":"assistant","content":[
                {"type":"thinking","thinking":"t","signature":"SIG"},
                {"type":"tool_use","id":"t1","name":"Edit","input":{}}]}]}"#;
        assert!(crate::proxy::latest_assistant_has_thinking(with_thinking));

        // redacted_thinking 同样算：降级对它是整块删。
        let redacted = br#"{"messages":[{"role":"assistant","content":[
            {"type":"redacted_thinking","data":"ZZZZ"}]}]}"#;
        assert!(crate::proxy::latest_assistant_has_thinking(redacted));

        // 隔着一条 user 的更早那轮不算：上游不会把它并进来，降级救不了被点名的那一轮。
        let only_earlier = br#"{"messages":[
            {"role":"assistant","content":[{"type":"thinking","thinking":"t","signature":"SIG"}]},
            {"role":"user","content":[{"type":"text","text":"go on"}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{}}]}]}"#;
        assert!(!crate::proxy::latest_assistant_has_thinking(only_earlier));

        // 连续两条 assistant：上游并成一轮，思考块在前一条上，降级动得到它 —— 该重试。
        // 只看数组里最后那一条会判成「没有」，把一条本可救回的会话判死。
        let merged_run = br#"{"messages":[
            {"role":"user","content":[{"type":"text","text":"hi"}]},
            {"role":"assistant","content":[{"type":"thinking","thinking":"t","signature":"SIG"}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{}}]}]}"#;
        assert!(
            crate::proxy::latest_assistant_has_thinking(merged_run),
            "相邻同角色会被上游并成一轮，思考块在串里就算有"
        );

        // 夹着一条 `role:"system"` 的两条 assistant：**按这份体**它们不相邻，串就只有最后
        // 那一条，判「没有」是对的。但 rewrite_body 会把这条 system 整条摘走
        // （hoist_system_role_messages / drop_empty_system_messages），出站时两条 assistant
        // 挨在一起、被上游并成一轮，那一轮是带思考块的——所以调用处必须传出站体。
        // 下面两条断言钉的就是这个差别：同一段历史，改写前后结论相反。
        let separated = br#"{"messages":[
            {"role":"user","content":[{"type":"text","text":"hi"}]},
            {"role":"assistant","content":[{"type":"thinking","thinking":"t","signature":"SIG"}]},
            {"role":"system","content":[{"type":"text","text":"deferred tools"}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{}}]}]}"#;
        assert!(
            !crate::proxy::latest_assistant_has_thinking(separated),
            "这份体里那条 system 还夹在中间，串确实只有最后一条"
        );
        let hoisted = br#"{"system":[{"type":"text","text":"deferred tools"}],"messages":[
            {"role":"user","content":[{"type":"text","text":"hi"}]},
            {"role":"assistant","content":[{"type":"thinking","thinking":"t","signature":"SIG"}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{}}]}]}"#;
        assert!(
            crate::proxy::latest_assistant_has_thinking(hoisted),
            "system 被提升走之后两条 assistant 相邻，这一轮带着思考块，该重试"
        );

        // 三条连着、思考块在最前面那条：整串都要看，不是只看倒数第二条。
        let long_run = br#"{"messages":[
            {"role":"assistant","content":[{"type":"thinking","thinking":"t","signature":"SIG"}]},
            {"role":"assistant","content":[{"type":"text","text":"a"}]},
            {"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"Edit","input":{}}]}]}"#;
        assert!(crate::proxy::latest_assistant_has_thinking(long_run));

        // 取不到就当没有：宁可少跑一次重试，也不拿一次上游往返去赌。
        for none in [
            &br#"{"messages":[{"role":"user","content":[{"type":"text","text":"hi"}]}]}"#[..],
            &br#"{"messages":[{"role":"assistant","content":"plain string"}]}"#[..],
            &br#"{"model":"x"}"#[..],
            &b"not json"[..],
        ] {
            assert!(
                !crate::proxy::latest_assistant_has_thinking(none),
                "不该算有: {}",
                String::from_utf8_lossy(none)
            );
        }
    }

    /// 轮摘要封顶：块标签不含正文，但一轮几十块拼起来照样刷屏。
    #[test]
    fn latest_assistant_diff_caps_a_long_turn_label() {
        let blocks = (0..80)
            .map(|i| format!(r#"{{"type":"tool_use","id":"t{i}","name":"Bash","input":{{}}}}"#))
            .collect::<Vec<_>>()
            .join(",");
        let body = format!(r#"{{"messages":[{{"role":"assistant","content":[{blocks}]}}]}}"#);
        let d = crate::proxy::latest_assistant_diff(body.as_bytes(), body.as_bytes());
        assert!(d.outbound_turn.contains("…(+"), "该截断: {}", d.outbound_turn);
        assert!(d.outbound_turn.chars().count() < 450, "截断后仍太长: {}", d.outbound_turn);
        assert_eq!(d.turn_same, Some(true), "截断只影响日志，不影响比对");
    }

    /// 块既没有 `signature` 也没有 `data`：没有可当身份的载荷，落 `unkeyed`。
    /// 不能落 `none`——那一档的意思是「luban 把它改坏了」。
    #[test]
    fn marks_a_payloadless_block_unkeyed_not_missing() {
        let body = concat!(
            r#"{"model":"claude-sonnet-5","messages":["#,
            r#"{"role":"user","content":[{"type":"text","text":"hi"}]},"#,
            r#"{"role":"assistant","content":[{"type":"thinking","thinking":"t"}]}]}"#
        )
        .as_bytes();
        let t = crate::proxy::trace_thinking_block(body, body, 1, 0).expect("该坐标上有思考块");
        assert_eq!(t.inbound_at, "unkeyed");
        assert_eq!(t.payload_len, 0);
        assert_eq!(t.turn_identical, None);
    }

    /// 签名那条 400 走的是同一段对照：`thinking` 块按 `signature` 配对，与密文侧对称。
    /// 配错块型的话 `payload_len` 会是密文那 9 个字节。
    #[test]
    fn traces_a_thinking_block_by_its_signature() {
        let body = traceable_body("ENCRYPTED", "Bash", false);
        let t = crate::proxy::trace_thinking_block(&body, &body, 1, 0).expect("该坐标上有思考块");
        assert_eq!(t.outbound_at, "messages.1.content.0");
        assert_eq!(t.inbound_at, "messages.1.content.0");
        assert_eq!(t.payload_len, "SIG".len(), "载荷记的是签名，不是同一轮里那段密文");
        assert_eq!(t.turn_identical, Some(true));
    }

    /// 签名侧的「luban 改坏了」：出站那个签名入站体里根本没有，`inbound_at=none`。
    #[test]
    fn traces_a_corrupted_thinking_signature() {
        let inbound = traceable_body("ENCRYPTED", "Bash", false);
        let outbound = String::from_utf8(inbound.clone())
            .expect("固定字面量")
            .replace("\"SIG\"", "\"XIG\"")
            .into_bytes();
        let t = crate::proxy::trace_thinking_block(&inbound, &outbound, 1, 0)
            .expect("该坐标上有思考块");
        assert_eq!(t.inbound_at, "none");
        assert_eq!(t.turn_identical, None, "入站那一轮都定位不到，无从比对");
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
