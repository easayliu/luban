import { useMemo } from 'react'
import {
  ArrowUpDownIcon,
  ChevronLeftIcon,
  ChevronRightIcon,
  LayoutGridIcon,
  ListChecksIcon,
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
  isAbnormal,
  isNearLimit,
  sortCreds,
  type SortDir,
  type SortKey,
} from '@/components/credential-shared'
import { CredentialListHeader, CredentialRow } from '@/components/credential-row'
import { OverviewMetric, OverviewMetricSkeleton } from '@/components/overview-metric'
import { Button } from '@/components/ui/button'
import {
  Card,
  CardFrame,
  CardFrameFooter,
  CardPanel,
} from '@/components/ui/card'
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
  match: (credential: Credential) => boolean
}[] = [
  { key: 'all', label: '全部', match: () => true },
  {
    key: 'schedulable',
    label: '可调度',
    match: (credential) =>
      !credential.disabled && !isAbnormal(credential) && credential.rate_limited_secs <= 0,
  },
  {
    key: 'attention',
    label: '需处理',
    match: (credential) =>
      isAbnormal(credential)
      || isNearLimit(credential)
      || (!credential.disabled && credential.rate_limited_secs > 0),
  },
  { key: 'enabled', label: '启用', match: (credential) => !credential.disabled },
  { key: 'disabled', label: '停用', match: (credential) => credential.disabled },
  { key: 'abnormal', label: '异常（已封禁）', match: isAbnormal },
  { key: 'nearLimit', label: '额度将满', match: isNearLimit },
  {
    key: 'cooldown',
    label: '冷却中',
    match: (credential) => !credential.disabled && credential.rate_limited_secs > 0,
  },
  { key: 'hasDevice', label: '已绑定设备', match: (credential) => credential.device_count > 0 },
  {
    key: 'deviceFull',
    label: '设备已满',
    match: (credential) =>
      credential.device_limit_effective > 0
      && credential.device_count >= credential.device_limit_effective,
  },
]

export const CREDENTIAL_FILTER_KEYS = FILTERS.map((filter) => filter.key)

export function preferredInitialCredentialView(): CredentialViewMode {
  return typeof window !== 'undefined' && window.matchMedia('(min-width: 64rem)').matches
    ? 'list'
    : 'card'
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
  batch: boolean
  selected: Set<number>
  page: number
  pageSize: CredentialPageSize
}

