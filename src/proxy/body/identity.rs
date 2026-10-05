//! 设备 / 会话身份：模拟身份的派生、会话绑定键、改写 `metadata.user_id`，以及出站身份。

use super::*;

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
pub(in crate::proxy) fn sim_device_id(
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
pub(in crate::proxy) fn device_fingerprint(
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
pub(in crate::proxy) fn sim_device_fingerprint(client_device_id: Option<&str>) -> String {
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
pub(in crate::proxy) fn sim_session_key(v: &serde_json::Value) -> String {
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
pub(in crate::proxy) fn session_binding_key(
    incoming_session_id: Option<&str>,
    prefix_key: &str,
) -> String {
    match incoming_session_id {
        Some(sid) => format!("{SESSION_KEY_VERSION}sid:{sid}"),
        None => format!("{SESSION_KEY_VERSION}pfx:{prefix_key}"),
    }
}

/// 匿名侧查询在流水上记的键：`lb:v2:anon:<来源>:<值>:<类别>`，由它本来的会话键
/// （[`session_binding_key`]）插上 `anon` 段、末尾接请求类别（[`super::CcRequestKind::tag`]）。
///
/// **只进流水，从不落 `session_bindings`**：这类请求没有设备身份，主线程落在哪个号上只能凭
/// 会话键碰运气，碰到了就跟过去、碰不到就分散到别的号，都不写绑定、不占会话名额（见
/// [`super::session_id::session_plan`]）。类别段让后台把标题、分类、探测这些用量与真正的对话
/// 分开看；保留来源与值，按会话 id 查流水仍查得到它。
pub(in crate::proxy) fn anon_session_key(binding_key: &str, class: &str) -> String {
    let rest = binding_key.strip_prefix(SESSION_KEY_VERSION).unwrap_or(binding_key);
    format!("{SESSION_KEY_VERSION}anon:{rest}:{class}")
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
pub(in crate::proxy) fn sync_metadata_session(v: &mut serde_json::Value, session_id: &str) -> bool {
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

pub(in crate::proxy) fn spoof_identity(
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
pub(in crate::proxy) fn replace_json_str_field(
    s: &str,
    key: &str,
    new_val: &str,
) -> Option<String> {
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
pub(in crate::proxy) struct OutboundIdentity {
    pub(in crate::proxy) device_id: String,
    pub(in crate::proxy) account_uuid: String,
    /// 出站体里那串 `user_id` 的**原文**（连编码形态一起）。
    ///
    /// 额度探测直接复用它，而不是拿上面两个字段重新拼一份 JSON：客户端可能用的是
    /// Windows 那种扁平串（`user_<device>_account_<account>_session_<session>`，
    /// 见 [`parse_flat_user_id`]），[`spoof_identity`] 改写完仍是扁平串。重新拼成 JSON
    /// 就会出现「同一个会话的两条请求，一条扁平一条 JSON」这种官方不产生的组合。
    ///
    /// 出站体压根没有 `metadata.user_id` 时为 `None`。
    pub(in crate::proxy) raw_user_id: Option<String>,
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
pub(in crate::proxy) fn outbound_identity(
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
pub(in crate::proxy) fn with_outbound_identity(body: Bytes, ident: &OutboundIdentity) -> Bytes {
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
