//! 缓存形态对齐：system 分块、消息断点与 `cache_control` 的写法。

use super::*;

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
pub(in crate::proxy) fn align_system_shape(v: &mut serde_json::Value, cache: CacheShape) -> bool {
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
/// （[`is_system_directive`]）与只管一轮的 system（[`is_turn_scoped_system`]，上游不许它带
/// `cache_control`，官方把断点放在它前一条上）。
///
/// 指令式那条 `content` 是空数组，挂不上断点；以前它被当空壳丢掉，前一条自然成了末条，
/// 现在原样留着，不跳过的话 [`align_message_shape`] / [`ensure_cc_message_breakpoint`] 摸到
/// 空数组就直接返回，前面整段历史失去自动缓存。断点落在它前一条上，缓存前缀覆盖的内容与
/// 丢掉它时一样；指令本身留在原位，不挪。
fn last_cacheable_message_mut(msgs: &mut [serde_json::Value]) -> Option<&mut serde_json::Value> {
    msgs.iter_mut().rev().find(|m| !is_system_directive(m) && !is_turn_scoped_system(m))
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
pub(in crate::proxy) fn align_message_shape(v: &mut serde_json::Value, shape: CacheShape) -> bool {
    let mut changed = false;
    let Some(msgs) = v.get_mut("messages").and_then(|m| m.as_array_mut()) else { return false };
    for m in msgs.iter_mut() {
        // 只管一轮的 system 不转：官方抓包里它恒为字符串（`cap/auto-2.1.291-20261006-full/00465`），
        // 它也不挂断点，转了没用处。
        if is_turn_scoped_system(m) {
            continue;
        }
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
pub(in crate::proxy) fn cache_prefix_of(v: &serde_json::Value) -> CachePrefix {
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
pub(in crate::proxy) fn ensure_cc_message_breakpoint(v: &mut serde_json::Value) -> bool {
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
pub(super) fn fill_cache_ttl(v: &mut serde_json::Value) -> bool {
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
pub(in crate::proxy) fn text_block(
    text: &str,
    cache_control: serde_json::Value,
) -> serde_json::Value {
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
/// （[`has_cc_identity`]：CC 那句含 agent-sdk 的逗号变体，以及 SDK 子代理那句）已在任一块里，就只在
/// 最前面插 billing header；官方序本来就是 billing 在身份句之前，客户端的身份句连同它自己的
/// `cache_control` 原样留在第二块。
///
/// `client` 是这个来访**自报**的身份（从它自己的 UA 里解出），补出来的 `cc_version` 与
/// `cc_entrypoint` 就用它，见 [`billing_header_text`]。
pub(super) fn ensure_cc_system_prefix(
    v: &mut serde_json::Value,
    client: Option<CcClient<'_>>,
    // 缺身份句时要不要一并补上；billing-only 下只补 billing header。
    with_identity: bool,
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
        Some(serde_json::Value::Array(blocks)) => blocks
            .iter()
            .any(|b| b.get("text").and_then(|t| t.as_str()).is_some_and(has_cc_identity)),
        Some(serde_json::Value::String(s)) => has_cc_identity(s),
        _ => false,
    };
    let mut prefix = vec![text_block_bare(&billing_header_text(
        v,
        client.map(|c| c.version),
        client.map(|c| c.entrypoint),
    ))];
    if !has_identity && with_identity {
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
    } else if !with_identity {
        tracing::info!("injected billing header only (billing-only) into system for a CC client");
    } else {
        tracing::info!(
            "injected billing header + identity into system for a CC client without them"
        );
    }
    true
}

/// 不带缓存断点的 `system` 文本块（官方的 `system[0]`/`system[1]` 都是这个形态）。
pub(in crate::proxy) fn text_block_bare(text: &str) -> serde_json::Value {
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
    pub(in crate::proxy) global: bool,
    /// 写 `ttl:"1h"`。官方**每个断点都带**，见 [`store::ForwardFlags::cache_ttl_1h`]。
    pub(in crate::proxy) ttl_1h: bool,
}

impl CacheShape {
    /// 非基座断点的形态：去掉 `scope`、保留 `ttl`——官方只在基座标 `scope`
    /// （`cap/raw/00006` 三个断点里仅一个有），而三个断点**都**有 `ttl`。
    pub(in crate::proxy) fn tail(self) -> Self {
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
pub(in crate::proxy) fn cache_control(shape: CacheShape) -> serde_json::Value {
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
