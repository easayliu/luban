import { useQuery } from '@tanstack/react-query'
import { getMe, type Me, type Role } from '@/api/auth'
import { ROLE_KEY, getToken } from '@/api/client'

const ROLES: readonly Role[] = ['admin', 'viewer', 'agent', 'user']

function roleHint(): Role | null {
  try {
    const v = localStorage.getItem(ROLE_KEY)
    return ROLES.includes(v as Role) ? (v as Role) : null
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
 * hook 在登录后才挂载的组件里调，那时会话已经存好，查询自然就发出去了。
 */
export function useMe() {
  return useQuery({
    queryKey: ['auth-me'],
    queryFn: getMe,
    enabled: !!getToken(),
    staleTime: Infinity,
    placeholderData: (): Me | undefined => {
      const role = roleHint()
      return role
        ? {
            id: 0,
            username: '',
            role,
            admin_env_managed: false,
            viewer_configured: false,
            viewer_env_managed: false,
          }
        : undefined
    },
  })
}

/** 当前身份：查回来的为准，没查回来退回登录时记下的；都没有为 null。 */
export function useRole(): Role | null {
  return useMe().data?.role ?? roleHint()
}

/**
 * 只读访客：页面照常看，改动类的按钮、开关、表单一律藏掉或禁用。后端对访客的写请求回 403，
 * 这里藏按钮只是为了别让人点了才知道不行。代理和用户不是只读：他们能管自己名下的号。
 *
 * **身份不明时按只读算**：`/auth/me` 重试耗尽后占位数据会被撤掉、`data` 变回 undefined，
 * 按「不是访客」算的话访客眼前会重新冒出写按钮。退回登录时记下的身份，连那个也没有就先只读，
 * 等身份查回来再放开。
 */
export function useReadOnly(): boolean {
  const role = useRole()
  return role === null || role === 'viewer'
}

/** 管理员：系统设置、接入 Key 这类全站的东西只有它能动。 */
export function useIsAdmin(): boolean {
  return useRole() === 'admin'
}

/**
 * 看得到全池数据（实时指标、请求查询）的身份：管理员与访客。代理和用户只看得到
 * 自己名下的号，全池的东西后端回 403，界面上直接不出现。
 */
export function useSeesWholePool(): boolean {
  const role = useRole()
  return role === 'admin' || role === 'viewer'
}

/** 能进「用户管理」：管理员与代理。 */
export function useCanManageUsers(): boolean {
  const role = useRole()
  return role === 'admin' || role === 'agent'
}

/**
 * 服务端**确认过**的身份（`/auth/me` 的真数据，不是占位、不是本地记下的身份）。
 *
 * 「把人送走」这类不可逆的动作（清掉设置页的深链接）只能对确认过的身份做，否则旧管理员
 * 打开书签、还没查回身份就被送回账号池，链接也丢了。
 */
export function useConfirmedRole(): Role | null {
  const me = useMe()
  return me.isPlaceholderData ? null : (me.data?.role ?? null)
}
