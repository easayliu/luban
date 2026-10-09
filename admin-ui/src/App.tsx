import { Suspense, lazy, useCallback, useEffect, useRef, useState } from 'react'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { PlusIcon, SearchIcon, ShieldAlertIcon } from 'lucide-react'
import { listCredentials } from '@/api/credentials'
import { getAuthState, logout } from '@/api/auth'
import { getSettings } from '@/api/settings'
import { TOKEN_KEY, UNAUTHORIZED_EVENT, getToken, setToken, clearToken } from '@/api/client'
import { numberOneOf, oneOf, usePersisted } from '@/lib/persisted'
import {
  SORT_DIR_DEFAULT,
  SORT_KEYS,
  type SortDir,
  type SortKey,
} from '@/components/credential-shared'
import {
  CREDENTIAL_FILTER_KEYS,
  CREDENTIAL_TIER_FILTER_KEYS,
  CREDENTIAL_PAGE_SIZES,
  CREDENTIAL_VIEW_MODES,
  CredentialWorkspace,
  preferredInitialCredentialView,
  type CredentialFilterKey,
  type CredentialTierFilterKey,
  type CredentialPageSize,
  type CredentialViewMode,
} from '@/components/credential-workspace'
import { AddAccount } from '@/components/add-account'
import { CredentialDetailPage } from '@/components/credential-detail-page'
import { RequestLookupDialog } from '@/components/request-lookup-dialog'
import { BanEventsDialog } from '@/components/ban-events-dialog'
import type { SettingsSection } from '@/components/settings-page'
import { LoginPage } from '@/components/login-page'
import { SetupPage } from '@/components/setup-page'
import { UsersPage } from '@/components/users-page'
import { BillingPage } from '@/components/billing-page'
import { AppFooter } from '@/components/app-footer'
import {
  AccountMenu,
  AppHeader,
  HEADER_ACTION_CLASS,
  MainNav,
  scrollToTop,
  type MainSection,
} from '@/components/app-header'
import { Button } from '@/components/ui/button'
import { Hint } from '@/components/ui/tooltip'
import { Skeleton } from '@/components/ui/skeleton'
import { MenuItem } from '@/components/ui/menu'
import { useI18n } from '@/lib/i18n'
import {
  rememberRole,
  useCanManageUsers,
  useConfirmedRole,
  useIsAdmin,
  useReadOnly,
  useSeesWholePool,
} from '@/lib/role'

// 设置页是另一棵大树（访问控制、转发、设备三块），账号页从不用它，
// 拆成单独 chunk 后首屏少解析一截。但 chunk 有 120 多 KB，远程访问时点进去要先白屏
// 等下载——所以账号页一空闲就把它预取回来（见 `useSettingsPrefetch`），点击时只剩挂载。
// 浏览器会缓存同一 specifier 的 import() 结果，重复调用不会再次请求。
const loadSettingsPage = () => import('@/components/settings-page')
const SettingsPage = lazy(() => loadSettingsPage().then((m) => ({ default: m.SettingsPage })))

/** 设置页 chunk 到达前的占位：保留与设置页同构的顶栏和标题骨架，切换时页面不至于整块变白。 */
function SettingsPageFallback({ onNavigate }: { onNavigate: (section: MainSection) => void }) {
  const { t } = useI18n()
  return (
    <div className="app-shell flex min-h-dvh flex-col text-foreground">
      <AppHeader
        actions={<AccountMenu />}
        nav={<MainNav current="settings" onNavigate={onNavigate} />}
        onNavigateHome={() => onNavigate('pool')}
      />
      <main aria-busy="true" className="page-frame flex-1 py-5 sm:py-8">
        <div className="max-w-2xl">
          <h1 className="text-xl font-semibold tracking-tight sm:text-2xl">
            {t('系统设置', 'System settings')}
          </h1>
          <Skeleton className="mt-3 h-4 w-3/4" />
        </div>
        <div className="mt-5 flex gap-8 sm:mt-7">
          <div className="hidden w-60 shrink-0 space-y-2 lg:block">
            {Array.from({ length: 6 }, (_, i) => <Skeleton className="h-15 w-full rounded-lg" key={i} />)}
          </div>
          <div className="min-w-0 flex-1 space-y-4">
            <Skeleton className="h-9 w-2/5" />
            <Skeleton className="h-40 w-full rounded-xl" />
            <Skeleton className="h-28 w-full rounded-xl" />
          </div>
        </div>
      </main>
    </div>
  )
}

