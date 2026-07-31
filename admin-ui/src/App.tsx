import { useEffect, useRef, useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import {
  EllipsisVerticalIcon, LogOutIcon, PlusIcon, SettingsIcon,
} from 'lucide-react'
import { listCredentials } from '@/api/credentials'
import { getAuthState } from '@/api/auth'
import { getPw, setPw, clearPw } from '@/api/client'
import { cn } from '@/lib/utils'
import { numberOneOf, oneOf, usePersisted } from '@/lib/persisted'
import {
  SORT_KEYS,
  type SortDir,
  type SortKey,
} from '@/components/credential-shared'
import {
  CREDENTIAL_FILTER_KEYS,
  CREDENTIAL_PAGE_SIZES,
  CREDENTIAL_VIEW_MODES,
  CredentialWorkspace,
  preferredInitialCredentialView,
  type CredentialFilterKey,
  type CredentialPageSize,
  type CredentialViewMode,
} from '@/components/credential-workspace'
import { AddAccount } from '@/components/add-account'
import { SettingsPage, type SettingsSection } from '@/components/settings-page'
import { LoginPage } from '@/components/login-page'
import { AppFooter } from '@/components/app-footer'
import { LogoMark } from '@/components/logo-mark'
import { Button, buttonVariants } from '@/components/ui/button'
import { Menu, MenuItem, MenuPopup, MenuSeparator, MenuTrigger } from '@/components/ui/menu'
import { Spinner } from '@/components/ui/spinner'

function readSettingsRoute(): SettingsSection | null {
  if (!window.location.hash.startsWith('#/settings')) return null
  return window.location.hash.includes('/forwarding') ? 'forwarding' : 'access'
}

function App() {
  const [adding, setAdding] = useState(false)
  const [settingsRoute, setSettingsRoute] = useState<SettingsSection | null>(readSettingsRoute)
  const [pw, setPwState] = useState<string | null>(getPw())
  const [selected, setSelected] = useState<Set<number>>(new Set())
  // 分页（纯前端切片：列表接口一次返回全部账号）。
  const [page, setPage] = useState(1)
  // 只有从账号页主动进入设置时，关闭设置才应该消费这条 history 记录。
  // 直接打开 #/settings/* 的深链接则在原地替换回账号页，避免把用户带离当前站点。
  const enteredSettingsFromAccounts = useRef(false)

  // 界面偏好与检索条件都写入 localStorage，刷新后保持当前工作上下文。
  const [sort, setSort] = usePersisted<SortKey>('sort', 'priority', oneOf(SORT_KEYS))
  const [dir, setDir] = usePersisted<SortDir>('sortDir', 'asc', oneOf(['asc', 'desc'] as const))
  const [pageSize, setPageSize] = usePersisted<CredentialPageSize>(
    'pageSize',
    CREDENTIAL_PAGE_SIZES[0],
    numberOneOf(CREDENTIAL_PAGE_SIZES) as (raw: string) => CredentialPageSize | null,
  )
  const [view, switchView] = usePersisted<CredentialViewMode>(
    'view',
    preferredInitialCredentialView(),
    oneOf(CREDENTIAL_VIEW_MODES),
  )
  const [filter, setFilter] = usePersisted<CredentialFilterKey>(
    'filter',
    'all',
    oneOf(CREDENTIAL_FILTER_KEYS),
  )
  const [query, setQuery] = usePersisted('query', '', (raw) => raw)
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

  if (authLoading || !authState) {
    return <LoadingState fullPage />
  }

  if (needLogin) {
    return <LoginPage onSuccess={(p) => { setPw(p); setPwState(p) }} />
  }

  if (settingsRoute) {
    return (
      <SettingsPage
        section={settingsRoute}
        onSectionChange={openSettings}
        onBack={closeSettings}
      />
    )
  }

  return (
    <div className="app-shell flex min-h-dvh flex-col text-foreground">
      <header className="app-header sticky top-0 z-20 border-b bg-background/92 backdrop-blur-md">
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
            <Button size="icon-lg" onClick={() => setAdding(true)} aria-label="添加账号">
              <PlusIcon />
            </Button>
            <Menu>
              <MenuTrigger
                className={buttonVariants({ size: 'icon-lg', variant: 'outline' })}
                aria-label="更多操作"
              >
                <EllipsisVerticalIcon />
              </MenuTrigger>
              <MenuPopup align="end" className="w-44">
                <MenuItem onClick={() => openSettings('access')}>
                  <SettingsIcon />系统设置
                </MenuItem>
                {authState.configured && pw && (
                  <>
                    <MenuSeparator />
                    <MenuItem variant="destructive" onClick={() => { clearPw(); setPwState(null) }}>
                      <LogOutIcon />退出登录
                    </MenuItem>
                  </>
                )}
              </MenuPopup>
            </Menu>
          </div>
          <div className="hidden items-center gap-2 sm:flex">
            <Button size="sm" variant="outline" onClick={() => openSettings('access')} title="系统设置" aria-label="系统设置">
              <SettingsIcon />
              <span>系统设置</span>
            </Button>
            <Button size="sm" onClick={() => setAdding(true)} aria-label="添加账号">
              <PlusIcon />
              <span>添加账号</span>
            </Button>
            {authState.configured && pw && (
              <Button size="sm" variant="ghost" title="退出登录" aria-label="退出登录"
                onClick={() => { clearPw(); setPwState(null) }}>
                <LogOutIcon />
              </Button>
            )}
          </div>
        </div>
      </header>

      <main className="page-frame relative flex-1 py-5 pb-8 sm:py-8 sm:pb-12">
        {/* 添加账号保持为短流程弹框；复杂设置使用独立页面。 */}
        <AddAccount open={adding} onOpenChange={setAdding} />

        <CredentialWorkspace
          data={{
            credentials: creds,
            isLoading,
            isError,
            isRefetchError,
            isFetching,
            error: credentialsError,
          }}
          state={{
            query,
            filter,
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

function LoadingState({ fullPage = false }: { fullPage?: boolean }) {
  return (
    <div className={cn('grid place-items-center', fullPage ? 'min-h-dvh' : 'py-16')}>
      <div className="flex items-center gap-2 text-sm text-muted-foreground" role="status" aria-live="polite">
        <Spinner />
        加载中
      </div>
    </div>
  )
}

export default App
