//! 入口闸门（1～2.5）：认人、限流、解析请求体、拦本地就能判的形态，最后定下会话方案。
//!
//! 每道闸门拒绝时就地回一条响应（[`admit`] 返回 `Err`），全部放行则给出 [`Inbound`]——
//! 这条请求在换号重试之间不变的全部事实——与要活到响应流结束的 [`Guards`]。

// 闸门 / 选号 / 组装失败时的 `Err` 就是要回给客户端的那条响应：只在拒绝时出现，装箱不划算。
#![allow(clippy::result_large_err)]
use super::*;

/// 闸门放行后，这条请求在换号重试之间不变的事实。字段与拆分前 `handle_inner` 里的同名局部
/// 变量一一对应，说明见各自算出它的那段。
pub(super) struct Inbound {
    pub(super) started: std::time::Instant,
    pub(super) method: Method,
    pub(super) headers: HeaderMap,
    pub(super) path_and_query: String,
    pub(super) client_request_id: Option<String>,
    pub(super) client_ua: String,
    /// 客户端原始请求体（prefill / 已废弃字段按策略剥过的那份）。换号与降级重试都从它重新改写。
    pub(super) body: Bytes,
    pub(super) body_json: Option<serde_json::Value>,
    pub(super) device_id: Option<String>,
    pub(super) has_user_id: bool,
    pub(super) cc_shaped: bool,
    pub(super) from_cc_client: bool,
    pub(super) req_model: Option<String>,
    pub(super) session_id: Option<String>,
    pub(super) billable: bool,
    pub(super) req_max_tokens: Option<i64>,
    pub(super) flags: store::ForwardFlags,
    pub(super) device_fp: String,
    pub(super) inbound_session: Option<String>,
    pub(super) prefix_key: Option<String>,
    pub(super) cc_kind: CcRequestKind,
    pub(super) plan: SessionPlan,
}

impl Inbound {
    /// 既没有设备身份、也没有会话 id 的来访：按应用学拒答、按应用记请求都只认这一类。
    pub(super) fn session_less(&self) -> bool {
        self.device_id.is_none() && self.session_id.is_none()
    }
}

/// 要活到响应流结束的两个守卫：建 [`ReqLog`] 时交给它拿着。
pub(super) struct Guards {
    pub(super) in_flight: InFlightGuard,
    pub(super) session_concurrency: SessionConcurrencyGuard,
}

/// 从请求体与 UA 读出来的那几项，几道闸门共用。
struct Facts {
    body_json: Option<serde_json::Value>,
    device_id: Option<String>,
    has_user_id: bool,
    cc_shaped: bool,
    from_cc_client: bool,
    req_model: Option<String>,
    session_id: Option<String>,
    billable: bool,
}

