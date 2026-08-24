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
  /** 每会话 RPM 上限（单个会话最近 60 秒最多转发多少条）；0 表示不限。全局一个值。 */
  session_rpm_limit: number
  /** 是否要求请求携带有效设备身份（metadata.user_id）；关闭后放行裸客户端。 */
  require_device_id: boolean
  /** 允许接入的最低 Claude Code 版本；空串表示不限。只卡 UA 里自报 claude-cli/<版本> 的请求。 */
  min_client_version: string
  /** 登录时申请的 OAuth scope（空格分隔）；恒为非空，未自定义时就是 oauth_scopes_default。 */
  oauth_scopes: string
  /** 官方 Claude Code 那一整套 scope，也是未配置时的默认值。 */
  oauth_scopes_default: string
  /** 精简 scope：只留 Luban 自己用得上的三项（推理、profile、文件上传）。 */
  oauth_scopes_minimal: string
  /** 单个账号在窗口内允许的裸请求条数；0 表示不限。 */
  bare_rate_limit: number
  /** 裸请求速率窗口（秒），默认 60。 */
  bare_rate_window_secs: number
  /** 上游 429 后最多追加尝试的账号数，不含首次请求；0 表示不重试。 */
  rate_limit_retry_max: number
  /** 5h 窗口用到多少百分比就提前把账号挪出调度池；0 表示关闭（等真收到 429 才停）。 */
  quota_pause_pct: number
  /** 7d 窗口的同一档阈值，另算；0（默认）表示不按周用量停号。 */
  quota_pause_pct_7d: number
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
 * 设置每会话 RPM 上限（0 表示不限）：单个会话最近 60 秒最多转发多少条，超了直接 429。
 * 与设备 RPM 两个粒度并存，两道都该配——会话 id 换一个就是新桶，只有设备那道拦得住换 id。
 */
export async function setSessionRpmLimit(limit: number): Promise<Settings> {
  const { data } = await api.post<Settings>('/settings/session-rpm-limit', {
    session_rpm_limit: limit,
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

/**
 * 设置「额度用到多少就提前停调度」的阈值（百分比；0 = 关闭，后端夹到 0~100）。
 *
 * 5h 与 7d 是两档、各存各的：同一个百分比在两个窗口上的后果差着数量级（5h 停号歇几小时就
 * 回来，7d 停号是歇到下个周重置），故 7d 那档默认关。`pct7d` 不传就保持现值。
 *
 * 与 429 后的冷却同受「429 自动换号」总开关：那个关掉时本项不生效。
 */
export async function setQuotaPausePct(pct: number, pct7d?: number): Promise<Settings> {
  const { data } = await api.post<Settings>('/settings/quota-pause-pct', {
    quota_pause_pct: pct,
    quota_pause_pct_7d: pct7d,
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
 * 设置登录时申请的 OAuth scope（空格分隔；空串恢复默认那一整套）。
 *
 * 只影响之后新加的账号——已存下来的凭证按当初授权的范围来，改这里不会追溯。
 * 后端会校验字符集、条数，并要求含 `user:inference`（缺了 token 调不了 /v1/*）。
 */
export async function setOauthScopes(scopes: string): Promise<Settings> {
  const { data } = await api.post<Settings>('/settings/oauth-scopes', {
    oauth_scopes: scopes,
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

// ---------- 迁移：导出 / 导入 ----------

/**
 * 迁移文件：导出接口的响应，也是导入接口的入参（原样喂回去即可）。
 *
 * `credentials` 里**含明文 access/refresh token**——迁移要的就是它们，所以这份文件等同于
 * 全部账号本身，只该在你自己的机器之间传。
 */
export interface ExportFile {
  /** 恒为 'luban-export'，导入侧据此认文件。 */
  kind: string
  /** 文件格式版本。 */
  version: number
  /** 导出时刻（Unix 秒）。 */
  exported_at: number
  /** 导出该文件的 luban 版本。 */
  luban_version: string
  /** 全部账号（含明文 token）。 */
  credentials: unknown[]
  /** settings 全表，不含管理密码。 */
  settings: Record<string, string>
}

/** 导入模式：merge = 按账号身份覆盖同名号、其余保留；replace = 先清空目标库再导入。 */
export type ImportMode = 'merge' | 'replace'

/** 导入结果计数。 */
export interface ImportResult {
  /** 新增的账号数。 */
  added: number
  /** 覆盖了已有账号的条数。 */
  updated: number
  /** 导入失败的条数（原因在服务端日志里）。 */
  failed: number
  /** replace 模式下被清掉的原有账号数。 */
  cleared: number
  /** 实际写入的设置项数。 */
  settings_applied: number
}

/** 导出全部账号与设置。未设管理密码时服务端拒绝（文件含明文 token）。 */
export async function exportAll(): Promise<ExportFile> {
  const { data } = await api.get<ExportFile>('/export')
  return data
}

/** 导入账号（可选连设置一起）。 */
export async function importAll(
  payload: ExportFile,
  mode: ImportMode,
  importSettings: boolean,
): Promise<ImportResult> {
  const { data } = await api.post<ImportResult>('/import', {
    payload,
    mode,
    import_settings: importSettings,
  })
  return data
}
