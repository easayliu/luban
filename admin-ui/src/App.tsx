import { useEffect, useMemo, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  PlusIcon, Cog6ToothIcon, ArrowRightStartOnRectangleIcon, ArrowsUpDownIcon, CheckIcon,
  QueueListIcon, ArrowPathIcon, XMarkIcon, ChevronLeftIcon, ChevronRightIcon,
  MagnifyingGlassIcon, FunnelIcon, Squares2X2Icon, Bars3Icon,
  PlayIcon, PauseIcon, TrashIcon,
  SignalIcon, ShieldCheckIcon, ExclamationTriangleIcon, DevicePhoneMobileIcon,
  EllipsisVerticalIcon,
} from '@heroicons/react/24/outline'
import { toast } from 'sonner'
import {
  deleteCredentials, listCredentials, setDeviceLimits, setDisabledMany, setPriorities,
  type Credential,
} from '@/api/credentials'
import { getAuthState } from '@/api/auth'
import { getPw, setPw, clearPw } from '@/api/client'
import { cn, extractError } from '@/lib/utils'
import { numberOneOf, oneOf, usePersisted } from '@/lib/persisted'
import { useDebounced } from '@/lib/use-debounced'
import {
  SORTS, SORT_DIR_DEFAULT, SORT_KEYS, inputToLimit, isAbnormal, isNearLimit, sortCreds,
  type SortDir, type SortKey,
} from '@/components/credential-shared'
import { CredentialCard } from '@/components/credential-card'
import { CredentialListHeader, CredentialRow } from '@/components/credential-row'
import { AddAccount } from '@/components/add-account'
import { SettingsPage, type SettingsSection } from '@/components/settings-page'
import { LoginPage } from '@/components/login-page'
import { AppFooter } from '@/components/app-footer'
import { OverviewMetric } from '@/components/overview-metric'
import { LogoMark } from '@/components/logo-mark'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Table, TableBody, TableCaption } from '@/components/ui/table'
import { Toaster } from '@/components/ui/sonner'
import { ConfirmDialog } from '@/components/ui/confirm-dialog'
import {
  DropdownMenu, DropdownMenuTrigger, DropdownMenuContent, DropdownMenuItem,
  DropdownMenuSeparator,
} from '@/components/ui/dropdown-menu'

type FilterKey =
  | 'all'
  | 'schedulable'
  | 'attention'
  | 'enabled'
  | 'disabled'
  | 'abnormal'
  | 'nearLimit'
  | 'cooldown'
  | 'deviceFull'
type ViewMode = 'card' | 'list'

/** 每页账号数可选档位（用 10/20/50 这类常规档，不迁就栅格列数）；账号少时分页条自动隐藏。 */
const PAGE_SIZES = [10, 20, 50] as const

const VIEW_MODES = ['card', 'list'] as const

const FILTERS: { key: FilterKey; label: string; match: (c: Credential) => boolean }[] = [
  { key: 'all', label: '全部', match: () => true },
  {
    key: 'schedulable',
    label: '可调度',
    match: (c) => !c.disabled && !isAbnormal(c) && c.rate_limited_secs <= 0,
  },
  {
    key: 'attention',
    label: '需处理',
    match: (c) => isAbnormal(c) || isNearLimit(c) || (!c.disabled && c.rate_limited_secs > 0),
  },
  { key: 'enabled', label: '启用', match: (c) => !c.disabled },
  { key: 'disabled', label: '停用', match: (c) => c.disabled },
  { key: 'abnormal', label: '异常（已封禁）', match: isAbnormal },
  { key: 'nearLimit', label: '额度将满', match: isNearLimit },
  { key: 'cooldown', label: '冷却中', match: (c) => !c.disabled && c.rate_limited_secs > 0 },
  {
    key: 'deviceFull',
    label: '设备已满',
    match: (c) => c.device_limit_effective > 0 && c.device_count >= c.device_limit_effective,
  },
]

function preferredInitialView(): ViewMode {
  return typeof window !== 'undefined' && window.matchMedia('(min-width: 64rem)').matches
    ? 'list'
    : 'card'
}

/** 关键字匹配：名称（忽略大小写）或 `#id`。 */
function matchQuery(c: Credential, q: string): boolean {
  const t = q.trim().toLowerCase()
  if (!t) return true
  return c.label.toLowerCase().includes(t) || `#${c.id}`.includes(t) || String(c.id) === t
}

function readSettingsRoute(): SettingsSection | null {
  if (!window.location.hash.startsWith('#/settings')) return null
  return window.location.hash.includes('/forwarding') ? 'forwarding' : 'access'
}

