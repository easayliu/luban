import { api } from './client'

/**
 * 上游报告的一个额度窗口。窗口名原样透传（`5h`/`7d`/`7d_oi`/`overage` …），不做白名单。
 *
 * 5h/7d 另有专用字段，是因为只有它们能反推窗口起点去聚合窗口内费用与请求数；这里的窗口
 * 只有上游给的三个字段。真正需要它的是 `7d_oi` 这类超额池——实测里被拒的常常正是它，
 * 而它没有专用列，缺了这份列表后台就只能看到「5h/7d 都没满」却解释不了账号为什么在烧钱。
 */
export interface QuotaWindow {
  name: string
  status?: string | null
  utilization?: number | null
  reset?: number | null
}

/** 订阅账号最新额度快照（来自上游 anthropic-ratelimit-unified-* 头）。 */
export interface Quota {
  /** 快照对应请求时间（Unix 秒）。 */
  ts: number
  unified_status: string | null
  rl_5h_utilization: number | null
  rl_5h_reset: number | null
  rl_7d_utilization: number | null
  rl_7d_reset: number | null
  rl_representative: string | null
  /**
   * 最近一次带限流头的响应是否动用了 **Usage credits**（上游头里叫 `overage-in-use`）：额度已满但上游
   * 照样 200，烧的是按量计费的钱——这种号永远不会 429，卡片上只有这个标记能暴露它。
   */
  overage_in_use: boolean | null
  /** 当前 5h / 7d 窗口内已用的等价费用（USD）。 */
  cost_5h: number | null
  cost_7d: number | null
  /** 当前 5h / 7d 窗口内经该账号转发的请求数。 */
  requests_5h: number | null
  requests_7d: number | null
  /**
   * 当前 5h / 7d 窗口内用掉的**总 token**，窗口与上面的费用/请求数完全一致。
   *
   * 口径按官方 `usage` 的四项相加：输入 + 输出 + 缓存写 + 缓存读（官方这四项互不重叠，
   * 缓存命中的部分不再计进输入）。**不加权**——计价才有缓存写 ×1.25、缓存读 ×0.1 的倍率，
   * token 数跟着加权就和上游用量页对不上了。所以「token 很多、花费很少」是常态（缓存读通常
   * 占大头），两个数要一起看。
   */
  tokens_5h: number | null
  tokens_7d: number | null
  /**
   * 上游本次报告的**全部**窗口（含上面那两个）。升级前落的老快照是空数组，
   * 下一条带限流头的响应就会补齐。
   */
  windows: QuotaWindow[]
}

/** 一个模型当前的冷却剩余时间，见 `Credential.rate_limited_models`。 */
export interface ModelCooldown {
  model: string
  secs: number
  /**
   * 这条冷却是否**挡着选号**。`true` 是额度池满那档，该模型确实不参与选号；`false` 是瞬时
   * 限速（容量/请求速率）那档，只是个标记，该模型照常参与选号——措辞必须分开，否则会把一个
   * 仍在服务的账号显示成停摆。
   */
  gated: boolean
}