/**
 * 账号页就位后趁空闲预取设置页：chunk 与 `GET /api/settings` 一起拉，点进去两段等待都免了。
 * 只在已通过鉴权后做——settings 是受保护接口，登录页阶段发出去只会得到 401。
 */
function useSettingsPrefetch(ready: boolean) {
  const qc = useQueryClient()
  useEffect(() => {
    if (!ready) return
    const run = () => {
      void loadSettingsPage()
      // 每次从设置页回到账号页 ready 都会再翻一次 true；一分钟内的预取结果直接复用，别重复打接口。
      void qc.prefetchQuery({ queryKey: ['settings'], queryFn: getSettings, staleTime: 60_000 })
    }
    // Safari 没有 requestIdleCallback，退回一个短延时，别抢首屏渲染。
    if (typeof window.requestIdleCallback === 'function') {
      const id = window.requestIdleCallback(run, { timeout: 2000 })
      return () => window.cancelIdleCallback(id)
    }
    const id = setTimeout(run, 800)
    return () => clearTimeout(id)
  }, [ready, qc])
}

/**
 * 账号页的检索条件同时写进 hash（`#/?filter=attention&sort=rpm…`），
 * 这样「我这边看到的这一屏」可以直接把地址发出去；本机偏好仍留在 localStorage 作兜底。
 *
 * 一律用 replaceState：筛选不是导航，逐次入栈会让后退键变成撤销筛选，
 * 用户想退回的是上一个页面。
 */
function readViewParams(): URLSearchParams {
  const hash = window.location.hash
  const start = hash.indexOf('?')
  return new URLSearchParams(start >= 0 ? hash.slice(start + 1) : '')
}

// 与 settings-page 的 SettingsSection 保持一致；那边是懒加载的，不从那里 import 以免把它拉进首包。
const SETTINGS_SECTIONS: readonly SettingsSection[] = ['access', 'groups', 'devices', 'proxies', 'forwarding', 'security', 'migration']

function readSettingsRoute(): SettingsSection | null {
  const match = /^#\/settings(?:\/([^/?]+))?/.exec(window.location.hash)
  if (!match) return null
  const section = match[1] as SettingsSection | undefined
  // 兼容旧的 #/settings 深链接；认不出的分区也回到「客户端接入」。
  return section && SETTINGS_SECTIONS.includes(section) ? section : 'access'
}

/** 与账号池平级的一级页面（`#/users`、`#/billing`）。 */
type MainPage = 'users' | 'billing'

function readMainRoute(): MainPage | null {
  const match = /^#\/(users|billing)(?:[/?]|$)/.exec(window.location.hash)
  return match ? (match[1] as MainPage) : null
}

/** `#/accounts/<id>` → 账号 id；其余地址不是详情页。 */
function readAccountRoute(): number | null {
  const match = /^#\/accounts\/(\d+)/.exec(window.location.hash)
  return match ? Number(match[1]) : null
}

