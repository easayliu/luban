import { useState } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import {
  ArrowPathIcon, TrashIcon, PencilIcon, CheckIcon, XMarkIcon, EllipsisHorizontalIcon,
  ChevronUpIcon, ChevronDownIcon, DevicePhoneMobileIcon, ExclamationTriangleIcon,
  CalendarDaysIcon, ClockIcon, WalletIcon,
} from '@heroicons/react/24/outline'
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
  const has5h = cred.quota?.rl_5h_utilization != null
  const has7d = cred.quota?.rl_7d_utilization != null
  const expiry = expiryMeta(cred)
  const status = statusMeta(cred, nearLimit)
  const abnormal = !!cred.ban_reason || cred.expired

  return (
    <Card
      className={cn(
        '@container/card group/card relative overflow-hidden rounded-2xl border-border/70 p-5 pl-[calc(1.25rem-3px)] shadow-card transition-all',
        'before:absolute before:inset-y-0 before:left-0 before:w-[3px] before:transition-colors',
        'hover:border-border hover:shadow-elev',
        cred.disabled && 'opacity-60',
        // 左侧状态轨：一眼分诊。正常态透明，异常态着色。
        status.rail,
        nearLimit && 'ring-1 ring-bad/20',
      )}
    >
      {/* 头部：头像 + 名称/徽章 + 开关/菜单 */}
      <div className="flex items-start gap-3.5">
        <div className="relative shrink-0">
          <div
            className={cn(
              'grid size-10 place-items-center rounded-xl text-sm font-semibold',
              cred.disabled
                ? 'bg-muted text-muted-foreground'
                : 'bg-primary text-primary-foreground',
            )}
            aria-hidden
          >
            {initial}
          </div>
          {/* 状态灯：绿=正常 红=异常 琥珀=将满/将过期 灰=停用，环切合卡片底色。 */}
          <span
            className={cn(
              'absolute -bottom-0.5 -right-0.5 size-3 rounded-full ring-2 ring-card',
              status.dot,
            )}
            title={status.label}
            aria-label={status.label}
          />
        </div>

        <div className="min-w-0 flex-1">
          {editing ? (
            <form
              className="flex items-center gap-1.5"
              onSubmit={(e) => { e.preventDefault(); rename.mutate(name.trim()) }}
            >
              <Input value={name} onChange={(e) => setName(e.target.value)} autoFocus className="h-8 w-56" />
              <Button type="submit" size="icon" variant="ghost" className="h-8 w-8" disabled={rename.isPending}>
                {rename.isPending ? <ArrowPathIcon className="animate-spin" /> : <CheckIcon />}
              </Button>
              <Button type="button" size="icon" variant="ghost" className="h-8 w-8"
                onClick={() => { setEditing(false); setName(cred.label) }}>
                <XMarkIcon />
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
                <PencilIcon className="size-3 shrink-0 text-muted-foreground opacity-0 transition-opacity group-hover/name:opacity-100" />
              </button>
              {nearLimit && (
                <Badge variant="bad" className="shrink-0">
                  <ExclamationTriangleIcon className="size-3" />
                  额度将满 {Math.round(quotaMax * 100)}%
                </Badge>
              )}
            </div>
          )}

          {/* 元信息：固定两行，封禁/正常态布局一致。
              第一行 套餐 + 状态/有效期；第二行 #id · token。 */}
          <div className="mt-1.5 space-y-1.5 text-2xs text-muted-foreground">
            <div className="flex items-center gap-x-3 gap-y-1.5">
              {cred.tier && (
                <Badge
                  variant="outline"
                  className={cn('h-5 shrink-0 gap-1 px-2 py-0 text-2xs font-medium', tierBadgeClass(cred.tier))}
                >
                  {cred.tier}
                </Badge>
              )}
              {/* 调度优先级：只读展示，修改入口在 ⋯ 菜单（低频配置）。 */}
              <Badge
                variant="outline"
                className="h-5 shrink-0 gap-1 px-2 py-0 font-mono text-2xs font-medium text-muted-foreground"
                title="调度优先级（数值小者优先），在 ⋯ 菜单中调整"
              >
                P{cred.priority}
              </Badge>
              <span
                className={cn('inline-flex min-w-0 items-center gap-1', expiry.className)}
                title={cred.ban_reason ?? undefined}
              >
                <ClockIcon className="size-3 shrink-0" />
                <span className="truncate">{expiry.text}</span>
              </span>
            </div>
            <div className="flex items-center gap-1 font-mono">
              <span className="tnum shrink-0">#{cred.id}</span>
              <Dot />
              <span className="min-w-0 truncate" title="refresh_token（脱敏）">
                {cred.token_hint}
              </span>
            </div>
          </div>
        </div>

        {/* 右上控制：启用开关 + 溢出菜单 */}
        <div className="flex shrink-0 items-center gap-1.5">
          {/* 启用开关：健康态开=绿；封禁/过期等异常态转中性灰（避免绿开关与红状态灯语义冲突）。
              切换中显示加载圈占位，避免布局跳动。 */}
          <span className="relative inline-flex items-center">
            <Switch
              variant="success"
              checked={!cred.disabled}
              onCheckedChange={(on) => toggle.mutate(!on)}
              disabled={toggle.isPending}
              title={switchTitle(cred)}
              className={cn(
                toggle.isPending && 'opacity-0',
                // 封禁/过期等异常态：开关转中性灰，不用健康绿，避免与红状态灯冲突。
                abnormal && 'data-[state=checked]:bg-muted-foreground/50',
              )}
            />
            {toggle.isPending && (
              <ArrowPathIcon className="absolute left-1/2 top-1/2 size-4 -translate-x-1/2 -translate-y-1/2 animate-spin text-muted-foreground" />
            )}
          </span>
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button size="icon" variant="ghost" className="size-8 text-muted-foreground">
                <EllipsisHorizontalIcon />
              </Button>
            </DropdownMenuTrigger>
            <DropdownMenuContent align="end">
              <DropdownMenuItem onClick={() => refresh.mutate()} disabled={refresh.isPending}>
                {refresh.isPending ? <ArrowPathIcon className="animate-spin" /> : <ArrowPathIcon />}
                刷新 token
              </DropdownMenuItem>
              <DropdownMenuItem onClick={() => setEditing(true)}>
                <PencilIcon />
                重命名
              </DropdownMenuItem>
              <DropdownMenuSeparator />
              {/* 调度优先级：内联步进器，选中不关闭菜单，可连续调（数值小者优先）。 */}
              <div className="flex items-center justify-between gap-2 px-2 py-1.5 text-sm">
                <span className="text-muted-foreground">调度优先级</span>
                <div
                  className="flex items-center overflow-hidden rounded-md border border-border bg-surface-2/40"
                  title="数值小者优先被调度"
                >
                  <button
                    className="grid h-6 w-6 place-items-center text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:opacity-40"
                    onClick={(e) => { e.preventDefault(); prio.mutate(cred.priority - 1) }}
                    disabled={busy}
                    aria-label="提升优先级"
                  >
                    <ChevronUpIcon className="size-3.5" />
                  </button>
                  <span className="w-7 border-x border-border bg-card text-center text-xs font-medium tnum leading-6">
                    {cred.priority}
                  </span>
                  <button
                    className="grid h-6 w-6 place-items-center text-muted-foreground transition-colors hover:bg-muted hover:text-foreground disabled:opacity-40"
                    onClick={(e) => { e.preventDefault(); prio.mutate(cred.priority + 1) }}
                    disabled={busy}
                    aria-label="降低优先级"
                  >
                    <ChevronDownIcon className="size-3.5" />
                  </button>
                </div>
              </div>
              <DropdownMenuSeparator />
              <DropdownMenuItem
                className="text-bad focus:bg-bad-soft"
                onClick={() => { if (confirm(`确定删除「${cred.label}」？`)) remove.mutate() }}
              >
                <TrashIcon />
                删除
              </DropdownMenuItem>
            </DropdownMenuContent>
          </DropdownMenu>
        </div>
      </div>

      {/* 额度区：5h / 7d 订阅额度（缺失窗口不占位，仅一个时占满整行） */}
      {cred.quota && (has5h || has7d) && (
        <div className={cn('mt-4 grid gap-2.5', has5h && has7d && '@sm/card:grid-cols-2')}>
          {has5h && (
            <QuotaBar
              label="5 小时额度"
              util={cred.quota.rl_5h_utilization}
              reset={cred.quota.rl_5h_reset}
              cost={cred.quota.cost_5h}
            />
          )}
          {has7d && (
            <QuotaBar
              label="7 天额度"
              util={cred.quota.rl_7d_utilization}
              reset={cred.quota.rl_7d_reset}
              cost={cred.quota.cost_7d}
            />
          )}
        </div>
      )}

      {/* 底部：统计信息合并为一行（添加 / 最近使用 / 累计花费 / 设备）。设备可点击编辑上限。 */}
      <div className="mt-4 flex flex-wrap items-center gap-x-3.5 gap-y-1.5 border-t border-border/60 pt-3 text-2xs text-muted-foreground">
        <span
          className="inline-flex items-center gap-1"
          title={`添加于 ${new Date(cred.created_at * 1000).toLocaleString()}`}
        >
          <CalendarDaysIcon className="size-3 shrink-0 opacity-70" />
          {relativeTime(cred.created_at)}
        </span>
        <span className="inline-flex items-center gap-1" title="最近一次转发使用">
          <ClockIcon className="size-3 shrink-0 opacity-70" />
          {cred.last_used != null ? relativeTime(cred.last_used) : '未使用'}
        </span>
        <span
          className="inline-flex items-center gap-1"
          title="该账号历史累计等价 API 费用（按官方定价估算）"
        >
          <WalletIcon className="size-3 shrink-0 opacity-70" />
          <span className="tnum">{formatUsd(cred.cost_total)}</span>
        </span>

        {/* 设备：非编辑态作为统计项，点击展开为上限输入框（0 表示不限）。 */}
        {editingLimit ? (
          <form
            className="ml-auto inline-flex items-center gap-1.5"
            onSubmit={(e) => {
              e.preventDefault()
              limit.mutate(Math.max(0, Math.floor(Number(limitVal) || 0)))
            }}
          >
            <DevicePhoneMobileIcon className="size-3 shrink-0 opacity-70" />
            <Input
              type="number"
              min={0}
              value={limitVal}
              onChange={(e) => setLimitVal(e.target.value)}
              autoFocus
              className="h-6 w-14 px-1.5 text-2xs"
              title="设备数上限；0 表示不限"
            />
            <Button type="submit" size="icon" variant="ghost" className="size-6" disabled={limit.isPending}>
              {limit.isPending ? <ArrowPathIcon className="size-3 animate-spin" /> : <CheckIcon className="size-3" />}
            </Button>
            <Button type="button" size="icon" variant="ghost" className="size-6"
              onClick={() => { setEditingLimit(false); setLimitVal(String(cred.device_limit)) }}>
              <XMarkIcon className="size-3" />
            </Button>
          </form>
        ) : (
          <button
            onClick={() => { setLimitVal(String(cred.device_limit)); setEditingLimit(true) }}
            className="group/limit ml-auto inline-flex items-center gap-1 transition-colors hover:text-foreground"
            title="点击设置设备数上限（0 表示不限）"
          >
            <DevicePhoneMobileIcon className="size-3 shrink-0 opacity-70" />
            <span className="tnum">
              设备 {cred.device_count}/{cred.device_limit > 0 ? cred.device_limit : '∞'}
            </span>
            <PencilIcon className="size-2.5 shrink-0 opacity-0 transition-opacity group-hover/limit:opacity-100" />
          </button>
        )}
      </div>
    </Card>
  )
}

