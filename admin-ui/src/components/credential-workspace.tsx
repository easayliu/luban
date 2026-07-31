import { useEffect, useMemo, useState } from 'react'
import {
  ArrowUpDownIcon,
  ChevronLeftIcon,
  ChevronRightIcon,
  LayoutGridIcon,
  ListFilterIcon,
  ListIcon,
  PlusIcon,
  RadioIcon,
  RefreshCwIcon,
  SearchIcon,
  ShieldCheckIcon,
  SmartphoneIcon,
  TriangleAlertIcon,
  XIcon,
} from 'lucide-react'
import type { Credential } from '@/api/credentials'
import { BatchActionsBar } from '@/components/batch-actions-bar'
import { CredentialCard } from '@/components/credential-card'
import { CredentialLoadingState } from '@/components/credential-loading'
import {
  SORTS,
  SORT_DIR_DEFAULT,
  evaluateCredential,
  sortCreds,
  type CredentialEvaluation,
  type SortDir,
  type SortKey,
} from '@/components/credential-shared'
import { CredentialListHeader, CredentialRow } from '@/components/credential-row'
import { OverviewMetric, OverviewMetricSkeleton } from '@/components/overview-metric'
import { Button, buttonVariants } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import {
  Empty,
  EmptyContent,
  EmptyDescription,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
} from '@/components/ui/empty'
import { InputGroup, InputGroupAddon, InputGroupInput } from '@/components/ui/input-group'
import {
  Menu,
  MenuPopup,
  MenuRadioGroup,
  MenuRadioItem,
  MenuTrigger,
} from '@/components/ui/menu'
import {
  Pagination as CossPagination,
  PaginationContent,
  PaginationItem,
  PaginationLink,
} from '@/components/ui/pagination'
import { Select, SelectItem, SelectPopup, SelectTrigger, SelectValue } from '@/components/ui/select'
import { Table, TableBody, TableCaption } from '@/components/ui/table'
import { ToggleGroup, ToggleGroupItem, ToggleGroupSeparator } from '@/components/ui/toggle-group'
import { Toolbar, ToolbarGroup, ToolbarSeparator } from '@/components/ui/toolbar'
import { useDebounced } from '@/lib/use-debounced'
import { cn, extractError } from '@/lib/utils'

export type CredentialFilterKey =
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

export type CredentialViewMode = 'card' | 'list'

export const CREDENTIAL_PAGE_SIZES = [10, 20, 50] as const
export type CredentialPageSize = (typeof CREDENTIAL_PAGE_SIZES)[number]

export const CREDENTIAL_VIEW_MODES = ['card', 'list'] as const

const PAGE_SIZE_ITEMS = CREDENTIAL_PAGE_SIZES.map((size) => ({
  value: String(size),
  label: `${size} 个`,
}))

const FILTERS: {
  key: CredentialFilterKey
  label: string
  match: (evaluation: CredentialEvaluation) => boolean
}[] = [
  { key: 'all', label: '全部', match: () => true },
  {
    key: 'schedulable',
    label: '可调度',
    match: (evaluation) => evaluation.schedulable,
  },
  {
    key: 'attention',
    label: '需处理',
    match: (evaluation) => evaluation.needsAttention,
  },
  { key: 'enabled', label: '启用', match: ({ credential }) => !credential.disabled },
  { key: 'disabled', label: '停用', match: ({ credential }) => credential.disabled },
  { key: 'abnormal', label: '异常（已封禁）', match: ({ credential }) => !!credential.ban_reason },
  { key: 'nearLimit', label: '额度风险', match: (evaluation) => evaluation.quotaRisk },
  {
    key: 'cooldown',
    label: '冷却中',
    match: ({ credential }) => !credential.disabled && credential.rate_limited_secs > 0,
  },
  { key: 'hasDevice', label: '已绑定设备', match: ({ credential }) => credential.device_count > 0 },
  {
    key: 'deviceFull',
    label: '设备已满',
    match: ({ credential }) =>
      credential.device_limit_effective > 0
      && credential.device_count >= credential.device_limit_effective,
  },
]

