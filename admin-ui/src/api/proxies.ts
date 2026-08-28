import { api } from './client'

/** 代理池中的一条记录。 */
export interface SavedProxy {
  id: number
  label: string
  url: string
  created_at: number
  /** 当前有多少凭证正在使用这条代理。 */
  credential_count: number
}

/** 列出代理池中所有记录（附带每条代理的使用量）。 */
export async function listProxies(): Promise<SavedProxy[]> {
  const { data } = await api.get<SavedProxy[]>('/proxies')
  return data
}

/** 向代理池中添加一条新记录。 */
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