/** 单个额度窗口条：标签 + 百分比 + 进度条 + 重置倒计时 + 本档已用金额。util 为空显示「未返回」。 */
function QuotaBar({
  label, util, reset, cost,
}: {
  label: string
  util: number | null
  reset: number | null
  cost: number | null
}) {
  if (util == null) {
    return (
      <div className="rounded-xl border border-dashed border-border/70 px-3 py-2.5 text-2xs text-muted-foreground">
        {label} · 暂无数据
      </div>
    )
  }
  const pct = Math.min(100, Math.max(0, Math.round(util * 100)))
  const critical = util >= 0.9
  const barColor = critical ? 'bg-bad' : util >= 0.7 ? 'bg-warn' : 'bg-ok'
  const pctColor = critical ? 'text-bad' : util >= 0.7 ? 'text-warn' : 'text-foreground'
  const remain = reset != null ? reset - Math.floor(Date.now() / 1000) : null
  return (
    <div className="rounded-xl border border-border/60 bg-surface-2/40 px-3 py-2.5">
      <div className="flex items-baseline justify-between gap-2">
        <span className="truncate text-2xs font-medium text-muted-foreground">{label}</span>
        <span className={cn('text-sm font-semibold tnum leading-none', pctColor)} title="额度使用率">
          {pct}
          <span className="ml-px text-2xs font-medium text-muted-foreground">%</span>
        </span>
      </div>
      <div
        className="mt-2 h-1.5 overflow-hidden rounded-full bg-border/80"
        role="progressbar"
        aria-valuenow={pct}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-label={label}
      >
        <div
          className={cn(
            'h-full rounded-full transition-[width] duration-500 ease-out',
            barColor,
            // 临界时轻微条纹动画，强化「快满了」的紧迫感。
            critical && 'bg-[length:0.75rem_0.75rem] bg-[image:repeating-linear-gradient(45deg,transparent,transparent_4px,rgba(255,255,255,0.22)_4px,rgba(255,255,255,0.22)_8px)]',
          )}
          style={{ width: `${pct}%` }}
        />
      </div>
      <div className="mt-1.5 flex items-baseline justify-between gap-2 text-2xs text-muted-foreground">
        <span title="本周期内已消耗的等价 API 费用">
          花费 <span className="tnum font-medium text-foreground/80">{formatUsd(cost ?? 0)}</span>
        </span>
        {remain != null && remain > 0 && (
          <span className="tnum" title="额度重置倒计时">{formatDuration(remain)}后重置</span>
        )}
      </div>
    </div>
  )
}

