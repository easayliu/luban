//! 请求体的只读判定：计费路径、设备 / 会话 id、stream、字节级粗筛。

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
pub(in crate::proxy) fn is_billable_messages(path: &str) -> bool {
    path.starts_with("/v1/messages") && path != "/v1/messages/count_tokens"
}

/// 从请求体提取「客户端设备标识」，用于粘性选择与设备指纹派生。
/// 兼容两种 `metadata.user_id` 格式：
/// - CC 内嵌 JSON（`{"device_id":...}`）：取 `device_id`。
/// - 扁平串 `user_<hash>_account_<acct>_session_<sess>`（如 Windows 客户端）：取 `<hash>`。
///
/// 解析失败或标识为空时返回 `None`（退化为纯优先级选择、不做粘性绑定）。
pub(in crate::proxy) fn extract_device_id(body: Option<&serde_json::Value>) -> Option<String> {
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
pub(in crate::proxy) fn extract_session_id(body: Option<&serde_json::Value>) -> Option<String> {
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
/// 这里只问「这个字段在不在」——决定的是要不要给它补一份官方身份（见 [`ensure_cc_metadata`](super::ensure_cc_metadata)），
/// 而字段已经在的话，改写它是 [`spoof_identity`](super::spoof_identity) 的活，两条路只能有一条动它。
pub(in crate::proxy) fn body_has_user_id(body: Option<&serde_json::Value>) -> bool {
    body.and_then(|v| Some(v.get("metadata")?.get("user_id")?.is_string())).unwrap_or(false)
}

/// 扁平 `metadata.user_id` 的三段：`user_<device>_account_<account>_session_<session>`。
///
/// [`spoof_identity`](super::spoof_identity) 只用 device 与 session（account 段由凭证真实值覆盖），
/// [`outbound_identity`](super::outbound_identity) 三段都要——它读的是**已经发出去**的那份，不能再替换任何一段。
pub(in crate::proxy) struct FlatUserId {
    pub(super) device: String,
    pub(super) account: String,
    pub(super) session: String,
}

/// 解析扁平 user_id；不匹配该形态时返回 `None`。
/// 按标记切分，允许 account 段为空（`account__session`）。
pub(in crate::proxy) fn parse_flat_user_id(s: &str) -> Option<FlatUserId> {
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
pub(in crate::proxy) fn stream_requested(body: &serde_json::Value) -> bool {
    body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false)
}

/// 把顶层 `stream` 置为 `true`；已经是 `true` 就返回 `false`（无改动）。
///
/// 位置由 `preserve_order` 保证：字段已在则原位改值，不在则追加到末尾——而官方线序里
/// `stream` 本来就是最后一个（见 [`insert_top_level`](super::insert_top_level) 的说明），两条路都落在官方位置上。
pub(in crate::proxy) fn set_stream_true(v: &mut serde_json::Value) -> bool {
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
/// 只在[`Simulation`](super::Simulation)那条路上补——那条路已经把头和体整套装成了 CC，URL 上再漏掉这个参数，
/// 就是「头上声明了一整串官方 beta、URL 却没开 beta 模式」这种真实客户端不产生的组合。
///
/// 客户端自己写了 `beta=`（含 `beta=false`）时不动：那是它自己的选择，替它改属于越权。
pub(in crate::proxy) fn ensure_beta_query(url: &str) -> String {
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
pub(in crate::proxy) fn body_contains(body: &[u8], needle: &[u8]) -> bool {
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
pub(in crate::proxy) fn body_has_pair(body: &[u8], key: &[u8], value: &[u8]) -> bool {
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