export const CREDENTIAL_FILTER_KEYS = FILTERS.map((filter) => filter.key)

export function preferredInitialCredentialView(): CredentialViewMode {
  return typeof window !== 'undefined' && window.matchMedia('(min-width: 80rem)').matches
    ? 'list'
    : 'card'
}

/** 额度 reset 与相对时间都依赖当前时刻；30 秒 tick 与接口刷新节奏一致。 */
function useNowSeconds(): number {
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000))

  useEffect(() => {
    const update = () => setNow(Math.floor(Date.now() / 1000))
    const onVisibilityChange = () => {
      if (!document.hidden) update()
    }
    const interval = window.setInterval(update, 30_000)
    window.addEventListener('focus', update)
    document.addEventListener('visibilitychange', onVisibilityChange)
    return () => {
      window.clearInterval(interval)
      window.removeEventListener('focus', update)
      document.removeEventListener('visibilitychange', onVisibilityChange)
    }
  }, [])

  return now
}

function matchQuery(credential: Credential, query: string): boolean {
  const value = query.trim().toLowerCase()
  if (!value) return true
  return credential.label.toLowerCase().includes(value)
    || `#${credential.id}`.includes(value)
    || String(credential.id) === value
}

interface CredentialWorkspaceData {
  credentials?: Credential[]
  isLoading: boolean
  isError: boolean
  isRefetchError: boolean
  isFetching: boolean
  error?: unknown
}

interface CredentialWorkspaceState {
  query: string
  filter: CredentialFilterKey
  sort: SortKey
  dir: SortDir
  view: CredentialViewMode
  selected: Set<number>
  page: number
  pageSize: CredentialPageSize
}

interface CredentialWorkspaceActions {
  onQueryChange: (value: string) => void
  onFilterChange: (value: CredentialFilterKey) => void
  onSortChange: (key: SortKey, dir: SortDir) => void
  onViewChange: (value: CredentialViewMode) => void
  onSelectedChange: (value: Set<number>) => void
  onPageChange: (value: number) => void
  onPageSizeChange: (value: CredentialPageSize) => void
  onRetry: () => void
  onAdd: () => void
}

export interface CredentialWorkspaceProps {
  data: CredentialWorkspaceData
  state: CredentialWorkspaceState
  actions: CredentialWorkspaceActions
}

/**
 * 账号页唯一的工作区组件。真实页面和离线预览共同使用这棵组件树，避免概览、工具栏、
 * 列表与分页在两处独立演进后产生视觉和交互差异。
 */
