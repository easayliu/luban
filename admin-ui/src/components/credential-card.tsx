import { useState } from 'react'
import {
  ArrowPathIcon, CheckIcon, XMarkIcon, EllipsisHorizontalIcon,
  DevicePhoneMobileIcon, ClockIcon, WalletIcon, ExclamationTriangleIcon, ChevronRightIcon,
  CalendarDaysIcon,
} from '@heroicons/react/24/outline'
import { type Credential } from '@/api/credentials'
import {
  cn, formatClockTime, formatFullTime, formatUsd, relativeTime,
} from '@/lib/utils'
import {
  ConnectivityTestDialog, credentialExpiryMeta, CredentialMenuContent,
  DeleteCredentialDialog, inputToLimit, isAbnormal, isNearLimit, limitToInput, liveQuota,
  statusMeta, switchTitle, tierBadgeClass, useCredentialActions,
} from '@/components/credential-shared'
import { CredentialDevicesDialog } from '@/components/credential-devices-dialog'
import { Card } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { DropdownMenu, DropdownMenuTrigger } from '@/components/ui/dropdown-menu'

export function CredentialCard({
  cred, selectable = false, selected = false, onSelectedChange,
}: {
  cred: Credential
  /** 批量模式：卡片显示勾选框。 */
  selectable?: boolean
  selected?: boolean
  onSelectedChange?: (next: boolean) => void
}) {
  const [editing, setEditing] = useState(false)
  const [name, setName] = useState(cred.label)
  const [editingLimit, setEditingLimit] = useState(false)
  const [limitVal, setLimitVal] = useState(limitToInput(cred.device_limit))
  // 设备明细放到弹窗里，避免多账号网格被单张展开卡片撑出大块空洞。
  const [devicesOpen, setDevicesOpen] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [testing, setTesting] = useState(false)

  const actions = useCredentialActions(
    cred,
    () => setEditing(false),
    () => setEditingLimit(false),
  )
  const { rename, toggle, limit } = actions

  // 额度接近上限（5h / 7d 任一 ≥90%）：用于状态标签与卡片描边。
  // 用 liveQuota 而非原始快照——窗口已重置的百分比是上个周期的，不该再触发告警。
  const { u5h, u7d } = liveQuota(cred)
  const nearLimit = isNearLimit(cred)
  const initial = cred.label.trim().charAt(0).toUpperCase() || '?'
  // 窗口已重置的仍然渲染额度条（由 QuotaBar 显示成「已重置」），只是不参与告警。
  const has5h = cred.quota?.rl_5h_utilization != null
  const has7d = cred.quota?.rl_7d_utilization != null
  const expiry = credentialExpiryMeta(cred)
  const status = statusMeta(cred, nearLimit)
  const abnormal = isAbnormal(cred)
  const effectiveDeviceLimit = cred.device_limit_effective > 0 ? cred.device_limit_effective : '∞'
  const deviceFull = cred.device_limit_effective > 0
    && cred.device_count >= cred.device_limit_effective

  return (
    <Card
      className={cn(
        '@container/card group/card relative flex h-full flex-col overflow-hidden rounded-xl border-border/70 bg-card p-0 shadow-sm transition-[border-color,box-shadow,background-color]',
        cred.disabled && 'bg-muted/15',
        abnormal && 'border-bad/25',
        !abnormal && nearLimit && 'border-warn/25',
        selected && 'border-primary/50 ring-1 ring-primary/15',
      )}
    >
      {/* Tailwind Application UI 风格的 card heading：头像、主标题、元信息、尾部操作。 */}
      <header className="px-4 py-4 sm:px-5">
        <div className="grid grid-cols-[2.5rem_minmax(0,1fr)_2.5rem] items-start gap-3">
          {selectable ? (
            <label
              className={cn(
                'grid size-10 cursor-pointer place-items-center rounded-full border border-border bg-background shadow-sm',
                selected && 'bg-primary/5',
              )}
            >
              <input
                type="checkbox"
                checked={selected}
                onChange={(e) => onSelectedChange?.(e.target.checked)}
                className="size-4 rounded border-border accent-primary"
                aria-label={`选择 ${cred.label}`}
              />
            </label>
          ) : (
            <div
              className={cn(
                'grid size-10 place-items-center rounded-full bg-muted text-xs font-semibold ring-1 ring-inset ring-border/70',
                cred.disabled ? 'bg-muted/70 text-muted-foreground/70' : 'bg-muted text-foreground',
              )}
              aria-hidden
            >
              {initial}
            </div>
          )}

          {editing ? (
            <form
              className="col-span-2 grid min-w-0 grid-cols-[minmax(0,1fr)_2.5rem_2.5rem] items-center gap-1"
              onSubmit={(e) => { e.preventDefault(); rename.mutate(name.trim()) }}
            >
              <Input
                value={name}
                onChange={(e) => setName(e.target.value)}
                autoFocus
                className="h-10 min-w-0 px-2.5 text-sm"
                aria-label="账号名称"
              />
              <Button
                type="submit"
                size="icon"
                variant="ghost"
                className="size-10"
                disabled={rename.isPending}
                aria-label="保存账号名称"
              >
                {rename.isPending ? <ArrowPathIcon className="animate-spin" /> : <CheckIcon />}
              </Button>
              <Button
                type="button"
                size="icon"
                variant="ghost"
                className="size-10"
                aria-label="取消重命名"
                onClick={() => { setEditing(false); setName(cred.label) }}
              >
                <XMarkIcon />
              </Button>
            </form>
          ) : (
            <>
              <div className="min-w-0">
                <div
                  className="line-clamp-2 break-words text-base font-semibold leading-5 tracking-tight @sm/card:line-clamp-1"
                  title={cred.label}
                >
                  {cred.label}
                </div>

                <div className="mt-2 flex flex-wrap items-center gap-x-2 gap-y-1.5 text-xs text-muted-foreground">
                  {/* 配色与 statusMeta 同源：access_token 的有效期不参与判色，token 到点会在
                      下次使用时自动刷新，而 `expires_in <= 300` 正是后端的刷新窗口。 */}
                  <Badge
                    variant={
                      cred.ban_reason
                        ? 'bad'
                        : cred.disabled
                          ? 'outline'
                          : cred.rate_limited_secs > 0 || nearLimit
                            ? 'warn'
                            : 'ok'
                    }
                    className={cn('h-5 px-2 py-0 text-xs', cred.disabled && 'text-muted-foreground')}
                  >
                    <span className={cn('size-1.5 rounded-full', status.dot)} aria-hidden />
                    {status.label}
                  </Badge>
                  <span className="font-mono tnum" title={`账号 ID：${cred.id}`}>
                    #{cred.id}
                  </span>
                  {cred.tier && (
                    <Badge
                      variant="outline"
                      className={cn('h-5 max-w-full truncate px-2 py-0 text-xs', tierBadgeClass(cred.tier))}
                      title={cred.tier}
                    >
                      {cred.tier}
                    </Badge>
                  )}
                  <span className="font-mono tnum" title="调度优先级，数值小者优先">
                    P{cred.priority}
                  </span>
                  <span
                    className="inline-flex items-center gap-1 whitespace-nowrap"
                    title={`添加于 ${formatFullTime(cred.created_at)}`}
                  >
                    <CalendarDaysIcon className="size-3.5" aria-hidden />
                    添加于 {relativeTime(cred.created_at)}
                  </span>
                </div>
              </div>

              {/* 菜单里会打开 Dialog。禁用菜单自身的 modal 指针锁，避免它与 Dialog 的
                  pointer-events 恢复互相覆盖，导致关闭测试弹窗后整页无法再次点击。 */}
              <DropdownMenu modal={false}>
                <DropdownMenuTrigger asChild>
                  <Button size="icon" variant="ghost" className="-mr-2 -mt-2 size-10 text-muted-foreground" aria-label={`打开 ${cred.label} 菜单`}>
                    <EllipsisHorizontalIcon />
                  </Button>
                </DropdownMenuTrigger>
                <CredentialMenuContent
                  cred={cred}
                  actions={actions}
                  onRename={() => setEditing(true)}
                  onDeviceLimit={() => { setLimitVal(limitToInput(cred.device_limit)); setEditingLimit(true) }}
                  onTest={() => setTesting(true)}
                  onRequestDelete={() => setConfirmDelete(true)}
                />
              </DropdownMenu>
            </>
          )}
        </div>
        {cred.ban_reason && !editing && (
          <div
            className="mt-3 flex items-start gap-2 rounded-md bg-bad-soft px-3 py-2.5 text-bad ring-1 ring-inset ring-bad/10"
            role="alert"
          >
            <ExclamationTriangleIcon className="mt-0.5 size-4 shrink-0" aria-hidden />
            <p
              className="line-clamp-2 min-w-0 flex-1 break-words text-xs leading-5 @sm/card:line-clamp-1"
              title={cred.ban_reason}
            >
              {cred.ban_reason}
            </p>
          </div>
        )}
      </header>

      {/* 主体只保留一层共享分隔线，避免在卡片里继续套小卡片。 */}
      <section className="flex flex-1 flex-col border-t border-border/70">
        <div className="flex items-center justify-between gap-3 px-4 pt-4 sm:px-5">
          <h3 className="text-sm font-semibold text-foreground">额度使用</h3>
          {cred.quota && (
            <span className="text-2xs text-muted-foreground" title={formatFullTime(cred.quota.ts)}>
              更新于 {relativeTime(cred.quota.ts)}
            </span>
          )}
        </div>
        {cred.quota && (has5h || has7d) ? (
          <div className={cn('mt-1 grid flex-1', has5h && has7d && '@sm/card:grid-cols-2')}>
            {has5h && (
              <QuotaBar
                label="5 小时"
                util={u5h}
                reset={cred.quota.rl_5h_reset}
                cost={cred.quota.cost_5h}
                snapshotTs={cred.quota.ts}
                className={cn(has7d && 'border-b border-border/70 @sm/card:border-b-0 @sm/card:border-r')}
              />
            )}
            {has7d && (
              <QuotaBar
                label="7 天"
                util={u7d}
                reset={cred.quota.rl_7d_reset}
                cost={cred.quota.cost_7d}
                snapshotTs={cred.quota.ts}
              />
            )}
          </div>
        ) : (
          <div className="grid min-h-24 flex-1 place-items-center px-4 text-sm text-muted-foreground sm:px-5">
            暂无额度数据
          </div>
        )}
      </section>

      {/* Tailwind card with gray footer：次级事实共享背景与边框，不再各自套圆角。 */}
      {editingLimit ? (
        <div className="mt-auto border-t border-border/70 bg-muted/20 px-4 py-4 sm:px-5">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <div>
              <div className="text-sm font-medium">设备上限</div>
              <p className="mt-0.5 text-xs text-muted-foreground">留空 = 默认，0 = 不限</p>
            </div>
            <form className="flex w-full items-center justify-end gap-1 sm:w-auto" onSubmit={(e) => { e.preventDefault(); limit.mutate(inputToLimit(limitVal)) }}>
              <Input
                type="number"
                min={0}
                value={limitVal}
                onChange={(e) => setLimitVal(e.target.value)}
                autoFocus
                placeholder="默认"
                className="h-9 w-20 px-2 text-xs"
                aria-label="设备上限"
              />
              <Button type="submit" size="icon" variant="ghost" className="size-9" disabled={limit.isPending} aria-label="保存设备上限">
                {limit.isPending ? <ArrowPathIcon className="size-3 animate-spin" /> : <CheckIcon className="size-3" />}
              </Button>
              <Button type="button" size="icon" variant="ghost" className="size-9" aria-label="取消修改设备上限"
                onClick={() => { setEditingLimit(false); setLimitVal(limitToInput(cred.device_limit)) }}>
                <XMarkIcon className="size-3" />
              </Button>
            </form>
          </div>
        </div>
      ) : (
        <div className="mt-auto grid grid-cols-2 border-t border-border/70 bg-muted/20 @lg/card:grid-cols-[repeat(4,minmax(0,1fr))_6rem]">
          <Button
            type="button"
            variant="ghost"
            onClick={() => setDevicesOpen(true)}
            className="h-auto min-h-16 min-w-0 items-stretch justify-start gap-0 whitespace-normal rounded-none border-b border-r border-border/70 px-3 py-3 text-left hover:bg-muted/40 @lg/card:border-b-0"
            title="查看已绑定设备"
            aria-haspopup="dialog"
            aria-label={`查看已绑定设备，当前 ${cred.device_count} 台`}
          >
            <span className="block min-w-0 flex-1">
              <span className="flex items-center gap-1.5 text-xs font-normal text-muted-foreground">
                <DevicePhoneMobileIcon className="size-3.5" />
                设备
              </span>
              <span className="mt-1 flex min-w-0 items-center justify-between gap-1.5">
                <span className="flex min-w-0 items-center gap-1.5">
                  <span
                    className={cn(
                      'shrink-0 text-sm font-semibold tnum text-foreground',
                      deviceFull && 'text-warn',
                    )}
                  >
                    {cred.device_count}/{effectiveDeviceLimit}
                  </span>
                  {cred.device_limit === 0 && (
                    <span className="shrink-0 rounded px-1 py-0.5 text-2xs font-normal leading-none text-muted-foreground ring-1 ring-inset ring-border/70">
                      默认
                    </span>
                  )}
                </span>
                <ChevronRightIcon className="size-3.5 shrink-0 text-muted-foreground/70" aria-hidden />
              </span>
            </span>
          </Button>
          <div className="min-h-16 min-w-0 border-b border-border/70 px-3 py-3 @lg/card:border-b-0 @lg/card:border-r" title="最近一次转发使用">
            <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
              <ClockIcon className="size-3.5" />
              最近使用
            </div>
            <div className="mt-1 truncate text-sm font-semibold tnum text-foreground">
              {cred.last_used != null ? relativeTime(cred.last_used) : '未使用'}
            </div>
          </div>
          <div className="min-h-16 min-w-0 border-r border-border/70 px-3 py-3" title="该账号历史累计等价 API 费用（按官方定价估算）">
            <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
              <WalletIcon className="size-3.5" />
              累计花费
            </div>
            <div className="mt-1 truncate text-sm font-semibold tnum text-foreground">{formatUsd(cred.cost_total)}</div>
          </div>
          <div className="min-h-16 min-w-0 px-3 py-3 @lg/card:border-r" title={expiry.title}>
            <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
              <ClockIcon className="size-3.5" />
              有效期
            </div>
            <div className={cn('mt-1 truncate text-sm font-semibold tnum', expiry.className)}>
              {expiry.text}
            </div>
          </div>
          <div className="col-span-2 flex min-h-14 items-center justify-between gap-3 border-t border-border/70 px-3 py-2.5 @lg/card:col-span-1 @lg/card:min-h-16 @lg/card:flex-col @lg/card:justify-center @lg/card:gap-1.5 @lg/card:border-t-0">
            <span className="text-xs text-muted-foreground">
              账号开关
              <span className={cn('ml-2 font-medium @lg/card:hidden', cred.disabled ? 'text-muted-foreground' : 'text-foreground')}>
                {cred.disabled ? '已关闭' : '已开启'}
              </span>
            </span>
            <span className="relative inline-flex items-center">
              <Switch
                variant="success"
                checked={!cred.disabled}
                onCheckedChange={(on) => toggle.mutate(!on)}
                disabled={toggle.isPending}
                title={switchTitle(cred)}
                aria-label={switchTitle(cred)}
                className={cn(
                  "relative after:absolute after:-inset-x-1 after:-inset-y-2.5 after:content-['']",
                  toggle.isPending && 'opacity-0',
                  abnormal && 'data-[state=checked]:bg-muted-foreground/50',
                )}
              />
              {toggle.isPending && (
                <ArrowPathIcon className="absolute left-1/2 top-1/2 size-4 -translate-x-1/2 -translate-y-1/2 animate-spin text-muted-foreground" />
              )}
            </span>
          </div>
        </div>
      )}

      <CredentialDevicesDialog
        cred={cred}
        open={devicesOpen}
        onOpenChange={setDevicesOpen}
        limit={limit}
      />
      <DeleteCredentialDialog
        cred={cred}
        actions={actions}
        open={confirmDelete}
        onOpenChange={setConfirmDelete}
      />
      <ConnectivityTestDialog cred={cred} open={testing} onOpenChange={setTesting} />
    </Card>
  )
}

