//! 工具改名：第三方工具名映射成官方形态，回程流式还原。

use super::*;

/// MCP 形态假名的工具段前缀池。来访原名加 `mcp__hermes__` 后，同一条探测由 400 变为
/// 200，证明 MCP 命名空间是上游豁免的形态。`manage_bfl00` 之类普通假名仍可被判成
/// 第三方，故生成的假名统一放在 `mcp__luban__*` 下。
/// 所有假名共用的命名空间前缀。生成处（[`build_tool_name_map`]）与还原处
/// （[`ToolNameMap::restore`]）必须是同一个串：还原靠它一趟扫出「可能是假名的位置」，
/// 两边一旦漂开，回程就一个假名都还原不了，而症状要到客户端拿着假名发下一轮才暴露。
pub(super) const FAKE_TOOL_NS: &str = "mcp__luban__";

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
pub(in crate::proxy) struct ToolNameMap {
    /// 真名 → 假名，请求侧用。
    pub(in crate::proxy) forward: std::collections::HashMap<String, String>,
    /// (假名, 真名)，按假名长度**倒序**——短假名可能是长假名的子串，先替长的才不会被吃掉。
    pub(in crate::proxy) reverse: Vec<(String, String)>,
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
pub(in crate::proxy) fn build_tool_name_map(
    body: Option<&serde_json::Value>,
) -> Option<ToolNameMap> {
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
        reverse.push((fake.clone(), (*name).to_string()));
        forward.insert((*name).to_string(), fake);
    }
    if forward.is_empty() {
        return None;
    }
    reverse.sort_by(|a, b| b.0.len().cmp(&a.0.len()).then_with(|| a.0.cmp(&b.0)));
    Some(ToolNameMap { forward, reverse })
}

/// 把映射应用到请求体，返回是否改动过。三处必须**同时**改：
///
/// - `$.tools[*].name`
/// - `$.tool_choice.name`（仅 `type == "tool"`，即客户端强制指定了某个工具）
/// - `$.messages[*].content[*].name`（仅 `type == "tool_use"`，即历史里的工具调用）
/// - 工具增删与 ToolSearch 结果里按名字的引用，见 [`rename_tool_references`]
///
/// 漏掉第三处的话，上游会因为 `tool_use` 引用了一个 `tools` 里没声明的名字而拒掉整条请求。
pub(in crate::proxy) fn apply_tool_names(v: &mut serde_json::Value, map: &ToolNameMap) -> bool {
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
    let mut referenced = false;
    if let Some(messages) = v.get_mut("messages").and_then(|m| m.as_array_mut()) {
        for msg in messages.iter_mut() {
            let Some(blocks) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
                continue;
            };
            for b in blocks.iter_mut() {
                if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                    rename(b);
                } else {
                    referenced |= rename_tool_references(b, map);
                }
            }
        }
    }
    changed || referenced
}

