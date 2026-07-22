import { useState } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import {
  RefreshCw, Trash2, Pencil, Check, X, Loader2, MoreHorizontal, ChevronUp, ChevronDown,
  Smartphone,
} from 'lucide-react'
import { toast } from 'sonner'
import {
  deleteCredential, refreshCredential, setDeviceLimit, setDisabled, setLabel, setPriority,
  type Credential,
} from '@/api/credentials'
import { cn, extractError, formatDuration, relativeTime } from '@/lib/utils'
import { Card, CardContent } from '@/components/ui/card'
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

  return (
    <Card className={cn('transition-shadow hover:shadow-elev', cred.disabled && 'opacity-60')}>
      <CardContent className="flex items-center gap-3 p-4">
        {/* 主体：名称 + 徽章 + 元信息 */}
        <div className="min-w-0 flex-1">
          <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
            {editing ? (
              <form
                className="flex items-center gap-1.5"
                onSubmit={(e) => { e.preventDefault(); rename.mutate(name.trim()) }}
              >
                <Input value={name} onChange={(e) => setName(e.target.value)} autoFocus className="h-8 w-48" />
                <Button type="submit" size="icon" variant="ghost" className="h-8 w-8" disabled={rename.isPending}>
                  {rename.isPending ? <Loader2 className="animate-spin" /> : <Check />}
                </Button>
                <Button type="button" size="icon" variant="ghost" className="h-8 w-8"
                  onClick={() => { setEditing(false); setName(cred.label) }}>
                  <X />
                </Button>
              </form>
            ) : (
              <>
                <button
                  onClick={() => setEditing(true)}
                  className="group inline-flex min-w-0 items-center gap-1.5"
                  title="点击重命名"
                >
                  <span className="truncate text-sm font-semibold">{cred.label}</span>
                  <Pencil className="size-3 shrink-0 text-muted-foreground opacity-0 transition-opacity group-hover:opacity-100" />
                </button>
                {cred.tier && (
                  <Badge variant="default" className="shrink-0 font-medium">{cred.tier}</Badge>
                )}
                <ExpiryBadge cred={cred} />
              </>
            )}
          </div>
          <div className="mt-1.5 flex flex-wrap items-center gap-2 font-mono text-2xs text-muted-foreground">
            <span className="tnum">#{cred.id}</span>
            <Dot />
            <span className="truncate">{cred.token_hint}</span>
            <Dot />
            {editingLimit ? (
              <form
                className="inline-flex items-center gap-1"
                onSubmit={(e) => {
                  e.preventDefault()
                  const n = Math.max(0, Math.floor(Number(limitVal) || 0))
                  limit.mutate(n)
                }}
              >
                <Smartphone className="size-3 shrink-0" />
                <Input
                  type="number"
                  min={0}
                  value={limitVal}
                  onChange={(e) => setLimitVal(e.target.value)}
                  autoFocus
                  className="h-6 w-16 px-1.5 text-2xs"
                  title="设备数上限；0 表示不限"
                />
                <Button type="submit" size="icon" variant="ghost" className="h-6 w-6" disabled={limit.isPending}>
                  {limit.isPending ? <Loader2 className="animate-spin" /> : <Check className="size-3" />}
                </Button>
                <Button type="button" size="icon" variant="ghost" className="h-6 w-6"
                  onClick={() => { setEditingLimit(false); setLimitVal(String(cred.device_limit)) }}>
                  <X className="size-3" />
                </Button>
              </form>
            ) : (
              <button
                onClick={() => { setLimitVal(String(cred.device_limit)); setEditingLimit(true) }}
                className="group inline-flex items-center gap-1 whitespace-nowrap hover:text-foreground"
                title="点击设置设备数上限（0 表示不限）"
              >
                <Smartphone className="size-3 shrink-0" />
                <span className="tnum">
                  设备 {cred.device_count}/{cred.device_limit > 0 ? cred.device_limit : '∞'}
                </span>
                <Pencil className="size-2.5 shrink-0 opacity-0 transition-opacity group-hover:opacity-100" />
              </button>
            )}
            <Dot />
            <span className="whitespace-nowrap">更新于 {relativeTime(cred.updated_at)}</span>
          </div>
        </div>

        {/* 右侧控制：优先级步进 + 启用开关 + 溢出菜单 */}
        <div className="flex shrink-0 items-center gap-2">
          <div className="flex items-center rounded-lg border border-border" title="优先级（数值小者优先）">
            <button
              className="grid h-8 w-7 place-items-center text-muted-foreground hover:text-foreground disabled:opacity-40"
              onClick={() => prio.mutate(cred.priority - 1)}
              disabled={busy}
              aria-label="提升优先级"
            >
              <ChevronUp className="size-4" />
            </button>
            <span className="w-8 border-x border-border text-center text-xs tnum leading-8">
              {cred.priority}
            </span>
            <button
              className="grid h-8 w-7 place-items-center text-muted-foreground hover:text-foreground disabled:opacity-40"
              onClick={() => prio.mutate(cred.priority + 1)}
              disabled={busy}
              aria-label="降低优先级"
            >
              <ChevronDown className="size-4" />
            </button>
          </div>

          <Switch
            checked={!cred.disabled}
            onCheckedChange={(on) => toggle.mutate(!on)}
            disabled={toggle.isPending}
            title={cred.disabled ? '已停用' : '已启用'}
          />

          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button size="icon" variant="ghost" className="h-8 w-8">
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
      </CardContent>
    </Card>
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
