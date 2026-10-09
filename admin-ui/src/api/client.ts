import axios from 'axios'

/** 登录后拿到的会话 token。只存 token，不存密码。 */
export const TOKEN_KEY = 'luban_session'
/** 没带会话的请求被 401 时派发，App 收到后重新拉鉴权状态。 */
export const UNAUTHORIZED_EVENT = 'luban:unauthorized'

/** 上次登录认出的身份，只用来在 `/auth/me` 回来之前不闪一下按钮；真正的权限在后端。 */
export const ROLE_KEY = 'luban_role'

// 旧版把管理密码明文存在这里，换成会话 token 之后不再用，顺手清掉。
try { localStorage.removeItem('luban_admin_pw') } catch { /* 存储不可用时无事可做 */ }

export const getToken = () => localStorage.getItem(TOKEN_KEY)
export const setToken = (token: string) => localStorage.setItem(TOKEN_KEY, token)
export const clearToken = () => {
  localStorage.removeItem(TOKEN_KEY)
  localStorage.removeItem(ROLE_KEY)
}

/** 全局 axios 实例：自动带上会话 token，401 时清除并回登录。 */
export const api = axios.create({
  baseURL: '/api',
  headers: { 'Content-Type': 'application/json' },
})

api.interceptors.request.use((cfg) => {
  const token = getToken()
  if (token) cfg.headers.Authorization = `Bearer ${token}`
  return cfg
})

api.interceptors.response.use(
  (r) => r,
  (err) => {
    const status = err?.response?.status
    const url: string = err?.config?.url ?? ''
    // 已存会话却 401 → 会话失效（过期、被停用、改了密码）：清除并回登录页
    // （排除登录 / 初始化接口自身，避免登录报错时误刷）。
    const authEndpoint = url.startsWith('/auth/login') || url.startsWith('/auth/setup') || url.startsWith('/auth/state')
    if (status === 401 && !authEndpoint) {
      if (getToken()) {
        clearToken()
        window.location.reload()
      } else {
        // 本地没有会话却被拒（本页打开后别处才设的密码）：不能重载了事，交给 App 去切登录页。
        window.dispatchEvent(new Event(UNAUTHORIZED_EVENT))
      }
    }
    return Promise.reject(err)
  },
)
