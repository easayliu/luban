import { useState } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import {
  RefreshCw, Trash2, Pencil, Check, X, Loader2, MoreHorizontal, ChevronUp, ChevronDown,
  Smartphone, AlertTriangle,
} from 'lucide-react'
import { toast } from 'sonner'
import {
  deleteCredential, refreshCredential, setDeviceLimit, setDisabled, setLabel, setPriority,
  type Credential,
} from '@/api/credentials'
import { cn, extractError, formatDuration, formatUsd, relativeTime } from '@/lib/utils'
import { Card } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import {
  DropdownMenu, DropdownMenuTrigger, DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator,
} from '@/components/ui/dropdown-menu'

export function CredentialCard({ cred }: { cred: Credential }) {
  const qc = useQueryClient()
  const invalidate = () => qc.invalidateQueries({ queryKey: ['credentials'] })
  const [editing, setEditing] = useState(false)
  const [name, setName] = useState(cred.label)
  const [editingLimit, setEditingLimit] = useState(false)
  const [limitVal, setLimitVal] = useState(String(cred.device_limit))

  const rename = useMutation({
    mutationFn: (label: string) => setLabel(cred.id, label),
    onSuccess: () => { setEditing(false); invalidate() },
    onError: (e) => toast.error('重命名失败', { description: extractError(e) }),
  })
  const toggle = useMutation({
    mutationFn: (disabled: boolean) => setDisabled(cred.id, disabled),
    onSuccess: invalidate,
    onError: (e) => toast.error('操作失败', { description: extractError(e) }),
  })
  const prio = useMutation({
    mutationFn: (p: number) => setPriority(cred.id, p),
    onSuccess: invalidate,
    onError: (e) => toast.error('设置优先级失败', { description: extractError(e) }),
  })
  const limit = useMutation({
    mutationFn: (n: number) => setDeviceLimit(cred.id, n),
    onSuccess: () => { setEditingLimit(false); invalidate() },
    onError: (e) => toast.error('设置设备上限失败', { description: extractError(e) }),
  })
  const refresh = useMutation({
    mutationFn: () => refreshCredential(cred.id),
    onSuccess: () => { toast.success('已刷新'); invalidate() },
    onError: (e) => toast.error('刷新失败', { description: extractError(e) }),
  })
  const remove = useMutation({
    mutationFn: () => deleteCredential(cred.id),
    onSuccess: () => { toast.success('已删除'); invalidate() },
    onError: (e) => toast.error('删除失败', { description: extractError(e) }),
  })

  const busy = prio.isPending

  // 额度接近上限（5h / 7d 任一 ≥90%）：卡片描边 + 角标提示。
  const quotaMax = Math.max(
    cred.quota?.rl_5h_utilization ?? 0,
    cred.quota?.rl_7d_utilization ?? 0,
  )
  const nearLimit = !cred.disabled && quotaMax >= 0.9
  const initial = cred.label.trim().charAt(0).toUpperCase() || '?'

  return (
    <Card
      className={cn(
        'group/card overflow-hidden rounded-2xl border-border/70 p-5 shadow-card transition-all',
        'hover:border-border hover:shadow-elev',
        cred.disabled && 'opacity-60',
        nearLimit && 'border-bad/40 ring-1 ring-bad/25',
      )}
    >
      {/* 头部：头像 + 名称/徽章 + 开关/菜单 */}
      <div className="flex items-start gap-3.5">
        <div
          className={cn(
            'grid size-10 shrink-0 place-items-center rounded-xl text-sm font-semibold',
            cred.disabled
              ? 'bg-muted text-muted-foreground'
              : 'bg-primary text-primary-foreground',
          )}
          aria-hidden
        >
          {initial}
        </div>

        <div className="min-w-0 flex-1">
          {editing ? (
            <form
              className="flex items-center gap-1.5"
              onSubmit={(e) => { e.preventDefault(); rename.mutate(name.trim()) }}
            >
              <Input value={name} onChange={(e) => setName(e.target.value)} autoFocus className="h-8 w-56" />
              <Button type="submit" size="icon" variant="ghost" className="h-8 w-8" disabled={rename.isPending}>
                {rename.isPending ? <Loader2 className="animate-spin" /> : <Check />}
              </Button>
              <Button type="button" size="icon" variant="ghost" className="h-8 w-8"
                onClick={() => { setEditing(false); setName(cred.label) }}>
                <X />
              </Button>
            </form>
          ) : (
            <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
              <button
                onClick={() => setEditing(true)}
                className="group/name inline-flex min-w-0 items-center gap-1.5"
                title="点击重命名"
              >
                <span className="truncate text-sm font-semibold tracking-tight">{cred.label}</span>
                <Pencil className="size-3 shrink-0 text-muted-foreground opacity-0 transition-opacity group-hover/name:opacity-100" />
              </button>
              {cred.tier && (
                <Badge variant="default" className="shrink-0 font-medium">{cred.tier}</Badge>
              )}
              <ExpiryBadge cred={cred} />
              {nearLimit && (
                <Badge variant="bad" className="shrink-0">
                  <AlertTriangle className="size-3" />
                  额度将满 {Math.round(quotaMax * 100)}%
                </Badge>
              )}
            </div>
          )}

          {/* 名称下方：轻量标识 */}
          <div className="mt-1 flex flex-wrap items-center gap-x-2 font-mono text-2xs text-muted-foreground">
            <span className="tnum">#{cred.id}</span>
            <Dot />
            <span className="truncate">{cred.token_hint}</span>
            <Dot />
            <span className="whitespace-nowrap">
              {cred.last_used != null ? `最近使用 ${relativeTime(cred.last_used)}` : '尚未使用'}
            </span>
            <Dot />
            <span className="whitespace-nowrap" title="按官方定价估算的累计等价 API 费用">
              累计 {formatUsd(cred.cost_total)}
            </span>
          </div>
        </div>

        {/* 右上控制：启用开关 + 溢出菜单 */}
        <div className="flex shrink-0 items-center gap-1.5">
          <Switch
            checked={!cred.disabled}
            onCheckedChange={(on) => toggle.mutate(!on)}
            disabled={toggle.isPending}
            title={cred.disabled ? '已停用' : '已启用'}
          />
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button size="icon" variant="ghost" className="size-8 text-muted-foreground">
                <MoreHorizontal />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              <DropdownMenuItem onClick={() => refresh.mutate()} disabled={refresh.isPending}>
                {refresh.isPending ? <Loader2 className="animate-spin" /> : <RefreshCw />}
                刷新 token
              </DropdownMenuItem>
              <DropdownMenuItem onClick={() => setEditing(true)}>
                <Pencil />
                重命名
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              <DropdownMenuItem
                className="text-bad focus:bg-bad-soft"
                onClick={() => { if (confirm(`确定删除「${cred.label}」？`)) remove.mutate() }}
              >
                <Trash2 />
                删除
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </div>

      {/* 额度区：5h / 7d 订阅额度 */}
      {cred.quota && (
        <div className="mt-4 grid gap-2.5 sm:grid-cols-2">
          <QuotaBar label="5 小时" util={cred.quota.rl_5h_utilization} reset={cred.quota.rl_5h_reset} />
          <QuotaBar label="7 天" util={cred.quota.rl_7d_utilization} reset={cred.quota.rl_7d_reset} />
        </div>
      )}

      {/* 底部：设备上限 + 优先级 */}
      <div className="mt-4 flex items-center justify-between gap-3 border-t border-border/60 pt-3">
        {editingLimit ? (
          <form
            className="inline-flex items-center gap-1.5 text-xs"
            onSubmit={(e) => {
              e.preventDefault()
              limit.mutate(Math.max(0, Math.floor(Number(limitVal) || 0)))
            }}
          >
            <Smartphone className="size-3.5 text-muted-foreground" />
            <Input
              type="number"
              min={0}
              value={limitVal}
              onChange={(e) => setLimitVal(e.target.value)}
              autoFocus
              className="h-7 w-16 px-2 text-xs"
              title="设备数上限；0 表示不限"
            />
            <Button type="submit" size="icon" variant="ghost" className="size-7" disabled={limit.isPending}>
              {limit.isPending ? <Loader2 className="animate-spin" /> : <Check className="size-3.5" />}
            </Button>
            <Button type="button" size="icon" variant="ghost" className="size-7"
              onClick={() => { setEditingLimit(false); setLimitVal(String(cred.device_limit)) }}>
              <X className="size-3.5" />
            </Button>
          </form>
        ) : (
          <button
            onClick={() => { setLimitVal(String(cred.device_limit)); setEditingLimit(true) }}
            className="group/limit inline-flex items-center gap-1.5 text-xs text-muted-foreground transition-colors hover:text-foreground"
            title="点击设置设备数上限（0 表示不限）"
          >
            <Smartphone className="size-3.5 shrink-0" />
            <span className="tnum">
              设备 {cred.device_count}/{cred.device_limit > 0 ? cred.device_limit : '∞'}
            </span>
            <Pencil className="size-2.5 shrink-0 opacity-0 transition-opacity group-hover/limit:opacity-100" />
          </button>
        )}

        <div className="flex items-center gap-2">
          <span className="label-eyebrow">优先级</span>
          <div className="flex items-center rounded-lg border border-border" title="优先级（数值小者优先）">
            <button
              className="grid h-7 w-7 place-items-center text-muted-foreground hover:text-foreground disabled:opacity-40"
              onClick={() => prio.mutate(cred.priority - 1)}
              disabled={busy}
              aria-label="提升优先级"
            >
              <ChevronUp className="size-4" />
            </button>
            <span className="w-8 border-x border-border text-center text-xs tnum leading-7">
              {cred.priority}
            </span>
            <button
              className="grid h-7 w-7 place-items-center text-muted-foreground hover:text-foreground disabled:opacity-40"
              onClick={() => prio.mutate(cred.priority + 1)}
              disabled={busy}
              aria-label="降低优先级"
            >
              <ChevronDown className="size-4" />
            </button>
          </div>
        </div>
      </div>
    </Card>
  )
}