/// 依次过完全部闸门。任何一道拒绝都就地返回那条响应。
pub(super) fn admit(
    state: &AppState,
    method: Method,
    uri: &Uri,
    headers: HeaderMap,
    body: Bytes,
    log_state: &RequestLogState,
) -> Result<(Inbound, Guards), Response> {
    let started = std::time::Instant::now();
    let client_request_id = client_request_id(&headers);
    let path_and_query =
        uri.path_and_query().map(|pq| pq.as_str()).unwrap_or(uri.path()).to_string();
    // 来访 UA：转发日志与各条拒绝日志都带上（都是 info/warn，不必开 debug）。整组识别头那条
    // debug 留着不动——排查形态时才需要那六项，日常只要认出「谁在发」，一项就够。
    let client_ua = ua_of(&headers);
    // 在途计数：入口就 +1，随后 move 进 ReqLog 活到响应流结束，见 [`InFlightGuard`]。
    let in_flight = InFlightGuard::new(state.in_flight.clone());

    client_gates(state, &method, &path_and_query, &client_ua, &headers)?;
    let (session_from_header, concurrency_limit, mut session_concurrency_guard) =
        header_session_gates(state, &method, &path_and_query, &client_ua, &headers, log_state)?;
    let facts = parse_facts(uri, &body, &client_ua, &session_from_header, log_state);
    identity_gates(
        state,
        &method,
        &path_and_query,
        &client_ua,
        &headers,
        &facts,
        &session_from_header,
        concurrency_limit,
        &mut session_concurrency_guard,
        log_state,
    )?;

    // 这条请求声明的输出上限。只为日志：裸 429 那一档要拿它对上游那套「每分钟输出 token」
    // 限额，见 [`UpstreamLoad`]。算在这里是因为 `body_json` 只解析一次（见上面 2 那段），
    // 而这个值逐轮不变。
    let req_max_tokens = request_max_tokens(facts.body_json.as_ref());
    let body = prefill_gate(state, &client_ua, &facts, body)?;
    shape_gates(state, &method, &path_and_query, &client_ua, &headers, &facts, log_state)?;
    let body = sampling_gate(state, &client_ua, &facts, body)?;
    device_rpm_gate(state, &method, &path_and_query, &client_ua, &facts, log_state)?;

    let Facts {
        body_json,
        device_id,
        has_user_id,
        cc_shaped,
        from_cc_client,
        req_model,
        session_id,
        billable,
    } = facts;
    // 2.5) 转发形态开关一条 SQL 读齐（默认全开 = 加入开关前的既有行为），以及这条请求走不走
    //      模拟——两者在选号之前就要定下来：模拟路径的会话键参与选号（下面的 `session_key`），
    //      设备指纹又按走不走模拟取两套。
    let flags = state.store.forward_flags();
    // 走不走模拟的判据与 [`Simulation::detect`] 同源（都走 [`simulates_cc`]），不然指纹会把
    // 一条请求算到另一台设备名下。
    let simulating = simulates_cc(body_json.as_ref(), &headers, from_cc_client, flags);
    // 设备指纹用于派生伪装 device_id。归一化开着时只取平台，关着时叠加客户端原始 device_id。
    // **模拟路径取实际发出去的那套头**（[`sim_device_fingerprint`]）：来访的平台头与 UA 一个
    // 都不会发出去，同一账号经模拟路径的全部请求是同一台设备；非模拟路径按来访的平台头与
    // UA 算（[`device_fingerprint`]，一台设备只能有一个客户端版本）。
    let fp_device = if flags.normalize_device_fp { None } else { device_id.as_deref() };
    let device_fp = if simulating {
        sim_device_fingerprint(fp_device)
    } else {
        device_fingerprint(fp_device, &headers, &client_ua)
    };
    // 来访自报的会话 id（校验过形态，见 [`incoming_session_id`]）：会话键与流水都要用，算一次。
    // 走模拟时上游看到的是另一个 uuid（按槽位派生或按账号钉住），来访这个不落库就彻底丢了
    // ——下游拿着自己那个 id 来查请求会一条都查不到，见 [`store::Forensics::session_id_in`]。
    let inbound_session = incoming_session_id(&headers, body_json.as_ref());
    // 缓存前缀加对话起点的指纹（[`sim_session_key`]，tools + system + 首条用户消息）：来访没带
    // 会话 id 时模拟路径用它派生出站的会话 id（[`Simulation::detect`]），按会话占名额而没带会话
    // id 的真实客户端也用它来分会话，见 [`needs_prefix_key`]。
    let prefix_key =
        needs_prefix_key(flags, simulating, device_id.is_some(), inbound_session.as_deref())
            .then(|| body_json.as_ref().map(sim_session_key))
            .flatten();
    // 参与选号的会话键（模拟路径，以及按会话占名额的带设备来访，见 [`session_plan`]）：
    // 来访自带合法会话 id 就用它——那正是出站会落的那条会话（按账号钉住之前的原值，键要跨账号
    // 稳定）；没带就用那个指纹。同一个键粘住同一个号（换号会连累 thinking 签名，理由同设备绑定）
    // 并占该号的一个**会话名额**：每个号最多同时活跃多少条模拟会话，与设备上限同一套三态与
    // TTL。一次性会话打一条就走的那类流量（封号复盘里最显眼的形态）在这里被封顶。
    // 键带命名空间与口径版本（`lb:v2:sid:<uuid>` / `lb:v2:pfx:<hex>`，见 [`session_binding_key`]）：
    // 来源那一段是明文，后台不必再靠「是不是 32 个 hex」去猜，改口径时旧行也认得出来。
    // **只有落库的这个键带前缀**，派生出站会话 id 的 seed 仍是裸的 `prefix_key`（下面的
    // `SimSessionSeed::Prefix`）——那是哈希的输入，动它会让在途对话的会话 id 全部换一遍。
    // 这条请求的会话键，以及选号时按设备还是按会话占名额，见 [`session_plan`]。
    let cc_kind = body_json
        .as_ref()
        .map(|v| CcRequestKind::of(v, &inbound_beta_list(&headers)))
        .unwrap_or(CcRequestKind::Main);
    // 一次性侧查询（标题、分类、探测……）：匿名来访的这几类只跟随、不占名额，见 [`session_plan`]。
    // **只有 `/v1/messages` 占会话**：`count_tokens` 这类不计费路径按路径认，不看体长什么样——
    // 体形态只认得出官方那一种（[`CcRequestKind::CountTokens`]），第三方的会被当成主线程去占名额。
    let side_class = if billable {
        body_json.as_ref().filter(|v| cc_kind.is_side_query(v)).map(|_| cc_kind.tag())
    } else {
        Some(CcRequestKind::CountTokens.tag())
    };
    let plan = session_plan(
        flags,
        simulating,
        device_id.is_some(),
        inbound_session.as_deref(),
        prefix_key.as_deref(),
        cc_kind == CcRequestKind::QuotaProbe || !billable,
        side_class,
    );
    // 流水上记的键：匿名侧查询带类别（`lb:v2:anon:…`），其余就是会话键。
    let log_session_key = plan.log_key.clone();
    // 本地拒绝的流水也要带这两样（见 [`RequestLogState::session_key`]）：下面这些闸拦下的
    // 请求走不到 `ReqLog`，外层补流水时从这里取。
    *log_state.session_key.lock() = log_session_key.clone();
    *log_state.session_id_in.lock() = inbound_session.clone();

    Ok((
        Inbound {
            started,
            method,
            headers,
            path_and_query,
            client_request_id,
            client_ua,
            body,
            body_json,
            device_id,
            has_user_id,
            cc_shaped,
            from_cc_client,
            req_model,
            session_id,
            billable,
            req_max_tokens,
            flags,
            device_fp,
            inbound_session,
            prefix_key,
            cc_kind,
            plan,
        },
        Guards { in_flight, session_concurrency: session_concurrency_guard },
    ))
}

/// 1～1.5：来访 API key 与最低客户端版本，只看头。
fn client_gates(
    state: &AppState,
    method: &Method,
    path_and_query: &str,
    client_ua: &str,
    headers: &HeaderMap,
) -> Result<(), Response> {
    // 1) 校验来访 API Key（未配置则放行）。生效 key：环境覆盖优先，否则用库中配置。
    if let Some(expected) = effective_client_key(state)
        && !client_authorized(headers, &expected)
    {
        tracing::warn!(%method, path = %path_and_query, ua = %client_ua, "rejected: invalid inbound API key");
        return Err(error_response(
            StatusCode::UNAUTHORIZED,
            "authentication_error",
            "invalid API key",
        ));
    }

    // 1.5) 最低客户端版本闸：只卡 UA 自报 `claude-cli/<版本>` 的请求，其余一律放行，
    //      判定见 [`below_min_client_version`]。放在这里是因为它只看一个头——比解析 body、
    //      挑账号都便宜，该拒的越早拒越好；也因此它在 API key 之后：先认人，再谈版本。
    if let Some((got, want)) =
        below_min_client_version(client_ua, state.store.min_client_version().as_deref())
    {
        tracing::warn!(%method, path = %path_and_query, ua = %client_ua, %got, %want, "rejected: client version below the configured minimum");
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "permission_error",
            format!(
                "Claude Code {got} is no longer accepted here; upgrade to {want} or newer \
                 (npm i -g @anthropic-ai/claude-code)"
            ),
        ));
    }
    Ok(())
}