/** 对外的凭证视图（后端已脱敏，无明文 token）。 */
export interface Credential {
  id: number
  label: string
  tier: string | null
  /**
   * 组织类型原值（`claude_team`/`claude_enterprise`/`claude_max`…），拉不到时为 null。
   * 团队号的额度是整个组织共享的席位额度，跟同档位的个人号不是一回事，界面上单独打标。
   */
  org_type: string | null
  priority: number
  disabled: boolean
  expires_in: number
  /** 过期时刻（Unix 秒）；展示用这个，`expires_in` 只用来判临界态。 */
  expires_at: number
  /**
   * access_token 是否已到期。**不代表账号有问题**：刷新是惰性的（下次被调度时才刷），
   * 闲置久了必然为 true，下一个请求会自动刷好。凭证真正失效（refresh_token 被作废）
   * 走的是 `ban_reason`。
   */
  expired: boolean
  created_at: number
  updated_at: number
  /** 账号自身的设备上限设置：>0 独立上限；0 跟随全局默认；<0 明确不限。 */
  device_limit: number
  /** 实际生效的设备上限（已套用全局默认）；0 表示不限。 */
  device_limit_effective: number
  /** 当前已绑定的设备数。 */
  device_count: number
  /** 自动检测到的上游账号级错误原因（如封号）；为 null 表示未被自动停用。 */
  ban_reason: string | null
  /**
   * 该账号专用的出站代理（`socks5://`/`http://` 等）；null 表示直连。
   *
   * 配了之后这个号的**全部**出站流量都走它——转发、token 刷新、profile、连通性测试。
   * 后端不脱敏原样返回：串里可能带账号密码，但打了码就没法确认自己配的是哪一条。
   */
  proxy: string | null
  token_hint: string
  /** 最新一次的订阅额度快照；无请求记录时为 null。 */
  quota: Quota | null
  /** 最近一次被使用（转发请求）的时间戳（Unix 秒）；从未使用为 null。 */
  last_used: number | null
  /** 累计等价 API 费用（USD）。 */
  cost_total: number
  /**
   * 当前 RPM：最近 60 秒经该账号转发的请求数（含失败的）。
   *
   * 和 `quota.requests_5h/7d` 不是一回事：那两个要等一条带限流头的响应才刷新，窗口起点
   * 还得由 `reset` 反推，看不出「此刻压了多少」；这个每次轮询都是实时重算的。
   */
  rpm: number
  /** 账号自身的 RPM 上限设置：>0 独立上限；0 跟随全局默认；<0 明确不限。 */
  rpm_limit: number
  /**
   * 实际生效的 RPM 上限（已套用全局默认）；0 表示不限。
   *
   * 和 `rpm` 同一个窗口（最近 60 秒），可以直接比着显示成「12 / 30」。
   */
  rpm_limit_effective: number
  /**
   * **账号级**进程内 429 冷却的剩余秒数；0 = 未冷却。
   *
   * 正常路径上几乎恒为 0——账号级限流走的是落库的 `resume_at`，这一项只反映「落库失败」
   * 的兜底状态。模型级冷却在 `rate_limited_models` 里，两者不可混用。
   */
  rate_limited_secs: number
  /**
   * **模型级**冷却明细（容量限制 / 超额池满那种，默认 30 秒，记在后端进程内）。
   *
   * 这一档**不代表账号有问题**：只有列出的这些模型在选号时让位，该号的其余模型照常服务，
   * 所以不能拿它把账号显示成「不可调度」。此前后端压根没透出这个字段，于是 fable 撞超额池
   * 被冷却时后台一片正常，而选号侧已经跳过它了。
   */
  rate_limited_models: ModelCooldown[]
  /**
   * 被上游账号级限流而**自动停用**时，到点自动恢复调度的时刻（Unix 秒）；null 表示不会
   * 自动恢复（正常在用、人工停用、或封号）。
   *
   * 「被限流暂停」和「已停用/已封禁」的 `disabled` 都是 true，区别只在这一项——判状态时
   * 必须先看它，否则一个只是额度用完的号会被显示成封号。恢复有三条路：到点自动、连通性
   * 测试通过、手动打开启用开关。
   */
  resume_at: number | null
}

/**
 * 一条设备明细。真实绑定那部分口径同 `device_count`：只含 TTL 内仍活跃的绑定；
 * 末尾可能追加 `simulated` 为真的伪设备，它们不占设备名额，故不计入 `device_count`。
 */
export interface DeviceBinding {
  /** 客户端 metadata 里的原始 device_id；模拟客户端是 `sim:` 前缀的派生值。 */
  device_id: string
  /** 该设备经此账号转发过的累计请求数（终身）。 */
  request_count: number
  /** 首次绑定时间（Unix 秒）；模拟客户端没有绑定，为 null。 */
  created_at: number | null
  /** 最近一次活跃时间（Unix 秒）；模拟客户端不参与 TTL，为 null。 */
  last_seen_at: number | null
  /**
   * 是否是模拟客户端的伪设备：非 Claude Code 客户端没有自己的设备身份，代理按账号派生一个
   * 顶替。它们不写绑定、不占设备名额、也无法解绑，只有用量与费用是真实的。
   */
  simulated: boolean
  /**
   * 该设备经**本账号**花掉的等价 API 费用（USD 合计）。
   *
   * 与 request_count 同源（都来自终身账本），故两者覆盖的时间范围一致。
   */
  cost_usd: number
  /** 该设备在**所有账号**上的累计费用（USD）；用于识别换号后仍在烧钱的同一台设备。 */
  cost_usd_all: number
}