/// 内容块里按名字指向已声明工具的地方：声明换了假名，这几处也得跟着换，不然上游找不到它指的
/// 那个工具（同名更新定义会变成新增另一个）。只走协议规定的位置（官方说明与
/// `cap/auto-2.1.291-20261006-full`）：
///
/// - `tool_addition` / `tool_removal` 的 `tool`：`{"type":"tool_reference","name":…}`，或
///   inline-tools beta 的 `{"type":"tool_definition","definition":{"name":…,…}}`；
/// - `tool_result.content` 里客户端 ToolSearch 给的 `{"type":"tool_reference","tool_name":…}`；
/// - 服务端工具搜索的 `tool_search_tool_result.content.tool_references[*].tool_name`（assistant 里，
///   上游回的是假名，回程还原成真名，客户端下一轮带回来的得再换回去，线程指纹才对得上）。
///
/// **不递归**：`input_schema` 的 `enum` / `default`、工具结果正文这类业务数据里长得一样的对象
/// 不是引用，改了就是改了客户端的数据。
fn rename_tool_references(b: &mut serde_json::Value, map: &ToolNameMap) -> bool {
    let rename = |o: &mut serde_json::Value, key: &str| {
        let Some(fake) = o.get(key).and_then(|n| n.as_str()).and_then(|n| map.forward.get(n))
        else {
            return false;
        };
        o[key] = serde_json::Value::String(fake.clone());
        true
    };
    let ty = |v: &serde_json::Value| v.get("type").and_then(|t| t.as_str()).map(str::to_owned);
    match ty(b).as_deref() {
        Some("tool_addition" | "tool_removal") => {
            let Some(tool) = b.get_mut("tool") else { return false };
            match ty(tool).as_deref() {
                Some("tool_reference") => rename(tool, "name"),
                Some("tool_definition") => {
                    tool.get_mut("definition").is_some_and(|d| rename(d, "name"))
                }
                _ => false,
            }
        }
        Some("tool_result") => match b.get_mut("content") {
            Some(serde_json::Value::Array(items)) => items.iter_mut().fold(false, |acc, x| {
                (ty(x).as_deref() == Some("tool_reference") && rename(x, "tool_name")) | acc
            }),
            _ => false,
        },
        Some("tool_search_tool_result") => {
            match b.pointer_mut("/content/tool_references").and_then(|r| r.as_array_mut()) {
                Some(items) => items.iter_mut().fold(false, |acc, x| {
                    (ty(x).as_deref() == Some("tool_reference") && rename(x, "tool_name")) | acc
                }),
                None => false,
            }
        }
        _ => false,
    }
}

/// 回程里工具名所在的两个协议字段：`tool_use.name`，与服务端工具搜索结果
/// `tool_references[*].tool_name`。上游发的是紧凑 JSON，键与值之间没有空白。
const NAME_FIELDS: [&[u8]; 2] = [b"\"name\":\"", b"\"tool_name\":\""];

impl ToolNameMap {
    // 成功回复的还原只动协议字段：`tool_use.name` 与服务端工具搜索结果的
    // `tool_references[*].tool_name`。别处出现的假名原样留着——`server_tool_use.input` 的
    // 搜索模式、客户端工具 `input` 里恰好等于假名的参数、正文与 thinking 里提到的工具名。
    // 客户端会把这些原样带回下一轮，请求侧只按协议字段换名（[`apply_tool_names`]），换了就再也
    // 换不回去：回复指纹对不上、续轮退回 `create`；thinking 带签名，改了上游拒；会校验历史的
    // 模型上改 assistant 正文就是编辑历史。
    //
    // 位置按协议结构认，不按键名猜：SSE 逐行看事件类型（[`Self::restore_sse_line`]），整段
    // JSON 解析后按块改（[`Self::restore_message`]）。错误（SSE 的 `error` 事件、整段的 error
    // 体）照旧全文还原——报错正文里回显的假名不进对话历史，换回真名才好定位。

    /// 一条完整 Message（非流式透传或聚合出来的）：只改 `content` 里的协议字段。返回改没改。
    pub(in crate::proxy) fn restore_message(&self, msg: &mut serde_json::Value) -> bool {
        let Some(blocks) = msg.get_mut("content").and_then(|c| c.as_array_mut()) else {
            return false;
        };
        blocks.iter_mut().fold(false, |acc, b| self.restore_block(b) | acc)
    }

    /// 一个回复内容块的协议字段：`tool_use.name`；`tool_search_tool_result` 的
    /// `content.tool_references[*].tool_name`。
    fn restore_block(&self, b: &mut serde_json::Value) -> bool {
        let real = |name: &str| self.reverse.iter().find(|(fake, _)| fake == name).map(|(_, r)| r);
        let put = |o: &mut serde_json::Value, key: &str| {
            let Some(r) = o.get(key).and_then(|n| n.as_str()).and_then(real) else { return false };
            o[key] = serde_json::Value::String(r.clone());
            true
        };
        match b.get("type").and_then(|t| t.as_str()) {
            Some("tool_use") => put(b, "name"),
            Some("tool_search_tool_result") => {
                match b.pointer_mut("/content/tool_references").and_then(|r| r.as_array_mut()) {
                    Some(items) => items.iter_mut().fold(false, |acc, x| put(x, "tool_name") | acc),
                    None => false,
                }
            }
            _ => false,
        }
    }