/// 1.6～1.7：头上带会话 id 的，先按会话限 RPM 与并发。返回头上的会话 id、并发上限与
/// 占住的并发格（头上没有会话 id 时是个空守卫，等 2.2c 按体里的会话 id 补判）。
fn header_session_gates(
    state: &AppState,
    method: &Method,
    path_and_query: &str,
    client_ua: &str,
    headers: &HeaderMap,
    log_state: &RequestLogState,
) -> Result<(Option<String>, i64, SessionConcurrencyGuard), Response> {
    // 1.6) 每会话 RPM 上限（头这一路）：这个会话最近 60 秒发得太多 → 直接 429 + `retry-after`。
    //
    //      **刻意排在 body 解析之前**，这是选会话维度顺带拿到的好处：会话 id 在
    //      `X-Claude-Code-Session-Id` 头上，而设备 id 只存在于 body 里（`metadata.user_id`），
    //      按设备限就非得先把整个 body 解析出来才判得了。长对话几 MB 是常态，一个不退避的
    //      客户端每秒撞十几次，那十几次全额解析纯属白烧 CPU——闸门前移正好把它省掉。
    //
    //      代价是形态拦截（2.3）与设备身份校验（2.2）都排在它后面，即「一条发都发不出去的
    //      请求也会占掉会话的名额」，与设备闸那句注释的取舍相反。这里认这个代价：反复发同一条
    //      坏形态本身就是该被节流的行为（那条路每次也要白解析一遍 body），把它算进窗口比放它
    //      过去更对。
    //
    //      头上没有这个值时不在这里判，等 body 解析出会话 id 再补判（见 2.2b）；两处互斥，
    //      同一条请求只会吃一个名额。官方客户端头体两处逐字相同，故先后两路落在同一个桶里。
    // 体还没解析，只看头；体里那个由 2.2b 补判。
    let session_from_header = incoming_session_id(headers, None);
    if let Some(sid) = session_from_header.as_deref()
        && let Some(retry) = state.store.take_session_rpm_slot(sid)
    {
        *log_state.local_reject.lock() = Some("session-rpm");
        return Err(session_rpm_rejection(
            &state.rejection_log,
            method,
            path_and_query,
            client_ua,
            sid,
            retry,
            "header",
        ));
    }

    // 1.7) 每会话并发在途上限（头这一路）：这个会话同时在飞的请求已达上限 → 直接 429。
    //      与 RPM 同理：头上有就在这里判，没有等 body 里拿到 session id 再补判。
    let concurrency_limit = state.store.session_concurrency_limit();
    let session_concurrency_guard = if let Some(sid) = session_from_header.as_deref() {
        match try_acquire_session_concurrency(&state.session_concurrency, sid, concurrency_limit) {
            Ok(guard) => guard,
            Err(current) => {
                *log_state.local_reject.lock() = Some("session-concurrency");
                return Err(session_concurrency_rejection(
                    &state.rejection_log,
                    method,
                    path_and_query,
                    client_ua,
                    sid,
                    current,
                    concurrency_limit,
                    "header",
                ));
            }
        }
    } else {
        SessionConcurrencyGuard::dummy(state.session_concurrency.clone())
    };
    Ok((session_from_header, concurrency_limit, session_concurrency_guard))
}