/**
 * 一条请求流水（`usage_logs` 的一行）。
 *
 * **与卡片上的累计值不同源**：卡片读的是终身账本，流水只保留近 30 天，所以明细逐条加起来
 * 通常小于卡片上的累计花费——不是哪一边算错了。
 */
export interface UsageLog {
  /** 自增主键，同时是排序键与翻页游标（见 listCredentialUsage 的 before）。 */
  id: number
  /** 请求完成时刻（Unix 秒）。 */
  ts: number
  cred_id: number | null
  cred_label: string
  /** 客户端 device_id；模拟客户端是 `sim:` 前缀的派生值，裸请求为 null。 */
  device_id: string | null
  /** 优先取响应回报的模型（可能与请求声明的不同），没有才用请求体里那个。 */
  model: string | null
  /** 来访原样的路径与查询串。 */
  path: string
  /**
   * **来访**客户端自报的 `User-Agent`（已截断）；没带该头的请求、连通性测试（不来自任何
   * 客户端）与 0.2.60 之前的旧记录为 null。
   *
   * 认「这条是谁发的」最省事的一项：`path` 里带不带 `?beta=true` 分不出官方 CC 与第三方
   * CC 兼容客户端——那个查询串是客户端自己加的，luban 只在出站 URL 上补。
   */
  ua: string | null
  /**
   * **实际发给上游**的那份 `User-Agent`；0.2.61 之前的旧记录为 null。
   *
   * 与 `ua` 不同即说明这条走了模拟（整套头换成官方的）；相同即原样转发。
   */
  ua_out: string | null
  status: number
  /**
   * 这条来访本来是非流式、被改写成流式发给上游再聚合回整段 JSON（转发设置里的
   * 「非流式请求流式化」）；0.2.63 之前的旧记录一律为 false。
   *
   * 它解释了这类记录里 `ttft_ms` 与 `total_ms` 为什么差很多：TTFT 记的是上游首字节，
   * 而客户端是在末尾一次性收到整段的。
   */
  sse_aggregated: boolean
  /** 响应里是否嗅探到 usage：为 false 时下面几个 token 数是缺失而非 0。 */
  has_usage: boolean
  input_tokens: number | null
  output_tokens: number | null
  cache_creation_tokens: number | null
  cache_read_tokens: number | null
  /** 首字节耗时（毫秒）。 */
  ttft_ms: number | null
  /** 整条请求耗时（毫秒）。 */
  total_ms: number | null
  /** 按模型价目表估算的等价 API 费用（USD）；模型认不出时为 null。 */
  cost_usd: number | null
}

/** 生成授权链接（后端暂存 PKCE）。 */
export async function getAuthorizeUrl(): Promise<{ url: string }> {
  const { data } = await api.get<{ url: string }>('/authorize')
  return data
}

/** 用粘贴的 code#state 交换并新增一条凭证。 */
export async function exchangeCode(code: string, label?: string): Promise<Credential> {
  const { data } = await api.post<Credential>('/exchange', { code, label })
  return data
}

/** 列出全部凭证。 */
export async function listCredentials(): Promise<Credential[]> {
  const { data } = await api.get<Credential[]>('/credentials')
  return data
}

/** 列出某账号当前绑定的设备（按最近活跃倒序）。 */
export async function listCredentialDevices(id: number): Promise<DeviceBinding[]> {
  const { data } = await api.get<DeviceBinding[]>(`/credentials/${id}/devices`)
  return data
}

