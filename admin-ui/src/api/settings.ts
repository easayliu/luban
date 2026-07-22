import { api } from './client'

export interface Settings {
  /** 当前接入 key（null = 未设置，不校验来访）。 */
  api_key: string | null
  /** 是否由环境变量接管（true 时网页只读）。 */
  env_managed: boolean
  /** 设备绑定有效期（秒）；0 表示永不过期。 */
  device_binding_ttl_secs: number
}

/** 读取接入设置。 */
export async function getSettings(): Promise<Settings> {
  const { data } = await api.get<Settings>('/settings')
  return data
}

/** 设置/清除接入 key（空串清除）。 */
export async function setApiKey(api_key: string): Promise<Settings> {
  const { data } = await api.post<Settings>('/settings/api-key', { api_key })
  return data
}

/** 设置设备绑定有效期（秒；0 表示永不过期）。 */
export async function setDeviceTtl(secs: number): Promise<Settings> {
  const { data } = await api.post<Settings>('/settings/device-ttl', {
    device_binding_ttl_secs: secs,
  })
  return data
}