export function CredentialWorkspace({ data, state, actions }: CredentialWorkspaceProps) {
  const {
    credentials,
    isLoading,
    isError,
    isRefetchError,
    isFetching,
    error,
  } = data
  const {
    query,
    filter,
    sort,
    dir,
    view,
    selected,
    page,
    pageSize,
  } = state
  const pool = credentials ?? []
  const debouncedQuery = useDebounced(query)
  const now = useNowSeconds()
  const evaluatedPool = useMemo(
    () => pool.map((credential) => evaluateCredential(credential, now)),
    [pool, now],
  )

  const sorted = useMemo(() => {
    const match = FILTERS.find((item) => item.key === filter)?.match ?? (() => true)
    return sortCreds(
      evaluatedPool
        .filter((evaluation) => (
          match(evaluation) && matchQuery(evaluation.credential, debouncedQuery)
        ))
        .map((evaluation) => evaluation.credential),
      sort,
      dir,
      now,
    )
  }, [evaluatedPool, sort, dir, filter, debouncedQuery, now])

  const metrics = useMemo(() => {
    const filterCounts: Record<CredentialFilterKey, number> = {
      all: 0,
      schedulable: 0,
      attention: 0,
      enabled: 0,
      disabled: 0,
      abnormal: 0,
      nearLimit: 0,
      cooldown: 0,
      hasDevice: 0,
      deviceFull: 0,
    }
    let nearLimitCount = 0
    let activeOverageCount = 0
    let unknownOverageCount = 0
    let deviceCount = 0
    let deviceCapacity = 0
    let unlimitedDeviceAccounts = 0

    for (const evaluation of evaluatedPool) {
      const credential = evaluation.credential
      filterCounts.all += 1
      if (evaluation.schedulable) filterCounts.schedulable += 1
      if (evaluation.needsAttention) filterCounts.attention += 1
      if (credential.disabled) filterCounts.disabled += 1
      else filterCounts.enabled += 1
      if (credential.ban_reason) filterCounts.abnormal += 1
      if (evaluation.quotaRisk) filterCounts.nearLimit += 1
      if (!credential.disabled && credential.rate_limited_secs > 0) filterCounts.cooldown += 1
      if (credential.device_count > 0) filterCounts.hasDevice += 1
      if (
        credential.device_limit_effective > 0
        && credential.device_count >= credential.device_limit_effective
      ) {
        filterCounts.deviceFull += 1
      }
      // 额度概览按「超额 > 待确认 > 将满」互斥归类，避免一个账号重复出现在两项里。
      if (
        evaluation.nearLimit
        && evaluation.quota.overage !== 'active'
        && evaluation.quota.overage !== 'unknown'
      ) {
        nearLimitCount += 1
      }
      if (!credential.disabled && evaluation.quota.overage === 'active') {
        activeOverageCount += 1
      }
      if (!credential.disabled && evaluation.quota.overage === 'unknown') {
        unknownOverageCount += 1
      }
      deviceCount += credential.device_count
      if (credential.device_limit_effective > 0) {
        deviceCapacity += credential.device_limit_effective
      } else {
        unlimitedDeviceAccounts += 1
      }
    }

    return {
      filterCounts,
      nearLimitCount,
      activeOverageCount,
      unknownOverageCount,
      deviceCount,
      deviceCapacity,
      unlimitedDeviceAccounts,
    }
  }, [evaluatedPool])

  const count = pool.length
  const total = sorted.length
  const enabledCount = metrics.filterCounts.enabled
  const schedulableCount = metrics.filterCounts.schedulable
  const abnormalCount = metrics.filterCounts.abnormal
  const cooldownCount = metrics.filterCounts.cooldown
  const attentionCount = metrics.filterCounts.attention
  const quotaRiskCount = metrics.filterCounts.nearLimit
  const fullDeviceCount = metrics.filterCounts.deviceFull
  const filtering = filter !== 'all' || debouncedQuery.trim() !== ''
  const pageCount = Math.max(1, Math.ceil(total / pageSize))
  const current = Math.min(page, pageCount)
  const pageItems = sorted.slice((current - 1) * pageSize, current * pageSize)
  const attentionStatus = [
    abnormalCount > 0 ? `${abnormalCount} 异常` : '',
    metrics.activeOverageCount > 0 ? `${metrics.activeOverageCount} 超额` : '',
    metrics.unknownOverageCount > 0 ? `${metrics.unknownOverageCount} 超额待确认` : '',
    cooldownCount > 0 ? `${cooldownCount} 冷却` : '',
    metrics.nearLimitCount > 0 ? `${metrics.nearLimitCount} 额度` : '',
  ].filter(Boolean).join(' · ') || undefined
  const quotaRiskStatus = [
    metrics.activeOverageCount > 0 ? `${metrics.activeOverageCount} 超额` : '',
    metrics.unknownOverageCount > 0 ? `${metrics.unknownOverageCount} 待确认` : '',
    metrics.nearLimitCount > 0 ? `${metrics.nearLimitCount} 将满` : '',
  ].filter(Boolean).join(' · ') || undefined
  const deviceStatus = fullDeviceCount > 0
    ? `${fullDeviceCount} 个账号已满`
    : metrics.unlimitedDeviceAccounts > 0
      ? `${metrics.unlimitedDeviceAccounts} 个不限额账号`
      : metrics.deviceCapacity > 0
        ? `共 ${metrics.deviceCapacity} 个名额`
        : undefined

  const clearSelection = () => actions.onSelectedChange(new Set())
  const changeQuery = (value: string) => {
    actions.onQueryChange(value)
    actions.onPageChange(1)
    clearSelection()
  }
  const changeFilter = (value: CredentialFilterKey) => {
    actions.onFilterChange(value)
    actions.onPageChange(1)
    clearSelection()
  }
  const changeSort = (key: SortKey) => {
    actions.onSortChange(
      key,
      key === sort ? (dir === 'asc' ? 'desc' : 'asc') : SORT_DIR_DEFAULT[key],
    )
    actions.onPageChange(1)
  }
  const toggleSelected = (id: number, checked: boolean) => {
    const next = new Set(selected)
    if (checked) next.add(id)
    else next.delete(id)
    actions.onSelectedChange(next)
  }
  const selectMetric = (key: CredentialFilterKey) => changeFilter(filter === key ? 'all' : key)

  return (
    <div className="space-y-4 sm:space-y-6" data-slot="credential-workspace">
      <section className="flex items-center justify-between gap-4" aria-labelledby="page-title">
        <h1 id="page-title" className="min-w-0 text-lg font-semibold tracking-tight sm:text-xl">
          账号池
        </h1>
        <div className="flex shrink-0 items-center gap-1.5 text-2xs text-muted-foreground sm:text-xs">
          {isRefetchError ? (
            <>
              <TriangleAlertIcon className="size-3.5 text-destructive-foreground" aria-hidden />
              <button
                type="button"
                className="rounded-sm font-medium text-destructive-foreground underline-offset-2 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/60"
                onClick={actions.onRetry}
              >
                刷新失败，重试
              </button>
            </>
          ) : (
            <>
              {isFetching && !isLoading ? (
                <RefreshCwIcon className="size-3.5 animate-spin" aria-hidden />
              ) : (
                <span className="size-1.5 rounded-full bg-success" aria-hidden />
              )}
              <span title="每 30 秒自动刷新">30 秒刷新</span>
            </>
          )}
        </div>
      </section>

      {isLoading ? (
        <section aria-label="正在加载账号池概览" className="grid grid-cols-2 overflow-hidden rounded-xl border bg-card shadow-xs/5 lg:grid-cols-4">
          <OverviewMetricSkeleton className="border-r border-b lg:border-b-0" />
          <OverviewMetricSkeleton className="border-b lg:border-r lg:border-b-0" />
          <OverviewMetricSkeleton className="border-r" />
          <OverviewMetricSkeleton />
        </section>
      ) : count > 0 && (
        <section aria-label="账号池概览" className="grid grid-cols-2 overflow-hidden rounded-xl border bg-card shadow-xs/5 lg:grid-cols-4">
          <OverviewMetric
            className="border-r border-b lg:border-b-0"
            label="可调度账号"
            value={`${schedulableCount}/${count}`}
            status={schedulableCount < count ? `${count - schedulableCount} 暂不可用` : `${enabledCount} 已启用`}
            icon={ShieldCheckIcon}
            tone={schedulableCount > 0 ? 'ok' : 'bad'}
            active={filter === 'schedulable'}
            onClick={() => selectMetric('schedulable')}
          />
          <OverviewMetric
            className="border-b lg:border-r lg:border-b-0"
            label="需处理"
            value={attentionCount}
            status={attentionStatus}
            icon={TriangleAlertIcon}
            tone={abnormalCount > 0 || metrics.activeOverageCount > 0
              ? 'bad'
              : attentionCount > 0
                ? 'warn'
                : 'neutral'}
            active={filter === 'attention'}
            onClick={() => selectMetric('attention')}
          />
          <OverviewMetric
            className="border-r"
            label="额度风险"
            value={quotaRiskCount}
            status={quotaRiskStatus}
            icon={RadioIcon}
            tone={metrics.activeOverageCount > 0 ? 'bad' : quotaRiskCount > 0 ? 'warn' : 'neutral'}
            active={filter === 'nearLimit'}
            onClick={() => selectMetric('nearLimit')}
          />
          <OverviewMetric
            label="绑定设备"
            value={metrics.deviceCount}
            status={deviceStatus}
            icon={SmartphoneIcon}
            tone={fullDeviceCount > 0 ? 'warn' : 'neutral'}
            active={filter === 'deviceFull' || filter === 'hasDevice'}
            onClick={() => selectMetric(fullDeviceCount > 0 ? 'deviceFull' : 'hasDevice')}
          />
        </section>
      )}

      <section className="min-w-0" aria-labelledby="account-list-title">
        <h2 id="account-list-title" className="sr-only">账号列表</h2>
        <p className="sr-only" aria-live="polite">
          {filtering ? `筛选出 ${total} 个，共 ${count} 个账号` : `共 ${count} 个账号`}
        </p>
        <div className="min-w-0 space-y-3 sm:space-y-4">
          {count > 0 && (
            <div className="rounded-xl border bg-card p-2.5 shadow-xs/5 sm:p-3">
              <Toolbar className="grid w-full grid-cols-[minmax(0,1fr)_auto] items-stretch gap-2 border-0 bg-transparent p-0 sm:flex sm:flex-row sm:flex-wrap sm:items-center">
                  <InputGroup className="col-span-2 sm:min-w-56 sm:flex-1 xl:max-w-72">
                    <InputGroupAddon><SearchIcon /></InputGroupAddon>
                    <InputGroupInput
                      value={query}
                      onChange={(event) => changeQuery(event.target.value)}
                      placeholder="搜索名称或 #id"
                      aria-label="搜索账号"
                    />
                    {query && (
                      <InputGroupAddon align="inline-end">
                        <Button size="icon-xs" variant="ghost" onClick={() => changeQuery('')} aria-label="清除搜索">
                          <XIcon />
                        </Button>
                      </InputGroupAddon>
                    )}
                  </InputGroup>

                  <ToolbarSeparator orientation="vertical" className="hidden sm:block" />
                  <ToolbarGroup className="grid min-w-0 grid-cols-2 sm:flex sm:flex-wrap">
                    <Menu>
                      <MenuTrigger
                        aria-label={`筛选：${FILTERS.find((item) => item.key === filter)!.label}`}
                        className={cn(
                          buttonVariants({ variant: filter === 'all' ? 'outline' : 'secondary' }),
                          'w-full min-w-0 justify-between max-sm:[&_svg]:hidden sm:w-auto',
                        )}
                      >
                        <ListFilterIcon />
                        <span className="min-w-0 truncate">
                          {FILTERS.find((item) => item.key === filter)!.label}
                        </span>
                      </MenuTrigger>
                      <MenuPopup align="end" className="w-52">
                        <MenuRadioGroup value={filter}>
                          {FILTERS.map((item) => (
                            <MenuRadioItem key={item.key} value={item.key} onClick={() => changeFilter(item.key)}>
                              <span className="flex min-w-0 flex-1 items-center justify-between gap-4">
                                <span>{item.label}</span>
                                <span className="tnum text-xs text-muted-foreground">
                                  {metrics.filterCounts[item.key]}
                                </span>
                              </span>
                            </MenuRadioItem>
                          ))}
                        </MenuRadioGroup>
                      </MenuPopup>
                    </Menu>

                    <Menu>
                      <MenuTrigger
                        aria-label={`排序：${SORTS.find((item) => item.key === sort)!.label}，${dir === 'asc' ? '升序' : '降序'}`}
                        className={cn(
                          buttonVariants({ variant: 'outline' }),
                          'w-full min-w-0 justify-between max-sm:[&_svg]:hidden sm:w-auto',
                        )}
                      >
                        <ArrowUpDownIcon />
                        <span className="min-w-0 truncate max-[22rem]:hidden">
                          {SORTS.find((item) => item.key === sort)!.label} {dir === 'asc' ? '↑' : '↓'}
                        </span>
                        <span className="hidden shrink-0 max-[22rem]:inline">
                          排序 {dir === 'asc' ? '↑' : '↓'}
                        </span>
                      </MenuTrigger>
                      <MenuPopup align="end" className="w-48">
                        <MenuRadioGroup value={sort}>
                          {SORTS.map((item) => (
                            <MenuRadioItem key={item.key} value={item.key} onClick={() => changeSort(item.key)}>
                              <span className="flex min-w-0 flex-1 items-center justify-between gap-4">
                                <span>{item.label}</span>
                                {sort === item.key && (
                                  <span className="text-xs text-muted-foreground">
                                    {dir === 'asc' ? '升序' : '降序'}
                                  </span>
                                )}
                              </span>
                            </MenuRadioItem>
                          ))}
                        </MenuRadioGroup>
                      </MenuPopup>
                    </Menu>
                  </ToolbarGroup>

                  <ToolbarSeparator orientation="vertical" className="hidden sm:ml-auto sm:block" />
                  <ToolbarGroup className="self-center justify-end">
                    <ToggleGroup
                      value={[view]}
                      onValueChange={(values) => {
                        const next = values[values.length - 1]
                        if (next === 'card' || next === 'list') actions.onViewChange(next)
                      }}
                      variant="outline"
                      aria-label="账号视图"
                    >
                      <ToggleGroupItem value="card" aria-label="卡片视图" title="卡片视图">
                        <LayoutGridIcon />
                      </ToggleGroupItem>
                      <ToggleGroupSeparator />
                      <ToggleGroupItem value="list" aria-label="列表视图" title="列表视图">
                        <ListIcon />
                      </ToggleGroupItem>
                    </ToggleGroup>
                  </ToolbarGroup>
              </Toolbar>
            </div>
          )}

          {count > 0 && selected.size > 0 && (
            <div className="relative">
              <BatchActionsBar
                all={sorted}
                selected={selected}
                onSelectedChange={actions.onSelectedChange}
                onClear={clearSelection}
              />
            </div>
          )}

          {isLoading ? (
            <div className="relative">
              <CredentialLoadingState view={view} selectable />
            </div>
          ) : isError && !credentials ? (
            <Card><ErrorState error={error} onRetry={actions.onRetry} /></Card>
          ) : count === 0 ? (
            <Card><EmptyState onAdd={actions.onAdd} /></Card>
          ) : total === 0 ? (
            <Card>
              <Empty>
                <EmptyHeader>
                  <EmptyMedia variant="icon"><SearchIcon /></EmptyMedia>
                  <EmptyTitle>没有符合条件的账号</EmptyTitle>
                  <EmptyDescription>尝试清除当前筛选条件或搜索关键字。</EmptyDescription>
                </EmptyHeader>
                <EmptyContent>
                  <Button
                    variant="outline"
                    onClick={() => {
                      actions.onQueryChange('')
                      changeFilter('all')
                    }}
                  >
                    清除筛选与搜索
                  </Button>
                </EmptyContent>
              </Empty>
            </Card>
          ) : view === 'list' ? (
            <Table variant="card" className="xl:min-w-[72rem]">
              <TableCaption className="sr-only">账号列表</TableCaption>
              <CredentialListHeader
                selectable
                sort={sort}
                dir={dir}
                onSortChange={changeSort}
                allSelected={sorted.length > 0 && sorted.every((item) => selected.has(item.id))}
                onSelectAll={(checked) => actions.onSelectedChange(
                  checked ? new Set(sorted.map((item) => item.id)) : new Set(),
                )}
              />
              <TableBody>
                {pageItems.map((item) => (
                  <CredentialRow
                    key={item.id}
                    cred={item}
                    now={now}
                    selectable
                    selected={selected.has(item.id)}
                    onSelectedChange={(checked) => toggleSelected(item.id, checked)}
                  />
                ))}
              </TableBody>
            </Table>
          ) : (
            <ul className="relative grid list-none items-stretch gap-3 p-0 [grid-template-columns:repeat(auto-fill,minmax(min(100%,27rem),1fr))] sm:gap-4">
              {pageItems.map((item) => (
                <CredentialCard
                  key={item.id}
                  cred={item}
                  now={now}
                  selectable
                  selected={selected.has(item.id)}
                  onSelectedChange={(checked) => toggleSelected(item.id, checked)}
                />
              ))}
            </ul>
          )}

          {!isLoading && pageCount > 1 && (
            <div className="relative py-2">
              <AccountPagination
                total={total}
                page={current}
                pageCount={pageCount}
                pageSize={pageSize}
                onPageChange={actions.onPageChange}
                onPageSizeChange={(size) => {
                  actions.onPageSizeChange(size)
                  actions.onPageChange(1)
                }}
              />
            </div>
          )}
        </div>
      </section>
    </div>
  )
}