/// 2～2.1：请求体只解析这一次，读出后面几道闸门共用的那几项。
fn parse_facts(
    uri: &Uri,
    body: &Bytes,
    client_ua: &str,
    session_from_header: &Option<String>,
    log_state: &RequestLogState,
) -> Facts {
    // 2) 请求体只解析这一次，下面五项判定全从这份结果上读。
    //
    //    此前 extract_device_id / body_has_user_id / request_model / request_speed 各自
    //    `from_slice` 一遍整个 body，`Simulation::detect` 再来一遍，加上 `rewrite_body`
    //    自己那次，一条请求要把同一份 JSON 完整解析 6 次以上（429 换号重试时后两项还按轮次
    //    翻倍）。body 上限刚放到 64MB，长对话几 MB 是常态，这是白烧的 CPU。
    //
    //    `rewrite_body` 仍自己解析：它要一份**可变且每轮独立**的副本（每次重试都从客户端
    //    原始体重新改写），共用这份只读的反而要多克隆一次。
    //
    //    解析失败（不是 JSON）时为 `None`，各项判定按「读不出来」退化，与逐个解析时一致。
    let body_json: Option<serde_json::Value> = serde_json::from_slice(body).ok();

    // 提取 device_id（在 metadata.user_id 里；兼容 CC 内嵌 JSON 与扁平串两种格式）。
    let device_id = extract_device_id(body_json.as_ref());
    // 给本地拒绝的流水留下这三样，外层就不必再解析体。见 [`RequestLogState::parsed`]。
    *log_state.parsed.lock() = Some(ParsedRequestBits {
        model: request_model(body_json.as_ref()),
        device_id: device_id.clone(),
        session_id: extract_session_id(body_json.as_ref()),
    });
    // 该字段在不在（与「能否解析出设备标识」是两回事）：决定要不要给它补一份官方身份。
    // body 逐轮不变，算一次即可。见 [`Upstream::bare_session`]。
    let has_user_id = body_has_user_id(body_json.as_ref());
    // 来访是不是本来就是 CC 形态（判据是 `system` 里那句话，见 [`is_cc_shaped`]）。
    // 这里只为日志算它：走不走模拟由 [`Simulation::detect`] 自己判，但它返回 `None` 时
    // 分不出是「本来就是 CC」还是「开关关着」，而这正是排查时要知道的那一位。
    let cc_shaped = body_json.as_ref().is_some_and(is_cc_shaped);
    // 来访是不是 Claude Code 客户端——这一位**只看 UA**：`claude-cli/<版本>` 在才算。
    // 它是 [`Simulation::detect`] 跳过模拟的必要条件之一，不是充分条件：还得体也是 CC 形态
    // （上面的 `cc_shaped`）且工具列表像 CC。带着正确 UA 与形态来的就是官方客户端，不动它：
    // 模拟那条路会把这串 UA 连同 `x-app`/`x-stainless-*` 一起换成 [`config::CC_SIM_HEADERS`]
    // 里的定值。
    //
    // `metadata.user_id` 和 `X-Claude-Code-Session-Id` **不**单独构成跳过模拟的理由：
    // 非 CC 的 UA（`Go-http-client`、`python-httpx`……）带着这些字段，只说明它抄了请求体
    // 或头，UA 不对齐照样是一条自相矛盾的请求，需要模拟接管。模拟路径下
    // [`rewrite_body`] 会先剥掉客户端已有的 `metadata.user_id`，再由
    // [`ensure_cc_metadata`] 用 `sim.session_id` 重建，确保头体自洽。
    //
    // **UA 可以伪造**，所以只认 UA 不够：照抄 `claude-cli/...` 却没抄 system 形态的第三方
    // 中转（封号复盘里的探活脚本就是），透传出去是一条头体矛盾的请求，比模拟更容易被上游
    // 标记——这类请求由 [`Simulation::detect`] 按形态识出来、一并走模拟接管。
    //
    // **自报的版本还得说得通**：不高于官方已发布的最新版（[`known_latest_release`]，从
    // `downloads.claude.ai/claude-code-releases/latest` 学来，下限是抓包证实过的
    // [`config::CC_LATEST_KNOWN_RELEASE`]）。一个自称 `claude-cli/2.5.0` 的客户端在官方只发到
    // 2.1.270 的时候不是官方客户端——按非 CC 客户端处理（走模拟），
    // 也不再沿用它那个不存在的版本号去补 billing header、跑启动握手、发额度探测。
    let from_cc_client = trusted_cc_version(client_ua).is_some();
    if !from_cc_client && let Some((a, b, c)) = cc_cli_version(client_ua) {
        let (la, lb, lc) = known_latest_release();
        tracing::warn!(
            ua = %client_ua,
            claimed = %format!("{a}.{b}.{c}"),
            latest = %format!("{la}.{lb}.{lc}"),
            "client claims a Claude Code version newer than the latest official release; not treating it as an official client"
        );
    }

    // 请求的模型名：好几处都要用它——本地形态拦截按模型索引，选号的冷却也按
    // 「账号 + 模型」分格（fable 那类模型级 429 不该拖累整个账号），本地作答那条回的也是它。
    let req_model = request_model(body_json.as_ref());
    // 这条请求的会话 id（头优先、body 兜底）：选号失败与本地拒绝那几条日志的抑制键，
    // 在没有设备身份时要拿它来分桶；每会话限流在 2.2b / 2.2c 用的也是它。
    let session_id = match &session_from_header {
        Some(sid) => Some(sid.clone()),
        None => extract_session_id(body_json.as_ref()),
    };

    // 2.1) 这条路径是否消耗订阅额度——决定要不要卡设备身份、要不要改写出站体。
    //      判定吃 `uri.path()` 而非上面那个带查询串的 `path_and_query`：豁免要精确匹配。
    let billable = is_billable_messages(uri.path());
    Facts {
        body_json,
        device_id,
        has_user_id,
        cc_shaped,
        from_cc_client,
        req_model,
        session_id,
        billable,
    }
}

