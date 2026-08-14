import { api } from './client'

export interface Settings {
  /** 当前接入 key（null = 未设置，不校验来访）。 */
  api_key: string | null
  /** 是否由环境变量接管（true 时网页只读）。 */
  env_managed: boolean
  /** 设备绑定有效期（秒）；0 表示永不过期。 */
  device_binding_ttl_secs: number
  /** 软绑定保留期（秒）：超过有效期的绑定不再占名额，但这段时间内设备回来仍优先回原号。0 = 永久保留。 */
  device_binding_retention_secs: number
  /** 全局默认设备数上限；0 表示默认不限。账号未单独配置时套用它。 */
  default_device_limit: number
  /** 全局默认账号 RPM 上限（最近 60 秒最多转发多少条）；0 表示默认不限。账号未单独配置时套用它。 */
  default_rpm_limit: number
  /** 每设备 RPM 上限（单台设备最近 60 秒最多转发多少条）；0 表示不限。全局一个值。 */
  device_rpm_limit: number
  /** 是否要求请求携带有效设备身份（metadata.user_id）；关闭后放行裸客户端。 */
  require_device_id: boolean
  /** 允许接入的最低 Claude Code 版本；空串表示不限。只卡 UA 里自报 claude-cli/<版本> 的请求。 */
  min_client_version: string
  /** 单个账号在窗口内允许的裸请求条数；0 表示不限。 */
  bare_rate_limit: number
  /** 裸请求速率窗口（秒），默认 60。 */
  bare_rate_window_secs: number
  /** 上游 429 后最多追加尝试的账号数，不含首次请求；0 表示不重试。 */
  rate_limit_retry_max: number
  /** 改写 metadata.user_id 里的 account_uuid/device_id 为凭证自洽身份。 */
  spoof_identity: boolean
  /** 给 x-anthropic-billing-header 补 cch（订阅模式独有字段）。 */
  billing_cch: boolean
  /** 补齐客户端未携带的 accept-encoding / anthropic-version / x-client-request-id。 */
  fill_client_headers: boolean
  /** 合并并按官方顺序重排 anthropic-beta（含塞入 oauth-2025-04-20）。 */
  merge_beta: boolean
  /** 把 system 对齐成官方订阅客户端的 4 块形态（拆/并块 + 块数封顶 4）。 */
  system_shape: boolean
  /** 按官方拼写与顺序发出头名（Accept-Encoding 大写、anthropic-beta 小写…）。 */
  orig_header_case: boolean
  /** 上游拒绝 thinking 块签名时，把历史 thinking 降级成 text 后重试一次。 */
  thinking_signature_retry: boolean
  /** 非 Claude Code 客户端的请求，按官方抓包形态模拟成 CC 请求（注入 system 前缀 + 整套官方头）。 */
  simulate_cc: boolean
  /** 已是 CC 形态但不带 metadata.user_id 的请求，补一份官方形态的身份（含同值的会话 id 头）。 */
  fill_metadata: boolean
  /** 上游回 429 时按账号/模型范围冷却并换号重试；关闭时不冷却、直接透传。 */
  rate_limit_retry: boolean
  /** 官方基座那块的缓存断点带不带 scope:"global"（跨账号共享同一份基座缓存）。 */
  cache_scope_global: boolean
  /** 请求自带设备标识时，要不要换成当前账号派生的那个（spoof_identity 的子项）。 */
  spoof_device_id: boolean
  /** 缓存断点写不写 ttl:"1h"（对齐官方）；关闭则沿用客户端自己传的时长。 */
  cache_ttl_1h: boolean
  /** 非流式 /v1/messages 改成流式发给上游，再把 SSE 聚合回整段 JSON 给客户端。 */
  nonstream_as_sse: boolean
  /** 剥掉官方客户端从不发送的顶层字段（缺省语义的 tool_choice、thinking.display）。 */
  strip_extra_fields: boolean
  /** 把会被上游判成第三方应用的工具名换成假名转发，回程再还原。 */
  tool_name_mimic: boolean
}

/** 转发开关的键（与后端 ForwardFlags 字段同名）。 */
export type ForwardingKey =
  | 'spoof_identity'
  | 'spoof_device_id'
  | 'billing_cch'
  | 'fill_client_headers'
  | 'merge_beta'
  | 'system_shape'
  | 'orig_header_case'
  | 'thinking_signature_retry'
  | 'simulate_cc'
  | 'fill_metadata'
  | 'rate_limit_retry'
  | 'cache_scope_global'
  | 'cache_ttl_1h'
  | 'nonstream_as_sse'
  | 'strip_extra_fields'
  | 'tool_name_mimic'

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

/** 设置软绑定保留期（秒；0 表示永久保留）。 */
export async function setDeviceRetention(secs: number): Promise<Settings> {
  const { data } = await api.post<Settings>('/settings/device-retention', {
    device_binding_retention_secs: secs,
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

/** 设置全局默认账号 RPM 上限（0 表示默认不限）。 */
export async function setDefaultRpmLimit(limit: number): Promise<Settings> {
  const { data } = await api.post<Settings>('/settings/default-rpm-limit', {
    default_rpm_limit: limit,
  })
  return data
}

/** 设置每设备 RPM 上限（0 表示不限）：单台设备最近 60 秒最多转发多少条，超了直接 429。 */
export async function setDeviceRpmLimit(limit: number): Promise<Settings> {
  const { data } = await api.post<Settings>('/settings/device-rpm-limit', {
    device_rpm_limit: limit,
  })
  return data
}

/**
 * 设置裸请求速率上限：单个账号在窗口内最多接多少条无 metadata.user_id 的请求。
 * 0 表示不限；window 只在传正数时才写（不传就保持现值）。
 */
export async function setBareRateLimit(limit: number, windowSecs?: number): Promise<Settings> {
  const { data } = await api.post<Settings>('/settings/bare-rate-limit', {
    bare_rate_limit: limit,
    bare_rate_window_secs: windowSecs,
  })
  return data
}

/** 设置上游 429 后追加尝试的账号数（不含首次请求；0 = 不重试；后端夹到 0~10）。 */
export async function setRateLimitRetryMax(n: number): Promise<Settings> {
  const { data } = await api.post<Settings>('/settings/rate-limit-retry-max', {
    rate_limit_retry_max: n,
  })
  return data
}

/** 开关设备身份校验（关闭后放行无 metadata.user_id 的请求）。 */
export async function setRequireDeviceId(required: boolean): Promise<Settings> {
  const { data } = await api.post<Settings>('/settings/require-device-id', { required })
  return data
}

/**
 * 设置最低 Claude Code 版本（`2.1.220` / `2.1` / `2`；空串清除，即不限）。
 *
 * 只影响 UA 里自报 claude-cli/<版本> 的请求；写不成版本号的值后端直接 400。
 */
export async function setMinClientVersion(version: string): Promise<Settings> {
  const { data } = await api.post<Settings>('/settings/min-client-version', {
    min_client_version: version,
  })
  return data
}

/**
 * 改一个转发形态开关。只发生变化的那一项，后端不会动其余开关。
 */
export async function setForwarding(key: ForwardingKey, enabled: boolean): Promise<Settings> {
  const { data } = await api.post<Settings>('/settings/forwarding', { [key]: enabled })
  return data
}
