import { createContext, useContext, useEffect, type ReactNode } from 'react'
import { useQuery } from '@tanstack/react-query'
import { getMe, type Me, type MemberCaps, type Role } from '@/api/auth'
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
            member_caps: null,
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
  const forced = useContext(ForcedReadOnly)
  const role = useRole()
  return forced || role === null || role === 'viewer'
}

const ForcedReadOnly = createContext(false)

/**
 * 把一棵子树按只读算（[useReadOnly] 在里面一律为真）：号的 `editable` 为假（代理看下属用户的号）
 * 时包在那一行 / 那张卡 / 详情页外面，菜单、开关、对话框里的写按钮跟着藏掉，不必逐个控件传参。
 */
export function ReadOnlyScope({ readOnly, children }: { readOnly: boolean; children: ReactNode }) {
  const outer = useContext(ForcedReadOnly)
  return <ForcedReadOnly.Provider value={outer || readOnly}>{children}</ForcedReadOnly.Provider>
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

/**
 * 代理和用户改自己号的调度参数时能到的边（见 [MemberCaps]）；管理员与访客为 null（不受限）。
 * 界面据此收窄可选项，免得点了保存才收到 403。
 *
 * **单独一个查询，不用 [useMe] 那份**：身份缓存是 `staleTime: Infinity`（身份在会话期间不变），
 * 而这几个边跟着全局设置走，管理员一改就变。
 *
 * **`active` 由编辑控件传「此刻是否打开」**：对话框关着时组件也常驻挂载，`staleTime` 只把
 * 数据标成过期、不会自己去取，全局又关了切回标签页时重取——只靠这两样，管理员放宽之后号主
 * 重开对话框看到的还是旧的边。所以在打开的那一刻（数据已过期时）主动重取一次；切回标签页
 * 时这个查询也单独开了重取。
 *
 * 默认 `false`：账号列表每一行的菜单、详情页的优先级下拉也在用它（只读 P2 这一档，不会变），
 * 那些地方挂载时 React Query 自己会按过期重取，不必再各自主动取。
 */
export function useMemberCaps(active = false): MemberCaps | null {
  const me = useMe().data
  const role = me?.role ?? roleHint()
  const member = role === 'agent' || role === 'user'
  const { data, isStale, refetch } = useQuery({
    queryKey: ['member-caps'],
    queryFn: async () => (await getMe()).member_caps ?? null,
    enabled: member && !!getToken(),
    staleTime: 10_000,
    refetchOnWindowFocus: true,
  })
  // 只在「打开」这一下判一次；把 isStale 也放进依赖的话，开着的对话框每 10 秒就重取一次。
  // `cancelRefetch: false`：已有一次在途就跟着它，不取消重发——默认会取消在途的那次再发一次，
  // 而被取消的 HTTP 请求其实照跑，几个控件同时打开就是几倍的请求。
  useEffect(() => {
    if (active && member && isStale) void refetch({ cancelRefetch: false })
  }, [active, member]) // eslint-disable-line react-hooks/exhaustive-deps
  // 还没取回来时先用登录时那份，免得刚打开时选项先放开、再收回去。
  return member ? (data ?? me?.member_caps ?? null) : null
}

/**
 * 三态上限（跟随默认 / 不限 / 独立上限）在天花板 `cap` 下的约束：`cap` 为 null 或 0 不设边。
 * 有边时「不限」不能选、自定义值最大到 `cap`。管理员给的宽限（这个号现在就是「不限」，或
 * 独立上限高过 `cap`）例外：原样保留不算放宽，后端不写也不拦，所以「不限」照留、输入框的
 * 上限放到现值（`max`），否则一打开编辑就被改成别的、保存时把宽限悄悄收掉。介于 `cap`
 * 与现值之间的数后端会拒。批量设置不传 `current`。
 */
export function limitCap(
  cap: number | null | undefined,
  current?: number,
): { allowUnlimited: boolean; cap?: number; max?: number } {
  if (!cap) return { allowUnlimited: true }
  return { allowUnlimited: (current ?? 0) < 0, cap, max: Math.max(cap, current ?? 0) }
}
