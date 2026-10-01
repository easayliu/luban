import { useQuery } from '@tanstack/react-query'
import { getMe, type Me, type Role } from '@/api/auth'
import { ROLE_KEY, getPw } from '@/api/client'

function roleHint(): Role | null {
  try {
    const v = localStorage.getItem(ROLE_KEY)
    return v === 'admin' || v === 'viewer' ? v : null
  } catch {
    return null
  }
}

/** 登录成功后记下身份，见 [ROLE_KEY]。 */
export function rememberRole(role: Role) {
  try { localStorage.setItem(ROLE_KEY, role) } catch { /* 存不下只是会闪一下 */ }
}

/**
 * 当前登录身份。各处直接调这个 hook，React Query 按 key 去重，只发一次请求。
 *
 * 没做成 Context：登录页走完不重载页面，挂在根上的 Provider 拿不到「刚登录」这一刻；
 * hook 在登录后才挂载的组件里调，那时密码已经存好，查询自然就发出去了。
 */
export function useMe() {
  return useQuery({
    queryKey: ['auth-me'],
    queryFn: getMe,
    enabled: !!getPw(),
    staleTime: Infinity,
    placeholderData: (): Me | undefined => {
      const role = roleHint()
      return role
        ? { role, viewer_configured: false, viewer_inactive: false, viewer_env_managed: false }
        : undefined
    },
  })
}

/**
 * 只读访客：页面照常看，改动类的按钮、开关、表单一律藏掉或禁用。后端对访客的写请求回 403，
 * 这里藏按钮只是为了别让人点了才知道不行。
 *
 * **身份不明时按只读算**：`/auth/me` 重试耗尽后占位数据会被撤掉、`data` 变回 undefined，
 * 按「不是访客」算的话访客眼前会重新冒出写按钮和系统设置。退回登录时记下的身份，连那个也
 * 没有（升级前登录、至今没重新登录过的管理员）就先只读，等身份查回来再放开。
 */
export function useReadOnly(): boolean {
  const role = useMe().data?.role ?? roleHint()
  return role !== 'admin'
}

/**
 * 服务端**确认过**的访客（`/auth/me` 的真数据，不是占位、不是本地记下的身份）。
 *
 * 只读判断（[useReadOnly]）身份不明时保守按只读，藏按钮无妨；但「把人送走」这类不可逆的
 * 动作（清掉设置页的深链接）只能对确认过的访客做，否则旧管理员打开书签、还没查回身份就被
 * 送回账号池，链接也丢了。
 */
export function useConfirmedViewer(): boolean {
  const me = useMe()
  return !me.isPlaceholderData && me.data?.role === 'viewer'
}