/** 一页请求明细 + 整个集合的口径（总条数、总花费、翻页锚点）。 */
export interface UsagePage {
  /** 锚点之下的总条数，用来算页数。 */
  total: number
  /** 同一集合的花费合计（USD）。 */
  total_cost: number
  /** 本轮翻页的锚点 id；空集为 null。 */
  anchor: number | null
  logs: UsageLog[]
}

/**
 * 取某账号请求明细的一页（按时间倒序）。
 *
 * **`until` 锚点必须一路带着**：流水只增，翻页期间新请求会插到最前面，光靠 offset 会把
 * 第二页整体往回错、重复吐出第一页的尾巴。首次不传，之后把响应里的 `anchor` 原样带回来，
 * 整轮翻页就钉在同一个快照上——页码、总条数、总花费三者始终自洽。
 */
export async function listCredentialUsage(
  id: number,
  params: { limit?: number; offset?: number; until?: number } = {},
): Promise<UsagePage> {
  const { data } = await api.get<UsagePage>(`/credentials/${id}/usage`, { params })
  return data
}

/**
 * 解除某设备与该账号的绑定，立即腾出一个设备名额。
 *
 * 解绑不是拉黑：该设备下次请求会重新选号，名额没满时可能又落回同一个账号。
 * device_id 来自客户端，编码后再拼进路径。
 */
export async function unbindCredentialDevice(id: number, deviceId: string): Promise<void> {
  await api.delete(`/credentials/${id}/devices/${encodeURIComponent(deviceId)}`)
}

/** 删除一条凭证。 */
export async function deleteCredential(id: number): Promise<void> {
  await api.delete(`/credentials/${id}`)
}

/** 启用/停用。 */
export async function setDisabled(id: number, disabled: boolean): Promise<Credential> {
  const { data } = await api.post<Credential>(`/credentials/${id}/disabled`, { disabled })
  return data
}

/** 设置优先级。 */
export async function setPriority(id: number, priority: number): Promise<Credential> {
  const { data } = await api.post<Credential>(`/credentials/${id}/priority`, { priority })
  return data
}

/** 批量把多个账号统一设为同一优先级，返回更新后的整份列表。 */
export async function setPriorities(ids: number[], priority: number): Promise<Credential[]> {
  const { data } = await api.post<Credential[]>('/credentials/priority', { ids, priority })
  return data
}

/** 批量设置设备数上限（三态同单账号接口：>0 独立上限；0 跟随全局默认；-1 不限）。 */
export async function setDeviceLimits(
  ids: number[],
  deviceLimit: number,
): Promise<Credential[]> {
  const { data } = await api.post<Credential[]>('/credentials/device-limit', {
    ids,
    device_limit: deviceLimit,
  })
  return data
}

/** 批量设置账号 RPM 上限（三态同单账号接口：>0 独立上限；0 跟随全局默认；-1 不限）。 */
export async function setRpmLimits(ids: number[], rpmLimit: number): Promise<Credential[]> {
  const { data } = await api.post<Credential[]>('/credentials/rpm-limit', {
    ids,
    rpm_limit: rpmLimit,
  })
  return data
}

/** 批量启用/停用。 */
export async function setDisabledMany(ids: number[], disabled: boolean): Promise<Credential[]> {
  const { data } = await api.post<Credential[]>('/credentials/disabled', { ids, disabled })
  return data
}

/** 批量删除（连带清历史用量与设备绑定）。用 POST 是因为带 body 的 DELETE 会被部分代理丢掉。 */
export async function deleteCredentials(ids: number[]): Promise<Credential[]> {
  const { data } = await api.post<Credential[]>('/credentials/delete', { ids })
  return data
}

/** 重命名。 */
export async function setLabel(id: number, label: string): Promise<Credential> {
  const { data } = await api.post<Credential>(`/credentials/${id}/label`, { label })
  return data
}

/** 设置/清除该账号专用的出站代理；传 null 或空串改回直连。 */
export async function setProxy(id: number, proxy: string | null): Promise<Credential> {
  const { data } = await api.post<Credential>(`/credentials/${id}/proxy`, { proxy })
  return data
}

