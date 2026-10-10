//! 转发形态开关 [`ForwardFlags`]。

/// 转发开关的集合。**默认全开**。
///
/// 前六项是**形态对齐**：上游实测（8 发对照，见 [`crate::config::known_fingerprint_gaps`]）
/// 全关掉也照样 200，唯一被强制的是 `system` 里那句 `You are Claude Code, …`，而它由客户端
/// 自己发。所以它们都是「像不像官方客户端」而非「能不能用」，可以按需一项项关掉做排查，
/// 全开 = 加入开关机制之前的既有行为。
///
/// 最后一项 [`Self::thinking_signature_retry`] 不是形态对齐而是**错误恢复**，只在特定
/// 400 上触发，正常路径完全不经过它。放在同一个集合里纯粹是因为它同样按请求读、同样
/// 一条 SQL 读齐、同样在「转发」那个设置面板里拨。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForwardFlags {
    /// 改写 `metadata.user_id` 里的 account_uuid/device_id 为凭证自洽身份。
    pub spoof_identity: bool,
    /// 来访**自带** `device_id` 时要不要换成派生值（[`Self::spoof_identity`] 的子项，
    /// 它关着这项就无从谈起）。
    ///
    /// 单独成一项的依据来自抓包：同一台机器、同一个客户端，`cap/raw/00002`（API-key 模式
    /// 经 luban）与 `00006`（订阅模式直连）发的 **`device_id` 完全相同**，两种模式的
    /// `metadata` 里只有 `account_uuid` 不同（前者空串、后者真 uuid）。也就是说把 API-key
    /// 形态转成订阅形态**并不需要**动 `device_id`——换掉它是**反关联**策略，不是形态要求。
    ///
    /// 两边各有代价，故交给用户拨：
    /// - **开**（默认，既有行为）：`device_id = f(账号, 机器)`，多个账号落在同一台机器上会
    ///   得到各不相同的设备身份，账号之间不因共用设备 id 而被串起来。代价是真实 CC 的
    ///   `device_id` 是**机器标识、跨账号恒定**，于是经 luban 的流量里「一台机器多个账号」
    ///   这个真实用户群里很常见的模式一次都不会出现，每个 (账号,机器) 都是全新设备。
    /// - **关**：来访自带的 `device_id` 原样透传，与官方两模式逐字节一致（`account_uuid`
    ///   照样补）。代价是同一台机器用多个账号时，上游能凭这个 id 把这些账号关联起来。
    ///
    /// **只作用于来访自带身份的那条路**（[`crate::proxy::spoof_identity`]）。模拟路径与
    /// 「CC 形态但缺 `metadata.user_id`」那条路上来访压根没有 `device_id`，只能派生，
    /// 不受本开关影响——否则产出的是一份没有 `device_id` 的 `metadata`，那是官方从不发的形态。
    pub spoof_device_id: bool,
    /// 设备指纹只取平台（arch/os），不含客户端原始 `device_id`（[`Self::spoof_device_id`]
    /// 的子项，它关着时指纹不参与，本项无从谈起）。
    ///
    /// - **开**（默认）：`fingerprint = arch|os|出站 UA` → 同平台**且同客户端版本**的客户端
    ///   收敛成同一个伪装 device_id，符合真实用户一人多设备的模式。
    /// - **关**：`fingerprint = client_device_id|arch|os|出站 UA` → 每个 (账号, 客户端设备)
    ///   都是独立的设备身份，客户端越多、上游看到该账号的设备数就越多。
    ///
    /// 出站 UA 那段两档都有、不受本开关约束：一台设备只能有一个客户端版本，换版本即换设备。
    /// 理由与代价（升级会换一次 device_id）见 [`crate::proxy::device_fingerprint`]。
    pub normalize_device_fp: bool,
    /// 给 `x-anthropic-billing-header` 补 `cch`。
    pub billing_cch: bool,
    /// 真实 CC 来访的 `cch` 策略（与 [`Self::billing_cch`] 互相独立）。`cch` 是官方出口层对
    /// 最终出站 body 算的 xxHash64（见 `proxy::body::compute_cch`），luban 一改写 body，
    /// 来访自带的那个就对不上了。
    ///
    /// - **开**（默认）：body 被改写过就按最终出站字节重算（来访自带的真值与 luban 补的占位
    ///   一视同仁）；没改写的原样透传，自带值本来就对。
    /// - **关**：来访自带的 `cch` 原样保留（改写后与 body 不符）；luban 替它补的那条填随机值。
    pub cch_real_recompute: bool,
    /// 模拟请求的 `cch` 策略（[`Self::simulate_cc`] 的子项）。
    ///
    /// - **开**（默认）：按最终出站字节算真值，与官方出口层同一算法。
    /// - **关**：每请求填一个随机的 5 位小写 hex（形状对、语义不对的旧做法）。
    pub cch_sim_compute: bool,
    /// 补齐客户端未携带的 `accept-encoding`/`anthropic-version`/`x-client-request-id`。
    pub fill_client_headers: bool,
    /// 合并并按官方顺序重排 `anthropic-beta`（含塞入 oauth beta）。
    pub merge_beta: bool,
    /// 把 `system` 对齐成官方订阅客户端的 4 块形态（见 [`crate::proxy::align_system_shape`]）。
    pub system_shape: bool,
    /// 按官方拼写与顺序发出头名（见 [`crate::config::CC_HEADER_ORDER`]）。
    pub orig_header_case: bool,
    /// 上游以「thinking 块签名无效」拒绝时，把历史 thinking 降级成 text 后重试一次
    /// （见 [`crate::proxy::demote_thinking_blocks`]）。
    pub thinking_signature_retry: bool,
    /// 上游以「`redacted_thinking` 块的 `data` 无效」拒绝时，降级历史 thinking 块后重试一次。
    /// 那段密文是上游自己签发的，验不过通常是会话中途换了号、或那一轮 assistant 消息被改写过
    /// （见 [`crate::proxy::trace_thinking_block`] 那行日志怎么分辨）。
    pub redacted_thinking_retry: bool,
    /// 非 Claude Code 客户端的请求，按官方抓包形态模拟成 CC 请求（注入 system 前缀 +
    /// 整套官方头，见 [`crate::proxy::Simulation`]）。
    pub simulate_cc: bool,
    /// 模拟路径补齐官方 `system` 的**第四块**（基座之后那段一万字节上下的「其余」正文，
    /// [`crate::config::CC_SYSTEM_REST`] 模板按请求填占位）。客户端自己的 system 两种取值下都
    /// 单独占末块，**位置**不受这项影响；受影响的是它压不压得住——这一块在告诉模型它是
    /// Claude Code（[`Self::simulate_cc`] 的子项，它关着这项无从谈起）。
    ///
    /// - **开**（默认）：出站 `system` 是 `[billing, 身份, 基座, 其余]` 再跟客户端那块，末块之前
    ///   与 2.1.277 四族同形；「其余」里唯一随机器变的记忆目录优先用来访自己写的工作目录，
    ///   来访没写才按账号加设备派生一个假路径（[`crate::proxy::client_env`]）。代价是每条模拟
    ///   请求多约 2700 token 的前缀——带 `ttl:1h` 断点、同一设备同一会话内稳定，基本走缓存读价；
    ///   模型也会被这段官方提示词带得更像 Claude Code：实测客户端要求「只回某个标记」时，它会
    ///   照回那个标记，但后面还会再加一句自我介绍。
    /// - **关**：这一块不发，出站 `system` 只剩 `[billing, 身份, 基座]` 加客户端那块（haiku 实测
    ///   385 token，开着是 2978），不注入任何环境信息，客户端的 system 也不被官方提示词稀释。
    pub simulate_full_system: bool,
    /// 模拟路径上来访**一个工具都没声明**（没有 `tools` 键、`tools: null`、`tools: []`）时，也按
    /// 主线程补齐官方工具（[`Self::simulate_cc`] 的子项）；来访 `tool_choice` 是 `any` / 指定工具
    /// 时不补。
    ///
    /// - **开**（默认）：补上那 14 个官方工具。模拟路径只发主线程 profile，官方主线程一条不带
    ///   工具的样本都没有（`cap/2.1.280` 恒为 19 / 20 个），「主线程的 beta 与 system、零个工具」
    ///   是官方不产生的组合。代价两条：这类来访多半是没有工具循环的纯聊天客户端，模型调了注入
    ///   的工具时它拿到的是一个处理不了的 `tool_use`（流水 `rewrites` 列：补了的打 `tools_filled`，
    ///   真调了再打 `injected_tool_called`，两者一比是命中率）；每个新会话首轮多付约两万 token 的工具声明写入价，之后走缓存读价。
    /// - **关**：不带工具的请求一个工具都不注（空数组原样发出）。自己带了工具的两种取值下都补缺。
    ///   luban 自己的连通性探测不受这项管，恒补。
    pub fill_absent_tools: bool,
    /// 模拟路径注入的官方工具去掉 `Artifact` / `ListAgents` / `SendFeedback` 三条（[`Self::simulate_cc`]
    /// 的子项），见 [`crate::proxy::cc_tools_core`]。
    ///
    /// - **开**（默认）：注入 11 条。官方客户端里这三条都能由用户自己关掉（环境变量
    ///   `CLAUDE_CODE_DISABLE_ARTIFACT=1`、`CLAUDE_CODE_HARBOR_KITE=0`、`CLAUDE_CODE_SEND_FEEDBACK=0`），
    ///   关掉后主线程正文正好少这三条、其余工具与 system 逐字节不变（`cap/auto-2.1.291-20261006`
    ///   四族各一对）；遥测照关掉后的样子报——`tengu_startup_telemetry.set_env_vars` 列出这三个
    ///   变量名、多一条 `tengu_artifact_disabled_session{mechanism: env}`、少几条 artifact 与跨会话
    ///   消息的事件、工具计数跟着变。省下约 41KB 工具声明（Artifact 一条就 34KB），新分词器下每个
    ///   新会话首轮约少 1.4 万 token 的写入。
    /// - **关**：注入完整的 14 条，与官方默认配置相同。
    pub sim_trim_tools: bool,
    /// 只在 `system[0]` 注一条最小 billing header（`cc_version` / `cc_entrypoint` / `cch`），
    /// 其余注入一概跳过（[`Self::simulate_cc`] 的子项，实验性）。对所有来访生效：真实 CC 客户端
    /// 不走模拟，自带的 billing header 照旧（缺了只补 billing header、不补身份句），`metadata.user_id`
    /// 的去留看 [`Self::sim_billing_keep_user_id`] / [`Self::real_billing_keep_user_id`]，身份补全 / 会话链 / 工具名混淆 / 断点与 system 整形 / 字段剥除等改写同样一概跳过，见 [`Self::billing_only`]。
    ///
    /// - **开**：不补身份句、官方基座、第四块、官方工具、`thread` / `diagnostics` /
    ///   `output_config`，不重排顶层键（`metadata.user_id` 另由两项子开关管）；客户端的 system 块、工具与参数原样透传（防 400 的归一照做）。
    ///   换头照旧；`cch` 跟随 [`Self::cch_sim_compute`]（开算真值、关填随机值）。上游放行只认
    ///   身份句或合法 billing header 二者其一，billing header 单独就能过闸、且不强加 CC 人格。代价：官方「仅 billing header」的请求几乎都是 0 工具、
    ///   一两轮的辅助调用，长多轮带工具的主对话官方从不这样发，属官方不产生的形态。
    /// - **关**（默认）：按完整官方形态模拟。
    pub sim_billing_only: bool,
    /// billing-only 下**模拟请求**带不带 `metadata.user_id`（[`Self::sim_billing_only`] 的子项）。
    /// 真实客户端另由 [`Self::real_billing_keep_user_id`] 管。官方每条请求都带 `user_id`，「仅
    /// billing header」那类辅助调用也一样（`cap/auto-2.1.293-20261008-full`）。
    ///
    /// - **开**（默认）：带了就保留，并照常按身份伪装 / 归一化规则改写（[`Self::spoof_identity`]、
    ///   [`Self::spoof_device_id`]、[`Self::normalize_device_fp`]），会话段对齐出站会话头；没带就按
    ///   官方形态补一份（会话段即出站会话头那个）。身份伪装关着时不补、不改。
    /// - **关**：整个剥掉（`metadata` 剥空了一并去掉）。
    pub sim_billing_keep_user_id: bool,
    /// billing-only 下**真实 CC 客户端**带不带 `metadata.user_id`（[`Self::sim_billing_only`]
    /// 的子项）。billing-only 下由它代替 [`Self::fill_metadata`] 决定补不补。
    ///
    /// - **开**（默认）：带了就保留，并照常按身份伪装 / 归一化规则改写（account 换成本号、device 按
    ///   设备指纹派生、会话段对齐按账号钉住的出站会话头）；没带就补一份（同
    ///   [`crate::proxy::bare_session_id`]）。身份伪装关着时原样透传、不补。
    /// - **关**：整个剥掉。官方本就有不带 `user_id` 的形态（Claude Desktop 不带，Claude Code 也能用
    ///   环境变量关掉）。
    pub real_billing_keep_user_id: bool,
    /// 模拟路径的主线程按官方的 message threads 形态写 `thread`（[`Self::simulate_cc`] 的子项；
    /// 2.1.285 的 fable-5-1 除外——官方那一版不发，2.1.291 起发）。
    ///
    /// - **开**（默认）：会话里一段对话的第一条写 `thread: {type: create}`；之后来访的历史若正好是
    ///   「上一轮 + 上游那条回复 + 新消息」，写 `thread: {type: continue}`、只发新增消息，接不上
    ///   （改了历史、重新生成、换了模型 / effort / system / tools、上一条失败）就再 `create`。
    ///   官方 opus / sonnet / haiku 主线程每条都带 `thread`，同一会话里除首轮与上述事件外全是
    ///   `continue`（`cap/auto-2.1.285-20260930`）。每条主线程请求（含 fable-5-1）末尾同时补上
    ///   官方的 `<total_tokens>N tokens left</total_tokens>` 提醒，N 按官方倒数算法（新输入重置为
    ///   1500 万，工具续轮减去本轮上下文的增量）。
    /// - **关**：不写 `thread`、不补提醒，每轮发完整上下文（2.1.280 非 auto 模式 opus / fable 的形态）。
    pub sim_message_threads: bool,
    /// 已是 CC 形态、但不带 `metadata.user_id` 的请求，补一份官方形态的身份
    /// （见 [`crate::proxy::bare_session_id`]）。
    pub fill_metadata: bool,
    /// 上游回 429 时给该号打冷却并换号重试（次数见
    /// [`CredentialStore::rate_limit_retry_max`](super::CredentialStore::rate_limit_retry_max)）；关掉即原样透传 429、也不打冷却。
    pub rate_limit_retry: bool,
    /// 官方基座那块的缓存断点带不带 `scope:"global"`（跨账号共享同一份基座缓存）。
    ///
    /// 单独成一项而不是并进 [`Self::system_shape`]：它要上游的 `prompt-caching-scope` beta
    /// 认（故还要 [`Self::merge_beta`] 开着），而且**官方从不单独发 `scope`**——官方那份总是
    /// `{type, ttl:1h, scope}`，luban 不再替客户端写 `ttl`（那是客户端掏钱买的时长），
    /// 于是发出去的是 `{type, scope}`。收益（跨账号复用基座）与这处形态偏差谁更重要，
    /// 交给用户自己拨。
    pub cache_scope_global: bool,
    /// 缓存断点写不写 `ttl:"1h"`。
    ///
    /// **默认开（对齐官方）**：四份订阅直连抓包的三个断点 3/3 全是 `ttl:"1h"`，而 API-key
    /// 模式那四份是裸的 `{"type":"ephemeral"}`——也就是说这个字段正是两种模式之间真实存在的
    /// 差别之一，不写就等于每条请求都留一处稳定差异。
    ///
    /// **代价要知情**：1h 的缓存**写入**单价是默认 5m 的 2 倍。命中与否取决于使用节奏——
    /// 长会话里 1h 往往反而更省（5m 内没接上话，下一轮就得按写入价重写整个前缀），
    /// 短促的一次性请求则是纯多付。所以给了开关：关掉即沿用客户端自己传的时长，
    /// luban 一个字节都不改。开着时只补订阅端官方带 1h 的那几类（主线程与「猜下一句」，
    /// 子代理、分叉、预热不补），且客户端自己写的短 `ttl` 一并升 1h（`ttl` 须按处理序单调不增）。
    ///
    /// 与 [`Self::cache_scope_global`] 一样要 beta 认（`extended-cache-ttl-2025-04-11`，
    /// 由 `merge_beta` 补），故还连着那个开关，见 [`crate::proxy::rewrite_body`]。
    pub cache_ttl_1h: bool,
    /// 工具声明补 `eager_input_streaming: true`，只补**抓包证实**该 profile 全带的那些组合
    /// （[`crate::config::CcEagerTools`]）：真 CC 按来访版本 × 模型 × 用途查表，模拟路径按出站
    /// profile；客户端显式写了的值不覆盖，`mcp__*` / 延迟占位 / 服务端工具不动。
    ///
    /// 订阅端主线程的每个内建工具都带这个字段、API-key 端一个不带，是两种模式间一处逐工具
    /// 重复的固定差异。它与出站头上的 `advanced-tool-use` beta 同现，故还连着 `merge_beta`
    /// （耦合点在 [`crate::proxy::rewrite_body`]）。收益是缩小声明差异，对封号率的影响幅度
    /// 没有量过，所以单独给开关。
    pub eager_tool_streaming: bool,
    /// 非流式 `/v1/messages` 改成流式发给上游，再把 SSE 聚合回整段 JSON 给客户端。
    ///
    /// **这是形态对齐里最硬的一项**：官方 CC 的 `/v1/messages` **恒为 `stream:true`**，
    /// 一条 `stream:false` 转发出去就是 100% 的判据，比 UA、比头序都硬（那些至少还有
    /// 第三方客户端会撞对）。而流/非流在**头上完全同形**——官方即便流式也发
    /// `accept: application/json`（见 `simulated_headers_replace_client_headers`），
    /// 差别只在 body 那一个字段，所以改起来只动一个 bool、不碰任何头。
    ///
    /// 只作用于计费路径（[`crate::proxy::is_billable_messages`]）：`count_tokens`
    /// 官方本来就是非流 JSON，动它反而制造偏差。
    ///
    /// 客户端侧完全无感：回给它的仍是 `content-type: application/json` + 整段 Message，
    /// 由 [`crate::proxy::aggregate_sse`] 按官方那套事件语义攒出来。上游中途出错时那条
    /// `event: error` 虽然裹在 200 里，也会按 `error.type` 翻译成非流式那边该有的状态码
    /// （见 [`crate::proxy::error_status`]），故客户端的错误分支照旧能走。
    ///
    /// 代价是整段响应要在内存里攒齐才发出（上限即 `max_tokens`，长文本级别，不是流量级别），
    /// 以及 `ttft_ms` 记的是上游首字节、与客户端的感知对不上——后者由 `usage_logs` 的
    /// `sse_aggregated` 列标出来。
    pub nonstream_as_sse: bool,
    /// 剥掉官方客户端**从不发送**的顶层字段（见 [`crate::proxy::strip_extra_fields`]）。
    ///
    /// 依据是两份直连抓包（`cap/raw/00006` opus-5、`00009` sonnet-5）的顶层键完全一致：
    /// `model, messages, system, tools, metadata, max_tokens, thinking, context_management,
    /// output_config, stream`。多出来的键都是官方不产生的形态。剥的有：等价于缺省的
    /// `tool_choice:{"type":"auto"}`、`thinking.display`、fable 族的 `thinking:disabled`。
    /// 另外顺手修补客户端自己写的、与 thinking 冲突必回 400 的组合：强制工具时删手动预算的
    /// thinking、`budget_tokens` 抬到 1024、剥掉 thinking 开着时的 `temperature≠1` 与
    /// `top_p<0.95`。luban 自己注入的 thinking 不靠本项收拾，见 `proxy::ensure_thinking`。
    ///
    /// **`thinking.display` 那项有代价**：剥掉后回程的 `thinking` 块文本为空，客户端看不到
    /// 思考摘要（功能不坏，只是没内容）。默认仍开——被判成第三方应用是**整条请求打不通**，
    /// 拿摘要换连通性划算；不接受这个代价就关掉本项。
    ///
    /// 对真实 CC 基本是空操作：它本来就不发这些。
    pub strip_extra_fields: bool,
    /// 把上游会判成第三方应用的工具名换成假名转发，回程再还原（见
    /// [`crate::proxy::ToolNameMap`]）。
    ///
    /// **实测**：`tools[*].name` 是上游判定第三方的一个判据——不在官方 CC 工具名集合内的
    /// custom tool 名会让整条请求回 400（`Third-party apps now draw from your extra usage…`，
    /// 额度改扣超额池）。映射到已验证豁免的 `mcp__luban__*` 命名空间后，同一条请求回 200。
    ///
    /// **白名单策略**：三类保留原名——server tool、`mcp__` 前缀（实测豁免）、
    /// [`crate::config::CC_TOOL_NAMES`] 里的官方 CC 工具名。其余 custom tool 一律混淆。
    /// 故对真实 CC 是空操作，不必再叠客户端判定。
    ///
    /// 代价：回程每个 chunk 要做 N 次字节替换（N = 被混淆的工具数），且客户端增删工具会让
    /// 整套假名重算、上游 prompt cache 失效一次。关掉即完全退回原样转发。
    pub tool_name_mimic: bool,
    /// 模拟路径下是否注入 `thinking`（及配套的 `context_management`）。
    ///
    /// 官方 CC 恒带 `thinking`，缺了可能被判第三方。但注入 thinking 会改变模型行为
    /// （输出更长、token 消耗更多），且会强制 `temperature=1`（`strip_extra_fields` 自动剥）。
    /// 不想要这些副作用就关掉——代价是模拟形态少一个正面信号。
    pub inject_thinking: bool,
    /// 本地拒绝带 OpenAI 格式转换残留的请求（messages 开头的 `role:"system"`、`call_` 前缀的
    /// 工具调用 id、OpenAI 方言的 `tool_choice` / `tools`、`n` / `stop` / `user` 等 OpenAI 专属
    /// 顶层字段），不修补、不转发，见 `proxy::find_openai_marker`。
    ///
    /// 关掉即原样转发，由上游返回官方的 400——客户端自己发错的形态 luban 不再替它修补。
    /// 对话中途的原生 system 消息不算残留，原样出站。模拟路径不受影响：它只接管本来就是 Anthropic 形态的非 CC 请求。
    pub reject_openai_shape: bool,
    /// 来访的会话 id 在**头与体两处不一致**时本地拒绝（400），不替它选一个。
    ///
    /// 官方两处逐字相同，不同值说明来访自己就不自洽——而 luban 拿会话 id 当会话链
    /// （`cc_prompt_id` / `cc_prev_req` / `diagnostics`）的键，选错一个就是把两条链
    /// 接到了一起。默认开：宁可让客户端修好自己的形态，也不猜。
    ///
    /// 关掉后退回「取头那个 + 打一条 warn」。见 [`crate::proxy::session_id_conflict`]。
    pub reject_session_conflict: bool,
    /// 本地就地回答**探针类**请求（200 + 一条最小的正常回复，见 `proxy::probe_reply`），
    /// 不转发。判据是一组只有探活脚本才会有的强特征，任一命中即算，不限 UA，
    /// 见 [`crate::proxy::probe_signature`]：
    /// - 带 system、没有 tools、只有一条消息、`max_tokens` 在 2..=16；
    /// - 带 system、没有 tools、只有一条消息、不是官方那两种无 tools 形态，且来自一台从没
    ///   见过的设备；
    /// - system 里 CC 身份句出现在不止一块里。
    ///
    /// 身份字段写错的（device 不是 64 位 hex、session 不是 uuid）**不在这里拒**：那是抄错了，
    /// 不是探针，由模拟路径重建身份（`proxy::cc_identity_well_formed`）。「没有 tools」按值算：
    /// 缺失、`null`、`[]` 都是没有，加空字段绕不过。只看形态、一条就判，不做计数。官方 CC 的
    /// 四种无 tools 请求（cache 预热、Helper 子代理、标题生成、安全分类）按 system 结构 + beta
    /// 头 + body 取值逐项对、都在判据之外，见 `cap/` 抓包与 `proxy::probe_signature`。
    ///
    /// 命中回的是 200 而不是 403（0.3.101 起）：探活正是下游中转判断「这个号还能不能用」的
    /// 那条请求，luban 的 403 在它那侧与「号被封了」长得一样，整个 key 会被摘下去、真流量跟着
    /// 停——而这条请求根本没到上游、账号一点事没有。本地作答的这条标得出来：响应头
    /// `x-luban-local: probe_reply` 与 `x-luban-probe-kind: <判据>`、Message id 以 `msg_luban`
    /// 开头、流水里标 `probe_reply` 且花费记 0。
    ///
    /// 只管这三条形态判据。从响应学来的两类规则各有自己的开关（[`Self::reject_refusals`]、
    /// [`Self::reject_empty_replies`]）：三件事的依据、误伤面、该不该开都不一样，共用一个键
    /// 就没法单独关一件。默认开。
    pub reject_probes: bool,
    /// 探针拒绝的**严格模式**（随 [`Self::reject_probes`] 一起才生效）：ping 不再要求无 tools
    /// （`max_tokens` 不超过 16 装不下一次 tool_use，带工具只给 16 个 token 只能是测活）；新增
    /// 「短开场」——无 system、无 tools、恰好一条不超过 32 字节（中文约十个字）的用户消息、`max_tokens != 1`
    /// （测活脚本的「hi」「ping」「test」）。覆盖封号复盘里默认判据放过去的那几批 Go-http-client
    /// 探活（4 个 tools + max_tokens 16；max_tokens 50 / 1024 / 32000 只问一句）。代价是真人用
    /// 裸聊天客户端经中转站发的第一句「你好」也会被拦下、收到探活那条一样的「OK」，
    /// 故**默认关**。
    pub reject_probes_strict: bool,
    /// 是否本地拦下 **上游分类器已经拒答过的那条提示词**的逐字重发（`kind = "refusal"`，
    /// 见 `proxy::known_refused_prompt`）——拦下时**原样回放上游那次的响应**（200 + 同一段
    /// `stop_reason: "refusal"` 的体，见 [`LearnedReply`](super::LearnedReply)），不是 luban 自己造一条 403：客户端
    /// 看到的与上游亲自拒一次完全一样。拒答是内容分类器给那一条提示词的确定性判决
    /// （`stop_details.category` 非空），换个号、换个形态重发结果一样，本地拦下省一次白跑。
    /// **只挡出站不带 `fallbacks` 的请求**：客户端自带、或 luban 按族开关补上 fallback 的请求，
    /// 上游会换模型重跑，那正是拒答该走的路，本地拦下反而让它永远走不到。规则随其他学到的
    /// 规则落库、7 天到期、控制台可删。默认开。
    pub reject_refusals: bool,
    /// 是否本地 403 **上游回过 200 却零输出的请求类**（模型 + 无 tools 单条消息 + 同一个
    /// `max_tokens`，不限 UA；`kind = "empty_reply"`，见 `proxy::known_empty_reply`）——那是
    /// 上游收了输入的钱、一个字没回，每条都是一次白白留下的「一台设备只问一句话」记录。
    /// 规则随其他学到的规则落库、7 天到期、控制台可删。默认开。
    pub reject_empty_replies: bool,
    /// 是否从上游 400 里**学请求形态错误**并在本地拒掉同样的组合（`effort: 'xhigh'`、
    /// `role: 'system'`、某个 tool type 之类；`kind = "shape"`，见 `proxy::remember_shape_rejection`
    /// 与 `proxy::known_shape_rejection`）。关掉即两头都停：不学，已学到的也不拦，400 照常
    /// 交给上游。规则随其他学到的规则落库、7 天到期、控制台可删。默认开。
    pub reject_learned_shapes: bool,
    /// 替每条转发成功的 `/v1/messages` 上报官方客户端会发的那串遥测：一方事件
    /// （`tengu_api_query` → `tengu_api_success` → `tengu_turn_end`，带上游 `request-id`、
    /// 逐项 token 与花费）、Datadog 日志、OTel 指标，身份取实际发往上游的那份，节奏照
    /// 抓包（30s / 10s / 5min 攒批）。见 [`crate::telemetry`]。
    ///
    /// 失败的请求走另一条收尾（`tengu_api_error` + `tengu_feature_bad{api_request}` +
    /// `terminal_reason: "api_error"` 的 `tengu_turn_end`），与官方一致。
    ///
    /// 关掉即只剩 [`crate::oauth`] 的保活遥测——上游那边这个账号就成了「有大量 API 用量、
    /// 遥测里却一条 API 调用都没有」的形态。
    pub api_telemetry: bool,
    /// 保活循环里的遥测那一半：每 30 分钟一组空闲版本检查事件（event_logging + Datadog）与
    /// 每 6 小时一次 GrowthBook 画像。有近期真实会话的凭证，事件挂到那个会话的身份上
    /// （同一 session_id / device_id / 版本），没有的才用按账号派生的空闲身份。
    ///
    /// 关掉只停这一半：token 刷新、bootstrap / policy_limits / settings 握手与 401/403
    /// 探测照常。不影响 [`Self::api_telemetry`]。
    pub keepalive_telemetry: bool,
    /// **fable 族**主线程请求补服务端 refusal fallback：安全分类器拒答（`stop_reason:
    /// "refusal"`，如 cyber 类）时由上游在同一次调用里换模型重跑，客户端拿到的是回答而不是
    /// 拒答。补的是官方 2.1.260 那份 `[{"model":"claude-opus-5"}]`（形态逐字同官方），出站头
    /// 一并带 `server-side-fallback-2026-06-01`。客户端自己带了数组形态的不动；只对主线程
    /// profile 补，辅助请求（helper / 标题 / 分类 / 额度探测）官方都不发。上游以 400 拒掉某个
    /// fallback 目标时，剥掉重发一次并记进「从上游学到的规则」，之后该模型不再补。落到
    /// fallback 的回复按实际服务的模型计价。**默认关**：形态有官方 2.1.260 抓包依据，但它替
    /// 用户决定了拒答后由 opus-5 作答、按 opus 计价、同一对话约一小时粘在 opus 上，且用户看不到
    /// 拒答本身——这些改变该由用户自己拨开；关着时客户端自带的 `fallbacks` 照样保留、只归一
    /// 形态（`"default"` → 数组），头上的 beta 由 [`Self::merge_beta`] 按版本补，「有 beta 没
    /// 字段」正是 2.1.260 之前的官方形态。见 `crate::proxy::refusal_fallbacks_for`。
    pub fable_refusal_fallback: bool,
    /// **opus-5 族**主线程请求补 luban 自定的 refusal fallback 链
    /// `[{"model":"claude-opus-4-8"},{"model":"claude-opus-4-6"}]`（`config::OPUS_REFUSAL_FALLBACKS`）。
    /// 官方 2.1.260 的 opus 客户端**不发**这个字段：补了就是一份官方从不产生的请求形态，
    /// 是风控层面的自证风险，故**默认关**、作为独立实验开关保留；关着时 opus-5 的请求形态与
    /// 官方一致（有 beta 没字段）。其余行为（只补主线程、客户端自带的不动、400 学习后不再补、
    /// 按实际作答模型计价）同 [`Self::fable_refusal_fallback`]。
    pub opus_refusal_fallback: bool,
}