function App() {
  const { t } = useI18n()
  const queryClient = useQueryClient()
  const [adding, setAdding] = useState(false)
  const [lookupOpen, setLookupOpen] = useState(false)
  const [bansOpen, setBansOpen] = useState(false)
  const [mainRoute, setMainRoute] = useState<MainPage | null>(readMainRoute)
  // 同设置页：从账号页点进来的，返回时消费 history；深链接直接打开的原地替换回账号页。
  const enteredMainFromAccounts = useRef(false)
  const [settingsRoute, setSettingsRoute] = useState<SettingsSection | null>(readSettingsRoute)
  const [session, setSession] = useState<string | null>(getToken())
  const [selected, setSelected] = useState<Set<number>>(new Set())
  // 分页（纯前端切片：列表接口一次返回全部账号）。
  const [page, setPage] = useState(1)
  // 只有从账号页主动进入设置时，关闭设置才应该消费这条 history 记录。
  // 直接打开 #/settings/* 的深链接则在原地替换回账号页，避免把用户带离当前站点。

  const [accountRoute, setAccountRoute] = useState<number | null>(readAccountRoute)
  // 同设置页：从账号列表点进详情才消费 history（返回＝后退），深链接直接打开的原地替换回列表。
  // 详情入口是 `<a href>`，由浏览器自己压栈，所以「是不是从列表进来的」在 hashchange 里判断。
  const enteredAccountFromList = useRef(false)
  // 进详情前列表滚到了哪儿，回来时还原——否则从第三页底部点进去，回来就被扔回顶部。
  const listScrollY = useRef<number | null>(null)
  const onList = useRef(false)
  onList.current = !settingsRoute && !mainRoute && accountRoute == null

  // 界面偏好与检索条件都写入 localStorage，刷新后保持当前工作上下文；
  // 链接里带了同名参数时以链接为准（见 readViewParams）。
  const seed = useRef(readViewParams()).current
  const [sort, setSort] = usePersisted<SortKey>(
    'sort', 'priority', oneOf(SORT_KEYS), String, seed.get('sort'),
  )
  const [dir, setDir] = usePersisted<SortDir>(
    'sortDir', 'asc', oneOf(['asc', 'desc'] as const), String, seed.get('dir'),
  )
  const [pageSize, setPageSize] = usePersisted<CredentialPageSize>(
    'pageSize',
    CREDENTIAL_PAGE_SIZES[0],
    numberOneOf(CREDENTIAL_PAGE_SIZES) as (raw: string) => CredentialPageSize | null,
    String,
    seed.get('size'),
  )
  const [view, switchView] = usePersisted<CredentialViewMode>(
    'view',
    preferredInitialCredentialView(),
    oneOf(CREDENTIAL_VIEW_MODES),
    String,
    seed.get('view'),
  )
  const [filter, setFilter] = usePersisted<CredentialFilterKey>(
    'filter',
    'all',
    oneOf(CREDENTIAL_FILTER_KEYS),
    String,
    seed.get('filter'),
  )
  const [tier, setTier] = usePersisted<CredentialTierFilterKey>(
    'tier',
    'all',
    oneOf(CREDENTIAL_TIER_FILTER_KEYS),
    String,
    seed.get('tier'),
  )
  const [query, setQuery] = usePersisted('query', '', (raw) => raw, String, seed.get('q'))
  // 页码只认链接，不进 localStorage：下次打开该从第一页看起。
  const initialPage = Number(seed.get('page'))
  useEffect(() => {
    if (Number.isInteger(initialPage) && initialPage > 1) setPage(initialPage)
    // 只在首屏消费一次链接里的页码。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [])

  useEffect(() => {
    if (settingsRoute || mainRoute || accountRoute != null) return
    const params = new URLSearchParams()
    if (query.trim()) params.set('q', query.trim())
    if (filter !== 'all') params.set('filter', filter)
    if (tier !== 'all') params.set('tier', tier)
    if (sort !== 'priority') params.set('sort', sort)
    if (dir !== SORT_DIR_DEFAULT[sort]) params.set('dir', dir)
    if (pageSize !== CREDENTIAL_PAGE_SIZES[0]) params.set('size', String(pageSize))
    if (page > 1) params.set('page', String(page))
    params.set('view', view)
    const next = `${window.location.pathname}${window.location.search}#/?${params.toString()}`
    if (window.location.href.endsWith(`#/?${params.toString()}`)) return
    window.history.replaceState(null, '', next)
  }, [query, filter, tier, sort, dir, view, page, pageSize, settingsRoute, mainRoute, accountRoute])
  useEffect(() => {
    const syncRoute = () => {
      const next = readSettingsRoute()
      const nextAccount = readAccountRoute()
      // 点一次链接 popstate 与 hashchange 会各来一遍；第一遍处理完就把 onList 放下，
      // 否则第二遍会把已经滚回顶部的 0 当成列表位置记下来。
      if (nextAccount != null && onList.current) {
        onList.current = false
        enteredAccountFromList.current = true
        listScrollY.current = window.scrollY
        window.scrollTo({ top: 0, behavior: 'instant' })
      }
      setSettingsRoute(next)
      setAccountRoute(nextAccount)
      const nextMain = readMainRoute()
      setMainRoute(nextMain)
      if (!nextMain && !next) enteredMainFromAccounts.current = false

      if (nextAccount == null) enteredAccountFromList.current = false
    }
    window.addEventListener('popstate', syncRoute)
    window.addEventListener('hashchange', syncRoute)
    return () => {
      window.removeEventListener('popstate', syncRoute)
      window.removeEventListener('hashchange', syncRoute)
    }
  }, [])

  // 设置页里切分区：同一页内的 Tab，不新增浏览器历史。
  const openSettings = (section: SettingsSection) => {
    window.history.replaceState(null, '', `#/settings/${section}`)
    setSettingsRoute(section)
    window.scrollTo({ top: 0, behavior: 'instant' })
  }
  const closeMain = () => {
    setMainRoute(null)
    setSettingsRoute(null)
    if (enteredMainFromAccounts.current) {
      enteredMainFromAccounts.current = false
      window.history.back()
    } else {
      window.history.replaceState(null, '', `${window.location.pathname}${window.location.search}`)
    }
    window.scrollTo({ top: 0, behavior: 'instant' })
  }
  // 主导航：账号池、费用、用户管理、系统设置四者平级。从账号池进去压一层历史（后退回
  // 账号池），一级页面之间互切原地替换，不在历史里越积越深。
  const navigateMain = (section: MainSection) => {
    if (section === 'pool') {
      closeMain()
      return
    }
    const url = section === 'settings' ? `#/settings/${settingsRoute ?? 'access'}` : `#/${section}`
    if (mainRoute || settingsRoute) {
      window.history.replaceState(null, '', url)
    } else {
      window.history.pushState(null, '', url)
      enteredMainFromAccounts.current = true
    }
    if (section === 'settings') {
      // 预取可能还没轮到（页面刚打开就点），这里再触发一次：import() 命中缓存则是空操作。
      void loadSettingsPage()
      setMainRoute(null)
      setSettingsRoute((current) => current ?? 'access')
    } else {
      setSettingsRoute(null)
      setMainRoute(section)
    }
    window.scrollTo({ top: 0, behavior: 'instant' })
  }
  const closeAccount = useCallback(() => {
    setAccountRoute(null)
    if (enteredAccountFromList.current) {
      enteredAccountFromList.current = false
      window.history.back()
    } else {
      listScrollY.current = null
      window.history.replaceState(null, '', `${window.location.pathname}${window.location.search}`)
      window.scrollTo({ top: 0, behavior: 'instant' })
    }
  }, [])
  // 列表重新挂上之后再还原滚动位置：列表数据在缓存里，这一帧就能渲染出原来的高度。
  useEffect(() => {
    if (accountRoute != null || listScrollY.current == null) return
    const top = listScrollY.current
    listScrollY.current = null
    requestAnimationFrame(() => window.scrollTo({ top, behavior: 'instant' }))
  }, [accountRoute])
  const {
    data: authState,
    isLoading: authLoading,
    isError: authFailed,
    refetch: refetchAuthState,
  } = useQuery({
    queryKey: ['auth-state'],
    queryFn: getAuthState,
  })
  // 没带会话的请求被 401（别处刚设了密码）→ 重新问一遍鉴权状态，已设密码就会切到登录页。
  useEffect(() => {
    const onUnauthorized = () => { void refetchAuthState() }
    window.addEventListener(UNAUTHORIZED_EVENT, onUnauthorized)
    return () => window.removeEventListener(UNAUTHORIZED_EVENT, onUnauthorized)
  }, [refetchAuthState])
  // 别的标签页登录、退出或换了账号：整页重载，内存里的状态与缓存一并换掉。
  useEffect(() => {
    const onStorage = (event: StorageEvent) => {
      if (event.key === TOKEN_KEY && event.newValue !== session) window.location.reload()
    }
    window.addEventListener('storage', onStorage)
    return () => window.removeEventListener('storage', onStorage)
  }, [session])

  const readOnly = useReadOnly()
  const isAdmin = useIsAdmin()
  const seesWholePool = useSeesWholePool()
  const canManageUsers = useCanManageUsers()
  // 系统设置只对管理员开放（后端也拒 `/settings`），用户管理只对管理员与代理开放：深链接、
  // 书签进来的落回账号池。只对确认过的身份清路由；身份未明时下面只是先不渲染，查回来有权限
  // 就照常打开。
  const confirmedRole = useConfirmedRole()
  useEffect(() => {
    if (!confirmedRole) return
    if (settingsRoute && confirmedRole !== 'admin') {
      window.history.replaceState(null, '', '#/')
      setSettingsRoute(null)
    }
    if (mainRoute === 'users' && confirmedRole !== 'admin' && confirmedRole !== 'agent') {
      window.history.replaceState(null, '', '#/')
      setMainRoute(null)
    }
  }, [confirmedRole, settingsRoute, mainRoute])
  const needLogin = authState?.configured && !session
  // 未设密码：管理接口一律拒绝（本机也一样），先用启动日志里的初始化口令设密码。
  const needSetup = !!authState?.setup_required
  const needAuth = needLogin || needSetup

  const {
    data: creds,
    isLoading,
    isError,
    isRefetchError,
    isFetching,
    error: credentialsError,
  } = useQuery({
    queryKey: ['credentials'],
    queryFn: listCredentials,
    refetchInterval: 30_000,
    enabled: !needAuth && !authLoading, // 未登录时不请求受保护接口
  })

  useEffect(() => {
    if (!needAuth && !settingsRoute && accountRoute == null) {
      document.title = t('luban · 授权代理', 'luban · Authorization Proxy')
    }
  }, [needAuth, settingsRoute, accountRoute, t])

  // 鉴权状态拿不到（后端重启、网络抖动）时不再干等骨架屏：放行到列表，让账号请求的报错与重试按钮接手。
  const isBootstrapping = authLoading || (!authState && !authFailed)
  const retry = () => {
    if (!authState) void refetchAuthState()
    void queryClient.invalidateQueries()
  }
  useSettingsPrefetch(!isBootstrapping && !needAuth && !settingsRoute && isAdmin)
  const signOut = () => {
    // 先作废服务端的会话再清本地；请求失败也照样清（会话过期等着自然失效即可）。
    // 重载而不是只清 state：缓存里的账号、设置（含客户端 Key）不能留给下一个登录的人。
    void logout().catch(() => {}).finally(() => {
      clearToken()
      window.location.reload()
    })
  }

  if (!isBootstrapping && needSetup) {
    return (
      <SetupPage
        onSuccess={(result) => {
          setToken(result.token)
          rememberRole(result.role)
          setSession(result.token)
          void refetchAuthState()
        }}
      />
    )
  }

  if (!isBootstrapping && needLogin) {
    return <LoginPage onSuccess={setSession} />
  }

  if (!isBootstrapping && mainRoute === 'users' && canManageUsers) {
    return <UsersPage onNavigate={navigateMain} onSignOut={signOut} />
  }

  if (!isBootstrapping && mainRoute === 'billing') {
    return <BillingPage onNavigate={navigateMain} onSignOut={signOut} />
  }

  if (!isBootstrapping && settingsRoute && isAdmin) {
    return (
      <Suspense fallback={<SettingsPageFallback onNavigate={navigateMain} />}>
        <SettingsPage
          section={settingsRoute}
          onSectionChange={openSettings}
          onNavigate={navigateMain}
          onSignOut={signOut}
        />
      </Suspense>
    )
  }

  // 详情页在鉴权就位前就接手渲染（自带骨架），深链接打开时不会先闪一下账号列表。
  if (accountRoute != null) {
    return (
      <CredentialDetailPage
        id={accountRoute}
        credentials={isBootstrapping ? undefined : creds}
        isLoading={isBootstrapping || isLoading}
        error={isBootstrapping ? null : credentialsError}
        onRetry={retry}
        onBack={closeAccount}
        onNavigate={(section) => {
          // 从详情页去别的一级页面压一层历史：在那边点「账号池」或浏览器后退，都回到这个号的详情。
          setAccountRoute(null)
          navigateMain(section)
        }}
        onSignOut={authState?.configured && session ? signOut : undefined}
      />
    )
  }

  return (
    <div className="app-shell flex min-h-dvh flex-col text-foreground">
      <AppHeader
        homeLabel={t('返回顶部', 'Back to top')}
        nav={<MainNav current="pool" onNavigate={navigateMain} />}
        onNavigateHome={scrollToTop}
        actions={
          <>
            {/* 「添加账号」是顶栏第一枚、也是唯一的实心按钮：这一页的主动作在这儿最好够。
                窄屏只留一枚橙色 `+`，≥640px 带上文字。 */}
            {!readOnly && (
              <Hint label={t('添加账号', 'Add account')}>
                <Button
                  aria-label={t('添加账号', 'Add account')}
                  className={HEADER_ACTION_CLASS}
                  disabled={isBootstrapping}
                  size="sm"
                  onClick={() => setAdding(true)}
                >
                  <PlusIcon />
                  <span className="max-sm:sr-only">{t('添加账号', 'Add account')}</span>
                </Button>
              </Hint>
            )}
            {/* 请求查询、封号记录看的是全池，只给管理员与访客。 */}
            {seesWholePool && (
              <Hint label={t('按请求 ID 查询请求记录', 'Look up a request by ID')}>
                <Button
                  aria-label={t('请求查询', 'Request lookup')}
                  className={HEADER_ACTION_CLASS}
                  disabled={isBootstrapping}
                  size="sm"
                  variant="outline"
                  onClick={() => setLookupOpen(true)}
                >
                  <SearchIcon />
                  <span className="max-sm:sr-only">{t('请求查询', 'Lookup')}</span>
                </Button>
              </Hint>
            )}
            {/* 系统设置已在主导航里；这里只留账号池自己的工具（封号记录看全池，只给管理员与访客）。 */}
            <AccountMenu onSignOut={authState?.configured && session ? signOut : undefined}>
              {seesWholePool && (
                <MenuItem disabled={isBootstrapping} onClick={() => setBansOpen(true)}>
                  <ShieldAlertIcon />{t('封号记录', 'Ban events')}
                </MenuItem>
              )}
            </AccountMenu>
          </>
        }
      />

      <main className="page-frame relative flex-1 py-4 pb-8 sm:py-5 sm:pb-10">
        {/* 添加账号保持为短流程弹框；复杂设置使用独立页面。 */}
        <AddAccount open={adding} onOpenChange={setAdding} />
        <RequestLookupDialog open={lookupOpen} onOpenChange={setLookupOpen} />
        {seesWholePool && <BanEventsDialog open={bansOpen} onOpenChange={setBansOpen} />}

        <CredentialWorkspace
          data={{
            credentials: isBootstrapping ? undefined : creds,
            isLoading: isBootstrapping || isLoading,
            isError: !isBootstrapping && isError,
            isRefetchError: !isBootstrapping && isRefetchError,
            isFetching: !isBootstrapping && isFetching,
            error: credentialsError,
          }}
          state={{
            query,
            filter,
            tier,
            sort,
            dir,
            view,
            selected,
            page,
            pageSize,
          }}
          actions={{
            onQueryChange: setQuery,
            onFilterChange: setFilter,
            onTierChange: setTier,
            onSortChange: (key, nextDir) => {
              setSort(key)
              setDir(nextDir)
            },
            onViewChange: switchView,
            onSelectedChange: setSelected,
            onPageChange: setPage,
            onPageSizeChange: setPageSize,
            onRetry: retry,
            onAdd: () => setAdding(true),
          }}
        />
      </main>
      <AppFooter />
    </div>
  )
}

export default App
