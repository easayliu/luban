import { api } from './client'

/** 代理池中的一条记录。 */
export interface SavedProxy {
  id: number
  label: string
  url: string
  created_at: number
  /** 当前有多少凭证正在使用这条代理。 */
  credential_count: number
  /** 使用该代理的凭证标签列表。 */
  credential_labels: string[]
}

/** 代理测试结果。 */
export interface ProxyTestResult {
  ok: boolean
  ip: string | null
  country: string | null
  city: string | null
  region: string | null
  org: string | null
  latency_ms: number
  error: string | null
}

/** 列出代理池中所有记录（附带每条代理的使用量与使用者）。 */
export async function listProxies(): Promise<SavedProxy[]> {
  const { data } = await api.get<SavedProxy[]>('/proxies')
  return data
}

/** 测试代理连通性：通过代理访问 ip-api.com 获取出口 IP 和地理信息。 */
export async function testProxy(url: string): Promise<ProxyTestResult> {
  const { data } = await api.post<ProxyTestResult>('/proxies/test', { url })
  return data
}

/** 向代理池中添加一条新记录。`label` 留空时由后端按 host:port 自动命名。 */
export async function addProxy(label: string, url: string): Promise<SavedProxy> {
  const { data } = await api.post<SavedProxy>('/proxies', { label, url })
  return data
}

/** 更新代理池中一条记录的名称和/或地址。 */
export async function updateProxy(id: number, label: string, url: string): Promise<SavedProxy> {
  const { data } = await api.post<SavedProxy>(`/proxies/${id}`, { label, url })
  return data
}

/** 从代理池中删除一条记录（不影响已配置该地址的凭证）。 */
export async function deleteProxy(id: number): Promise<void> {
  await api.delete(`/proxies/${id}`)
}

/** 批量删除代理池记录（单事务，不影响已配置这些地址的凭证），返回实际删掉的条数。 */
export async function deleteProxies(ids: number[]): Promise<number> {
  const { data } = await api.post<{ deleted: number }>('/proxies/delete', { ids })
  return data.deleted
}

/** 批量导入里一条的结果，与入参按下标一一对应。 */
export interface BatchProxyItem {
  /** added：可导入（预览）/ 已导入；exists：池里已有；duplicate：与本批前面某条重复；invalid：地址不合法。 */
  status: 'added' | 'exists' | 'duplicate' | 'invalid'
  /** 归一化后的地址，invalid 时为 null。 */
  url: string | null
  /** 最终名称（留空时已自动起好），只有 added 才有。 */
  label: string | null
  error: string | null
  /** duplicate 时指向本批第一次出现这个地址的下标。 */
  duplicate_of: number | null
  /** 真正导入后的记录 id；预览时为 null。 */
  id: number | null
}

/**
 * 批量添加代理。`dryRun` 时只校验、归一化、查重、起名，不写库——导入前的预览就走它，
 * 与真正导入用的是同一套判据。
 */
export async function addProxies(
  items: { label: string; url: string }[],
  dryRun: boolean,
): Promise<BatchProxyItem[]> {
  const { data } = await api.post<{ items: BatchProxyItem[] }>('/proxies/batch', {
    items,
    dry_run: dryRun,
  })
  return data.items
}