#[allow(clippy::too_many_arguments)]
/// 2.1a～2.2c：探针就地作答、没有设备身份的拒掉，再按体里的会话 id 补判会话 RPM 与并发。
fn identity_gates(
    state: &AppState,
    method: &Method,
    path_and_query: &str,
    client_ua: &str,
    headers: &HeaderMap,
    facts: &Facts,
    session_from_header: &Option<String>,
    concurrency_limit: i64,
    session_concurrency_guard: &mut SessionConcurrencyGuard,
    log_state: &RequestLogState,
) -> Result<(), Response> {
    let Facts {
        ref body_json,
        ref device_id,
        ref req_model,
        ref session_id,
        from_cc_client,
        billable,
        ..
    } = *facts;
    // 2.1a) 探针类请求 → **本地回一条最小的正常回复**（200，见 [`probe_reply`]），不到上游，
    //       见 [`probe_signature`]。身份写错的不在这里拒，由 [`Simulation::detect`] 送进模拟
    //       重建身份。
    //       **位置有意排在下面 2.2 的设备身份闸之前**：探活多半根本不带 `metadata.user_id`
    //       （封号复盘里 Go-http-client 那批就是），而 `require_device_id` 默认开着、会先回一条
    //       403——那正是下游把整个 key 摘下去的信号，这一版要修的就是它。排在这里也意味着探针
    //       不占每会话 RPM 与并发名额（2.2b / 2.2c）：它根本不出站。
    //       0.3.101 之前回的是 403 permission_error：探活恰恰是下游中转用来判断「这个号还能
    //       不能用」的那条请求，luban 的 403 在它那侧与「号被封了」长得一样，整个 key 被摘下
    //       去、真流量跟着停——而这条请求根本没到上游、账号一点事没有。回 200 后探活看到的是
    //       「健康」，上游那边一条请求都没多。是 luban 就地答的、没到上游这件事标在三处：
    //       响应头 `x-luban-local: probe_reply` 与 `x-luban-probe-kind: <判据>`、Message id 的
    //       `msg_luban` 前缀、以及流水里的 `probe_reply` 标签（花费 0）。
    //       下游中转的探活脚本发一条无 tools 的单句小请求，每条在上游侧都是「一台设备开一个
    //       一次性会话只问一句话」——封号复盘里最显眼的判据。判据是形态与身份上的强特征，
    //       一条就够判，不做计数。**不限 UA**（0.3.99 起）：此前只判自报 claude-cli 的，
    //       Go-http-client 的探活反而走模拟、被装成官方形态发了出去。UA 只在一处反向参与：
    //       1 token 探活（[`ProbeKind::OneTokenPing`]）对不可信 UA 一律算、对可信 UA 只在没带
    //       身份时算——官方的预热与额度探测都来自可信 UA。只判计费路径；`reject_probes` 关掉
    //       即放行。
    if billable
        && state.store.forward_flags().reject_probes
        && let Some(kind) = probe_signature(
            body_json.as_ref(),
            device_id.as_deref(),
            &inbound_beta_list(headers),
            from_cc_client,
            state.store.forward_flags().reject_probes_strict,
            || device_id.as_deref().is_some_and(|d| state.store.device_is_known(d)),
        )
    {
        // 抑制键按「类别 + 设备」分桶：探活脚本多半几十秒一条，同一台设备反复撞这里；
        // 类别分开是因为同一台设备先撞 ping、再撞身份句重复，是两件事。
        let who = device_id.as_deref().or(session_id.as_deref()).unwrap_or("-");
        if let Some(suppressed) =
            take_rejection_log_slot(&state.rejection_log, &format!("probe:{}:{who}", kind.tag()))
        {
            let device_short: String = who.chars().take(8).collect();
            tracing::warn!(
                %method, path = %path_and_query, ua = %client_ua,
                model = %req_model.as_deref().unwrap_or("-"), device = %device_short,
                kind = kind.tag(), from_cc_client, suppressed, reason = kind.message(),
                "not forwarded: request matches a probe / health-check signature; answered locally with a minimal 200"
            );
        }
        *log_state.local_replay.lock() = Some(REWRITE_PROBE_REPLY);
        return Err(probe_reply(
            kind,
            req_model.as_deref(),
            body_json.as_ref().is_some_and(stream_requested),
        ));
    }

    // 2.2) 无有效设备身份（无 metadata / 无法识别的 user_id 格式）→ 计费路径默认直接拒绝：
    //      这类请求既无法做身份伪装、也无从计入设备上限（会绕过 device_limit）。
    //      网页可关掉该校验（放行裸客户端），此时它们退化为不绑定、不占名额的负载均衡挑选。
    //      不带身份的**探针**到不了这里：它在 2.1a 就被就地答掉了——这道闸回的 403 与「号被封了」
    //      在下游那侧长得一样，探活撞上它整个 key 就被摘下去。
    if device_id.is_none() {
        if billable && state.store.require_device_id() {
            tracing::warn!(%method, path = %path_and_query, ua = %client_ua, "rejected: request has no usable device identity (metadata.user_id missing or unrecognized)");
            *log_state.local_reject.lock() = Some("no-device-id");
            return Err(error_response(
                StatusCode::FORBIDDEN,
                "permission_error",
                "missing a usable device identity (metadata.user_id)",
            ));
        }
        tracing::debug!(%method, path = %path_and_query, billable, "allowing a request with no device identity");
    }

    // 2.2b) 每会话 RPM 上限（body 这一路）：头上没带会话 id，但 `metadata.user_id` 里有。
    //       只在头那路没判过时才判（`session_from_header.is_none()`），否则同一条请求会吃掉
    //       两个名额——官方两处同值，那等于把上限砍半。
    //       会话 id（头优先、body 兜底）在上面 2.1 就定下来了。
    if session_from_header.is_none()
        && let Some(sid) = session_id.as_deref()
        && let Some(retry) = state.store.take_session_rpm_slot(sid)
    {
        *log_state.local_reject.lock() = Some("session-rpm");
        return Err(session_rpm_rejection(
            &state.rejection_log,
            method,
            path_and_query,
            client_ua,
            sid,
            retry,
            "body",
        ));
    }

    // 2.2c) 每会话并发在途上限（body 这一路）：头那路没判过时补判。
    if session_from_header.is_none()
        && let Some(sid) = session_id.as_deref()
    {
        match try_acquire_session_concurrency(&state.session_concurrency, sid, concurrency_limit) {
            Ok(guard) => *session_concurrency_guard = guard,
            Err(current) => {
                *log_state.local_reject.lock() = Some("session-concurrency");
                return Err(session_concurrency_rejection(
                    &state.rejection_log,
                    method,
                    path_and_query,
                    client_ua,
                    sid,
                    current,
                    concurrency_limit,
                    "body",
                ));
            }
        }
    }
    Ok(())
}

/// 2.3a：末尾 assistant 轮（prefill）按 `prefill_policy` 剥掉、拒掉或放过。
fn prefill_gate(
    state: &AppState,
    client_ua: &str,
    facts: &Facts,
    body: Bytes,
) -> Result<Bytes, Response> {
    let Facts { ref body_json, ref req_model, .. } = *facts;
    // 2.3a) 4.6+ 全系列不支持 assistant message prefill（末尾 role=assistant 的轮次），
    //       上游会返回 400。策略由 `prefill_policy` 控制：
    //       - strip（默认）：主动剥掉末尾 assistant 轮后转发，省去白跑一趟。
    //       - reject：本地直接 400 拒绝，不往上游送。
    //       - off：不做任何处理，原样转发，上游的 400 也原样回给客户端。
    //       strip / reject 下后面那条被动重试（[`is_prefill_not_supported_error`] →
    //       [`retry_without_prefill`]）仍作兜底：万一新模型不在列表里、或者上游的拒绝消息换了措辞。
    let body = if req_model.as_deref().is_some_and(model_rejects_prefill)
        && has_trailing_assistant(body_json.as_ref())
    {
        match state.store.prefill_policy() {
            store::PrefillPolicy::Strip => match strip_assistant_prefill(&body) {
                Some(stripped) => {
                    tracing::info!(
                        model = %req_model.as_deref().unwrap_or("-"),
                        "proactively stripped trailing assistant prefill for a model that does not support it"
                    );
                    stripped
                }
                None => body,
            },
            store::PrefillPolicy::Reject => {
                tracing::info!(
                    model = %req_model.as_deref().unwrap_or("-"),
                    ua = %client_ua,
                    "rejected: assistant message prefill is not supported by this model (prefill_policy=reject)"
                );
                return Err(error_response(
                    StatusCode::BAD_REQUEST,
                    "invalid_request_error",
                    "This model does not support assistant message prefill. The conversation must end with a user message.",
                ));
            }
            store::PrefillPolicy::Off => body,
        }
    } else {
        body
    };
    Ok(body)
}

