//! billing header 里的 `cch`：按官方算法计算、在出站字节上就地回填。

use super::*;

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
pub(in crate::proxy) fn cch_value() -> String {
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
pub(in crate::proxy) const CCH_SEED: u64 = 0x4d65_9218_e32a_3268;

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
pub(in crate::proxy) fn apply_cch(bytes: &mut [u8]) -> Option<String> {
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
pub(super) fn real_cch_stale(
    body: &[u8],
    sim: Option<&Simulation>,
    flags: store::ForwardFlags,
) -> bool {
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
pub(super) fn finalize_cch(bytes: &mut [u8], compute: bool, ours: bool) -> Option<String> {
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
pub(super) fn sync_value_cch(v: &mut serde_json::Value, cch: &str) {
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
pub(in crate::proxy) fn compute_cch(body: &[u8]) -> String {
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
