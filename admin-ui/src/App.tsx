import { Suspense, lazy, useEffect, useRef, useState } from 'react'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import {
  EllipsisVerticalIcon, LogOutIcon, PlusIcon, SettingsIcon,
} from 'lucide-react'
import { listCredentials } from '@/api/credentials'
import { getAuthState } from '@/api/auth'
import { getSettings } from '@/api/settings'
import { getPw, setPw, clearPw } from '@/api/client'
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
import type { SettingsSection } from '@/components/settings-page'
import { LoginPage } from '@/components/login-page'
import { AppFooter } from '@/components/app-footer'
import { LanguageSwitcher } from '@/components/language-switcher'
import { ThemeSwitcher } from '@/components/theme-switcher'
import { LogoMark } from '@/components/logo-mark'
import { Button, buttonVariants } from '@/components/ui/button'
import { Skeleton } from '@/components/ui/skeleton'
import { Menu, MenuItem, MenuPopup, MenuSeparator, MenuTrigger } from '@/components/ui/menu'
import { useI18n } from '@/lib/i18n'

// 设置页是另一棵大树（访问控制、转发、设备三块），账号页从不用它，
// 拆成单独 chunk 后首屏少解析一截。但 chunk 有 120 多 KB，远程访问时点进去要先白屏
// 等下载——所以账号页一空闲就把它预取回来（见 `useSettingsPrefetch`），点击时只剩挂载。
// 浏览器会缓存同一 specifier 的 import() 结果，重复调用不会再次请求。
const loadSettingsPage = () => import('@/components/settings-page')
const SettingsPage = lazy(() => loadSettingsPage().then((m) => ({ default: m.SettingsPage })))

