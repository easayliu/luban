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
  /** 可用全部号（没绑定分组）。为假时只能用 `groups`；绑定的分组被删光时一个号都不能用。 */
  all_groups: boolean
  /** 绑定的分组，按优先顺序。 */
  groups: number[]
}

export async function listApiKeys(): Promise<ApiKey[]> {
  const { data } = await api.get<ApiKey[]>('/api-keys')
  return data
}

/** 新建一把 Key，回明文。`allGroups` 为真时可用全部号（`groupIds` 被忽略）。 */
export async function createApiKey(
  label: string,
  groupIds: number[],
  allGroups: boolean,
): Promise<{ id: number; key: string }> {
  const { data } = await api.post<{ id: number; key: string }>('/api-keys', {
    label,
    group_ids: groupIds,
    all_groups: allGroups,
  })
  return data
}

/**
 * 改一把 Key。`all_groups` 不传则范围原样不动（只改名称与启停）；传了才按它与 `group_ids`
 * 改范围——绝不会因为分组列表为空就变成全部号。
 */
export async function updateApiKey(
  id: number,
  input: { label: string; disabled: boolean; all_groups?: boolean; group_ids?: number[] },
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

/** 一把上号 Key（不含明文）：脚本拿它只能走「添加账号」，上的号落在所属账号名下。 */
export interface ProvisionKey {
  id: number
  user_id: number
  /** 所属账号的用户名（admin 看全部时认人用）。 */
  username: string
  label: string
  /** 明文开头几位。 */
  prefix: string
  disabled: boolean
  created_at: number
  /** 最近一次被使用的时间；从没用过为 null。 */
  last_used_at: number | null
}

export async function listProvisionKeys(): Promise<ProvisionKey[]> {
  const { data } = await api.get<ProvisionKey[]>('/provision-keys')
  return data
}

/** 给自己新建一把上号 Key，回明文——只回这一次。 */
export async function createProvisionKey(label: string): Promise<{ id: number; key: string }> {
  const { data } = await api.post<{ id: number; key: string }>('/provision-keys', { label })
  return data
}

export async function updateProvisionKey(id: number, input: { label: string; disabled: boolean }): Promise<void> {
  await api.post(`/provision-keys/${id}`, input)
}

export async function deleteProvisionKey(id: number): Promise<void> {
  await api.delete(`/provision-keys/${id}`)
}
