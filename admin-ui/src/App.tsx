import { useEffect, useMemo, useRef, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  PlusIcon, SettingsIcon, LogOutIcon, ArrowUpDownIcon,
  ListChecksIcon, RefreshCwIcon, XIcon, ChevronLeftIcon, ChevronRightIcon,
  SearchIcon, ListFilterIcon, LayoutGridIcon, ListIcon,
  PlayIcon, PauseIcon, Trash2Icon,
  RadioIcon, ShieldCheckIcon, TriangleAlertIcon, SmartphoneIcon,
  EllipsisVerticalIcon,
} from 'lucide-react'
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
  SORTS, SORT_DIR_DEFAULT, SORT_KEYS, isAbnormal, isNearLimit, sortCreds,
  type SortDir, type SortKey,
} from '@/components/credential-shared'
import { CredentialCard } from '@/components/credential-card'
import { CredentialLoadingState } from '@/components/credential-loading'
import { CredentialListHeader, CredentialRow } from '@/components/credential-row'
import { AddAccount } from '@/components/add-account'
import { SettingsPage, type SettingsSection } from '@/components/settings-page'
import { LoginPage } from '@/components/login-page'
import { AppFooter } from '@/components/app-footer'
import { OverviewMetric, OverviewMetricSkeleton } from '@/components/overview-metric'
import { LogoMark } from '@/components/logo-mark'
import { Button } from '@/components/ui/button'
import { Table, TableBody, TableCaption } from '@/components/ui/table'
import {
  Menu, MenuTrigger, MenuPopup, MenuItem, MenuRadioGroup, MenuRadioItem, MenuSeparator,
} from '@/components/ui/menu'
import {
  Card, CardPanel, CardFrame, CardFrameAction, CardFrameDescription,
  CardFrameFooter, CardFrameHeader, CardFrameTitle, CardHeader, CardTitle,
  CardDescription, CardAction,
} from '@/components/ui/card'
import { Toolbar, ToolbarGroup, ToolbarSeparator } from '@/components/ui/toolbar'
import { InputGroup, InputGroupAddon, InputGroupInput } from '@/components/ui/input-group'
import { ToggleGroup, ToggleGroupItem, ToggleGroupSeparator } from '@/components/ui/toggle-group'
import {
  Pagination as CossPagination, PaginationContent, PaginationItem, PaginationLink,
} from '@/components/ui/pagination'
import { Select, SelectItem, SelectPopup, SelectTrigger, SelectValue } from '@/components/ui/select'
import {
  Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle,
} from '@/components/ui/empty'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import {
  AlertDialog, AlertDialogClose, AlertDialogDescription, AlertDialogFooter,
  AlertDialogHeader, AlertDialogPopup, AlertDialogTitle,
} from '@/components/ui/alert-dialog'
import { Checkbox } from '@/components/ui/checkbox'
import {
  NumberField, NumberFieldDecrement, NumberFieldGroup, NumberFieldIncrement,
  NumberFieldInput,
} from '@/components/ui/number-field'
import { Spinner } from '@/components/ui/spinner'
import { toastManager } from '@/components/ui/toast'

type FilterKey =
  | 'all'
  | 'schedulable'
  | 'attention'
  | 'enabled'
  | 'disabled'
  | 'abnormal'
  | 'nearLimit'
  | 'cooldown'
  | 'hasDevice'
  | 'deviceFull'
type ViewMode = 'card' | 'list'

/** 每页账号数可选档位（用 10/20/50 这类常规档，不迁就栅格列数）；账号少时分页条自动隐藏。 */
const PAGE_SIZES = [10, 20, 50] as const
const PAGE_SIZE_ITEMS = PAGE_SIZES.map((size) => ({ value: String(size), label: `${size} 个` }))

