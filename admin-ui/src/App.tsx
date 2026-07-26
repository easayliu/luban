import { useMemo, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  PlusIcon, Cog6ToothIcon, ArrowRightStartOnRectangleIcon, ArrowsUpDownIcon, CheckIcon,
  QueueListIcon, ArrowPathIcon, XMarkIcon, ChevronLeftIcon, ChevronRightIcon,
} from '@heroicons/react/24/outline'
import { toast } from 'sonner'
import { listCredentials, setPriorities, type Credential } from '@/api/credentials'
import { getAuthState } from '@/api/auth'
import { getPw, setPw, clearPw } from '@/api/client'
import { cn, extractError } from '@/lib/utils'
import { CredentialCard } from '@/components/credential-card'
import { AddAccount } from '@/components/add-account'
import { AccessSettings } from '@/components/access-settings'
import { LoginPage } from '@/components/login-page'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Toaster } from '@/components/ui/sonner'
import {
  DropdownMenu, DropdownMenuTrigger, DropdownMenuContent, DropdownMenuItem,
} from '@/components/ui/dropdown-menu'

type SortKey = 'priority' | 'usage5h' | 'cost' | 'recent' | 'created'

/** 每页账号数可选档位；账号少时分页条自动隐藏。 */
const PAGE_SIZES = [12, 24, 48] as const

const SORTS: { key: SortKey; label: string }[] = [
  { key: 'priority', label: '优先级' },
  { key: 'usage5h', label: '5h 使用率' },
  { key: 'cost', label: '累计花费' },
  { key: 'recent', label: '最近使用' },
  { key: 'created', label: '添加时间' },
]

/** 按所选维度排序（不改原数组）。除优先级升序外，其余均降序、缺失值垫底。 */
function sortCreds(list: Credential[], key: SortKey): Credential[] {
  const arr = [...list]
  switch (key) {
    case 'usage5h':
      return arr.sort((a, b) => (b.quota?.rl_5h_utilization ?? -1) - (a.quota?.rl_5h_utilization ?? -1))
    case 'cost':
      return arr.sort((a, b) => (b.cost_total ?? 0) - (a.cost_total ?? 0))
    case 'recent':
      return arr.sort((a, b) => (b.last_used ?? 0) - (a.last_used ?? 0))
    case 'created':
      return arr.sort((a, b) => b.created_at - a.created_at)
    case 'priority':
    default:
      return arr.sort((a, b) => a.priority - b.priority || a.id - b.id)
  }
}