impl ForwardFlags {
    /// 真实客户端（不走模拟）带设备身份时**按会话**占名额、设备上限不生效：设备指纹归一化开着、
    /// 身份伪装连同 device 一起换，出站 device_id 只剩「账号 + 平台 + 客户端版本」那几种，绑了
    /// 几台真实机器上游看不见。见 [`Select::per_session`](super::Select::per_session) 与 `crate::proxy::session_plan`。
    /// billing-only 下保留的 `user_id` 同样按这套规则改写、剥掉的上游看不见设备，口径不变。
    pub fn devices_by_session(self) -> bool {
        self.normalize_device_fp && self.spoof_identity && self.spoof_device_id
    }

    /// [`Self::sim_billing_only`] 实际生效：它挂在「模拟 Claude Code」下（后者又要 `merge_beta`），
    /// 父开关关着就不生效。生效时**所有来访**——模拟的第三方与不走模拟的真实 CC 客户端——都只
    /// 保证 `system[0]` 有合法 billing header，其余请求体原样透传。
    pub fn billing_only(self) -> bool {
        self.sim_billing_only && self.simulate_cc && self.merge_beta
    }
}

impl Default for ForwardFlags {
    fn default() -> Self {
        Self {
            spoof_identity: true,
            spoof_device_id: true,
            normalize_device_fp: true,
            billing_cch: true,
            cch_real_recompute: true,
            cch_sim_compute: true,
            fill_client_headers: true,
            merge_beta: true,
            system_shape: true,
            orig_header_case: true,
            thinking_signature_retry: true,
            redacted_thinking_retry: true,
            simulate_cc: true,
            simulate_full_system: true,
            fill_absent_tools: true,
            sim_trim_tools: true,
            sim_billing_only: false,
            sim_billing_keep_user_id: true,
            real_billing_keep_user_id: true,
            sim_message_threads: true,
            fill_metadata: true,
            rate_limit_retry: true,
            cache_scope_global: true,
            cache_ttl_1h: true,
            eager_tool_streaming: true,
            nonstream_as_sse: true,
            strip_extra_fields: true,
            tool_name_mimic: true,
            inject_thinking: true,
            reject_openai_shape: true,
            reject_session_conflict: true,
            reject_probes: true,
            reject_probes_strict: false,
            reject_refusals: true,
            reject_empty_replies: true,
            reject_learned_shapes: true,
            api_telemetry: true,
            keepalive_telemetry: true,
            fable_refusal_fallback: false,
            opus_refusal_fallback: false,
        }
    }
}
