import { useMemo, useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import {
  PlusIcon, Cog6ToothIcon, ArrowRightStartOnRectangleIcon, ArrowsUpDownIcon, CheckIcon,
} from '@heroicons/react/24/outline'
import { listCredentials, type Credential } from '@/api/credentials'
import { getAuthState } from '@/api/auth'
import { getPw, setPw, clearPw } from '@/api/client'
import { cn, formatUsd } from '@/lib/utils'
import { CredentialCard } from '@/components/credential-card'
import { AddAccount } from '@/components/add-account'
import { AccessSettings } from '@/components/access-settings'
import { LoginPage } from '@/components/login-page'
import { Button } from '@/components/ui/button'
import { Toaster } from '@/components/ui/sonner'
import {
  DropdownMenu, DropdownMenuTrigger, DropdownMenuContent, DropdownMenuItem,
} from '@/components/ui/dropdown-menu'

type SortKey = 'priority' | 'usage5h' | 'cost' | 'recent' | 'created'

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
  const active = creds?.filter((c) => !c.disabled).length ?? 0
  const costTotal = creds?.reduce((s, c) => s + (c.cost_total ?? 0), 0) ?? 0
  const cost5hTotal = creds?.reduce((s, c) => s + (c.quota?.cost_5h ?? 0), 0) ?? 0
  const nearLimitCount =
    creds?.filter(
      (c) =>
        !c.disabled &&
        Math.max(c.quota?.rl_5h_utilization ?? 0, c.quota?.rl_7d_utilization ?? 0) >= 0.9,
    ).length ?? 0

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
            <Button
              size="sm"
              variant={showSettings ? 'secondary' : 'outline'}
              onClick={() => setShowSettings((s) => !s)}
              title="接入设置"
            >
              <Cog6ToothIcon />
              <span className="hidden sm:inline">接入设置</span>
            </Button>
            {!adding && (
              <Button size="sm" onClick={() => setAdding(true)}>
                <PlusIcon />
                <span className="hidden sm:inline">添加账号</span>
              </Button>
            )}
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
        {/* 可展开面板 */}
        {adding && <AddAccount onClose={() => setAdding(false)} />}
        {showSettings && <AccessSettings />}

        {/* KPI 概览 */}
        {count > 0 && (
          <div className="grid grid-cols-2 gap-3 @xl:grid-cols-4">
            <StatCard label="账号" value={count} sub={`${active} 个启用`} />
            <StatCard label="累计花费" value={formatUsd(costTotal)} sub="等价 API 费用" />
            <StatCard label="近 5 小时花费" value={formatUsd(cost5hTotal)} sub="当前额度周期" />
            <StatCard
              label="额度告警"
              value={nearLimitCount}
              sub="使用率 ≥ 90%"
              tone={nearLimitCount > 0 ? 'bad' : undefined}
            />
          </div>
        )}

        {/* 工具栏 */}
        {count > 0 && (
          <div className="flex items-center justify-between gap-2">
            <h2 className="text-sm font-semibold tracking-tight">账号列表</h2>
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
        )}

        {/* 卡片栅格 */}
        {isLoading ? (
          <div className="py-16 text-center text-sm text-muted-foreground">加载中…</div>
        ) : count === 0 && !adding ? (
          <EmptyState onAdd={() => setAdding(true)} />
        ) : (
          // 单账号：单列自适应铺满内容宽度（与上方 KPI 行对齐）；多账号：容器查询两列。
          <div className={cn('grid grid-cols-1 gap-4', count > 1 && '@4xl:grid-cols-2')}>
            {sorted.map((c) => (
              <CredentialCard key={c.id} cred={c} />
            ))}
          </div>
        )}
      </main>
      <Toaster position="top-right" />
    </div>
  )
}

/** dashboard 概览的小指标卡。 */
function StatCard({
  label, value, sub, tone,
}: {
  label: string
  value: string | number
  sub?: string
  tone?: 'bad'
}) {
  return (
    <div className="rounded-xl border border-border/70 bg-card p-4 shadow-card">
      <div className="label-eyebrow">{label}</div>
      <div className={cn('mt-1.5 text-xl font-semibold tracking-tight tabular-nums', tone === 'bad' && 'text-bad')}>
        {value}
      </div>
      {sub && <div className="mt-0.5 text-2xs text-muted-foreground">{sub}</div>}
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