const LIMIT_MODE_ITEMS = [
  { value: 'default', label: '跟随默认' },
  { value: 'unlimited', label: '不限设备' },
  { value: 'custom', label: '独立上限' },
] as const

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
  { key: 'hasDevice', label: '已绑定设备', match: (c) => c.device_count > 0 },
  {
    key: 'deviceFull',
    label: '设备已满',
    match: (c) => c.device_limit_effective > 0 && c.device_count >= c.device_limit_effective,
  },
]
const FILTER_KEYS = FILTERS.map((filter) => filter.key)

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
  // 只有从账号页主动进入设置时，关闭设置才应该消费这条 history 记录。
  // 直接打开 #/settings/* 的深链接则在原地替换回账号页，避免把用户带离当前站点。
  const enteredSettingsFromAccounts = useRef(false)

  // 界面偏好与检索条件都写入 localStorage，刷新后保持当前工作上下文。
  const [sort, setSort] = usePersisted<SortKey>('sort', 'priority', oneOf(SORT_KEYS))
  const [dir, setDir] = usePersisted<SortDir>('sortDir', 'asc', oneOf(['asc', 'desc'] as const))
  const [pageSize, setPageSize] = usePersisted('pageSize', PAGE_SIZES[0], numberOneOf(PAGE_SIZES))
  const [view, switchView] = usePersisted<ViewMode>('view', preferredInitialView(), oneOf(VIEW_MODES))
  const [filter, setFilter] = usePersisted<FilterKey>('filter', 'all', oneOf(FILTER_KEYS))
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
  const attentionStatus = [
    abnormalCount > 0 ? `${abnormalCount} 异常` : '',
    cooldownCount > 0 ? `${cooldownCount} 冷却` : '',
    nearLimitCount > 0 ? `${nearLimitCount} 额度` : '',
  ].filter(Boolean).join(' · ') || undefined
  const deviceStatus =
    fullDeviceCount > 0 ? `${fullDeviceCount} 个账号已满`
      : unlimitedDeviceAccounts > 0 ? `${unlimitedDeviceAccounts} 个不限额账号`
        : deviceCapacity > 0 ? `共 ${deviceCapacity} 个名额`
          : undefined
  const selectMetric = (key: FilterKey) =>
    resetPage(setFilter)(filter === key ? 'all' : key)

  return (
    <div className="app-shell flex min-h-dvh flex-col text-foreground">
      <header className="app-header sticky top-0 z-20 border-b bg-background/92 backdrop-blur-md">
        <div className="page-frame flex h-16 items-center justify-between gap-3">
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
              <MenuTrigger render={<Button size="icon-lg" variant="outline" aria-label="更多操作" />}>
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

      <main className="page-frame relative flex-1 py-6 pb-10 sm:py-8 sm:pb-12">
        {/* 添加账号保持为短流程弹框；复杂设置使用独立页面。 */}
        <AddAccount open={adding} onOpenChange={setAdding} />

        <div className="space-y-6 sm:space-y-8">
          <section className="sm:flex sm:items-end sm:justify-between sm:gap-8" aria-labelledby="page-title">
            <div className="min-w-0">
              <h1 id="page-title" className="text-2xl font-semibold tracking-tight sm:text-3xl">账号调度中心</h1>
              <p className="mt-2 max-w-2xl text-sm leading-6 text-muted-foreground">
                统一查看账号健康、额度与设备容量，快速处理会影响转发的状态。
              </p>
            </div>
            <div className="mt-4 flex shrink-0 items-center gap-1.5 text-xs text-muted-foreground sm:mt-0 sm:pb-0.5">
                {isRefetchError ? (
                  <>
                    <TriangleAlertIcon className="size-3.5 text-destructive-foreground" aria-hidden />
                    <button
                      type="button"
                      className="rounded-sm font-medium text-destructive-foreground underline-offset-2 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/60"
                      onClick={() => { void refetchCredentials() }}
                    >
                      刷新失败，点击重试
                    </button>
                  </>
                ) : (
                  <>
                    {isFetching && !isLoading ? (
                      <RefreshCwIcon className="size-3.5 animate-spin" aria-hidden />
                    ) : (
                      <span className="size-1.5 rounded-full bg-success" aria-hidden />
                    )}
                    <span>每 30 秒自动刷新</span>
                  </>
                )}
            </div>
          </section>

          {isLoading ? (
            <section
              aria-label="正在加载账号池概览"
              className="grid grid-cols-2 gap-3 lg:grid-cols-4"
            >
              <OverviewMetricSkeleton />
              <OverviewMetricSkeleton />
              <OverviewMetricSkeleton />
              <OverviewMetricSkeleton />
            </section>
          ) : count > 0 && (
            <section
              aria-label="账号池概览"
              className="grid grid-cols-2 gap-3 lg:grid-cols-4"
            >
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
                icon={TriangleAlertIcon}
                tone={abnormalCount > 0 ? 'bad' : attentionCount > 0 ? 'warn' : 'neutral'}
                active={filter === 'attention'}
                onClick={() => selectMetric('attention')}
              />
              <OverviewMetric
                label="额度预警"
                value={nearLimitCount}
                status={nearLimitCount > 0 ? '使用率 ≥ 90%' : undefined}
                icon={RadioIcon}
                tone={nearLimitCount > 0 ? 'warn' : 'neutral'}
                active={filter === 'nearLimit'}
                onClick={() => selectMetric('nearLimit')}
              />
              <OverviewMetric
                label="绑定设备"
                value={deviceCount}
                status={deviceStatus}
                icon={SmartphoneIcon}
                tone={fullDeviceCount > 0 ? 'warn' : 'neutral'}
                active={filter === 'deviceFull' || filter === 'hasDevice'}
                onClick={() => selectMetric(fullDeviceCount > 0 ? 'deviceFull' : 'hasDevice')}
              />
            </section>
          )}

          <section className="min-w-0" aria-labelledby="account-list-title">
            <CardFrame>
              <CardFrameHeader>
                <CardFrameTitle id="account-list-title">账号列表</CardFrameTitle>
                <CardFrameDescription>
                  {filtering ? (
                    <>筛选出 <span className="tnum font-medium text-foreground">{total}</span> / 共 <span className="tnum">{count}</span> 个</>
                  ) : (
                    <>共 <span className="tnum font-medium text-foreground">{count}</span> 个{enabledCount < count && ` · 启用 ${enabledCount}`}</>
                  )}
                </CardFrameDescription>
                {count > 0 && (
                  <CardFrameAction>
                    <span className="hidden text-xs text-muted-foreground md:inline">筛选条件会在刷新后保留</span>
                  </CardFrameAction>
                )}
              </CardFrameHeader>

              {count > 0 && (
                <Card>
                  <CardPanel className="p-3">
                    <Toolbar className="w-full flex-col items-stretch sm:flex-row sm:flex-wrap sm:items-center">
                      <InputGroup className="sm:min-w-56 sm:flex-1 xl:max-w-72">
                        <InputGroupAddon><SearchIcon /></InputGroupAddon>
                        <InputGroupInput
                          value={query}
                          onChange={(event) => resetPage(setQuery)(event.target.value)}
                          placeholder="搜索名称或 #id"
                          aria-label="搜索账号"
                        />
                        {query && (
                          <InputGroupAddon align="inline-end">
                            <Button size="icon-xs" variant="ghost" onClick={() => resetPage(setQuery)('')} aria-label="清除搜索">
                              <XIcon />
                            </Button>
                          </InputGroupAddon>
                        )}
                      </InputGroup>

                      <ToolbarSeparator orientation="vertical" className="hidden sm:block" />
                      <ToolbarGroup className="grid grid-cols-2 sm:flex sm:flex-wrap">
                        <Menu>
                          <MenuTrigger render={<Button variant={filter === 'all' ? 'outline' : 'secondary'} />}>
                            <ListFilterIcon />
                            {FILTERS.find((item) => item.key === filter)!.label}
                          </MenuTrigger>
                          <MenuPopup align="end" className="w-52">
                            <MenuRadioGroup value={filter}>
                              {FILTERS.map((item) => (
                                <MenuRadioItem key={item.key} value={item.key} onClick={() => resetPage(setFilter)(item.key)}>
                                  <span className="flex min-w-0 flex-1 items-center justify-between gap-4">
                                    <span>{item.label}</span>
                                    <span className="tnum text-xs text-muted-foreground">{pool.filter(item.match).length}</span>
                                  </span>
                                </MenuRadioItem>
                              ))}
                            </MenuRadioGroup>
                          </MenuPopup>
                        </Menu>

                        <Menu>
                          <MenuTrigger render={<Button variant="outline" />}>
                            <ArrowUpDownIcon />
                            {SORTS.find((item) => item.key === sort)!.label} {dir === 'asc' ? '↑' : '↓'}
                          </MenuTrigger>
                          <MenuPopup align="end" className="w-48">
                            <MenuRadioGroup value={sort}>
                              {SORTS.map((item) => (
                                <MenuRadioItem key={item.key} value={item.key} onClick={() => changeSort(item.key)}>
                                  <span className="flex min-w-0 flex-1 items-center justify-between gap-4">
                                    <span>{item.label}</span>
                                    {sort === item.key && <span className="text-xs text-muted-foreground">{dir === 'asc' ? '升序' : '降序'}</span>}
                                  </span>
                                </MenuRadioItem>
                              ))}
                            </MenuRadioGroup>
                          </MenuPopup>
                        </Menu>
                      </ToolbarGroup>

                      <ToolbarSeparator orientation="vertical" className="hidden sm:block" />
                      <ToolbarGroup className="justify-between">
                        <ToggleGroup
                          value={[view]}
                          onValueChange={(values) => {
                            const next = values[values.length - 1]
                            if (next === 'card' || next === 'list') switchView(next)
                          }}
                          variant="outline"
                          aria-label="账号视图"
                        >
                          <ToggleGroupItem value="card" aria-label="卡片视图" title="卡片视图"><LayoutGridIcon /></ToggleGroupItem>
                          <ToggleGroupSeparator />
                          <ToggleGroupItem value="list" aria-label="列表视图" title="列表视图"><ListIcon /></ToggleGroupItem>
                        </ToggleGroup>
                        <Button
                          variant={batch ? 'secondary' : 'outline'}
                          onClick={() => { setBatch((value) => !value); setSelected(new Set()) }}
                        >
                          <ListChecksIcon />批量
                        </Button>
                      </ToolbarGroup>
                    </Toolbar>
                  </CardPanel>
                </Card>
              )}

              {count > 0 && batch && (
                <div className="relative p-4 pb-0">
                  <BatchActionsBar
                    all={sorted}
                    selected={selected}
                    onSelectedChange={setSelected}
                    onClose={() => { setBatch(false); setSelected(new Set()) }}
                  />
                </div>
              )}

              {isLoading ? (
                <div className="relative p-4"><CredentialLoadingState view={view} selectable={batch} /></div>
              ) : isError && !creds ? (
                <Card><ErrorState error={credentialsError} onRetry={() => { void refetchCredentials() }} /></Card>
              ) : count === 0 ? (
                <Card><EmptyState onAdd={() => setAdding(true)} /></Card>
              ) : total === 0 ? (
                <Card>
                  <Empty>
                    <EmptyHeader>
                      <EmptyMedia variant="icon"><SearchIcon /></EmptyMedia>
                      <EmptyTitle>没有符合条件的账号</EmptyTitle>
                      <EmptyDescription>尝试清除当前筛选条件或搜索关键字。</EmptyDescription>
                    </EmptyHeader>
                    <EmptyContent>
                      <Button variant="outline" onClick={() => { resetPage(setFilter)('all'); setQuery('') }}>清除筛选与搜索</Button>
                    </EmptyContent>
                  </Empty>
                </Card>
              ) : view === 'list' ? (
                <Table variant="card" className="table-fixed">
                  <TableCaption className="sr-only">账号列表</TableCaption>
                  <CredentialListHeader
                    selectable={batch}
                    sort={sort}
                    dir={dir}
                    onSortChange={changeSort}
                    allSelected={sorted.length > 0 && sorted.every((item) => selected.has(item.id))}
                    onSelectAll={(on) => setSelected(on ? new Set(sorted.map((item) => item.id)) : new Set())}
                  />
                  <TableBody>
                    {pageItems.map((item) => (
                      <CredentialRow
                        key={item.id}
                        cred={item}
                        selectable={batch}
                        selected={selected.has(item.id)}
                        onSelectedChange={(on) => toggleSelected(item.id, on)}
                      />
                    ))}
                  </TableBody>
                </Table>
              ) : (
                <div className="relative grid items-stretch gap-4 p-4 [grid-template-columns:repeat(auto-fill,minmax(min(100%,27rem),1fr))]">
                  {pageItems.map((item) => (
                    <CredentialCard
                      key={item.id}
                      cred={item}
                      selectable={batch}
                      selected={selected.has(item.id)}
                      onSelectedChange={(on) => toggleSelected(item.id, on)}
                    />
                  ))}
                </div>
              )}

              {!isLoading && pageCount > 1 && (
                <CardFrameFooter className="relative">
                  <AccountPagination
                    total={total}
                    page={current}
                    pageCount={pageCount}
                    pageSize={pageSize}
                    onPageChange={setPage}
                    onPageSizeChange={(size) => { setPageSize(size); setPage(1) }}
                  />
                </CardFrameFooter>
              )}
            </CardFrame>
          </section>
        </div>
      </main>
      <AppFooter />
    </div>
  )
}

