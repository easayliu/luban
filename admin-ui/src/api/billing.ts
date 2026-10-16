import { api } from './client'

/** 账单按哪一维拆。 */
export type BillingDim = 'owner' | 'cred' | 'model' | 'key' | 'group' | 'day'

export interface BillingTotals {
  requests: number
  input_tokens: number
  output_tokens: number
  /** 缓存写合计 = 5 分钟档 + 1 小时档 + 未分档。 */
  cache_write_tokens: number
  cache_write_1h_tokens: number
  /** 分不出档的历史缓存写（升级前、流水对不上账单的部分），不算进任何一档。 */
  cache_write_unknown_tokens: number
  cache_read_tokens: number
  cost_usd: number
}

/** 账单的一行：`key` 是这一维的取值（id、模型名，或按日拆时那天本地零点的 Unix 秒）。 */
export interface BillingRow extends BillingTotals {
  key: string
  /** 显示名；已删的号 / 账号 / Key / 分组为 null。模型与日期由前端按 key 显示。 */
  label: string | null
}

export interface BillingResp {
  from: number
  to: number
  rows: BillingRow[]
  total: BillingTotals
}

export interface BillingParams {
  from: number
  to: number
  by: BillingDim
  owner_id?: number
}

export async function getBilling(params: BillingParams): Promise<BillingResp> {
  const { data } = await api.get<BillingResp>('/billing', {
    params: { ...params, tz_offset_secs: -new Date().getTimezoneOffset() * 60 },
  })
  return data
}