interface CredentialWorkspaceActions {
  onQueryChange: (value: string) => void
  onFilterChange: (value: CredentialFilterKey) => void
  onSortChange: (key: SortKey, dir: SortDir) => void
  onViewChange: (value: CredentialViewMode) => void
  onBatchChange: (value: boolean) => void
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
    batch,
    selected,
    page,
    pageSize,
  } = state
  const pool = credentials ?? []
  const debouncedQuery = useDebounced(query)

  const sorted = useMemo(() => {
    const match = FILTERS.find((item) => item.key === filter)?.match ?? (() => true)
    return sortCreds(
      pool.filter((credential) => match(credential) && matchQuery(credential, debouncedQuery)),
      sort,
      dir,
    )
  }, [pool, sort, dir, filter, debouncedQuery])

  const count = pool.length
  const total = sorted.length
  const enabledCount = pool.filter((credential) => !credential.disabled).length
  const schedulableCount = pool.filter(
    (credential) =>
      !credential.disabled && !isAbnormal(credential) && credential.rate_limited_secs <= 0,
  ).length
  const abnormalCount = pool.filter(isAbnormal).length
  const nearLimitCount = pool.filter(isNearLimit).length
  const cooldownCount = pool.filter(
    (credential) => !credential.disabled && credential.rate_limited_secs > 0,
  ).length
  const attentionCount = pool.filter(
    (credential) =>
      isAbnormal(credential)
      || isNearLimit(credential)
      || (!credential.disabled && credential.rate_limited_secs > 0),
  ).length
  const deviceCount = pool.reduce((sum, credential) => sum + credential.device_count, 0)
  const deviceCapacity = pool.reduce(
    (sum, credential) =>
      sum + (credential.device_limit_effective > 0 ? credential.device_limit_effective : 0),
    0,
  )
  const unlimitedDeviceAccounts = pool.filter(
    (credential) => credential.device_limit_effective <= 0,
  ).length
  const fullDeviceCount = pool.filter(
    (credential) =>
      credential.device_limit_effective > 0
      && credential.device_count >= credential.device_limit_effective,
  ).length
  const filtering = filter !== 'all' || debouncedQuery.trim() !== ''
  const pageCount = Math.max(1, Math.ceil(total / pageSize))
  const current = Math.min(page, pageCount)
  const pageItems = sorted.slice((current - 1) * pageSize, current * pageSize)
  const attentionStatus = [
    abnormalCount > 0 ? `${abnormalCount} 异常` : '',
    cooldownCount > 0 ? `${cooldownCount} 冷却` : '',
    nearLimitCount > 0 ? `${nearLimitCount} 额度` : '',
  ].filter(Boolean).join(' · ') || undefined
  const deviceStatus = fullDeviceCount > 0
    ? `${fullDeviceCount} 个账号已满`
    : unlimitedDeviceAccounts > 0
      ? `${unlimitedDeviceAccounts} 个不限额账号`
      : deviceCapacity > 0
        ? `共 ${deviceCapacity} 个名额`
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
    <div className="space-y-6 sm:space-y-8" data-slot="credential-workspace">
      <section className="sm:flex sm:items-end sm:justify-between sm:gap-8" aria-labelledby="page-title">
        <div className="min-w-0">
          <h1 id="page-title" className="text-2xl font-semibold tracking-tight sm:text-3xl">
            账号调度中心
          </h1>
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
                onClick={actions.onRetry}
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
        <section aria-label="正在加载账号池概览" className="grid grid-cols-2 gap-3 lg:grid-cols-4">
          <OverviewMetricSkeleton />
          <OverviewMetricSkeleton />
          <OverviewMetricSkeleton />
          <OverviewMetricSkeleton />
        </section>
      ) : count > 0 && (
        <section aria-label="账号池概览" className="grid grid-cols-2 gap-3 lg:grid-cols-4">
          <OverviewMetric
            label="可调度账号"
            value={`${schedulableCount}/${count}`}
            status={schedulableCount < count ? `${count - schedulableCount} 暂不可用` : `${enabledCount} 已启用`}
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
        <h2 id="account-list-title" className="sr-only">账号列表</h2>
        <p className="sr-only" aria-live="polite">
          {filtering ? `筛选出 ${total} 个，共 ${count} 个账号` : `共 ${count} 个账号`}
        </p>
        <CardFrame className="min-w-0">
          {count > 0 && (
            <Card>
              <CardPanel className="p-3">
                <Toolbar className="w-full flex-col items-stretch sm:flex-row sm:flex-wrap sm:items-center">
                  <InputGroup className="sm:min-w-56 sm:flex-1 xl:max-w-72">
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
                  <ToolbarGroup className="grid grid-cols-2 sm:flex sm:flex-wrap">
                    <Menu>
                      <MenuTrigger render={<Button variant={filter === 'all' ? 'outline' : 'secondary'} />}>
                        <ListFilterIcon />
                        {FILTERS.find((item) => item.key === filter)!.label}
                      </MenuTrigger>
                      <MenuPopup align="end" className="w-52">
                        <MenuRadioGroup value={filter}>
                          {FILTERS.map((item) => (
                            <MenuRadioItem key={item.key} value={item.key} onClick={() => changeFilter(item.key)}>
                              <span className="flex min-w-0 flex-1 items-center justify-between gap-4">
                                <span>{item.label}</span>
                                <span className="tnum text-xs text-muted-foreground">
                                  {pool.filter(item.match).length}
                                </span>
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
                  <ToolbarGroup className="justify-end">
                    <Button
                      variant={batch ? 'secondary' : 'outline'}
                      onClick={() => {
                        actions.onBatchChange(!batch)
                        clearSelection()
                      }}
                    >
                      <ListChecksIcon />批量
                    </Button>
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
              </CardPanel>
            </Card>
          )}

          {count > 0 && batch && (
            <div className="relative p-4 pb-0">
              <BatchActionsBar
                all={sorted}
                selected={selected}
                onSelectedChange={actions.onSelectedChange}
                onClose={() => {
                  actions.onBatchChange(false)
                  clearSelection()
                }}
              />
            </div>
          )}

          {isLoading ? (
            <div className="relative p-4">
              <CredentialLoadingState view={view} selectable={batch} />
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
            <Table variant="card" className="table-fixed">
              <TableCaption className="sr-only">账号列表</TableCaption>
              <CredentialListHeader
                selectable={batch}
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
                    selectable={batch}
                    selected={selected.has(item.id)}
                    onSelectedChange={(checked) => toggleSelected(item.id, checked)}
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
                  onSelectedChange={(checked) => toggleSelected(item.id, checked)}
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
                onPageChange={actions.onPageChange}
                onPageSizeChange={(size) => {
                  actions.onPageSizeChange(size)
                  actions.onPageChange(1)
                }}
              />
            </CardFrameFooter>
          )}
        </CardFrame>
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