/** 单个额度窗口条：标签 + 百分比 + 进度条 + 重置倒计时。util 为空显示「未返回」。 */
function QuotaBar({ label, util, reset }: { label: string; util: number | null; reset: number | null }) {
  if (util == null) {
    return (
      <div className="rounded-xl border border-dashed border-border/70 px-3 py-2.5 text-2xs text-muted-foreground">
        {label} · 本次响应未返回
      </div>
    )
  }
  const pct = Math.min(100, Math.max(0, Math.round(util * 100)))
  const barColor = util >= 0.9 ? 'bg-bad' : util >= 0.7 ? 'bg-warn' : 'bg-ok'
  const remain = reset != null ? reset - Math.floor(Date.now() / 1000) : null
  return (
    <div className="rounded-xl border border-border/60 bg-surface-2/40 px-3 py-2.5">
      <div className="flex items-baseline justify-between">
        <span className="text-2xs font-medium text-muted-foreground">{label}</span>
        <span className="text-xs font-semibold tnum">{pct}%</span>
      </div>
      <div className="mt-2 h-1.5 overflow-hidden rounded-full bg-border">
        <div className={cn('h-full rounded-full transition-all', barColor)} style={{ width: `${pct}%` }} />
      </div>
      {remain != null && remain > 0 && (
        <div className="mt-1.5 text-2xs text-muted-foreground">{formatDuration(remain)}后重置</div>
      )}
    </div>
  )
}

function ExpiryBadge({ cred }: { cred: Credential }) {
  if (cred.disabled) return <Badge variant="outline" className="shrink-0">已停用</Badge>
  if (cred.expired) return <Badge variant="bad" className="shrink-0">已过期</Badge>
  if (cred.expires_in <= 300) return <Badge variant="warn" className="shrink-0">即将过期</Badge>
  return <Badge variant="ok" className="shrink-0">剩余 {formatDuration(cred.expires_in)}</Badge>
}

function Dot() {
  return <span className="opacity-40">·</span>
}
