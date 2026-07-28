import { useMemo, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  PlusIcon, Cog6ToothIcon, ArrowRightStartOnRectangleIcon, ArrowsUpDownIcon, CheckIcon,
  QueueListIcon, ArrowPathIcon, XMarkIcon, ChevronLeftIcon, ChevronRightIcon,
  MagnifyingGlassIcon, FunnelIcon, Squares2X2Icon, Bars3Icon,
  PlayIcon, PauseIcon, TrashIcon,
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
import { AccessSettings } from '@/components/access-settings'
import { LoginPage } from '@/components/login-page'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Table, TableBody, TableCaption } from '@/components/ui/table'
import { Toaster } from '@/components/ui/sonner'
import { ConfirmDialog } from '@/components/ui/confirm-dialog'
import {
  DropdownMenu, DropdownMenuTrigger, DropdownMenuContent, DropdownMenuItem,
} from '@/components/ui/dropdown-menu'

type FilterKey = 'all' | 'enabled' | 'disabled' | 'abnormal' | 'nearLimit'
type ViewMode = 'card' | 'list'

/** 每页账号数可选档位（用 10/20/50 这类常规档，不迁就栅格列数）；账号少时分页条自动隐藏。 */
const PAGE_SIZES = [10, 20, 50] as const

const VIEW_MODES = ['card', 'list'] as const

const FILTERS: { key: FilterKey; label: string; match: (c: Credential) => boolean }[] = [
  { key: 'all', label: '全部', match: () => true },
  { key: 'enabled', label: '启用', match: (c) => !c.disabled },
  { key: 'disabled', label: '停用', match: (c) => c.disabled },
  { key: 'abnormal', label: '异常（封禁/过期）', match: isAbnormal },
  { key: 'nearLimit', label: '额度将满', match: isNearLimit },
]


/** 关键字匹配：名称（忽略大小写）或 `#id`。 */
function matchQuery(c: Credential, q: string): boolean {
  const t = q.trim().toLowerCase()
  if (!t) return true
  return c.label.toLowerCase().includes(t) || `#${c.id}`.includes(t) || String(c.id) === t
}


