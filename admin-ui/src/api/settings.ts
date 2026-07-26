import { api } from './client'

export interface Settings {
  /** 当前接入 key（null = 未设置，不校验来访）。 */
  api_key: string | null
  /** 是否由环境变量接管（true 时网页只读）。 */
  env_managed: boolean
  /** 设备绑定有效期（秒）；0 表示永不过期。 */
  device_binding_ttl_secs: number
  /** 是否对转发请求做身份伪装（改写 metadata.user_id 的 account_uuid/device_id）。 */
  spoof_identity_enabled: boolean
  /** 全局默认设备数上限；0 表示默认不限。账号未单独配置时套用它。 */
  default_device_limit: number
  /** 是否要求请求携带有效设备身份（metadata.user_id）；关闭后放行裸客户端。 */
  require_device_id: boolean
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

/** 设置全局默认设备数上限（0 表示默认不限）。 */
export async function setDefaultDeviceLimit(limit: number): Promise<Settings> {
  const { data } = await api.post<Settings>('/settings/default-device-limit', {
    default_device_limit: limit,
  })
  return data
}

/** 开关设备身份校验（关闭后放行无 metadata.user_id 的请求）。 */
export async function setRequireDeviceId(required: boolean): Promise<Settings> {
  const { data } = await api.post<Settings>('/settings/require-device-id', { required })
  return data
}

/** 开关身份伪装。 */
export async function setSpoofIdentity(enabled: boolean): Promise<Settings> {
  const { data } = await api.post<Settings>('/settings/spoof-identity', { enabled })
  return data
}
