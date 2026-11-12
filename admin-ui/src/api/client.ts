import axios from 'axios'

/** 登录后拿到的会话 token。只存 token，不存密码。 */
export const TOKEN_KEY = 'luban_session'
/** 没带会话的请求被 401 时派发，App 收到后重新拉鉴权状态。 */
export const UNAUTHORIZED_EVENT = 'luban:unauthorized'

/** 上次登录认出的身份，只用来在 `/auth/me` 回来之前不闪一下按钮；真正的权限在后端。 */
export const ROLE_KEY = 'luban_role'

/** 分享链接凭据的前缀（同 `auth.ts` 的 SHARE_KEY_PREFIX；这里不 import 它，免得两个模块互相引用）。 */
export const SHARE_TOKEN_PREFIX = 'lbs-'
/** 分享链接失效被退出时，在 sessionStorage 里留的记号，登录页读一次即删。 */
export const SHARE_ENDED_KEY = 'luban_share_ended'

// 旧版把管理密码明文存在这里，换成会话 token 之后不再用，顺手清掉。
try { localStorage.removeItem('luban_admin_pw') } catch { /* 存储不可用时无事可做 */ }

/**
 * 只在本标签页生效的分享凭据（sessionStorage）。本浏览器已有正式登录时，打开分享链接不去动
 * localStorage 里那份——否则别的标签页收到 storage 事件一齐刷新，管理员点开自己发的链接
 * 试一下，全部标签页都被换成了访客。见 [setShareToken]。
 */
const SHARE_SESSION_KEY = 'luban_share_session'

function tabShareToken(): string | null {
  try { return sessionStorage.getItem(SHARE_SESSION_KEY) } catch { return null }
}

/** 当前标签页用的会话：本标签页的分享凭据优先，其次是全浏览器共用的那份。 */
export const getToken = () => tabShareToken() ?? localStorage.getItem(TOKEN_KEY)
export const setToken = (token: string) => localStorage.setItem(TOKEN_KEY, token)

/**
 * 存下分享链接里的凭据：本浏览器没有正式登录（或原本就是分享访客）时照常存进 localStorage，
 * 多开标签页都认；已有正式登录时只存进本标签页的 sessionStorage，别处的登录原样不动。
 */
export function setShareToken(token: string) {
  const existing = localStorage.getItem(TOKEN_KEY)
  if (existing && !existing.startsWith(SHARE_TOKEN_PREFIX)) {
    try {
      sessionStorage.setItem(SHARE_SESSION_KEY, token)
      return
    } catch { /* sessionStorage 不可用就退回下面的做法 */ }
  }
  try { sessionStorage.removeItem(SHARE_SESSION_KEY) } catch { /* 无事可做 */ }
  localStorage.setItem(TOKEN_KEY, token)
  localStorage.setItem(ROLE_KEY, 'viewer')
}

/** 清掉当前标签页用的会话：本标签页的分享凭据只清它自己，不连带全浏览器共用的登录。 */
export const clearToken = () => {
  if (tabShareToken() != null) {
    try { sessionStorage.removeItem(SHARE_SESSION_KEY) } catch { /* 无事可做 */ }
    return
  }
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
      const token = getToken()
      if (token) {
        clearToken()
        // 分享链接到期或被删：记一笔，登录页据此说明为什么回到了这里（访客手里没有密码）。
        // 只在清完之后已无任何登录、确实要进登录页时才记：本标签页的分享凭据清掉后若退回到
        // 本浏览器的正式登录，这个记号没人读，会一直留着，日后真进登录页时误报。
        if (token.startsWith(SHARE_TOKEN_PREFIX) && !getToken()) {
          try { sessionStorage.setItem(SHARE_ENDED_KEY, '1') } catch { /* 存不下只是少一句说明 */ }
        }
        window.location.reload()
      } else {
        // 本地没有会话却被拒（本页打开后别处才设的密码）：不能重载了事，交给 App 去切登录页。
        window.dispatchEvent(new Event(UNAUTHORIZED_EVENT))
      }
    }
    return Promise.reject(err)
  },
)
