import { api } from './client'

export interface Settings {
  /** 当前接入 key（null = 未设置，不校验来访）。 */
  api_key: string | null
  /** 是否由环境变量接管（true 时网页只读）。 */
  env_managed: boolean
  /** 设备绑定有效期（秒）；0 表示永不过期。 */
  device_binding_ttl_secs: number
  /** 全局默认设备数上限；0 表示默认不限。账号未单独配置时套用它。 */
  default_device_limit: number
  /** 是否要求请求携带有效设备身份（metadata.user_id）；关闭后放行裸客户端。 */
  require_device_id: boolean
  /** 改写 metadata.user_id 里的 account_uuid/device_id 为凭证自洽身份。 */
  spoof_identity: boolean
  /** 给 x-anthropic-billing-header 补 cch（订阅模式独有字段）。 */
  billing_cch: boolean
  /** 补齐客户端未携带的 accept-encoding / anthropic-version / x-client-request-id。 */
  fill_client_headers: boolean
  /** 合并并按官方顺序重排 anthropic-beta（含塞入 oauth-2025-04-20）。 */
  merge_beta: boolean
  /** 把 system 对齐成官方订阅客户端的 4 块形态（拆块 + 断点全 1h + 基座 global）。 */
  system_shape: boolean
  /** 按官方拼写与顺序发出头名（Accept-Encoding 大写、anthropic-beta 小写…）。 */
  orig_header_case: boolean
  /** 上游拒绝 thinking 块签名时，把历史 thinking 降级成 text 后重试一次。 */
  thinking_signature_retry: boolean
}

/** 转发开关的键（与后端 ForwardFlags 字段同名）。 */
export type ForwardingKey =
  | 'spoof_identity'
  | 'billing_cch'
  | 'fill_client_headers'
  | 'merge_beta'
  | 'system_shape'
  | 'orig_header_case'
  | 'thinking_signature_retry'

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

/**
 * 改一个转发形态开关。只发生变化的那一项，后端不会动其余开关。
 */
export async function setForwarding(key: ForwardingKey, enabled: boolean): Promise<Settings> {
  const { data } = await api.post<Settings>('/settings/forwarding', { [key]: enabled })
  return data
}
