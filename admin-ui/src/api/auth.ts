import { api } from './client'

export interface AuthState {
  /** 是否已设置管理密码（true = 需登录）。 */
  configured: boolean
  /** 管理密码是否由环境变量接管（true = 网页不可改）。 */
  env_managed: boolean
  /** 未设密码：须先用启动日志里的初始化口令设置管理密码（本机访问也一样）。 */
  setup_required: boolean
  /** 能否用访客账号登录；登录页据此改文案。 */
  viewer_enabled: boolean
}

/** 鉴权状态（公开接口）。 */
export async function getAuthState(): Promise<AuthState> {
  const { data } = await api.get<AuthState>('/auth/state')
  return data
}

/**
 * 登录身份：
 * - admin：唯一的管理员，什么都能做；
 * - viewer：唯一的只读访客，全站只能看；
 * - agent：代理，管自己的号，能开挂在自己名下的用户；
 * - user：用户，只管自己的号。
 */
export type Role = 'admin' | 'viewer' | 'agent' | 'user'

export interface LoginResult {
  token: string
  role: Role
  username: string
}

/** 用户名 + 密码登录，回会话 token 与身份。 */
export async function login(username: string, password: string): Promise<LoginResult> {
  const { data } = await api.post<LoginResult>('/auth/login', { username, password })
  return data
}

/** 退出登录：作废当前会话。 */
export async function logout(): Promise<void> {
  await api.post('/auth/logout')
}

export interface Me {
  id: number
  username: string
  role: Role
  /** 管理密码是否由环境变量接管（仅管理员有意义）。 */
  admin_env_managed: boolean
  /** 是否已设访客密码（仅管理员看得到，其他人恒为 false）。 */
  viewer_configured: boolean
  /** 访客密码是否由环境变量接管（true = 网页不可改）。 */
  viewer_env_managed: boolean
}

/** 当前登录身份（已鉴权）。 */
export async function getMe(): Promise<Me> {
  const { data } = await api.get<Me>('/auth/me')
  return data
}

/** 设置/清除访客密码（空串=清除，仅管理员）。 */
export async function setViewerPassword(password: string): Promise<void> {
  await api.post('/auth/viewer-password', { password })
}

/** 首次设置管理密码，须带启动日志里的初始化口令（token）；成功即登录。 */
export async function setup(password: string, token: string): Promise<LoginResult> {
  const { data } = await api.post<LoginResult>('/auth/setup', { password, token })
  return data
}

/** 修改自己的密码（管理员传空串=清除管理密码，已鉴权）。 */
export async function changePassword(password: string): Promise<void> {
  await api.post('/auth/password', { password })
}