function AccountPagination({
  total,
  page,
  pageCount,
  pageSize,
  onPageChange,
  onPageSizeChange,
}: {
  total: number
  page: number
  pageCount: number
  pageSize: CredentialPageSize
  onPageChange: (page: number) => void
  onPageSizeChange: (pageSize: CredentialPageSize) => void
}) {
  const from = (page - 1) * pageSize + 1
  const to = Math.min(page * pageSize, total)
  const start = Math.max(1, Math.min(page - 2, pageCount - 4))
  const pages = Array.from({ length: Math.min(5, pageCount) }, (_, index) => start + index)
  const navigate = (event: React.MouseEvent<HTMLAnchorElement>, next: number) => {
    event.preventDefault()
    if (next >= 1 && next <= pageCount) onPageChange(next)
  }

  return (
    <div className="grid grid-cols-[1fr_auto] items-center gap-3 text-xs text-muted-foreground md:grid-cols-[1fr_auto_1fr]">
      <span className="min-w-0">
        <span className="sm:hidden">
          <span className="tnum text-foreground">{from}–{to}</span>
          {' / '}
          <span className="tnum text-foreground">{total}</span>
        </span>
        <span className="hidden sm:inline">
          第 <span className="tnum text-foreground">{from}-{to}</span> 个，共{' '}
          <span className="tnum text-foreground">{total}</span> 个账号
        </span>
      </span>
      <CossPagination className="col-span-2 row-start-2 justify-center md:col-span-1 md:col-start-2 md:row-start-1">
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
      <div className="row-start-1 flex items-center gap-2 justify-self-end md:col-start-3">
        <span className="max-sm:sr-only">每页</span>
        <Select
          items={PAGE_SIZE_ITEMS}
          value={String(pageSize)}
          onValueChange={(value) => {
            const next = Number(value)
            if (CREDENTIAL_PAGE_SIZES.includes(next as CredentialPageSize)) {
              onPageSizeChange(next as CredentialPageSize)
            }
          }}
        >
          <SelectTrigger aria-label="每页账号数" size="sm" className="min-w-20">
            <SelectValue />
          </SelectTrigger>
          <SelectPopup align="end">
            {PAGE_SIZE_ITEMS.map((item) => (
              <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>
            ))}
          </SelectPopup>
        </Select>
      </div>
    </div>
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