/** 设置设备数上限：>0 独立上限；0 跟随全局默认；-1 明确不限。 */
export async function setDeviceLimit(id: number, deviceLimit: number): Promise<Credential> {
  const { data } = await api.post<Credential>(`/credentials/${id}/device-limit`, {
    device_limit: deviceLimit,
  })
  return data
}

/**
 * 设置该账号每分钟最多转发多少条请求：>0 独立上限；0 跟随全局默认；-1 明确不限。
 *
 * 计数在后端进程内存里，改完即时生效——已经记在窗口里的那些既不会被追认、也不会被抹掉。
 */
export async function setRpmLimit(id: number, rpmLimit: number): Promise<Credential> {
  const { data } = await api.post<Credential>(`/credentials/${id}/rpm-limit`, {
    rpm_limit: rpmLimit,
  })
  return data
}

/** 手动刷新 token。 */
export async function refreshCredential(id: number): Promise<Credential> {
  const { data } = await api.post<Credential>(`/credentials/${id}/refresh`)
  return data
}

/** 手动解除限流冷却。冷却只是选号提示：解除错了，下一条请求撞上 429 会重新打上。 */
export async function clearCooldown(id: number): Promise<Credential> {
  const { data } = await api.delete<Credential>(`/credentials/${id}/cooldown`)
  return data
}

/**
 * 一次测试从上游限流头读到的额度快照。
 *
 * 与账号卡片上的 [`Quota`] 同名同义，但**没有窗口花费和请求数**——它们是后端按窗口起点
 * 聚合出来的，单次响应的头里没有。这份读数会随用量日志落库，所以卡片上的额度也会跟着更新。
 */
export interface ProbeQuota {
  /** `allowed` / `allowed_warning` / `rejected`。 */
  unified_status: string | null
  rl_5h_utilization: number | null
  rl_5h_reset: number | null
  rl_7d_utilization: number | null
  rl_7d_reset: number | null
  /** 上游认为「当前是哪个窗口在管事」。 */
  rl_representative: string | null
  /** `retry-after`（秒）；只有 429 才有，是这次拒绝给出的等待时间。 */
  retry_after_secs: number | null
  /** 本次请求是否由 Usage credits 放行（套餐额度满了但照样 200，花的是按量计费的钱）。 */
  overage_in_use: boolean | null
}

/** 一次连通性测试的结果。 */
export interface ProbeResult {
  /** 上游是否 2xx。 */
  ok: boolean
  /** 上游 HTTP 状态码；**0 表示请求根本没到上游**（取 token 失败/连不上/超时），原因见 error。 */
  status: number
  /** 从发出到读完响应的耗时（毫秒）。 */
  latency_ms: number
  /** 上游实际回报的模型名（成功时才有）；别名会在上游解析成具体版本，故可能与请求的不同。 */
  model: string | null
  /** 上游错误类型（`error.type`）。 */
  error_type: string | null
  /** 失败原因原文。 */
  error: string | null
  /** 本次响应的限流头快照；请求没到上游、或响应没带这些头时为 null。 */
  quota: ProbeQuota | null
}

/**
 * 连通性测试：用**这一个**账号向上游发一条最小请求（`max_tokens=1`），看它能不能用该模型。
 *
 * 不走负载均衡选号、不占设备名额、失败也不会自动停用账号，但会写一条用量日志（`device_id`
 * 标为 `probe`，卡片上的额度与累计花费据此更新），也会真的打到上游、消耗一点点订阅额度。
 * 上游拒绝同样是 200 + 一份结果（状态码在 `status` 里），不是 HTTP 错误。
 */
export async function probeCredential(
  id: number,
  model: string,
  signal?: AbortSignal,
): Promise<ProbeResult> {
  const { data } = await api.post<ProbeResult>(
    `/credentials/${id}/test`,
    { model },
    {
      signal,
      // 后端总探测上限是 30 秒；再留 5 秒给本机与代理传输，避免旧服务或断链让按钮永久 pending。
      timeout: 35_000,
    },
  )
  return data
}
