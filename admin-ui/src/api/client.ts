import axios from 'axios'

export const PW_KEY = 'luban_admin_pw'
/** 没带密码的请求被 401 时派发，App 收到后重新拉鉴权状态。 */
export const UNAUTHORIZED_EVENT = 'luban:unauthorized'

export const getPw = () => localStorage.getItem(PW_KEY)
export const setPw = (pw: string) => localStorage.setItem(PW_KEY, pw)
export const clearPw = () => localStorage.removeItem(PW_KEY)

/** 全局 axios 实例：自动带上管理密码，401 时清除并回登录。 */
export const api = axios.create({
  baseURL: '/api',
  headers: { 'Content-Type': 'application/json' },
})

api.interceptors.request.use((cfg) => {
  const pw = getPw()
  // 请求头只能放 ASCII：中文等字符原样塞进去浏览器会直接抛错，所以统一百分号编码，后端解码后比对。
  if (pw) cfg.headers.Authorization = `Bearer ${encodeURIComponent(pw)}`
  return cfg
})

api.interceptors.response.use(
  (r) => r,
  (err) => {
    const status = err?.response?.status
    const url: string = err?.config?.url ?? ''
    // 已存密码却 401 → 密码失效：清除并回登录页（排除鉴权自身接口，避免登录报错时误刷）
    if (status === 401 && !url.startsWith('/auth/')) {
      if (getPw()) {
        clearPw()
        window.location.reload()
      } else {
        // 本地没有密码却被拒（本页打开后别处才设的密码）：不能重载了事，交给 App 去切登录页。
        window.dispatchEvent(new Event(UNAUTHORIZED_EVENT))
      }
    }
    return Promise.reject(err)
  },
)
