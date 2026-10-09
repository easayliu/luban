import { api } from './client'

/** 一个号池分组。号数与开放名单只有管理员与访客看得到。 */
export interface PoolGroup {
  id: number
  name: string
  note: string
  /** 默认分组：对所有人开放、不能删。 */
  is_default: boolean
  created_at: number
  credential_count: number | null
  /** 开放给了谁（控制台账号 id）。 */
  grants: number[] | null
}

/** 列出分组：管理员与访客看全部，代理和用户只看开放给自己的。 */
export async function listGroups(): Promise<PoolGroup[]> {
  const { data } = await api.get<PoolGroup[]>('/groups')
  return data
}

export async function createGroup(name: string, note: string): Promise<number> {
  const { data } = await api.post<{ id: number }>('/groups', { name, note })
  return data.id
}

export async function updateGroup(id: number, name: string, note: string): Promise<void> {
  await api.post(`/groups/${id}`, { name, note })
}

export async function deleteGroup(id: number): Promise<void> {
  await api.delete(`/groups/${id}`)
}

/** 整体替换开放名单：代理（名下用户自动继承）或管理员直属的用户。 */
export async function setGroupGrants(id: number, userIds: number[]): Promise<void> {
  await api.post(`/groups/${id}/grants`, { user_ids: userIds })
}

/** 一把接入 Key（不含明文）。 */
export interface ApiKey {
  id: number
  label: string
  /** 明文开头几位。 */
  prefix: string
  disabled: boolean
  created_at: number
  /** 绑定的分组，按优先顺序；空 = 用全部号。 */
  groups: number[]
}

export async function listApiKeys(): Promise<ApiKey[]> {
  const { data } = await api.get<ApiKey[]>('/api-keys')
  return data
}

/** 新建一把 Key，回明文。 */
export async function createApiKey(label: string, groupIds: number[]): Promise<{ id: number; key: string }> {
  const { data } = await api.post<{ id: number; key: string }>('/api-keys', { label, group_ids: groupIds })
  return data
}

export async function updateApiKey(
  id: number,
  input: { label: string; disabled: boolean; group_ids: number[] },
): Promise<void> {
  await api.post(`/api-keys/${id}`, input)
}

export async function deleteApiKey(id: number): Promise<void> {
  await api.delete(`/api-keys/${id}`)
}

/** 取一把 Key 的明文。 */
export async function revealApiKey(id: number): Promise<string> {
  const { data } = await api.get<{ key: string }>(`/api-keys/${id}/reveal`)
  return data.key
}
