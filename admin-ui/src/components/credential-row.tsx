import { useState } from 'react'
import {
  ArrowPathIcon, CheckIcon, XMarkIcon, EllipsisHorizontalIcon, DevicePhoneMobileIcon,
} from '@heroicons/react/24/outline'
import { type Credential } from '@/api/credentials'
import { cn, formatUsd, relativeTime } from '@/lib/utils'
import {
  CredentialMenuContent, expiryMeta, isAbnormal, isNearLimit, statusMeta, switchTitle,
  tierBadgeClass, useCredentialActions,
} from '@/components/credential-shared'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { DropdownMenu, DropdownMenuTrigger } from '@/components/ui/dropdown-menu'

/**
 * 紧凑列表行：一行一个账号，账号多时一屏能看十几个。
 *
 * 信息取卡片视图的关键子集——状态灯、名称、档位/优先级、5h 额度条、设备、花费、最近使用，
 * 写操作（启用开关、⋯ 菜单、重命名）与卡片共用 [`useCredentialActions`]。
 * 设备上限这类低频配置留在卡片视图里改，列表只读展示。
 */
export function CredentialRow({
  cred, selectable = false, selected = false, onSelectedChange,
}: {
  cred: Credential
  selectable?: boolean
  selected?: boolean
  onSelectedChange?: (next: boolean) => void
}) {
  const [editing, setEditing] = useState(false)
  const [name, setName] = useState(cred.label)
  const actions = useCredentialActions(cred, () => setEditing(false))
  const { rename, toggle } = actions

  const nearLimit = isNearLimit(cred)
  const status = statusMeta(cred, nearLimit)
  const expiry = expiryMeta(cred)
  const util = cred.quota?.rl_5h_utilization ?? null

  return (
    <div
      className={cn(
        'relative flex items-center gap-3 border-b border-border/60 px-3 py-2.5 text-xs transition-colors last:border-b-0 hover:bg-muted/40',
        'before:absolute before:inset-y-0 before:left-0 before:w-[3px]',
        status.rail,
        cred.disabled && 'opacity-60',
      )}
    >
      {selectable && (
        <input
          type="checkbox"
          checked={selected}
          onChange={(e) => onSelectedChange?.(e.target.checked)}
          className="size-4 shrink-0 accent-primary"
          aria-label={`选择 ${cred.label}`}
        />
      )}

      {/* 状态灯 */}
      <span
        className={cn('size-2 shrink-0 rounded-full', status.dot)}
        title={`${status.label}${cred.ban_reason ? ` · ${cred.ban_reason}` : ''}`}
        aria-label={status.label}
      />

      {/* 名称（点击重命名） + 档位 + 优先级 */}
      <div className="flex min-w-0 flex-1 items-center gap-2">
        {editing ? (
          <form
            className="flex min-w-0 items-center gap-1"
            onSubmit={(e) => { e.preventDefault(); rename.mutate(name.trim()) }}
          >
            <Input value={name} onChange={(e) => setName(e.target.value)} autoFocus className="h-7 w-44 text-xs" />
            <Button type="submit" size="icon" variant="ghost" className="size-7" disabled={rename.isPending}>
              {rename.isPending ? <ArrowPathIcon className="size-3.5 animate-spin" /> : <CheckIcon className="size-3.5" />}
            </Button>
            <Button type="button" size="icon" variant="ghost" className="size-7"
              onClick={() => { setEditing(false); setName(cred.label) }}>
              <XMarkIcon className="size-3.5" />
            </Button>
          </form>
        ) : (
          <button
            onClick={() => setEditing(true)}
            className="min-w-0 truncate font-medium hover:underline"
            title={`${cred.label} · #${cred.id} · 点击重命名`}
          >
            {cred.label}
          </button>
        )}
        {cred.tier && (
          <Badge
            variant="outline"
            className={cn('hidden h-5 shrink-0 px-1.5 py-0 text-2xs font-medium @md:inline-flex', tierBadgeClass(cred.tier))}
          >
            {cred.tier}
          </Badge>
        )}
        <span className="shrink-0 font-mono text-2xs text-muted-foreground" title="调度优先级（数值小者优先）">
          P{cred.priority}
        </span>
        {/* 异常态直接把原因摆到名字后面，不用悬停才看得到。 */}
        {(isAbnormal(cred) || nearLimit) && (
          <span className={cn('hidden shrink-0 text-2xs @2xl:inline', expiry.className)}>
            {nearLimit && !isAbnormal(cred) ? `额度 ${Math.round(util != null ? util * 100 : 0)}%` : expiry.text}
          </span>
        )}
      </div>

      {/* 5h 额度条 */}
      <div className="hidden w-28 shrink-0 items-center gap-1.5 @xl:flex" title="5 小时额度使用率">
        {util != null ? (
          <>
            <div className="h-1.5 flex-1 overflow-hidden rounded-full bg-border/80">
              <div
                className={cn(
                  'h-full rounded-full',
                  util >= 0.9 ? 'bg-bad' : util >= 0.7 ? 'bg-warn' : 'bg-ok',
                )}
                style={{ width: `${Math.min(100, Math.max(0, Math.round(util * 100)))}%` }}
              />
            </div>
            <span className="w-8 shrink-0 text-right tnum text-2xs text-muted-foreground">
              {Math.round(util * 100)}%
            </span>
          </>
        ) : (
          <span className="flex-1 text-right text-2xs text-muted-foreground">额度未知</span>
        )}
      </div>

      {/* 设备 / 花费 / 最近使用 */}
      <span
        className="hidden w-16 shrink-0 items-center justify-end gap-1 tnum text-muted-foreground @md:flex"
        title={cred.device_limit === 0 ? '设备数 / 上限（跟随全局默认）' : '设备数 / 上限'}
      >
        <DevicePhoneMobileIcon className="size-3 shrink-0 opacity-70" />
        {cred.device_count}/{cred.device_limit_effective > 0 ? cred.device_limit_effective : '∞'}
      </span>
      <span className="hidden w-16 shrink-0 text-right tnum text-muted-foreground @lg:block" title="累计等价 API 费用">
        {formatUsd(cred.cost_total)}
      </span>
      <span className="hidden w-16 shrink-0 text-right text-muted-foreground @2xl:block" title="最近一次转发使用">
        {cred.last_used != null ? relativeTime(cred.last_used) : '未使用'}
      </span>

      {/* 启用开关 + ⋯ 菜单 */}
      <span className="relative inline-flex shrink-0 items-center">
        <Switch
          variant="success"
          checked={!cred.disabled}
          onCheckedChange={(on) => toggle.mutate(!on)}
          disabled={toggle.isPending}
          title={switchTitle(cred)}
          className={cn(
            'scale-90',
            toggle.isPending && 'opacity-0',
            isAbnormal(cred) && 'data-[state=checked]:bg-muted-foreground/50',
          )}
        />
        {toggle.isPending && (
          <ArrowPathIcon className="absolute left-1/2 top-1/2 size-3.5 -translate-x-1/2 -translate-y-1/2 animate-spin text-muted-foreground" />
        )}
      </span>
      <DropdownMenu>
        <DropdownMenuTrigger asChild>
          <Button size="icon" variant="ghost" className="size-7 shrink-0 text-muted-foreground">
            <EllipsisHorizontalIcon className="size-4" />
          </Button>
        </DropdownMenuTrigger>
        <CredentialMenuContent cred={cred} actions={actions} onRename={() => setEditing(true)} />
      </DropdownMenu>
    </div>
  )
}