/** 账号档位徽章配色：Max 20x/5x/Max/Pro/Free 用冷色系区分（避开到期徽章的绿/橙/红）。 */
function tierBadgeClass(tier: string): string {
  const t = tier.toLowerCase()
  if (t.includes('20x'))
    return 'border-violet-200 bg-violet-100 text-violet-700 dark:border-violet-500/30 dark:bg-violet-500/15 dark:text-violet-300'
  if (t.includes('5x'))
    return 'border-indigo-200 bg-indigo-100 text-indigo-700 dark:border-indigo-500/30 dark:bg-indigo-500/15 dark:text-indigo-300'
  if (t.includes('max'))
    return 'border-blue-200 bg-blue-100 text-blue-700 dark:border-blue-500/30 dark:bg-blue-500/15 dark:text-blue-300'
  if (t.includes('pro'))
    return 'border-sky-200 bg-sky-100 text-sky-700 dark:border-sky-500/30 dark:bg-sky-500/15 dark:text-sky-300'
  if (t.includes('free'))
    return 'border-border bg-muted text-muted-foreground'
  return 'border-border bg-secondary text-secondary-foreground'
}

/** 凭证综合状态 → 头像状态灯颜色 + 左侧轨道色 + 文案。优先级：封禁 > 停用 > 过期 > 将满/将过期 > 正常。 */
function statusMeta(
  cred: Credential,
  nearLimit: boolean,
): { dot: string; rail: string; label: string } {
  if (cred.ban_reason) return { dot: 'bg-bad', rail: 'before:bg-bad', label: '已封禁' }
  if (cred.disabled) return { dot: 'bg-muted-foreground/50', rail: 'before:bg-transparent', label: '已停用' }
  if (cred.expired) return { dot: 'bg-bad', rail: 'before:bg-bad', label: '已过期' }
  if (nearLimit) return { dot: 'bg-warn', rail: 'before:bg-warn', label: '额度将满' }
  if (cred.expires_in <= 300) return { dot: 'bg-warn', rail: 'before:bg-warn', label: '即将过期' }
  return { dot: 'bg-ok', rail: 'before:bg-transparent', label: '运行正常' }
}

/** 凭证状态/有效期 → 元信息行的文案与配色。异常态着色，正常「剩余」保持中性。 */
function expiryMeta(cred: Credential): { text: string; className: string } {
  if (cred.ban_reason) return { text: '已封禁', className: 'font-medium text-bad' }
  if (cred.disabled) return { text: '已停用', className: 'text-muted-foreground' }
  if (cred.expired) return { text: '已过期', className: 'font-medium text-bad' }
  if (cred.expires_in <= 300) return { text: '即将过期', className: 'font-medium text-warn' }
  return { text: `剩余 ${formatDuration(cred.expires_in)}`, className: 'text-muted-foreground' }
}

/** 启用开关的 hover 提示：封禁态说明「已被上游封禁」并提示仍可手动停用，避免误以为账号在正常工作。 */
function switchTitle(cred: Credential): string {
  if (cred.disabled) return '已停用（点击启用）'
  if (cred.ban_reason) return `${cred.ban_reason} · 点击可手动停用`
  if (cred.expired) return '凭证已过期 · 点击可手动停用'
  return '已启用（点击停用）'
}

function Dot() {
  return <span className="opacity-40">·</span>
}
