import { api } from './client'

/** 一条用量日志（与后端 store::UsageLog 对应）。 */
export interface UsageLog {
  id: number
  ts: number
  cred_id: number | null
  cred_label: string
  device_id: string | null
  model: string | null
  path: string
  status: number
  has_usage: boolean
  input_tokens: number | null
  output_tokens: number | null
  cache_creation_tokens: number | null
  /** 缓存写细分：5 分钟 / 1 小时档。 */
  cache_5m_tokens: number | null
  cache_1h_tokens: number | null
  cache_read_tokens: number | null
  ttft_ms: number | null
  total_ms: number | null
  /** 订阅账号限流（anthropic-ratelimit-unified-*）。 */
  unified_status: string | null
  rl_5h_status: string | null
  rl_5h_reset: number | null
  rl_5h_utilization: number | null
  rl_7d_status: string | null
  rl_7d_reset: number | null
  rl_7d_utilization: number | null
  rl_representative: string | null
  ratelimit_raw: string | null
  /** 等价 API 费用（USD）。 */
  cost_usd: number | null
}

/** 拉取最近的用量日志（按时间倒序）。 */
export async function listUsage(limit = 200): Promise<UsageLog[]> {
  const { data } = await api.get<UsageLog[]>('/usage', { params: { limit } })
  return data
}
