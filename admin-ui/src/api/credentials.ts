import { api } from './client'

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
  token_hint: string
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