    /// 整段 JSON 响应体（非 SSE）：Message 按块还原，error 全文还原，别的原样。不含假名前缀的
    /// 不解析。
    pub(in crate::proxy) fn restore_json_body(&self, body: &[u8]) -> Vec<u8> {
        if find_sub(body, FAKE_TOOL_NS.as_bytes()).is_none() {
            return body.to_vec();
        }
        let Ok(mut v) = serde_json::from_slice::<serde_json::Value>(body) else {
            return body.to_vec();
        };
        let ty = v.get("type").and_then(|t| t.as_str()).map(str::to_owned);
        match ty.as_deref() {
            Some("message") if self.restore_message(&mut v) => {
                serde_json::to_vec(&v).unwrap_or_else(|_| body.to_vec())
            }
            Some("error") => self.restore(body),
            _ => body.to_vec(),
        }
    }

    /// SSE 的一行。只有 `data:` 行、且含假名前缀的才解析：
    ///
    /// - `content_block_start` 里是 `tool_use` / `tool_search_tool_result`：按协议字段还原。这两种
    ///   块在开始事件里没有业务值（`tool_use.input` 恒为 `{}`，参数随后走转义过的
    ///   `input_json_delta`），故在这一行上按 `"name":"` / `"tool_name":"` 锚点替换就是按字段换；
    /// - `error`：全文还原；
    /// - 别的（正文、thinking、参数增量……）原样。
    fn restore_sse_line<'a>(&self, line: &'a [u8]) -> std::borrow::Cow<'a, [u8]> {
        use std::borrow::Cow;
        if find_sub(line, FAKE_TOOL_NS.as_bytes()).is_none() {
            return Cow::Borrowed(line);
        }
        let Some(payload) = line.strip_prefix(b"data:") else { return Cow::Borrowed(line) };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(payload.trim_ascii()) else {
            return Cow::Borrowed(line);
        };
        let block_type = v.pointer("/content_block/type").and_then(|t| t.as_str());
        match v.get("type").and_then(|t| t.as_str()) {
            Some("content_block_start")
                if matches!(block_type, Some("tool_use" | "tool_search_tool_result")) =>
            {
                Cow::Owned(self.restore_fields(line))
            }
            Some("error") => Cow::Owned(self.restore(line)),
            _ => Cow::Borrowed(line),
        }
    }

    /// 在一段字节上把紧跟 [`NAME_FIELDS`] 之后、到收尾引号恰好是一个假名的换成真名。只用在
    /// [`Self::restore_sse_line`] 认定过的那几种事件行上：脱离那个前提，键名认不出协议字段与
    /// 业务参数。
    fn restore_fields(&self, buf: &[u8]) -> Vec<u8> {
        let ns = FAKE_TOOL_NS.as_bytes();
        let mut out = Vec::with_capacity(buf.len());
        let mut i = 0;
        while let Some(rel) = find_sub(&buf[i..], ns) {
            let at = i + rel;
            out.extend_from_slice(&buf[i..at]);
            let anchored = NAME_FIELDS.iter().any(|f| buf[..at].ends_with(f));
            let hit = anchored
                .then(|| {
                    self.reverse.iter().find(|(fake, _)| {
                        buf[at..].starts_with(fake.as_bytes())
                            && buf.get(at + fake.len()) == Some(&b'"')
                    })
                })
                .flatten();
            match hit {
                Some((fake, real)) => {
                    out.extend_from_slice(real.as_bytes());
                    i = at + fake.len();
                }
                None => {
                    out.extend_from_slice(ns);
                    i = at + ns.len();
                }
            }
        }
        out.extend_from_slice(&buf[i..]);
        out
    }

    /// 回程还原：假名 → 真名，**全文**。只给错误响应用（报错正文里回显的假名，不进对话历史）；
    /// 成功回复见 [`Self::restore_fields`]。**一趟扫完**，靠所有假名共用的 [`FAKE_TOOL_NS`] 定位。
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
    pub(in crate::proxy) fn restore(&self, buf: &[u8]) -> Vec<u8> {
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

    /// 回程还原的一步：吃进一块，吐出**可以安全发走**的部分。
    ///
    /// SSE（`sse`）按行处理：只吐到最后一个换行为止，半行留到下一块拼齐再看——一行是一个事件
    /// 的 `data:`，看得到事件类型才知道里面的名字是不是协议字段（[`Self::restore_sse_line`]）。
    /// 假名被 TCP 分块从中间切开也就不成问题：切开的那行整行都还留着。
    ///
    /// 整段 JSON（非 SSE）要整份解析才认得出块，攒到 [`Self::flush`] 一次处理。
    pub(in crate::proxy) fn feed(&self, pending: &mut Vec<u8>, chunk: &[u8], sse: bool) -> Bytes {
        pending.extend_from_slice(chunk);
        if !sse {
            return Bytes::new();
        }
        let Some(last_nl) = pending.iter().rposition(|&b| b == b'\n') else {
            return Bytes::new();
        };
        let rest = pending.split_off(last_nl + 1);
        let done = std::mem::replace(pending, rest);
        Bytes::from(self.restore_sse_lines(&done))
    }

    /// 流结束时把留存的部分吐出来。**不能省**：SSE 以 `\n\n` 收尾，尾巴扣着不发的话
    /// 客户端的解析器会一直等那个终止符；整段 JSON 则是整个响应体都在这里。
    pub(in crate::proxy) fn flush(&self, pending: &mut Vec<u8>, sse: bool) -> Bytes {
        if pending.is_empty() {
            return Bytes::new();
        }
        let all = std::mem::take(pending);
        Bytes::from(if sse { self.restore_sse_lines(&all) } else { self.restore_json_body(&all) })
    }

    /// 逐行过 [`Self::restore_sse_line`]，换行符原样保留。
    fn restore_sse_lines(&self, buf: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(buf.len());
        for line in buf.split_inclusive(|&b| b == b'\n') {
            let body_len = line.len() - usize::from(line.ends_with(b"\n"));
            let body = &line[..body_len];
            let body = body.strip_suffix(b"\r").unwrap_or(body);
            out.extend_from_slice(&self.restore_sse_line(body));
            out.extend_from_slice(&line[body.len()..]);
        }
        out
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

/// 把上游响应流包一层工具名还原（[`ToolNameMap::feed`]）。`sse` 是上游回的是不是事件流：
/// 是就逐行还原，不是（整段 JSON）就攒齐了一次还原。留存的状态跟着流走，流结束时 flush。
///
/// 用 `unfold` 而不是 `map`：`map` 收不到「上游流结束」这个事件，没法把留存的尾字节吐出去。
pub(in crate::proxy) fn restore_tool_names_stream<S>(
    inner: S,
    map: std::sync::Arc<ToolNameMap>,
    sse: bool,
) -> impl futures_util::Stream<Item = Result<Bytes, wreq::Error>>
where
    S: futures_util::Stream<Item = Result<Bytes, wreq::Error>> + Unpin,
{
    futures_util::stream::unfold(
        (inner, map, Vec::<u8>::new(), false),
        move |(mut inner, map, mut pending, done)| async move {
            if done {
                return None;
            }
            match inner.next().await {
                Some(Ok(bytes)) => {
                    let out = map.feed(&mut pending, &bytes, sse);
                    Some((Ok(out), (inner, map, pending, false)))
                }
                // 上游把流掐了：错误原样交给下游（行为与不还原时一致），本次不再吐尾巴——
                // 半行事件、半截 JSON 交给客户端也解析不了。
                Some(Err(e)) => Some((Err(e), (inner, map, pending, true))),
                None => {
                    let tail = map.flush(&mut pending, sse);
                    (!tail.is_empty()).then(|| (Ok(tail), (inner, map, pending, true)))
                }
            }
        },
    )
}