/** 分页条：当前区间说明 + 每页条数 + 上/下一页（页码多时只渲染当前页附近的按钮）。 */
function AccountPagination({
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
  const navigate = (event: React.MouseEvent<HTMLAnchorElement>, next: number) => {
    event.preventDefault()
    if (next >= 1 && next <= pageCount) onPageChange(next)
  }

  return (
    <div className="grid gap-3 text-xs text-muted-foreground md:grid-cols-[1fr_auto_1fr] md:items-center">
      <span>
        第 <span className="tnum text-foreground">{from}-{to}</span> 个，共{' '}
        <span className="tnum text-foreground">{total}</span> 个账号
      </span>
      <CossPagination className="justify-start md:justify-center">
        <PaginationContent>
          <PaginationItem>
            <PaginationLink
              href="#"
              size="icon-sm"
              className={cn(page <= 1 && 'pointer-events-none opacity-50')}
              aria-disabled={page <= 1}
              aria-label="上一页"
              onClick={(event) => navigate(event, page - 1)}
            >
              <ChevronLeftIcon />
            </PaginationLink>
          </PaginationItem>
          {pages.map((item) => (
            <PaginationItem key={item} className="max-sm:hidden">
              <PaginationLink
                href="#"
                size="icon-sm"
                isActive={item === page}
                onClick={(event) => navigate(event, item)}
              >
                <span className="tnum">{item}</span>
              </PaginationLink>
            </PaginationItem>
          ))}
          <PaginationItem className="sm:hidden">
            <span className="tnum px-2 text-foreground">{page} / {pageCount}</span>
          </PaginationItem>
          <PaginationItem>
            <PaginationLink
              href="#"
              size="icon-sm"
              className={cn(page >= pageCount && 'pointer-events-none opacity-50')}
              aria-disabled={page >= pageCount}
              aria-label="下一页"
              onClick={(event) => navigate(event, page + 1)}
            >
              <ChevronRightIcon />
            </PaginationLink>
          </PaginationItem>
        </PaginationContent>
      </CossPagination>
      <div className="flex items-center gap-2 md:justify-self-end">
        <span>每页</span>
        <Select
          items={PAGE_SIZE_ITEMS}
          value={String(pageSize)}
          onValueChange={(value) => {
            const next = Number(value)
            if (PAGE_SIZES.includes(next as (typeof PAGE_SIZES)[number])) onPageSizeChange(next)
          }}
        >
          <SelectTrigger aria-label="每页账号数" size="sm" className="min-w-20"><SelectValue /></SelectTrigger>
          <SelectPopup align="end">
            {PAGE_SIZE_ITEMS.map((item) => <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>)}
          </SelectPopup>
        </Select>
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
  const [priority, setPriority] = useState(0)
  const [limitMode, setLimitMode] = useState<'default' | 'unlimited' | 'custom'>('default')
  const [customLimit, setCustomLimit] = useState(1)
  const [confirmDelete, setConfirmDelete] = useState(false)

  const ids = [...selected]
  const n = selected.size
  const notify = (msg: string, clearSelection = false) => {
    toastManager.add({ title: msg, type: 'success' })
    qc.invalidateQueries({ queryKey: ['credentials'] })
    if (clearSelection) onSelectedChange(new Set())
  }
  const onError = (error: unknown) => toastManager.add({
    title: '批量操作失败',
    description: extractError(error),
    type: 'error',
  })

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
  const deviceLimit = limitMode === 'default' ? 0 : limitMode === 'unlimited' ? -1 : Math.max(1, Math.floor(customLimit))

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">批量操作</CardTitle>
        <CardDescription aria-live="polite">
          已选 <span className="tnum font-medium text-foreground">{n}</span> / {all.length} 个账号
        </CardDescription>
        <CardAction>
          <Button size="icon-sm" variant="ghost" onClick={onClose} title="退出批量模式" aria-label="退出批量模式">
            <XIcon />
          </Button>
        </CardAction>
      </CardHeader>
      <CardPanel className="space-y-5">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <label className="flex cursor-pointer items-center gap-2 text-sm font-medium">
            <Checkbox
              checked={allSelected}
              indeterminate={n > 0 && !allSelected}
              onCheckedChange={(checked) => onSelectedChange(checked ? new Set(all.map((item) => item.id)) : new Set())}
            />
            选择当前筛选结果
          </label>
          <Toolbar className="p-1">
            <Button size="sm" variant="outline" disabled={none || busy} loading={applyDisabled.isPending && applyDisabled.variables === false} onClick={() => applyDisabled.mutate(false)}>
              <PlayIcon />启用
            </Button>
            <Button size="sm" variant="outline" disabled={none || busy} loading={applyDisabled.isPending && applyDisabled.variables === true} onClick={() => applyDisabled.mutate(true)}>
              <PauseIcon />停用
            </Button>
            <Button size="sm" variant="destructive-outline" disabled={none || busy} onClick={() => setConfirmDelete(true)}>
              <Trash2Icon />删除
            </Button>
          </Toolbar>
        </div>

        {none && (
          <Alert>
            <ListChecksIcon />
            <AlertTitle>尚未选择账号</AlertTitle>
            <AlertDescription>先在当前筛选结果中勾选需要处理的账号。</AlertDescription>
          </Alert>
        )}

        <div className="grid gap-4 lg:grid-cols-2">
          <div className="space-y-2">
            <div>
              <div className="text-sm font-medium">调度优先级</div>
              <div className="text-xs text-muted-foreground">数值越小越优先</div>
            </div>
            <div className="flex items-end gap-2">
              <NumberField
                id="batch-priority"
                value={priority}
                min={0}
                step={1}
                size="sm"
                onValueChange={(value) => setPriority(Math.max(0, Math.floor(value ?? 0)))}
              >
                <NumberFieldGroup>
                  <NumberFieldDecrement />
                  <NumberFieldInput aria-label="批量设置优先级" />
                  <NumberFieldIncrement />
                </NumberFieldGroup>
              </NumberField>
              <Button size="sm" loading={applyPriority.isPending} disabled={none || busy} onClick={() => applyPriority.mutate(priority)}>
                应用
              </Button>
            </div>
          </div>

          <div className="space-y-2">
            <div>
              <div className="text-sm font-medium">设备上限</div>
              <div className="text-xs text-muted-foreground">明确选择默认、不限或独立上限</div>
            </div>
            <div className="flex items-end gap-2">
              <Select items={LIMIT_MODE_ITEMS} value={limitMode} onValueChange={(value) => value && setLimitMode(value as typeof limitMode)}>
                <SelectTrigger aria-label="批量设置设备上限策略" size="sm" className="min-w-28"><SelectValue /></SelectTrigger>
                <SelectPopup>
                  {LIMIT_MODE_ITEMS.map((item) => (
                    <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>
                  ))}
                </SelectPopup>
              </Select>
              {limitMode === 'custom' && (
                <NumberField value={customLimit} min={1} step={1} size="sm" onValueChange={(value) => setCustomLimit(Math.max(1, Math.floor(value ?? 1)))}>
                  <NumberFieldGroup>
                    <NumberFieldDecrement />
                    <NumberFieldInput aria-label="批量设置独立设备上限" />
                    <NumberFieldIncrement />
                  </NumberFieldGroup>
                </NumberField>
              )}
              <Button size="sm" loading={applyLimit.isPending} disabled={none || busy} onClick={() => applyLimit.mutate(deviceLimit)}>
                应用
              </Button>
            </div>
          </div>
        </div>
      </CardPanel>

      <AlertDialog open={confirmDelete} onOpenChange={setConfirmDelete}>
        <AlertDialogPopup>
          <AlertDialogHeader>
            <AlertDialogTitle>删除 {n} 个账号</AlertDialogTitle>
            <AlertDialogDescription>
              确定删除选中的 {n} 个账号？历史用量记录与设备绑定将一并清除，且无法恢复。
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogClose render={<Button variant="outline" />}>取消</AlertDialogClose>
            <Button variant="destructive" loading={applyDelete.isPending} onClick={() => applyDelete.mutate()}>
              删除 {n} 个
            </Button>
          </AlertDialogFooter>
        </AlertDialogPopup>
      </AlertDialog>
    </Card>
  )
}

function EmptyState({ onAdd }: { onAdd: () => void }) {
  return (
    <Empty>
      <EmptyHeader>
        <EmptyMedia variant="icon"><PlusIcon /></EmptyMedia>
        <EmptyTitle>建立第一个调度账号</EmptyTitle>
        <EmptyDescription>完成 Claude OAuth 授权后，账号会加入当前网关的调度池。</EmptyDescription>
      </EmptyHeader>
      <EmptyContent>
        <Button onClick={onAdd}><PlusIcon />添加第一个账号</Button>
      </EmptyContent>
    </Empty>
  )
}

function ErrorState({ error, onRetry }: { error: unknown; onRetry: () => void }) {
  return (
    <Empty role="alert">
      <EmptyHeader>
        <EmptyMedia variant="icon"><TriangleAlertIcon /></EmptyMedia>
        <EmptyTitle>暂时无法读取账号</EmptyTitle>
        <EmptyDescription className="break-words">{extractError(error)}</EmptyDescription>
      </EmptyHeader>
      <EmptyContent>
        <Button variant="outline" onClick={onRetry}><RefreshCwIcon />重新加载</Button>
      </EmptyContent>
    </Empty>
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