/// 2.3～2.3a5：本地就能判的形态错误与学到的拒答，命中即拒或回放，不往上游送。
fn shape_gates(
    state: &AppState,
    method: &Method,
    path_and_query: &str,
    client_ua: &str,
    headers: &HeaderMap,
    facts: &Facts,
    log_state: &RequestLogState,
) -> Result<(), Response> {
    let Facts {
        ref body_json,
        ref device_id,
        ref req_model,
        ref session_id,
        cc_shaped,
        billable,
        ..
    } = *facts;
    // 2.3) 上游已经拒过一次的「模型 + 请求里的某个取值」组合（`effort: 'xhigh'`、
    //      `role: 'system'` 之类）→ 本地直接拒，不往上游送。这是纯粹的请求形态错误：
    //      换哪个号发都是同一条 400，送上去只会白占一次请求配额，并在日志里留下一条与
    //      账号状态无关的 4xx。规则不是写死的，是上游那条 400 自己喂出来的，回给客户端的
    //      也是它当初那句原话，见 [`remember_shape_rejection`]。`reject_learned_shapes` 关掉时不拦。
    let flags = state.store.forward_flags();
    let system_hoisted = billable && hoists_system_role(&flags, cc_shaped);
    if flags.reject_learned_shapes
        && let Some((field, value, message)) = known_shape_rejection(
            &state.shape_rejections,
            req_model.as_deref(),
            body_json.as_ref(),
            system_hoisted,
        )
    {
        tracing::warn!(
            %method, path = %path_and_query, ua = %client_ua,
            model = %req_model.as_deref().unwrap_or("-"), %field, %value,
            "rejected locally: upstream has already rejected this request shape"
        );
        return Err(error_response(StatusCode::BAD_REQUEST, "invalid_request_error", &message));
    }

    // 2.3') 对话中途的 `role:"system"` 摆错位置（一段 system 之后紧跟 user）→ 本地直接拒，
    //       回上游那句原话，见 [`misplaced_system_role`]。规则是上游报错里写明的，不靠学：
    //       0.3.188 之前它被形态记忆学成「这个模型不收 system」，之后同模型带中途 system 的
    //       请求不论位置对错全被本地拒掉。出站会被整条提升的不判（上游看不到这个位置）；
    //       只判计费路径，`count_tokens` 等原样交给上游。
    if billable
        && !system_hoisted
        && let Some(message) = misplaced_system_role(body_json.as_ref())
    {
        tracing::warn!(
            %method, path = %path_and_query, ua = %client_ua,
            model = %req_model.as_deref().unwrap_or("-"), cc_shaped,
            "rejected locally: a mid-conversation system message is not followed by an assistant message"
        );
        return Err(error_response(StatusCode::BAD_REQUEST, "invalid_request_error", message));
    }

    // 2.3a) OpenAI 格式转换残留 → 本地直接拒，不修补，见 [`find_openai_marker`]。
    //       全部由 `reject_openai_shape` 拨（`image_url` 也是：上游对它恒 400，关着照样原样
    //       送上去）：开着一律拒，关着退回旧的修补路径
    //       （`hoist_system_role` 挪 system、[`normalize_tool_choice`] 翻译 tool_choice）。
    //       模拟路径不受影响：它只接管**本来就是 Anthropic 形态**的非 CC 请求。
    let reject_openai_shape = state.store.forward_flags().reject_openai_shape;
    if let Some(marker) = find_openai_marker(body_json.as_ref(), cc_shaped, reject_openai_shape) {
        // `cc_shaped` 与 `value` 是这条拒绝**唯一**能被复核的两项：判据只认形态，而同一条
        // `tool_call_id` 既可能是转换器回填的真残留，也可能是个自报 CC、历史却经过转换器的
        // 客户端。没有这两项，事后只能看见「拒了一条」，看不出拒得对不对。
        // 拒绝本身不受它们影响——判据与开关都没变，这里只是把证据留下。
        tracing::warn!(
            %method, path = %path_and_query, ua = %client_ua,
            location = %marker.location, kind = %marker.kind,
            cc_shaped,
            value = %marker.sample.as_deref().unwrap_or("-"),
            "rejected locally: request carries OpenAI-format residue, not accepted as an Anthropic Messages request"
        );
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            marker.message(),
        ));
    }

    // 2.3a2) 会话 id 头体不一致 → 本地直接拒，见 [`session_id_conflict`]。
    //        官方那两处逐字相同；两个都合法却不同的 uuid，luban 没有任何依据挑一个，而它
    //        正是会话链（`cc_prompt_id` / `cc_prev_req` / `diagnostics`）的键——挑错就是把
    //        两条链接到一起，事后再也看不出来。默认拒（`reject_session_conflict`）；关掉后
    //        退回「取头那个 + 打一条 warn」。
    if state.store.forward_flags().reject_session_conflict
        && let Some((header, body)) = session_id_conflict(headers, body_json.as_ref())
    {
        tracing::warn!(
            %method, path = %path_and_query, ua = %client_ua,
            header = %header, body = %body,
            "rejected locally: the session id differs between X-Claude-Code-Session-Id and metadata.user_id"
        );
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            format!(
                "session id mismatch: X-Claude-Code-Session-Id is {header} but \
                 metadata.user_id carries {body}; send the same value in both \
                 (disable reject_session_conflict to fall back to the header)"
            ),
        ));
    }

    // 2.3a4) 上游分类器拒答过的提示词（同一模型、system + messages + tools + tool_choice 逐字
    //        相同）→ **原样回放上游那次的响应**（200 + 同一段 `stop_reason: "refusal"` 的体，
    //        见 [`replay_refusal`]），不再送。不是 luban 自己造一条 403：客户端看到的与上游
    //        亲自再拒一次完全一样（0.3.98 之前回的是 403 permission_error，客户端按错误
    //        处理、看不到 stop_details）。学进来的只有带 `stop_details.category` 的分类器判决
    //        （见 [`UsageSniffer::classifier_refusal`]），那是确定性的：换个号、换个形态重发
    //        结果一样；见 [`known_refused_prompt`]。自己的开关 `reject_refusals`（0.3.93 之前
    //        借用 `reject_probes`，关探针就把这条一起放行了）。
    //
    //        **出站会带 `fallbacks` 的不拦**（客户端自带的，或 luban 按族开关要补的，见
    //        [`outbound_carries_fallbacks`]）：带 fallback 的请求上游拒答后会换模型重跑，那正是
    //        拒答该走的路。此前不看这一点，fallback 关着时学到的一条拒答，之后即便把 fallback
    //        打开也永远走不到上游——本地先 403 了。
    if billable
        && state.store.forward_flags().reject_refusals
        && !outbound_carries_fallbacks(
            body_json.as_ref(),
            req_model.as_deref(),
            state.store.forward_flags(),
            &inbound_beta_list(headers),
            &state.deprecated_fields,
        )
        && let Some(refused) =
            known_refused_prompt(&state.empty_replies, req_model.as_deref(), body_json.as_ref())
    {
        let model = req_model.as_deref().unwrap_or("-");
        let who = device_id.as_deref().or(session_id.as_deref()).unwrap_or("-");
        let device_short: String = who.chars().take(8).collect();
        let wants_stream = body_json.as_ref().is_some_and(stream_requested);
        match replay_refusal(&refused.reply, wants_stream) {
            Some(resp) => {
                if let Some(suppressed) =
                    take_rejection_log_slot(&state.rejection_log, &format!("refusal:{model}:{who}"))
                {
                    tracing::warn!(
                        %method, path = %path_and_query, ua = %client_ua,
                        %model, device = %device_short, suppressed,
                        stream = wants_stream, replay_sse = refused.reply.sse,
                        verdict = %refused.verdict.chars().take(300).collect::<String>(),
                        "not forwarded: upstream has already refused this exact prompt; replaying upstream's refusal (200 + the same body) locally"
                    );
                }
                *log_state.local_replay.lock() = Some(REWRITE_REFUSAL_REPLAY);
                return Err(resp);
            }
            // 学到的体按这次要的形态拼不出来（理论上不会：学的时候要求流完整收尾）——
            // 照常送上游，宁可多送一条，不回一段残缺的响应。
            None => tracing::warn!(
                %method, path = %path_and_query, ua = %client_ua,
                %model, device = %device_short,
                stream = wants_stream, replay_sse = refused.reply.sse,
                "the recorded upstream refusal could not be replayed in the shape this request asked for; forwarding upstream"
            ),
        }
    }

    // 2.3a4c) 按应用学到的拒答（只对**识别不了会话**的来访）：同一模型 + 同一份 system 被上游
    //         分类器拒得够多（至少 3 条且占三成以上，见 [`record_app_request`]）→ 之后这个应用
    //         的每条请求都回放最近那条拒答，见 [`known_app_refusal`]。
    //         封号复盘里 Go-http-client 那场风暴每条正文都不同、按提示词学的规则一条都命不中，
    //         而它只有 4 种 system——对不带身份的中转流量，system 就是「哪个应用」。带会话 id 或
    //         device_id 的来访不走这条：它们的 system 是客户端每轮都在变的官方形态，且真人对话
    //         偶发一次拒答不该连坐整个会话。与 2.3a4 共用 `reject_refusals` 开关与 fallbacks 例外。
    let session_less = device_id.is_none() && session_id.is_none();
    if billable
        && session_less
        && state.store.forward_flags().reject_refusals
        && !outbound_carries_fallbacks(
            body_json.as_ref(),
            req_model.as_deref(),
            state.store.forward_flags(),
            &inbound_beta_list(headers),
            &state.deprecated_fields,
        )
        && let Some(refused) =
            known_app_refusal(&state.empty_replies, req_model.as_deref(), body_json.as_ref())
    {
        let model = req_model.as_deref().unwrap_or("-");
        let wants_stream = body_json.as_ref().is_some_and(stream_requested);
        match replay_refusal(&refused.reply, wants_stream) {
            Some(resp) => {
                if let Some(suppressed) = take_rejection_log_slot(
                    &state.rejection_log,
                    &format!("app-refusal:{model}:{client_ua}"),
                ) {
                    tracing::warn!(
                        %method, path = %path_and_query, ua = %client_ua,
                        %model, suppressed, stream = wants_stream, replay_sse = refused.reply.sse,
                        verdict = %refused.verdict.chars().take(300).collect::<String>(),
                        "not forwarded: upstream has already refused this session-less app (same model + system); replaying upstream's refusal locally"
                    );
                }
                *log_state.local_replay.lock() = Some(REWRITE_APP_REFUSAL_REPLAY);
                return Err(resp);
            }
            None => tracing::warn!(
                %method, path = %path_and_query, ua = %client_ua, %model,
                "the recorded app-level refusal could not be replayed in the shape this request asked for; forwarding upstream"
            ),
        }
    }

    // 2.3a5) 上游对这一类请求（模型 + 无 tools 单条消息 + 这个 max_tokens）回过 200 却零输出
    //        → 本地 403，不再送。规则不是写死的，是上一条零输出的回复自己喂出来的，见
    //        [`known_empty_reply`]；回给客户端的文案带上上游当时的原话。自己的开关
    //        `reject_empty_replies`，不限 UA——模拟路径重建的是身份，改不了「问一句、上游一个字
    //        不回」这件事。
    if billable
        && state.store.forward_flags().reject_empty_replies
        && let Some((max_tokens, excerpt)) =
            known_empty_reply(&state.empty_replies, req_model.as_deref(), body_json.as_ref())
    {
        let model = req_model.as_deref().unwrap_or("-");
        let who = device_id.as_deref().or(session_id.as_deref()).unwrap_or("-");
        if let Some(suppressed) = take_rejection_log_slot(
            &state.rejection_log,
            &format!("empty-reply:{model}:{max_tokens}:{who}"),
        ) {
            let device_short: String = who.chars().take(8).collect();
            tracing::warn!(
                %method, path = %path_and_query, ua = %client_ua,
                %model, max_tokens, device = %device_short, suppressed,
                "rejected locally: upstream has already answered this request class with zero output tokens"
            );
        }
        return Err(error_response(
            StatusCode::FORBIDDEN,
            "permission_error",
            format!(
                "not forwarded: upstream has already answered this request class (model {model}, \
                 tool-less single-message, max_tokens {max_tokens}) with 200 and zero output tokens; \
                 upstream reply was: {}",
                excerpt.chars().take(300).collect::<String>()
            ),
        ));
    }
    Ok(())
}