/** 设置页 chunk 到达前的占位：保留与设置页同构的顶栏和标题骨架，切换时页面不至于整块变白。 */
function SettingsPageFallback({ onBack }: { onBack: () => void }) {
  const { t } = useI18n()
  return (
    <div className="app-shell flex min-h-dvh flex-col text-foreground">
      <header className="app-header sticky top-0 z-20 border-b bg-background">
        <div className="page-frame flex h-14 items-center justify-between gap-3 sm:h-16">
          <Button
            aria-label={t('返回账号页', 'Back to accounts')}
            className="-ml-2 h-auto min-w-0 justify-start gap-2.5 px-2 py-1.5 sm:gap-3"
            variant="ghost"
            onClick={onBack}
          >
            <span className="brand-mark flex size-8 shrink-0 items-center justify-center rounded-lg text-white">
              <LogoMark className="size-[1.125rem]" />
            </span>
            <span className="min-w-0 text-left">
              <span className="block text-sm font-semibold leading-none tracking-tight">Luban</span>
              <span className="mt-1 hidden whitespace-nowrap text-xs font-normal text-muted-foreground sm:block">
                Claude Code Gateway
              </span>
            </span>
          </Button>
          <div className="flex items-center gap-2">
            <LanguageSwitcher compact />
            <ThemeSwitcher compact />
          </div>
        </div>
      </header>
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
      void qc.prefetchQuery({ queryKey: ['settings'], queryFn: getSettings })
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

function readSettingsRoute(): SettingsSection | null {
  if (!window.location.hash.startsWith('#/settings')) return null
  if (window.location.hash.includes('/devices')) return 'devices'
  if (window.location.hash.includes('/forwarding')) return 'forwarding'
  if (window.location.hash.includes('/security')) return 'security'
  if (window.location.hash.includes('/migration')) return 'migration'
  // 兼容旧的 #/settings 与 #/settings/access 深链接。
  return 'access'
}

function App() {
  const { t } = useI18n()
  const [adding, setAdding] = useState(false)
  const [settingsRoute, setSettingsRoute] = useState<SettingsSection | null>(readSettingsRoute)
  const [pw, setPwState] = useState<string | null>(getPw())
  const [selected, setSelected] = useState<Set<number>>(new Set())
  // 分页（纯前端切片：列表接口一次返回全部账号）。
  const [page, setPage] = useState(1)
  // 只有从账号页主动进入设置时，关闭设置才应该消费这条 history 记录。
  // 直接打开 #/settings/* 的深链接则在原地替换回账号页，避免把用户带离当前站点。
  const enteredSettingsFromAccounts = useRef(false)

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
    if (settingsRoute) return
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
  }, [query, filter, tier, sort, dir, view, page, pageSize, settingsRoute])
  useEffect(() => {
    const syncRoute = () => {
      const next = readSettingsRoute()
      setSettingsRoute(next)
      if (!next) enteredSettingsFromAccounts.current = false
    }
    window.addEventListener('popstate', syncRoute)
    window.addEventListener('hashchange', syncRoute)
    return () => {
      window.removeEventListener('popstate', syncRoute)
      window.removeEventListener('hashchange', syncRoute)
    }
  }, [])

  const openSettings = (section: SettingsSection) => {
    // 预取可能还没轮到（页面刚打开就点），这里再触发一次：import() 命中缓存则是空操作。
    void loadSettingsPage()
    const url = `#/settings/${section}`
    if (settingsRoute) {
      // Tab 切换属于同一设置页，不应为每次切换新增浏览器历史。
      window.history.replaceState(null, '', url)
    } else {
      window.history.pushState(null, '', url)
      enteredSettingsFromAccounts.current = true
    }
    setSettingsRoute(section)
    window.scrollTo({ top: 0, behavior: 'instant' })
  }
  const closeSettings = () => {
    setSettingsRoute(null)
    if (enteredSettingsFromAccounts.current) {
      enteredSettingsFromAccounts.current = false
      window.history.back()
    } else {
      window.history.replaceState(null, '', `${window.location.pathname}${window.location.search}`)
    }
    window.scrollTo({ top: 0, behavior: 'instant' })
  }
  const { data: authState, isLoading: authLoading } = useQuery({
    queryKey: ['auth-state'],
    queryFn: getAuthState,
  })

  const needLogin = authState?.configured && !pw

  const {
    data: creds,
    isLoading,
    isError,
    isRefetchError,
    isFetching,
    error: credentialsError,
    refetch: refetchCredentials,
  } = useQuery({
    queryKey: ['credentials'],
    queryFn: listCredentials,
    refetchInterval: 30_000,
    enabled: !needLogin && !authLoading, // 未登录时不请求受保护接口
  })

  useEffect(() => {
    if (!needLogin && !settingsRoute) {
      document.title = t('luban · 授权代理', 'luban · Authorization Proxy')
    }
  }, [needLogin, settingsRoute, t])

  const isBootstrapping = authLoading || !authState
  useSettingsPrefetch(!isBootstrapping && !needLogin && !settingsRoute)

  if (!isBootstrapping && needLogin) {
    return <LoginPage onSuccess={(p) => { setPw(p); setPwState(p) }} />
  }

  if (!isBootstrapping && settingsRoute) {
    return (
      <Suspense fallback={<SettingsPageFallback onBack={closeSettings} />}>
        <SettingsPage
          section={settingsRoute}
          onSectionChange={openSettings}
          onBack={closeSettings}
        />
      </Suspense>
    )
  }

  return (
    <div className="app-shell flex min-h-dvh flex-col text-foreground">
      <header className="app-header sticky top-0 z-20 border-b bg-background">
        <div className="page-frame flex h-14 items-center justify-between gap-3 sm:h-16">
          <div className="flex min-w-0 items-center gap-2.5 sm:gap-3">
            <div className="brand-mark flex size-8 shrink-0 items-center justify-center rounded-lg text-white">
              <LogoMark className="size-[1.125rem]" />
            </div>
            <div className="min-w-0">
              <div className="text-sm font-semibold leading-none tracking-tight">Luban</div>
              <div className="mt-1 hidden whitespace-nowrap text-xs text-muted-foreground sm:block">Claude Code Gateway</div>
            </div>
          </div>
          <div className="flex items-center gap-2 sm:hidden">
            <Button
              size="icon-lg"
              disabled={isBootstrapping}
              onClick={() => setAdding(true)}
              aria-label={t('添加账号', 'Add account')}
            >
              <PlusIcon />
            </Button>
            <LanguageSwitcher compact />
            <ThemeSwitcher compact />
            <Menu>
              <MenuTrigger
                className={buttonVariants({ size: 'icon-lg', variant: 'outline' })}
                disabled={isBootstrapping}
                aria-label={t('更多操作', 'More actions')}
              >
                <EllipsisVerticalIcon />
              </MenuTrigger>
              <MenuPopup align="end" className="w-44">
                <MenuItem onClick={() => openSettings('access')}>
                  <SettingsIcon />{t('系统设置', 'System settings')}
                </MenuItem>
                {authState?.configured && pw && (
                  <>
                    <MenuSeparator />
                    <MenuItem variant="destructive" onClick={() => { clearPw(); setPwState(null) }}>
                      <LogOutIcon />{t('退出登录', 'Sign out')}
                    </MenuItem>
                  </>
                )}
              </MenuPopup>
            </Menu>
          </div>
          <div className="hidden items-center gap-2 sm:flex">
            <LanguageSwitcher />
            <ThemeSwitcher />
            <Button
              size="sm"
              variant="outline"
              disabled={isBootstrapping}
              onClick={() => openSettings('access')}
              title={t('系统设置', 'System settings')}
              aria-label={t('系统设置', 'System settings')}
            >
              <SettingsIcon />
              <span>{t('系统设置', 'Settings')}</span>
            </Button>
            <Button
              size="sm"
              disabled={isBootstrapping}
              onClick={() => setAdding(true)}
              aria-label={t('添加账号', 'Add account')}
            >
              <PlusIcon />
              <span>{t('添加账号', 'Add account')}</span>
            </Button>
            {authState?.configured && pw && (
              <Button
                size="sm"
                variant="ghost"
                title={t('退出登录', 'Sign out')}
                aria-label={t('退出登录', 'Sign out')}
                onClick={() => { clearPw(); setPwState(null) }}>
                <LogOutIcon />
              </Button>
            )}
          </div>
        </div>
      </header>

      <main className="page-frame relative flex-1 py-4 pb-8 sm:py-5 sm:pb-10">
        {/* 添加账号保持为短流程弹框；复杂设置使用独立页面。 */}
        <AddAccount open={adding} onOpenChange={setAdding} />

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
            onRetry: () => { void refetchCredentials() },
            onAdd: () => setAdding(true),
          }}
        />
      </main>
      <AppFooter />
    </div>
  )
}

export default App