function App() {
  const [adding, setAdding] = useState(false)
  const [showSettings, setShowSettings] = useState(false)
  const [sort, setSort] = useState<SortKey>('priority')
  const [pw, setPwState] = useState<string | null>(getPw())
  // 批量模式：卡片出现勾选框，工具栏变成批量操作条。
  const [batch, setBatch] = useState(false)
  const [selected, setSelected] = useState<Set<number>>(new Set())
  // 分页（纯前端切片：列表接口一次返回全部账号）。
  const [page, setPage] = useState(1)
  const [pageSize, setPageSize] = useState<number>(PAGE_SIZES[0])

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
  const sorted = useMemo(() => sortCreds(creds ?? [], sort), [creds, sort])
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
  const enabledCount = sorted.filter((c) => !c.disabled).length

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

        {/* 工具栏 */}
        {count > 0 && (
          <div className="flex items-center justify-between gap-2">
            <h2 className="flex items-baseline gap-2 text-sm font-semibold tracking-tight">
              账号列表
              <span className="text-xs font-normal text-muted-foreground">
                共 <span className="tnum font-medium text-foreground">{total}</span> 个
                {enabledCount < total && `（启用 ${enabledCount}）`}
              </span>
            </h2>
            <div className="flex items-center gap-2">
              <Button
                size="sm"
                variant={batch ? 'secondary' : 'outline'}
                className="h-8 gap-1.5 px-2.5 text-xs"
                onClick={() => { setBatch((b) => !b); setSelected(new Set()) }}
                title="批量调整优先级"
              >
                <QueueListIcon className="size-3.5" />
                批量优先级
              </Button>
              <DropdownMenu>
                <DropdownMenuTrigger asChild>
                  <Button size="sm" variant="outline" className="h-8 gap-1.5 px-2.5 text-xs">
                    <ArrowsUpDownIcon className="size-3.5" />
                    {SORTS.find((s) => s.key === sort)!.label}
                  </Button>
                </DropdownMenuTrigger>
                <DropdownMenuContent align="end">
                  {SORTS.map((s) => (
                    <DropdownMenuItem key={s.key} onClick={() => setSort(s.key)}>
                      <CheckIcon className={cn('size-3.5', sort === s.key ? 'opacity-100' : 'opacity-0')} />
                      {s.label}
                    </DropdownMenuItem>
                  ))}
                </DropdownMenuContent>
              </DropdownMenu>
            </div>
          </div>
        )}

        {/* 批量操作条 */}
        {count > 0 && batch && (
          <BatchPriorityBar
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
        ) : (
          // 单账号：单列自适应铺满内容宽度（与上方 KPI 行对齐）；多账号：容器查询两列。
          <div className={cn('grid grid-cols-1 gap-4', count > 1 && '@4xl:grid-cols-2')}>
            {pageItems.map((c) => (
              <CredentialCard
                key={c.id}
                cred={c}
                selectable={batch}
                selected={selected.has(c.id)}
                onSelectedChange={(on) =>
                  setSelected((prev) => {
                    const next = new Set(prev)
                    if (on) next.add(c.id)
                    else next.delete(c.id)
                    return next
                  })
                }
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

/** 批量操作条：全选/清空 + 目标优先级输入 + 应用。数值小者优先，同档内按设备数负载均衡。 */
function BatchPriorityBar({
  all, selected, onSelectedChange, onClose,
}: {
  all: Credential[]
  selected: Set<number>
  onSelectedChange: (next: Set<number>) => void
  onClose: () => void
}) {
  const qc = useQueryClient()
  const [priority, setPriority] = useState('0')

  const apply = useMutation({
    mutationFn: (p: number) => setPriorities([...selected], p),
    onSuccess: (_list, p) => {
      toast.success(`已把 ${selected.size} 个账号设为 P${p}`)
      qc.invalidateQueries({ queryKey: ['credentials'] })
      onClose()
    },
    onError: (e) => toast.error('批量设置失败', { description: extractError(e) }),
  })

  const allSelected = all.length > 0 && selected.size === all.length
  const parsed = Math.floor(Number(priority) || 0)

  return (
    <div className="flex flex-wrap items-center gap-2 rounded-xl border border-border bg-surface-2/40 px-3 py-2.5 text-xs">
      <Button
        size="sm"
        variant="ghost"
        className="h-7 px-2 text-xs"
        title="选择全部账号（跨页，勾选状态在翻页后保留）"
        onClick={() => onSelectedChange(allSelected ? new Set() : new Set(all.map((c) => c.id)))}
      >
        {allSelected ? '取消全选' : `全选 ${all.length} 个`}
      </Button>
      <span className="text-muted-foreground">已选 <span className="tnum font-medium text-foreground">{selected.size}</span> 个</span>
      <div className="ml-auto flex items-center gap-2">
        <span className="text-muted-foreground">优先级</span>
        <Input
          type="number"
          value={priority}
          onChange={(e) => setPriority(e.target.value)}
          className="h-7 w-20 font-mono text-xs"
          title="数值小者优先被调度；同一档内按设备数负载均衡"
        />
        <Button
          size="sm"
          className="h-7 px-2.5 text-xs"
          disabled={selected.size === 0 || apply.isPending}
          onClick={() => apply.mutate(parsed)}
        >
          {apply.isPending ? <ArrowPathIcon className="size-3.5 animate-spin" /> : <CheckIcon className="size-3.5" />}
          应用
        </Button>
        <Button size="icon" variant="ghost" className="size-7" onClick={onClose} title="退出批量模式">
          <XMarkIcon className="size-3.5" />
        </Button>
      </div>
      <p className="w-full text-2xs text-muted-foreground">
        数值小者优先调度；同一优先级的账号在其内部按设备数负载均衡。想要「榨干一个再用下一个」，把它们设成不同数值即可。
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