function App() {
  const [adding, setAdding] = useState(false)
  const [settingsRoute, setSettingsRoute] = useState<SettingsSection | null>(readSettingsRoute)
  const [pw, setPwState] = useState<string | null>(getPw())
  // 批量模式：卡片出现勾选框，工具栏变成批量操作条。
  const [batch, setBatch] = useState(false)
  const [selected, setSelected] = useState<Set<number>>(new Set())
  // 分页（纯前端切片：列表接口一次返回全部账号）。
  const [page, setPage] = useState(1)

  // 界面偏好：排序、每页条数、视图，均写 localStorage，刷新后保持。
  const [sort, setSort] = usePersisted<SortKey>('sort', 'priority', oneOf(SORT_KEYS))
  const [dir, setDir] = usePersisted<SortDir>('sortDir', 'asc', oneOf(['asc', 'desc'] as const))
  const [pageSize, setPageSize] = usePersisted('pageSize', PAGE_SIZES[0], numberOneOf(PAGE_SIZES))
  const [view, switchView] = usePersisted<ViewMode>('view', preferredInitialView(), oneOf(VIEW_MODES))

  // 筛选与搜索刻意不持久化：它们会隐藏账号，刷新后仍生效容易让人以为账号丢了。
  const [filter, setFilter] = useState<FilterKey>('all')
  const [query, setQuery] = useState('')
  useEffect(() => {
    const syncRoute = () => setSettingsRoute(readSettingsRoute())
    window.addEventListener('popstate', syncRoute)
    window.addEventListener('hashchange', syncRoute)
    return () => {
      window.removeEventListener('popstate', syncRoute)
      window.removeEventListener('hashchange', syncRoute)
    }
  }, [])

  const openSettings = (section: SettingsSection) => {
    window.history.pushState(null, '', `#/settings/${section}`)
    setSettingsRoute(section)
    window.scrollTo({ top: 0, behavior: 'instant' })
  }
  const closeSettings = () => {
    window.history.pushState(null, '', `${window.location.pathname}${window.location.search}`)
    setSettingsRoute(null)
    window.scrollTo({ top: 0, behavior: 'instant' })
  }
  // 输入框绑 query 保持跟手，筛选/排序用延迟值：连续敲键时不必每个字符都过一遍全量账号。
  const debouncedQuery = useDebounced(query)
  // 条件变化后停留在旧页码可能整页为空，统一回到第一页。
  const resetPage = <T,>(set: (v: T) => void) => (v: T) => {
    set(v)
    setPage(1)
    // 隐藏的勾选项参与批量操作风险很高；搜索/筛选变化时清空，作用域始终与眼前列表一致。
    setSelected(new Set())
  }
  /**
   * 切换排序。表头列和工具栏下拉共用：点当前维度翻转升/降序，换维度则按该维度的
   * 默认方向起步。排序变了内容顺序全变，停在旧页码没有意义，统一回第一页。
   */
  const changeSort = (key: SortKey) => {
    if (key === sort) setDir(dir === 'asc' ? 'desc' : 'asc')
    else { setSort(key); setDir(SORT_DIR_DEFAULT[key]) }
    setPage(1)
  }
  const toggleSelected = (id: number, on: boolean) =>
    setSelected((prev) => {
      const next = new Set(prev)
      if (on) next.add(id)
      else next.delete(id)
      return next
    })

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

  // 注意：Hook 必须在任何提前 return 之前调用，避免渲染间 Hook 数量变化（React #310）。
  // 顺序：筛选 → 搜索 → 排序 → 分页切片。
  const sorted = useMemo(() => {
    const match = FILTERS.find((f) => f.key === filter)?.match ?? (() => true)
    const list = (creds ?? []).filter((c) => match(c) && matchQuery(c, debouncedQuery))
    return sortCreds(list, sort, dir)
  }, [creds, sort, dir, filter, debouncedQuery])
  const total = sorted.length
  const pageCount = Math.max(1, Math.ceil(total / pageSize))
  // 删账号/改每页条数可能让当前页越界，取值时夹紧（不写回 state，避免多余渲染）。
  const current = Math.min(page, pageCount)
  const pageItems = useMemo(
    () => sorted.slice((current - 1) * pageSize, current * pageSize),
    [sorted, current, pageSize],
  )

  if (authLoading || !authState) {
    return <LoadingState fullPage />
  }

  if (needLogin) {
    return (
      <>
        <LoginPage onSuccess={(p) => { setPw(p); setPwState(p) }} />
        <Toaster position="top-right" />
      </>
    )
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

  const pool = creds ?? []
  const count = pool.length
  const enabledCount = pool.filter((c) => !c.disabled).length
  const schedulableCount = pool.filter(
    (c) => !c.disabled && !isAbnormal(c) && c.rate_limited_secs <= 0,
  ).length
  const abnormalCount = pool.filter(isAbnormal).length
  const nearLimitCount = pool.filter(isNearLimit).length
  const cooldownCount = pool.filter((c) => !c.disabled && c.rate_limited_secs > 0).length
  const attentionCount = pool.filter(
    (c) => isAbnormal(c) || isNearLimit(c) || (!c.disabled && c.rate_limited_secs > 0),
  ).length
  const deviceCount = pool.reduce((sum, c) => sum + c.device_count, 0)
  const deviceCapacity = pool.reduce(
    (sum, c) => sum + (c.device_limit_effective > 0 ? c.device_limit_effective : 0),
    0,
  )
  const unlimitedDeviceAccounts = pool.filter((c) => c.device_limit_effective <= 0).length
  const fullDeviceCount = pool.filter(
    (c) => c.device_limit_effective > 0 && c.device_count >= c.device_limit_effective,
  ).length
  // 跟 total 同源用延迟值，否则敲键的那一瞬文案先切成「筛选出 N / 共 M」而 N 还是旧的。
  const filtering = filter !== 'all' || debouncedQuery.trim() !== ''
  const healthLabel = attentionCount > 0 ? `${attentionCount} 个账号需关注` : '账号池运行平稳'
  const attentionStatus =
    abnormalCount > 0 ? `${abnormalCount} 异常`
      : cooldownCount > 0 ? `${cooldownCount} 冷却中`
        : nearLimitCount > 0 ? '额度风险'
          : '无需处理'
  const deviceStatus =
    fullDeviceCount > 0 ? `${fullDeviceCount} 个账号已满`
      : unlimitedDeviceAccounts > 0 ? `${unlimitedDeviceAccounts} 个不限额账号`
        : deviceCapacity > 0 ? `共 ${deviceCapacity} 个名额`
          : '尚未绑定'
  const selectMetric = (key: FilterKey) =>
    resetPage(setFilter)(filter === key ? 'all' : key)

  return (
    <div className="app-shell flex min-h-dvh flex-col text-foreground">
      {/* 置顶操作栏 */}
      <header className="app-header sticky top-0 z-20 border-b border-border/70 bg-background/90 backdrop-blur-xl">
        <div className="page-frame flex items-center justify-between gap-3 py-2.5 sm:py-3">
          <div className="flex min-w-0 items-center gap-2.5 sm:gap-3">
            <div className="brand-mark flex size-8 shrink-0 items-center justify-center rounded-md text-brand-foreground sm:size-9 sm:rounded-lg">
              <LogoMark className="size-5" />
            </div>
            <div className="min-w-0">
              <div className="text-[0.9375rem] font-semibold leading-none tracking-tight">Luban</div>
              <div className="label-eyebrow mt-1.5 hidden whitespace-nowrap sm:block">Claude Code Gateway</div>
            </div>
          </div>
          <div className="flex items-center gap-2 sm:hidden">
            <Button size="icon" className="size-10" onClick={() => setAdding(true)} aria-label="添加账号">
              <PlusIcon />
            </Button>
            <DropdownMenu>
              <DropdownMenuTrigger asChild>
                <Button size="icon" variant="outline" className="size-10" aria-label="更多操作">
                  <EllipsisVerticalIcon />
                </Button>
              </DropdownMenuTrigger>
              <DropdownMenuContent align="end" className="w-44">
                <DropdownMenuItem onClick={() => openSettings('access')}>
                  <Cog6ToothIcon />系统设置
                </DropdownMenuItem>
                {authState.configured && pw && (
                  <>
                    <DropdownMenuSeparator />
                    <DropdownMenuItem className="text-bad focus:text-bad" onClick={() => { clearPw(); setPwState(null) }}>
                      <ArrowRightStartOnRectangleIcon />退出登录
                    </DropdownMenuItem>
                  </>
                )}
              </DropdownMenuContent>
            </DropdownMenu>
          </div>
          <div className="hidden items-center gap-2 sm:flex">
            <Button size="sm" variant="outline" onClick={() => openSettings('access')} title="系统设置" aria-label="系统设置">
              <Cog6ToothIcon />
              <span>系统设置</span>
            </Button>
            <Button size="sm" onClick={() => setAdding(true)} aria-label="添加账号">
              <PlusIcon />
              <span>添加账号</span>
            </Button>
            {authState.configured && pw && (
              <Button size="sm" variant="ghost" title="退出登录" aria-label="退出登录"
                onClick={() => { clearPw(); setPwState(null) }}>
                <ArrowRightStartOnRectangleIcon />
              </Button>
            )}
          </div>
        </div>
      </header>

      <main className="page-frame relative flex-1 py-5 pb-8 sm:py-6 sm:pb-10">
        {/* 添加账号保持为短流程弹框；复杂设置使用独立页面。 */}
        <AddAccount open={adding} onOpenChange={setAdding} />

        <div className="grid gap-5 xl:grid-cols-[14rem_minmax(0,1fr)] xl:items-start xl:gap-6">
          <aside className="min-w-0 space-y-4 xl:sticky xl:top-24">
            <div>
              <div className="flex flex-wrap items-center gap-2">
                <span className="label-eyebrow">Account pool</span>
                {count > 0 && (
                  <span
                    className={cn(
                      'inline-flex items-center gap-1.5 rounded-full px-2 py-1 text-2xs font-medium',
                      abnormalCount > 0
                        ? 'bg-bad-soft text-bad'
                        : attentionCount > 0
                          ? 'bg-warn-soft text-warn'
                          : 'bg-ok-soft text-ok',
                    )}
                  >
                    <span className="size-1.5 rounded-full bg-current" aria-hidden />
                    {healthLabel}
                  </span>
                )}
              </div>
              <h1 className="mt-2 text-xl font-semibold tracking-tight sm:text-2xl">账号调度中心</h1>
              <p className="mt-2 hidden max-w-xl text-xs leading-5 text-muted-foreground sm:block">
                巡检账号健康、额度与设备容量，优先处理会影响转发的状态。
              </p>
              <div className="mt-3 flex items-center gap-1.5 text-2xs text-muted-foreground">
                <ArrowPathIcon className={cn('size-3.5', isFetching && !isLoading && 'animate-spin')} />
                {isRefetchError ? (
                  <button
                    type="button"
                    className="rounded-sm font-medium text-bad underline-offset-2 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/60"
                    onClick={() => { void refetchCredentials() }}
                  >
                    刷新失败，点击重试
                  </button>
                ) : (
                  <span>每 30 秒自动刷新</span>
                )}
              </div>
            </div>

            {count > 0 && (
              <div className="grid grid-cols-2 gap-2 sm:grid-cols-4 xl:grid-cols-1">
              <OverviewMetric
                label="可调度账号"
                value={`${schedulableCount}/${count}`}
                status={
                  schedulableCount < count
                    ? `${count - schedulableCount} 暂不可用`
                    : `${enabledCount} 已启用`
                }
                icon={ShieldCheckIcon}
                tone={schedulableCount > 0 ? 'ok' : 'bad'}
                active={filter === 'schedulable'}
                onClick={() => selectMetric('schedulable')}
              />
              <OverviewMetric
                label="需处理"
                value={attentionCount}
                status={attentionStatus}
                icon={ExclamationTriangleIcon}
                tone={abnormalCount > 0 ? 'bad' : attentionCount > 0 ? 'warn' : 'neutral'}
                active={filter === 'attention'}
                onClick={() => selectMetric('attention')}
              />
              <OverviewMetric
                label="额度预警"
                value={nearLimitCount}
                status={nearLimitCount > 0 ? '已达 90%' : '无预警'}
                icon={SignalIcon}
                tone={nearLimitCount > 0 ? 'warn' : 'neutral'}
                active={filter === 'nearLimit'}
                onClick={() => selectMetric('nearLimit')}
              />
              <OverviewMetric
                label="绑定设备"
                value={deviceCount}
                status={deviceStatus}
                icon={DevicePhoneMobileIcon}
                tone={fullDeviceCount > 0 ? 'warn' : 'neutral'}
                active={filter === 'deviceFull'}
                onClick={fullDeviceCount > 0 ? () => selectMetric('deviceFull') : undefined}
              />
              </div>
            )}
          </aside>

          <section className="min-w-0 overflow-hidden rounded-xl border border-border/80 bg-card/90 shadow-panel">
          {/* 工具栏：计数 + 搜索 + 筛选 + 视图切换 + 批量 + 排序 */}
          <div className="grid gap-3 border-b border-border/80 px-3.5 py-4 sm:px-4 lg:grid-cols-[auto_minmax(0,1fr)] lg:items-center">
            <h2 className="flex items-baseline gap-2 text-sm font-semibold tracking-tight sm:text-base">
              账号列表
              <span className="text-xs font-normal text-muted-foreground">
                {filtering ? (
                  <>
                    筛选出 <span className="tnum font-medium text-foreground">{total}</span> / 共{' '}
                    <span className="tnum">{count}</span> 个
                  </>
                ) : (
                  <>
                    共 <span className="tnum font-medium text-foreground">{count}</span> 个
                    {enabledCount < count && `（启用 ${enabledCount}）`}
                  </>
                )}
              </span>
            </h2>
            {count > 0 && (
            <div className="min-w-0 space-y-2 lg:flex lg:items-center lg:justify-end lg:space-y-0">
              {/* 搜索：名称或 #id */}
              <div className="relative lg:mr-2">
                <MagnifyingGlassIcon className="pointer-events-none absolute left-2 top-1/2 size-3.5 -translate-y-1/2 text-muted-foreground" />
                <Input
                  value={query}
                  onChange={(e) => resetPage(setQuery)(e.target.value)}
                  placeholder="搜索名称或 #id"
                  aria-label="搜索账号"
                  className="h-10 w-full pl-8 pr-9 text-xs sm:h-9 lg:w-52 2xl:w-64"
                />
                {query && (
                  <Button
                    type="button"
                    size="icon"
                    variant="ghost"
                    className="absolute right-1 top-1/2 size-8 -translate-y-1/2"
                    onClick={() => resetPage(setQuery)('')}
                    title="清除搜索"
                    aria-label="清除搜索"
                  >
                    <XMarkIcon className="size-3" />
                  </Button>
                )}
              </div>

              <div className="grid grid-cols-2 gap-2 sm:flex sm:flex-wrap sm:items-center sm:justify-end">
                {/* 状态筛选 */}
                <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button
                    size="sm"
                    variant={filter === 'all' ? 'outline' : 'secondary'}
                    className="h-10 w-full justify-center gap-1.5 px-2.5 text-xs sm:h-8 sm:w-auto"
                  >
                    <FunnelIcon className="size-3.5" />
                    {FILTERS.find((f) => f.key === filter)!.label}
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end">
                  {FILTERS.map((f) => (
                    <DropdownMenuItem key={f.key} onClick={() => resetPage(setFilter)(f.key)}>
                      <CheckIcon className={cn('size-3.5', filter === f.key ? 'opacity-100' : 'opacity-0')} />
                      {f.label}
                      <span className="ml-auto pl-3 tnum text-2xs text-muted-foreground">
                        {(creds ?? []).filter(f.match).length}
                      </span>
                    </DropdownMenuItem>
                  ))}
                </DropdownMenuContent>
                </DropdownMenu>

                <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button size="sm" variant="outline" className="h-10 w-full justify-center gap-1.5 px-2.5 text-xs sm:h-8 sm:w-auto">
                    <ArrowsUpDownIcon className="size-3.5" />
                    {SORTS.find((s) => s.key === sort)!.label}
                    {dir === 'asc' ? '↑' : '↓'}
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end">
                  {/* 与表头共用 changeSort：选中项再点一次即翻转升/降序 */}
                  {SORTS.map((s) => (
                    <DropdownMenuItem key={s.key} onClick={() => changeSort(s.key)}>
                      <CheckIcon className={cn('size-3.5', sort === s.key ? 'opacity-100' : 'opacity-0')} />
                      {s.label}
                      {sort === s.key && (
                        <span className="ml-auto pl-3 text-2xs text-muted-foreground">
                          {dir === 'asc' ? '升序' : '降序'}
                        </span>
                      )}
                    </DropdownMenuItem>
                  ))}
                </DropdownMenuContent>
                </DropdownMenu>

                {/* 视图切换：卡片 / 紧凑列表 */}
              <div className="grid h-10 grid-cols-2 items-center overflow-hidden rounded-md border border-border sm:flex sm:h-8 sm:shrink-0">
                <Button
                  type="button"
                  size="icon"
                  variant="ghost"
                  className="h-full w-full rounded-none focus-visible:z-10 sm:w-8"
                  onClick={() => switchView('card')}
                  title="卡片视图"
                  aria-label="卡片视图"
                  aria-pressed={view === 'card'}
                >
                  <Squares2X2Icon className="size-4" />
                </Button>
                <Button
                  type="button"
                  size="icon"
                  variant="ghost"
                  className="h-full w-full rounded-none border-l border-border focus-visible:z-10 sm:w-8"
                  onClick={() => switchView('list')}
                  title="紧凑列表视图"
                  aria-label="紧凑列表视图"
                  aria-pressed={view === 'list'}
                >
                  <Bars3Icon className="size-4" />
                </Button>
                </div>

                <Button
                size="sm"
                variant={batch ? 'secondary' : 'outline'}
                className="h-10 w-full justify-center gap-1.5 px-2.5 text-xs sm:h-8 sm:w-auto"
                onClick={() => { setBatch((b) => !b); setSelected(new Set()) }}
                title="批量调整优先级"
              >
                <QueueListIcon className="size-3.5" />
                批量
                </Button>
              </div>
            </div>
            )}
          </div>

        {/* 批量操作条 */}
        {count > 0 && batch && (
          <div className="border-b border-border bg-muted/30 p-3 sm:p-4">
          <BatchActionsBar
            all={sorted}
            selected={selected}
            onSelectedChange={setSelected}
            onClose={() => { setBatch(false); setSelected(new Set()) }}
          />
          </div>
        )}

        {/* 卡片栅格（按当前页切片） */}
        {isLoading ? (
          <LoadingState />
        ) : isError && !creds ? (
          <ErrorState error={credentialsError} onRetry={() => { void refetchCredentials() }} />
        ) : count === 0 ? (
          <EmptyState onAdd={() => setAdding(true)} />
        ) : total === 0 ? (
          <div className="px-4 py-14 text-center">
            <span className="mx-auto grid size-10 place-items-center rounded-md bg-muted text-muted-foreground">
              <MagnifyingGlassIcon className="size-5" />
            </span>
            <p className="mt-3 text-sm font-medium">没有符合条件的账号</p>
            <Button
              variant="outline"
              size="sm"
              className="mt-3"
              onClick={() => { resetPage(setFilter)('all'); setQuery('') }}
            >
              清除筛选与搜索
            </Button>
          </div>
        ) : view === 'list' ? (
          <div>
            <Table className="table-fixed">
              {/* 组件默认可见（mt-4 的说明文字），这里只给读屏用 */}
              <TableCaption className="sr-only">账号列表</TableCaption>
              <CredentialListHeader
                selectable={batch}
                sort={sort}
                dir={dir}
                onSortChange={changeSort}
                // 表头勾选框与批量条的「全选」同义：都作用于当前筛选结果（跨页）。
                allSelected={sorted.length > 0 && sorted.every((c) => selected.has(c.id))}
                onSelectAll={(on) =>
                  setSelected(on ? new Set(sorted.map((c) => c.id)) : new Set())
                }
              />
              <TableBody>
                {pageItems.map((c) => (
                  <CredentialRow
                    key={c.id}
                    cred={c}
                    selectable={batch}
                    selected={selected.has(c.id)}
                    onSelectedChange={(on) => toggleSelected(c.id, on)}
                  />
                ))}
              </TableBody>
            </Table>
          </div>
        ) : (
          // 每张卡至少 27rem；auto-fill 保留空轨，账号少时也不会把单张卡拉满整块内容区。
          <div className="grid items-start gap-3 bg-muted/25 p-2 [grid-template-columns:repeat(auto-fill,minmax(min(100%,27rem),1fr))] sm:gap-4 sm:p-4">
            {pageItems.map((c) => (
              <CredentialCard
                key={c.id}
                cred={c}
                selectable={batch}
                selected={selected.has(c.id)}
                onSelectedChange={(on) => toggleSelected(c.id, on)}
              />
            ))}
          </div>
        )}

        {/* 分页条：实际超过一页才出现。 */}
        {!isLoading && pageCount > 1 && (
          <div className="px-3 pb-3 sm:px-4 sm:pb-4">
          <Pagination
            total={total}
            page={current}
            pageCount={pageCount}
            pageSize={pageSize}
            onPageChange={setPage}
            onPageSizeChange={(n) => { setPageSize(n); setPage(1) }}
          />
          </div>
        )}
          </section>
        </div>
      </main>
      <AppFooter />
      <Toaster position="top-right" />
    </div>
  )
}

/** 分页条：当前区间说明 + 每页条数 + 上/下一页（页码多时只渲染当前页附近的按钮）。 */
function Pagination({
  total, page, pageCount, pageSize, onPageChange, onPageSizeChange,
}: {
  total: number
  page: number
  pageCount: number
  pageSize: number
  onPageChange: (p: number) => void
  onPageSizeChange: (n: number) => void
}) {
  const from = (page - 1) * pageSize + 1
  const to = Math.min(page * pageSize, total)
  // 页码窗口：当前页 ±2，两端夹紧，避免账号多时挤满一行。
  const start = Math.max(1, Math.min(page - 2, pageCount - 4))
  const pages = Array.from({ length: Math.min(5, pageCount) }, (_, i) => start + i)

  return (
    <div className="grid gap-3 border-t border-border/60 pt-4 text-xs text-muted-foreground sm:flex sm:items-center sm:justify-between">
      <span>
        第 <span className="tnum text-foreground">{from}-{to}</span> 个，共{' '}
        <span className="tnum text-foreground">{total}</span> 个账号
      </span>
      <div className="flex items-center justify-between sm:hidden">
        <Button
          size="icon" variant="outline" className="size-9"
          disabled={page <= 1} onClick={() => onPageChange(page - 1)} title="上一页"
          aria-label="上一页"
        >
          <ChevronLeftIcon className="size-4" />
        </Button>
        <span className="tnum text-foreground">第 {page} / {pageCount} 页</span>
        <Button
          size="icon" variant="outline" className="size-9"
          disabled={page >= pageCount} onClick={() => onPageChange(page + 1)} title="下一页"
          aria-label="下一页"
        >
          <ChevronRightIcon className="size-4" />
        </Button>
      </div>
      <div className="hidden items-center gap-1.5 sm:flex">
        <span>每页</span>
        {PAGE_SIZES.map((n) => (
          <Button
            key={n}
            size="sm"
            variant={n === pageSize ? 'secondary' : 'ghost'}
            className="h-7 min-w-7 px-2 text-xs tnum"
            onClick={() => onPageSizeChange(n)}
          >
            {n}
          </Button>
        ))}
        <span className="mx-1 h-4 w-px bg-border" />
        <Button
          size="icon" variant="outline" className="size-7"
          disabled={page <= 1} onClick={() => onPageChange(page - 1)} title="上一页"
          aria-label="上一页"
        >
          <ChevronLeftIcon className="size-3.5" />
        </Button>
        {pages.map((p) => (
          <Button
            key={p}
            size="sm"
            variant={p === page ? 'default' : 'ghost'}
            className="h-7 min-w-7 px-2 text-xs tnum"
            onClick={() => onPageChange(p)}
          >
            {p}
          </Button>
        ))}
        <Button
          size="icon" variant="outline" className="size-7"
          disabled={page >= pageCount} onClick={() => onPageChange(page + 1)} title="下一页"
          aria-label="下一页"
        >
          <ChevronRightIcon className="size-3.5" />
        </Button>
      </div>
    </div>
  )
}

/**
 * 批量操作条：全选/清空 + 优先级 / 设备上限 / 启停 / 删除。
 *
 * 所有操作都作用于**当前筛选结果里被勾选的账号**（跨页保留勾选）。写操作走各自的批量
 * 接口，后端在单事务内完成，不会出现「改了一半」的中间态。
 */
export function BatchActionsBar({
  all, selected, onSelectedChange, onClose,
}: {
  all: Credential[]
  selected: Set<number>
  onSelectedChange: (next: Set<number>) => void
  onClose: () => void
}) {
  const qc = useQueryClient()
  const [priority, setPriority] = useState('0')
  const [limit, setLimit] = useState('')
  const [confirmDelete, setConfirmDelete] = useState(false)

  const ids = [...selected]
  const n = selected.size
  const notify = (msg: string, clearSelection = false) => {
    toast.success(msg)
    qc.invalidateQueries({ queryKey: ['credentials'] })
    if (clearSelection) onSelectedChange(new Set())
  }
  const onError = (e: unknown) => toast.error('批量操作失败', { description: extractError(e) })

  const applyPriority = useMutation({
    mutationFn: (p: number) => setPriorities(ids, p),
    onSuccess: (_r, p) => notify(`已把 ${n} 个账号设为 P${p}`),
    onError,
  })
  const applyLimit = useMutation({
    mutationFn: (v: number) => setDeviceLimits(ids, v),
    onSuccess: (_r, v) =>
      notify(
        v > 0 ? `已把 ${n} 个账号的设备上限设为 ${v}`
          : v === 0 ? `已把 ${n} 个账号改为跟随全局默认上限`
          : `已把 ${n} 个账号设为不限设备数`,
      ),
    onError,
  })
  const applyDisabled = useMutation({
    mutationFn: (d: boolean) => setDisabledMany(ids, d),
    onSuccess: (_r, d) => notify(`已${d ? '停用' : '启用'} ${n} 个账号`),
    onError,
  })
  const applyDelete = useMutation({
    mutationFn: () => deleteCredentials(ids),
    // 账号已不存在，留着勾选没有意义，顺手清空。批量条不会随之卸载，确认框得自己关。
    onSuccess: () => { setConfirmDelete(false); notify(`已删除 ${n} 个账号`, true) },
    onError: (e) => { setConfirmDelete(false); onError(e) },
  })

  const busy =
    applyPriority.isPending || applyLimit.isPending ||
    applyDisabled.isPending || applyDelete.isPending
  const none = n === 0
  const allSelected = all.length > 0 && n === all.length

  return (
    <section className="overflow-hidden rounded-lg border border-border bg-card text-xs">
      <div className="flex items-center gap-3 border-b border-border bg-muted/30 px-3 py-3 sm:px-4">
        <span className="grid size-8 shrink-0 place-items-center rounded-md border border-border bg-background text-muted-foreground">
          <QueueListIcon className="size-4" />
        </span>
        <div className="min-w-0 flex-1">
          <h3 className="text-sm font-semibold">批量操作</h3>
          <p className="mt-0.5 text-2xs text-muted-foreground" aria-live="polite">
            已选 <span className="tnum font-semibold text-foreground">{n}</span> / {all.length} 个账号
          </p>
        </div>
        <Button
          size="sm"
          variant={allSelected ? 'secondary' : 'outline'}
          className="h-8 shrink-0 px-2.5 text-xs"
          title="选择当前筛选结果中的全部账号，包含其他分页"
          onClick={() => onSelectedChange(allSelected ? new Set() : new Set(all.map((c) => c.id)))}
        >
          {allSelected ? <><CheckIcon className="size-3.5" />已全选</> : '全选'}
        </Button>
        <Button size="icon" variant="ghost" className="size-8 shrink-0" onClick={onClose} title="退出批量模式" aria-label="退出批量模式">
          <XMarkIcon className="size-4" />
        </Button>
      </div>

      <div className="grid gap-3 p-3 sm:p-4 lg:grid-cols-[minmax(0,0.8fr)_minmax(0,1.2fr)]">
        <div className="rounded-md border border-border bg-background p-3">
          <div className="mb-2">
            <h4 className="text-xs font-medium">快捷操作</h4>
          </div>
          <div className="grid grid-cols-3 gap-2">
          <Button
            size="sm" variant="outline" className="w-full px-2 text-xs text-ok hover:text-ok"
            disabled={none || busy}
            onClick={() => applyDisabled.mutate(false)}
          >
            <PlayIcon className="size-3.5" />启用
          </Button>
          <Button
            size="sm" variant="outline" className="w-full px-2 text-xs"
            disabled={none || busy}
            onClick={() => applyDisabled.mutate(true)}
          >
            <PauseIcon className="size-3.5" />停用
          </Button>
          <Button
            size="sm" variant="outline" className="w-full px-2 text-xs text-bad hover:text-bad"
            disabled={none || busy}
            onClick={() => setConfirmDelete(true)}
          >
            <TrashIcon className="size-3.5" />删除
          </Button>
          </div>
          {none && <p className="mt-2.5 text-2xs text-muted-foreground">请先勾选需要处理的账号</p>}
        </div>

        <div className="overflow-hidden rounded-md border border-border bg-muted/20 sm:grid sm:grid-cols-2 sm:divide-x sm:divide-y-0">
          <div className="border-b border-border p-3 sm:border-b-0">
            <div className="mb-2 flex items-baseline justify-between gap-3">
              <label htmlFor="batch-priority" className="text-xs font-medium">调度优先级</label>
              <span className="text-2xs text-muted-foreground">数值越小越优先</span>
            </div>
            <div className="flex items-center gap-2">
              <Input
                id="batch-priority"
                type="number"
                value={priority}
                onChange={(e) => setPriority(e.target.value)}
                className="h-8 min-w-0 flex-1 font-mono text-xs"
                aria-label="批量设置优先级"
              />
              <Button
                size="sm" className="h-8 shrink-0 px-2.5 text-xs"
                disabled={none || busy}
                onClick={() => applyPriority.mutate(Math.floor(Number(priority) || 0))}
              >
                {applyPriority.isPending ? <ArrowPathIcon className="size-3.5 animate-spin" /> : <CheckIcon className="size-3.5" />}
                应用
              </Button>
            </div>
          </div>

          <div className="p-3">
            <div className="mb-2 flex items-baseline justify-between gap-3">
              <label htmlFor="batch-device-limit" className="text-xs font-medium">设备上限</label>
              <span className="text-2xs text-muted-foreground">空=默认 · 0=不限</span>
            </div>
            <div className="flex items-center gap-2">
              <Input
                id="batch-device-limit"
                type="number"
                min={0}
                value={limit}
                onChange={(e) => setLimit(e.target.value)}
                placeholder="默认"
                className="h-8 min-w-0 flex-1 font-mono text-xs"
                aria-label="批量设置设备上限"
              />
              <Button
                size="sm" className="h-8 shrink-0 px-2.5 text-xs"
                disabled={none || busy}
                onClick={() => applyLimit.mutate(inputToLimit(limit))}
              >
                {applyLimit.isPending ? <ArrowPathIcon className="size-3.5 animate-spin" /> : <CheckIcon className="size-3.5" />}
                应用
              </Button>
            </div>
          </div>
        </div>
      </div>

      {/* 删号会连带清掉历史用量与设备绑定，批量更需要明确确认。 */}
      <ConfirmDialog
        open={confirmDelete}
        onOpenChange={setConfirmDelete}
        title={`删除 ${n} 个账号`}
        confirmText={`删除 ${n} 个`}
        pending={applyDelete.isPending}
        onConfirm={() => applyDelete.mutate()}
        description={
          <>
            确定删除选中的 <span className="font-medium text-foreground">{n}</span> 个账号？
            它们的历史用量记录与设备绑定将一并清除，不可恢复。
          </>
        }
      />
    </section>
  )
}

function EmptyState({ onAdd }: { onAdd: () => void }) {
  return (
    <div className="px-5 py-14 text-center sm:py-16">
      <span className="mx-auto grid size-11 place-items-center rounded-lg bg-brand-soft text-brand">
        <PlusIcon className="size-5" />
      </span>
      <p className="mt-3 text-sm font-semibold">建立第一个调度账号</p>
      <p className="mx-auto mt-1 max-w-sm text-xs leading-5 text-muted-foreground">
        完成 Claude OAuth 授权后，账号会加入当前网关的调度池。
      </p>
      <Button className="mt-4" onClick={onAdd}>
        <PlusIcon />
        添加第一个账号
      </Button>
    </div>
  )
}

function ErrorState({ error, onRetry }: { error: unknown; onRetry: () => void }) {
  return (
    <div className="px-5 py-14 text-center sm:py-16">
      <span className="mx-auto grid size-11 place-items-center rounded-lg bg-bad-soft text-bad">
        <ExclamationTriangleIcon className="size-5" />
      </span>
      <p className="mt-3 text-sm font-semibold">暂时无法读取账号</p>
      <p className="mx-auto mt-1 max-w-md text-xs leading-5 text-muted-foreground">
        {extractError(error)}
      </p>
      <Button variant="outline" className="mt-4" onClick={onRetry}>
        <ArrowPathIcon />
        重新加载
      </Button>
    </div>
  )
}

function LoadingState({ fullPage = false }: { fullPage?: boolean }) {
  return (
    <div className={cn('grid place-items-center', fullPage ? 'min-h-dvh' : 'py-16')}>
      <div className="flex items-center gap-2 text-sm text-muted-foreground" role="status" aria-live="polite">
        <ArrowPathIcon className="size-4 animate-spin" />
        加载中
      </div>
    </div>
  )
}

export default App
