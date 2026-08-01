import { api } from './client'

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
   * 最近一次带限流头的响应是否由**超额计费**放行（`overage-in-use`）：额度已满但上游
   * 照样 200，烧的是按量计费的钱——这种号永远不会 429，卡片上只有这个标记能暴露它。
   */
  overage_in_use: boolean | null
  /** 当前 5h / 7d 窗口内已用的等价费用（USD）。 */
  cost_5h: number | null
  cost_7d: number | null
  /** 当前 5h / 7d 窗口内经该账号转发的请求数。 */
  requests_5h: number | null
  requests_7d: number | null
}

/** 对外的凭证视图（后端已脱敏，无明文 token）。 */
export interface Credential {
  id: number
  label: string
  tier: string | null
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
  token_hint: string
  /** 最新一次的订阅额度快照；无请求记录时为 null。 */
  quota: Quota | null
  /** 最近一次被使用（转发请求）的时间戳（Unix 秒）；从未使用为 null。 */
  last_used: number | null
  /** 累计等价 API 费用（USD）。 */
  cost_total: number
  /** 账号级 429 冷却的剩余秒数；模型级冷却不在账号列表展示；0 = 未冷却。 */
  rate_limited_secs: number
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

/** 一条设备绑定明细；口径同 `device_count`：只含 TTL 内仍活跃的绑定。 */
export interface DeviceBinding {
  /** 客户端 metadata 里的原始 device_id。 */
  device_id: string
  /** 该设备经此账号转发过的累计请求数。 */
  request_count: number
  /** 首次绑定时间（Unix 秒）。 */
  created_at: number
  /** 最近一次活跃时间（Unix 秒）。 */
  last_seen_at: number
  /**
   * 该设备经**本账号**花掉的等价 API 费用（USD 合计）。
   *
   * 与 request_count 不同源：请求数随绑定行走（解绑/停用后从零重数），费用来自用量日志、
   * 一直累计，所以它覆盖的时间范围可能比请求数更长。
   */
  cost_usd: number
  /** 该设备在**所有账号**上的累计费用（USD）；用于识别换号后仍在烧钱的同一台设备。 */
  cost_usd_all: number
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

/** 设置设备数上限：>0 独立上限；0 跟随全局默认；-1 明确不限。 */
export async function setDeviceLimit(id: number, deviceLimit: number): Promise<Credential> {
  const { data } = await api.post<Credential>(`/credentials/${id}/device-limit`, {
    device_limit: deviceLimit,
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
  /** 本次请求是否由超额计费放行（额度满了但照样 200，烧的是按量计费的钱）。 */
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
