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
  /** 当前 5h / 7d 窗口内已用的等价费用（USD）。 */
  cost_5h: number | null
  cost_7d: number | null
}

/** 对外的凭证视图（后端已脱敏，无明文 token）。 */
export interface Credential {
  id: number
  label: string
  tier: string | null
  priority: number
  disabled: boolean
  expires_in: number
  expired: boolean
  created_at: number
  updated_at: number
  /** 允许绑定的设备数上限；0 表示不限。 */
  device_limit: number
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

/** 重命名。 */
export async function setLabel(id: number, label: string): Promise<Credential> {
  const { data } = await api.post<Credential>(`/credentials/${id}/label`, { label })
  return data
}

/** 设置设备数上限（0 表示不限）。 */
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