/// 2.3b：已废弃的采样参数按 `sampling_policy` 剥掉或拒掉。
fn sampling_gate(
    state: &AppState,
    client_ua: &str,
    facts: &Facts,
    body: Bytes,
) -> Result<Bytes, Response> {
    let Facts { ref body_json, ref req_model, .. } = *facts;
    // 2.3b) 上游曾以 `deprecated` 拒过的字段（`temperature`、`top_p` 之类）。
    //       策略由 `sampling_policy` 控制：strip（默认）= 剥掉后转发，reject = 本地 400，
    //       off = 原样转发——静态名单和学到的都不用，上游 400 也不学（见 respond 里的学习入口）。
    //       与 2.3 共享「从上游 400 里学」的范式，但行为相反：那条路是拒绝，这条路是修补。
    let sampling_policy = state.store.sampling_policy();
    if sampling_policy == store::PrefillPolicy::Off {
        return Ok(body);
    }
    // reject 策略下静态名单与学到的组合一视同仁：都是「这个模型不收这个参数」的既定事实。
    let body = if sampling_policy == store::PrefillPolicy::Reject
        && ((req_model.as_deref().is_some_and(model_rejects_sampling)
            && has_deprecated_sampling_field(body_json.as_ref()))
            || has_learned_deprecated_field(
                &state.deprecated_fields,
                req_model.as_deref(),
                body_json.as_ref(),
            )) {
        tracing::info!(
            model = %req_model.as_deref().unwrap_or("-"),
            ua = %client_ua,
            "rejected: sampling parameters (temperature/top_p/top_k) are deprecated for this model (sampling_policy=reject)"
        );
        return Err(error_response(
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
            "Sampling parameters (temperature, top_p, top_k) are deprecated for this model.",
        ));
    } else {
        maybe_strip_deprecated(
            &state.deprecated_fields,
            req_model.as_deref(),
            body_json.as_ref(),
            body,
        )
    };
    Ok(body)
}