function App() {
  const [adding, setAdding] = useState(false)
  const [showSettings, setShowSettings] = useState(false)
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
  const [view, switchView] = usePersisted<ViewMode>('view', 'card', oneOf(VIEW_MODES))

  // 筛选与搜索刻意不持久化：它们会隐藏账号，刷新后仍生效容易让人以为账号丢了。
  const [filter, setFilter] = useState<FilterKey>('all')
  const [query, setQuery] = useState('')
  // 输入框绑 query 保持跟手，筛选/排序用延迟值：连续敲键时不必每个字符都过一遍全量账号。
  const debouncedQuery = useDebounced(query)
  // 条件变化后停留在旧页码可能整页为空，统一回到第一页。
  const resetPage = <T,>(set: (v: T) => void) => (v: T) => { set(v); setPage(1) }
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

  const { data: creds, isLoading } = useQuery({
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
    return <div className="grid min-h-screen place-items-center text-sm text-muted-foreground">加载中…</div>
  }

  if (needLogin) {
    return (
      <>
        <LoginPage onSuccess={(p) => { setPw(p); setPwState(p) }} />
        <Toaster position="top-right" />
      </>
    )
  }

  const count = creds?.length ?? 0
  const enabledCount = (creds ?? []).filter((c) => !c.disabled).length
  // 跟 total 同源用延迟值，否则敲键的那一瞬文案先切成「筛选出 N / 共 M」而 N 还是旧的。
  const filtering = filter !== 'all' || debouncedQuery.trim() !== ''

  return (
    <div className="min-h-screen bg-background text-foreground">
      {/* 置顶操作栏 */}
      <header className="sticky top-0 z-20 border-b border-border/60 bg-background/80 backdrop-blur-sm">
        <div className="mx-auto flex max-w-5xl items-center justify-between gap-3 px-5 py-3">
          <div className="flex items-center gap-2.5">
            <div className="flex size-9 items-center justify-center rounded-md bg-foreground text-background">
              <span className="font-mono text-sm font-bold">鲁</span>
            </div>
            <div>
              <div className="text-sm font-semibold leading-none tracking-tight">luban</div>
              <div className="label-eyebrow mt-1">Claude Code 授权代理</div>
            </div>
          </div>
          <div className="flex items-center gap-2">
            <Button size="sm" variant="outline" onClick={() => setShowSettings(true)} title="接入设置">
              <Cog6ToothIcon />
              <span className="hidden sm:inline">接入设置</span>
            </Button>
            <Button size="sm" onClick={() => setAdding(true)}>
              <PlusIcon />
              <span className="hidden sm:inline">添加账号</span>
            </Button>
            {authState.configured && pw && (
              <Button size="sm" variant="ghost" title="退出登录"
                onClick={() => { clearPw(); setPwState(null) }}>
                <ArrowRightStartOnRectangleIcon />
              </Button>
            )}
          </div>
        </div>
      </header>

      <main className="@container mx-auto w-full max-w-5xl space-y-6 px-5 py-8">
        {/* 弹窗：添加账号 / 接入设置 */}
        <AddAccount open={adding} onOpenChange={setAdding} />
        <AccessSettings open={showSettings} onOpenChange={setShowSettings} />

        {/* 工具栏：计数 + 搜索 + 筛选 + 视图切换 + 批量 + 排序 */}
        {count > 0 && (
          <div className="flex flex-wrap items-center justify-between gap-2">
            <h2 className="flex items-baseline gap-2 text-sm font-semibold tracking-tight">
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
            <div className="flex flex-wrap items-center gap-2">
              {/* 搜索：名称或 #id */}
              <div className="relative">
                <MagnifyingGlassIcon className="pointer-events-none absolute left-2 top-1/2 size-3.5 -translate-y-1/2 text-muted-foreground" />
                <Input
                  value={query}
                  onChange={(e) => resetPage(setQuery)(e.target.value)}
                  placeholder="搜索名称或 #id"
                  className="h-8 w-40 pl-7 pr-7 text-xs"
                />
                {query && (
                  <button
                    className="absolute right-1.5 top-1/2 grid size-5 -translate-y-1/2 place-items-center rounded text-muted-foreground hover:bg-muted hover:text-foreground"
                    onClick={() => resetPage(setQuery)('')}
                    title="清除搜索"
                  >
                    <XMarkIcon className="size-3" />
                  </button>
                )}
              </div>

              {/* 状态筛选 */}
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button
                    size="sm"
                    variant={filter === 'all' ? 'outline' : 'secondary'}
                    className="h-8 gap-1.5 px-2.5 text-xs"
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

              {/* 视图切换：卡片 / 紧凑列表 */}
              <div className="flex items-center overflow-hidden rounded-xl border border-border">
                <button
                  className={cn(
                    'grid h-8 w-8 place-items-center transition-colors',
                    view === 'card' ? 'bg-muted text-foreground' : 'text-muted-foreground hover:bg-muted/60',
                  )}
                  onClick={() => switchView('card')}
                  title="卡片视图"
                  aria-pressed={view === 'card'}
                >
                  <Squares2X2Icon className="size-4" />
                </button>
                <button
                  className={cn(
                    'grid h-8 w-8 place-items-center border-l border-border transition-colors',
                    view === 'list' ? 'bg-muted text-foreground' : 'text-muted-foreground hover:bg-muted/60',
                  )}
                  onClick={() => switchView('list')}
                  title="紧凑列表视图"
                  aria-pressed={view === 'list'}
                >
                  <Bars3Icon className="size-4" />
                </button>
              </div>

              <Button
                size="sm"
                variant={batch ? 'secondary' : 'outline'}
                className="h-8 gap-1.5 px-2.5 text-xs"
                onClick={() => { setBatch((b) => !b); setSelected(new Set()) }}
                title="批量调整优先级"
              >
                <QueueListIcon className="size-3.5" />
                批量
              </Button>
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button size="sm" variant="outline" className="h-8 gap-1.5 px-2.5 text-xs">
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
            </div>
          </div>
        )}

        {/* 批量操作条 */}
        {count > 0 && batch && (
          <BatchActionsBar
            all={sorted}
            selected={selected}
            onSelectedChange={setSelected}
            onClose={() => { setBatch(false); setSelected(new Set()) }}
          />
        )}

        {/* 卡片栅格（按当前页切片） */}
        {isLoading ? (
          <div className="py-16 text-center text-sm text-muted-foreground">加载中…</div>
        ) : count === 0 ? (
          <EmptyState onAdd={() => setAdding(true)} />
        ) : total === 0 ? (
          <div className="rounded-2xl border border-dashed border-border py-14 text-center">
            <p className="text-sm text-muted-foreground">没有符合条件的账号</p>
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
          // Table 自带横向滚动容器；外层这层只负责圆角描边（overflow-hidden 裁掉溢出的直角）。
          <div className="overflow-hidden rounded-2xl border border-border bg-card shadow-card">
            <Table>
              {/* 组件默认可见（mt-4 的说明文字），这里只给读屏用 */}
              <TableCaption className="sr-only">账号列表</TableCaption>
              <CredentialListHeader
                selectable={batch}
                sort={sort}
                dir={dir}
                onSortChange={changeSort}
                // 表头勾选框与批量条的「全选」同义：都作用于当前筛选结果（跨页）。
                allSelected={sorted.length > 0 && selected.size === sorted.length}
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
          // 卡片始终单列铺满内容宽度：卡片内的额度条等区块用的是卡片自身的容器查询，
          // 单列下它们能横向展开，信息密度反而比挤成两列更好。
          <div className="grid grid-cols-1 gap-4">
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

        {/* 分页条：账号超过一页才出现 */}
        {!isLoading && total > PAGE_SIZES[0] && (
          <Pagination
            total={total}
            page={current}
            pageCount={pageCount}
            pageSize={pageSize}
            onPageChange={setPage}
            onPageSizeChange={(n) => { setPageSize(n); setPage(1) }}
          />
        )}
      </main>
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
    <div className="flex flex-wrap items-center justify-between gap-3 border-t border-border/60 pt-4 text-xs text-muted-foreground">
      <span>
        第 <span className="tnum text-foreground">{from}-{to}</span> 个，共{' '}
        <span className="tnum text-foreground">{total}</span> 个账号
      </span>
      <div className="flex items-center gap-1.5">
        <span className="hidden sm:inline">每页</span>
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
function BatchActionsBar({
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
    <div className="space-y-2 rounded-xl border border-border bg-surface-2/40 px-3 py-2.5 text-xs">
      {/* 第一行：选择状态 + 无需填值的操作（启停 / 删除） */}
      <div className="flex flex-wrap items-center gap-2">
        <Button
          size="sm"
          variant="ghost"
          className="h-7 px-2 text-xs"
          title="选中当前筛选结果里的全部账号（跨页，勾选状态在翻页后保留）"
          onClick={() => onSelectedChange(allSelected ? new Set() : new Set(all.map((c) => c.id)))}
        >
          {allSelected ? '取消全选' : `全选 ${all.length} 个`}
        </Button>
        <span className="text-muted-foreground">
          已选 <span className="tnum font-medium text-foreground">{n}</span> 个
        </span>

        <div className="ml-auto flex items-center gap-1.5">
          <Button
            size="sm" variant="outline" className="h-7 px-2.5 text-xs"
            disabled={none || busy}
            onClick={() => applyDisabled.mutate(false)}
          >
            <PlayIcon className="size-3.5" />启用
          </Button>
          <Button
            size="sm" variant="outline" className="h-7 px-2.5 text-xs"
            disabled={none || busy}
            onClick={() => applyDisabled.mutate(true)}
          >
            <PauseIcon className="size-3.5" />停用
          </Button>
          <Button
            size="sm" variant="ghost" className="h-7 px-2.5 text-xs text-bad hover:text-bad"
            disabled={none || busy}
            onClick={() => setConfirmDelete(true)}
          >
            <TrashIcon className="size-3.5" />删除
          </Button>
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
          <span className="mx-0.5 h-4 w-px bg-border" />
          <Button size="icon" variant="ghost" className="size-7" onClick={onClose} title="退出批量模式">
            <XMarkIcon className="size-3.5" />
          </Button>
        </div>
      </div>

      {/* 第二行：需要填值的操作（优先级 / 设备上限） */}
      <div className="flex flex-wrap items-center gap-x-4 gap-y-2 border-t border-border/60 pt-2">
        <div className="flex items-center gap-1.5">
          <span className="text-muted-foreground">优先级</span>
          <Input
            type="number"
            value={priority}
            onChange={(e) => setPriority(e.target.value)}
            className="h-7 w-20 font-mono text-xs"
            title="数值小者优先被调度；同一档内按设备数负载均衡"
          />
          <Button
            size="sm" className="h-7 px-2.5 text-xs"
            disabled={none || busy}
            onClick={() => applyPriority.mutate(Math.floor(Number(priority) || 0))}
          >
            {applyPriority.isPending ? <ArrowPathIcon className="size-3.5 animate-spin" /> : <CheckIcon className="size-3.5" />}
            应用
          </Button>
        </div>

        <div className="flex items-center gap-1.5">
          <span className="text-muted-foreground">设备上限</span>
          <Input
            type="number"
            min={0}
            value={limit}
            onChange={(e) => setLimit(e.target.value)}
            placeholder="默认"
            className="h-7 w-20 font-mono text-xs"
            title="留空 = 跟随全局默认；0 = 不限；正数 = 独立上限"
          />
          <Button
            size="sm" className="h-7 px-2.5 text-xs"
            disabled={none || busy}
            onClick={() => applyLimit.mutate(inputToLimit(limit))}
          >
            {applyLimit.isPending ? <ArrowPathIcon className="size-3.5 animate-spin" /> : <CheckIcon className="size-3.5" />}
            应用
          </Button>
        </div>
      </div>

      <p className="text-2xs text-muted-foreground">
        优先级数值小者优先调度，同档内按设备数负载均衡；设备上限留空表示跟随接入设置里的全局默认，填 0 表示不限。
      </p>
    </div>
  )
}

function EmptyState({ onAdd }: { onAdd: () => void }) {
  return (
    <div className="rounded-2xl border border-dashed border-border py-16 text-center">
      <p className="text-sm text-muted-foreground">还没有账号</p>
      <Button className="mt-4" onClick={onAdd}>
        <PlusIcon />
        添加第一个账号
      </Button>
    </div>
  )
}

export default App