/**
 * 单个额度窗口条：标签 + 百分比 + 进度条 + 重置时刻 + 本档已用金额。
 *
 * `util` 传的是 [`liveQuota`] 的结果而非原始快照：窗口过了重置时刻就置空，这里退化成
 * 一行说明。此时进度条上的旧百分比、以及按旧窗口起点累加出来的花费，都已经不成立了，
 * 与其画一根 95% 的红条再在旁边写「已重置」，不如干脆不画。
 */
function QuotaBar({
  label, util, reset, cost, snapshotTs, className,
}: {
  label: string
  util: number | null
  reset: number | null
  cost: number | null
  className?: string
  /** 快照对应的请求时间（Unix 秒），用于说明这份数据有多旧。 */
  snapshotTs: number
}) {
  if (util == null) {
    const reason = reset != null
      ? `窗口已在 ${formatFullTime(reset)} 重置，之后该账号没有新请求，因此没有新数据`
      : '上游未返回该窗口的额度信息'
    return (
      <div
        className={cn('flex h-full min-h-24 flex-col justify-center px-4 py-4 text-sm text-muted-foreground sm:px-5', className)}
        title={`${reason}。最后一次快照：${formatFullTime(snapshotTs)}`}
      >
        {label} · {reset != null ? '已重置，暂无新用量' : '暂无数据'}
      </div>
    )
  }
  const pct = Math.min(100, Math.max(0, Math.round(util * 100)))
  const critical = util >= 0.9
  const barColor = critical ? 'bg-bad' : util >= 0.7 ? 'bg-warn' : 'bg-ok'
  const pctColor = critical ? 'text-bad' : util >= 0.7 ? 'text-warn' : 'text-foreground'
  return (
    <div className={cn('flex h-full min-h-24 flex-col px-4 pb-4 pt-3 sm:px-5', className)}>
      <div className="flex items-baseline justify-between gap-2">
        <span className="truncate text-sm font-medium text-muted-foreground">{label}</span>
        {/* 百分比只在有请求经过时才刷新，标注快照时间，免得把很旧的数当成实时值。 */}
        <span
          className={cn('text-xl font-semibold tnum leading-none', pctColor)}
          title={`额度使用率 · 快照于 ${relativeTime(snapshotTs)}（${formatFullTime(snapshotTs)}）`}
        >
          {pct}
          <span className="ml-px text-xs font-medium text-muted-foreground">%</span>
        </span>
      </div>
      <div
        className="mt-3 h-2 overflow-hidden rounded-full bg-muted"
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
      <div className="mt-auto flex items-baseline justify-between gap-2 pt-2 text-xs text-muted-foreground">
        <span title="本周期内已消耗的等价 API 费用">
          花费 <span className="tnum font-medium text-foreground/80">{formatUsd(cost ?? 0)}</span>
        </span>
        {reset != null && (
          <span className="whitespace-nowrap tnum" title={`${formatFullTime(reset)} 重置`}>
            {formatClockTime(reset)} 重置
          </span>
        )}
      </div>
    </div>
  )
}
