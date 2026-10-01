import { api } from './client'

export interface AuthState {
  /** 是否已设置管理密码（true = 需登录）。 */
  configured: boolean
  /** 是否由环境变量接管（true = 网页不可改）。 */
  env_managed: boolean
  /** 未设密码：须先用启动日志里的初始化口令设置管理密码（本机访问也一样）。 */
  setup_required: boolean
  /** 能否用访客密码登录（已设且生效）；登录页据此改文案。 */
  viewer_enabled: boolean
}

/** 鉴权状态（公开接口）。 */
export async function getAuthState(): Promise<AuthState> {
  const { data } = await api.get<AuthState>('/auth/state')
  return data
}

/** 登录身份：管理员什么都能做；访客只能看，改动类接口一律 403。 */
export type Role = 'admin' | 'viewer'

/** 校验登录密码，回这个密码对应的身份。 */
export async function login(password: string): Promise<Role> {
  const { data } = await api.post<{ ok: boolean; role?: Role }>('/auth/login', { password })
  return data.role ?? 'admin'
}

export interface Me {
  role: Role
  /** 是否已设访客密码（仅管理员看得到，访客恒为 false）。 */
  viewer_configured: boolean
  /** 设了却没生效：与管理密码互为编码，或升级前的管理密码还没重新校验过（管理员登录一次即可）。 */
  viewer_inactive: boolean
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

/** 首次设置管理密码，须带启动日志里的初始化口令（token）。 */
export async function setup(password: string, token: string): Promise<void> {
  await api.post('/auth/setup', { password, token })
}

/** 修改/清除管理密码（空串=清除，已鉴权）。 */
export async function changePassword(password: string): Promise<void> {
  await api.post('/auth/password', { password })
}
