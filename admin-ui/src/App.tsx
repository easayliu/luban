import { Suspense, lazy, useEffect, useRef, useState } from 'react'
import { useQuery, useQueryClient } from '@tanstack/react-query'
import { SearchIcon, SettingsIcon, ShieldAlertIcon } from 'lucide-react'
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
import { RequestLookupDialog } from '@/components/request-lookup-dialog'
import { BanEventsDialog } from '@/components/ban-events-dialog'
import type { SettingsSection } from '@/components/settings-page'
import { LoginPage } from '@/components/login-page'
import { AppFooter } from '@/components/app-footer'
import { AppHeader, Breadcrumb, PreferencesMenu, scrollToTop } from '@/components/app-header'
import { Button } from '@/components/ui/button'
import { Skeleton } from '@/components/ui/skeleton'
import { MenuItem } from '@/components/ui/menu'
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
      <AppHeader actions={<PreferencesMenu />} onNavigateHome={onBack} />
      <main aria-busy="true" className="page-frame flex-1 py-5 sm:py-8">
        {/* 与真设置页同构：面包屑也在骨架里占住位置，chunk 到达时标题不上下跳。 */}
        <Breadcrumb
          current={t('系统设置', 'System settings')}
          parent={t('账号池', 'Account pool')}
          onNavigateParent={onBack}
        />
        <div className="mt-5 max-w-2xl">
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
  const [lookupOpen, setLookupOpen] = useState(false)
  const [bansOpen, setBansOpen] = useState(false)
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
      <AppHeader
        homeLabel={t('回到顶部', 'Back to top')}
        onNavigateHome={scrollToTop}
        actions={
          <>
            {/* 顶栏只留这一枚主动作（相当于 Cloudflare 顶栏里的全局搜索），其余全部收进菜单。
                「添加账号」不在这儿了——它是页面级动作，挪到了下面「账号池」标题那一行。 */}
            <Button
              aria-label={t('请求查询', 'Request lookup')}
              className="max-sm:size-10 max-sm:px-0"
              disabled={isBootstrapping}
              size="sm"
              title={t('按请求 ID 查流水', 'Look up a request by ID')}
              variant="outline"
              onClick={() => setLookupOpen(true)}
            >
              <SearchIcon />
              <span className="max-sm:sr-only">{t('请求查询', 'Lookup')}</span>
            </Button>
            <PreferencesMenu
              onSignOut={
                authState?.configured && pw
                  ? () => { clearPw(); setPwState(null) }
                  : undefined
              }
            >
              <MenuItem disabled={isBootstrapping} onClick={() => setBansOpen(true)}>
                <ShieldAlertIcon />{t('封号记录', 'Ban events')}
              </MenuItem>
              <MenuItem disabled={isBootstrapping} onClick={() => openSettings('access')}>
                <SettingsIcon />{t('系统设置', 'System settings')}
              </MenuItem>
            </PreferencesMenu>
          </>
        }
      />

      <main className="page-frame relative flex-1 py-4 pb-8 sm:py-5 sm:pb-10">
        {/* 添加账号保持为短流程弹框；复杂设置使用独立页面。 */}
        <AddAccount open={adding} onOpenChange={setAdding} />
        <RequestLookupDialog open={lookupOpen} onOpenChange={setLookupOpen} />
        <BanEventsDialog open={bansOpen} onOpenChange={setBansOpen} />

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