/// 2.4：每设备 RPM 上限。
fn device_rpm_gate(
    state: &AppState,
    method: &Method,
    path_and_query: &str,
    client_ua: &str,
    facts: &Facts,
    log_state: &RequestLogState,
) -> Result<(), Response> {
    let Facts { ref device_id, .. } = *facts;
    // 2.4) 每设备 RPM 上限：这台机器最近 60 秒发得太多 → 直接 429 + `retry-after`。
    //      **不换号**：账号打满换个号还能发，设备打满换哪个号都是同一台机器在刷，换号只会
    //      白白改绑设备（还会连累 thinking 签名，见 [`store::RpmLimited::sticky`]）。故这道闸
    //      独立于选号，也因此排在形态拦截之后：一条发都发不出去的请求不该占掉设备的名额。
    //      没有设备身份的请求（网页关了校验的那些）不受此闸管——它们由裸请求速率上限兜着。
    //
    //      与会话闸（1.6 / 2.2b）是**同一件事的两个粒度**：那道贴合单个对话的真实节奏，这道
    //      兜「这台机器总量别失控」——会话 id 轮换免费，只有它拦不住换 id 的客户端。语义与
    //      两个阈值该怎么配见 [`store::SESSION_RPM_LIMIT`]。
    if let Some(dev) = device_id.as_deref()
        && let Some(retry) = state.store.take_device_rpm_slot(dev)
    {
        // 日志抑制：撞满的客户端多半每几十毫秒就再撞一次，一条一行会把日志刷没。
        // 憋掉的条数记在下一行的 `suppressed=` 上，见 [`take_rejection_log_slot`]。
        if let Some(suppressed) =
            take_rejection_log_slot(&state.rejection_log, &format!("device:{dev}"))
        {
            let device_short: String = dev.chars().take(8).collect();
            tracing::warn!(%method, path = %path_and_query, ua = %client_ua, device = %device_short, retry_after = retry, suppressed, "rejected: this device has reached its RPM limit");
        }
        *log_state.local_reject.lock() = Some("device-rpm");
        return Err(rate_limit_response(
            retry,
            format!("this device has reached its RPM limit; retry in {retry} seconds"),
        ));
    }
    Ok(())
}
