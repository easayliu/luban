import { api } from './client'

/** 控制台账号（代理或用户）。 */
export interface ConsoleUser {
  id: number
  username: string
  role: 'agent' | 'user'
  parent_id: number | null
  /** 上级用户名（admin 或代理）。 */
  parent_username: string | null
  /** 自己这一行的停用标记。 */
  disabled: boolean
  /** 上级被停用而连带停用。 */
  parent_disabled: boolean
  password_set: boolean
  created_at: number
  updated_at: number
  /** 名下号数。 */
  credential_count: number
  /** 名下下属用户数（只有代理有）。 */
  child_count: number
}

/** 列出账号：管理员看全部代理和用户，代理只看自己名下的用户。 */
export async function listUsers(): Promise<ConsoleUser[]> {
  const { data } = await api.get<ConsoleUser[]>('/users')
  return data
}

export interface CreateUserInput {
  username: string
  password: string
  /** 代理开的只能是 user。 */
  role?: 'agent' | 'user'
  /** 用户挂在谁名下（管理员开用户时可指定某个代理），缺省管理员自己。 */
  parent_id?: number
}

export async function createUser(input: CreateUserInput): Promise<ConsoleUser> {
  const { data } = await api.post<ConsoleUser>('/users', input)
  return data
}

export async function resetUserPassword(id: number, password: string): Promise<void> {
  await api.post(`/users/${id}/password`, { password })
}

export async function setUserDisabled(id: number, disabled: boolean): Promise<void> {
  await api.post(`/users/${id}/disabled`, { disabled })
}

/** 把用户转到另一个代理（或管理员）名下，仅管理员。 */
export async function setUserParent(id: number, parentId: number): Promise<void> {
  await api.post(`/users/${id}/parent`, { parent_id: parentId })
}

export async function deleteUser(id: number): Promise<void> {
  await api.delete(`/users/${id}`)
}
