import { type ReactNode, useState } from 'react'
import {
  ArrowPathIcon,
  CalendarDaysIcon,
  ChevronDownIcon,
  ChevronRightIcon,
  ChevronUpIcon,
  ClockIcon,
  DevicePhoneMobileIcon,
} from '@heroicons/react/24/outline'
import { type Credential } from '@/api/credentials'
import { CredentialDevicesDialog } from '@/components/credential-devices-dialog'
import {
  credentialExpiryMeta,
  expiryMeta,
  isNearLimit,
  liveQuota,
  statusMeta,
  switchTitle,
  tierBadgeClass,
  useCredentialActions,
  type CredentialActions,
  type SortDir,
  type SortKey,
} from '@/components/credential-shared'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Switch } from '@/components/ui/switch'
import { TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { cn, formatFullTime, formatUsd, relativeTime } from '@/lib/utils'

/**
 * 列表是只读信息表，唯一保留的行内写操作是调度开关。
 * 优先级与账号等级各占一列，避免再把“可编辑控件”伪装成普通信息。
 */
const COL = {
  select: 'w-10',
  account: 'w-[22%] text-left',
  schedule: 'w-[7%] text-center',
  priority: 'w-[7%] text-center',
  tier: 'w-[9%] text-left',
  quota: 'w-[25%] text-left',
  devices: 'w-[9%] text-left',
  recent: 'w-[12%] text-left',
  cost: 'w-[8%] text-right',
} as const

export function CredentialListHeader({
  selectable, sort, dir, onSortChange, allSelected, onSelectAll,
}: {
  selectable?: boolean
  sort: SortKey
  dir: SortDir
  onSortChange: (key: SortKey) => void
  allSelected?: boolean
  onSelectAll?: (next: boolean) => void
}) {
  const sortable = (label: string, key: SortKey, align: 'left' | 'center' | 'right' = 'left') => {
    const active = sort === key
    const Arrow = active && dir === 'asc' ? ChevronUpIcon : ChevronDownIcon

    return (
      <Button
        type="button"
        size="sm"
        variant="ghost"
        onClick={() => onSortChange(key)}
        className={cn(
          'h-10 w-full justify-start gap-1 rounded-none px-0 text-xs font-medium text-muted-foreground hover:bg-transparent hover:text-foreground',
          align === 'center' && 'justify-center',
          align === 'right' && 'justify-end',
          active && 'font-semibold text-foreground',
        )}
        title={active ? `按${label}排序（点击切换升降序）` : `按${label}排序`}
      >
        {label}
        <Arrow className={cn('size-3 shrink-0', active ? 'opacity-100' : 'opacity-0')} />
      </Button>
    )
  }
  const sortProps = (key: SortKey) =>
    sort === key ? ({ 'aria-sort': dir === 'asc' ? 'ascending' : 'descending' } as const) : {}

  return (
    <TableHeader className="hidden bg-muted/30 xl:table-header-group">
      <TableRow className="border-border/80 hover:bg-transparent">
        <TableHead className={cn(COL.select, selectable ? 'pl-4 pr-0' : 'p-0')}>
          {selectable && (
            <input
              type="checkbox"
              checked={!!allSelected}
              onChange={(event) => onSelectAll?.(event.target.checked)}
              className="size-4 rounded border-border accent-primary"
              aria-label="全选当前筛选结果"
            />
          )}
        </TableHead>
        <TableHead className={cn(COL.account, 'px-3')} {...sortProps('name')}>
          {sortable('账号', 'name')}
        </TableHead>
        <TableHead className={cn(COL.schedule, 'px-2 text-xs text-muted-foreground')}>调度</TableHead>
        <TableHead className={cn(COL.priority, 'px-2')} {...sortProps('priority')}>
          {sortable('优先级', 'priority', 'center')}
        </TableHead>
        <TableHead className={cn(COL.tier, 'px-2')} {...sortProps('tier')}>
          {sortable('账号等级', 'tier')}
        </TableHead>
        <TableHead className={cn(COL.quota, 'px-3')} {...sortProps('usage5h')}>
          {sortable('额度', 'usage5h')}
        </TableHead>
        <TableHead className={cn(COL.devices, 'px-2')} {...sortProps('devices')}>
          {sortable('设备', 'devices')}
        </TableHead>
        <TableHead className={cn(COL.recent, 'px-2')} {...sortProps('recent')}>
          {sortable('最近使用', 'recent')}
        </TableHead>
        <TableHead className={cn(COL.cost, 'px-3')} {...sortProps('cost')}>
          {sortable('累计花费', 'cost', 'right')}
        </TableHead>
      </TableRow>
    </TableHeader>
  )
}

export function CredentialRow({
  cred, selectable = false, selected = false, onSelectedChange,
}: {
  cred: Credential
  selectable?: boolean
  selected?: boolean
  onSelectedChange?: (next: boolean) => void
}) {
  const [devicesOpen, setDevicesOpen] = useState(false)
  const actions = useCredentialActions(cred)
  const nearLimit = isNearLimit(cred)
  const status = statusMeta(cred, nearLimit)
  const expiry = expiryMeta(cred)
  const credentialExpiry = credentialExpiryMeta(cred)
  const { u5h, u7d } = liveQuota(cred)
  const initial = cred.label.trim().charAt(0).toUpperCase() || '?'
  const deviceLimit = cred.device_limit_effective > 0 ? cred.device_limit_effective : '∞'
  const devicePolicy = cred.device_limit === 0 ? '默认' : cred.device_limit < 0 ? '不限' : '独立'
  const added = relativeTime(cred.created_at)

  return (
    <>
      {/* 小屏使用官方 stacked-list 的信息层级，不在表格行里塞编辑器。 */}
      <TableRow
        className={cn('hover:bg-muted/20 xl:hidden', cred.disabled && 'bg-muted/15')}
        data-state={selected ? 'selected' : undefined}
      >
        <TableCell colSpan={9} className="whitespace-normal p-0">
          <article className="px-4 py-4 sm:px-5">
            <div className="flex items-start gap-3">
              {selectable && (
                <input
                  type="checkbox"
                  checked={selected}
                  onChange={(event) => onSelectedChange?.(event.target.checked)}
                  className="mt-3 size-4 shrink-0 rounded border-border accent-primary"
                  aria-label={`选择 ${cred.label}`}
                />
              )}

              <AccountAvatar cred={cred} initial={initial} />

              <div className="min-w-0 flex-1">
                <div className="flex min-w-0 items-center gap-2">
                  <h3 className="truncate text-sm font-semibold text-foreground" title={cred.label}>
                    {cred.label}
                  </h3>
                  <span className="shrink-0 font-mono text-xs text-muted-foreground">#{cred.id}</span>
                </div>
                <div className="mt-1.5 text-xs text-muted-foreground">
                  <span className="inline-flex items-center gap-1" title={`添加于 ${formatFullTime(cred.created_at)}`}>
                    <CalendarDaysIcon className="size-3.5" />
                    添加于 {added}
                  </span>
                </div>
              </div>

              <div className="flex shrink-0 flex-col items-end gap-1.5">
                <StatusLabel status={status} expiryTitle={expiry.title} />
                <ScheduleSwitch cred={cred} actions={actions} />
              </div>
            </div>

            <div className="mt-4 grid grid-cols-2 gap-4 border-t border-border/70 pt-4">
              <ListQuotaMeter label="5h" util={u5h} reset={cred.quota?.rl_5h_reset ?? null} />
              <ListQuotaMeter label="7d" util={u7d} reset={cred.quota?.rl_7d_reset ?? null} />
            </div>

            <dl className="mt-4 grid grid-cols-2 overflow-hidden rounded-lg border border-border/70 bg-muted/10 sm:grid-cols-3">
              <MobileFact label="优先级" className="border-b border-r sm:border-b">
                <span className="font-mono">P{cred.priority}</span>
              </MobileFact>
              <MobileFact label="账号等级" className="border-b sm:border-r">
                {cred.tier ? (
                  <Badge variant="outline" className={cn('h-5 px-1.5 py-0 text-xs', tierBadgeClass(cred.tier))}>
                    {cred.tier}
                  </Badge>
                ) : '—'}
              </MobileFact>
              <MobileFact label="设备" className="border-b border-r sm:border-b sm:border-r-0">
                <button
                  type="button"
                  onClick={() => setDevicesOpen(true)}
                  className="inline-flex items-center gap-1 font-medium text-foreground outline-none hover:text-primary focus-visible:ring-2 focus-visible:ring-ring"
                  aria-label={`查看 ${cred.label} 的已绑定设备`}
                >
                  <span className="tnum">{cred.device_count}/{deviceLimit}</span>
                  <span className="font-normal text-muted-foreground">{devicePolicy}</span>
                  <ChevronRightIcon className="size-3.5" />
                </button>
              </MobileFact>
              <MobileFact label="最近使用" className="border-b sm:border-b-0 sm:border-r">
                {cred.last_used != null ? relativeTime(cred.last_used) : '未使用'}
              </MobileFact>
              <MobileFact label="累计花费" className="border-r">
                <span className="tnum">{formatUsd(cred.cost_total)}</span>
              </MobileFact>
              <MobileFact label="凭证有效期" title={credentialExpiry.title}>
                <span className={cn('tnum', credentialExpiry.className)}>{credentialExpiry.text}</span>
              </MobileFact>
            </dl>
          </article>
        </TableCell>
      </TableRow>

      {/* 桌面端是只读数据表：名称、优先级、等级和设备策略都只展示。 */}
      <TableRow
        className={cn('group/row hidden hover:bg-muted/20 xl:table-row', cred.disabled && 'bg-muted/10')}
        data-state={selected ? 'selected' : undefined}
      >
        <TableCell className={cn(COL.select, selectable ? 'pl-4 pr-0' : 'p-0')}>
          {selectable && (
            <input
              type="checkbox"
              checked={selected}
              onChange={(event) => onSelectedChange?.(event.target.checked)}
              className="size-4 rounded border-border accent-primary"
              aria-label={`选择 ${cred.label}`}
            />
          )}
        </TableCell>

        <TableCell className={cn(COL.account, 'whitespace-normal px-3 py-3.5')}>
          <div className="flex min-w-0 items-center gap-3">
            <AccountAvatar cred={cred} initial={initial} />
            <div className="min-w-0 flex-1">
              <div className="flex min-w-0 items-center gap-2">
                <span className="truncate text-sm font-semibold text-foreground" title={cred.label}>{cred.label}</span>
                <span className="shrink-0 font-mono text-xs text-muted-foreground">#{cred.id}</span>
              </div>
              <div className="mt-1 flex min-w-0 items-center text-xs text-muted-foreground">
                <span className="truncate" title={`添加于 ${formatFullTime(cred.created_at)}`}>添加于 {added}</span>
              </div>
            </div>
          </div>
        </TableCell>

        <TableCell className={cn(COL.schedule, 'px-2 py-3.5 text-center')}>
          <div className="flex flex-col items-center gap-1.5">
            <ScheduleSwitch cred={cred} actions={actions} />
            <StatusLabel status={status} expiryTitle={expiry.title} />
          </div>
        </TableCell>

        <TableCell className={cn(COL.priority, 'px-2 py-3.5 text-center')}>
          <span className="font-mono text-sm font-semibold text-foreground" title="数值越小，调度优先级越高">
            P{cred.priority}
          </span>
        </TableCell>

        <TableCell className={cn(COL.tier, 'px-2 py-3.5')}>
          {cred.tier ? (
            <Badge
              variant="outline"
              className={cn('h-6 max-w-full truncate px-2 py-0 text-xs font-medium', tierBadgeClass(cred.tier))}
              title={cred.tier}
            >
              {cred.tier}
            </Badge>
          ) : (
            <span className="text-muted-foreground">—</span>
          )}
        </TableCell>

        <TableCell className={cn(COL.quota, 'px-3 py-3.5')}>
          <div className="grid grid-cols-2 gap-4">
            <ListQuotaMeter label="5h" util={u5h} reset={cred.quota?.rl_5h_reset ?? null} />
            <ListQuotaMeter label="7d" util={u7d} reset={cred.quota?.rl_7d_reset ?? null} />
          </div>
        </TableCell>

        <TableCell className={cn(COL.devices, 'px-2 py-3.5')}>
          <Button
            type="button"
            size="sm"
            variant="ghost"
            className="h-auto min-w-0 justify-start gap-2 rounded-md px-1.5 py-1 text-left hover:bg-muted"
            onClick={() => setDevicesOpen(true)}
            title={`查看已绑定设备 · ${devicePolicy}策略`}
          >
            <DevicePhoneMobileIcon className="size-4 shrink-0 text-muted-foreground" />
            <span className="min-w-0">
              <span className="block tnum text-xs font-semibold text-foreground">{cred.device_count}/{deviceLimit}</span>
              <span className="block text-[0.6875rem] font-normal text-muted-foreground">{devicePolicy}</span>
            </span>
            <ChevronRightIcon className="ml-auto size-3.5 shrink-0 text-muted-foreground" />
          </Button>
          <CredentialDevicesDialog
            cred={cred}
            open={devicesOpen}
            onOpenChange={setDevicesOpen}
            limit={actions.limit}
          />
        </TableCell>

        <TableCell className={cn(COL.recent, 'px-2 py-3.5')}>
          <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
            <ClockIcon className="size-4 shrink-0" />
            <span className="truncate text-foreground/85" title="最近一次转发使用">
              {cred.last_used != null ? relativeTime(cred.last_used) : '未使用'}
            </span>
          </div>
        </TableCell>

        <TableCell className={cn(COL.cost, 'px-3 py-3.5 text-right')}>
          <span className="tnum text-sm font-medium text-foreground" title="累计等价 API 费用">
            {formatUsd(cred.cost_total)}
          </span>
        </TableCell>
      </TableRow>
    </>
  )
}

function AccountAvatar({
  cred, initial,
}: {
  cred: Credential
  initial: string
}) {
  return (
    <div className="relative shrink-0" aria-hidden>
      <div
        className={cn(
          'grid size-10 place-items-center rounded-full bg-muted text-sm font-semibold ring-1 ring-inset ring-border/80',
          cred.disabled ? 'text-muted-foreground/70' : 'text-foreground',
        )}
      >
        {initial}
      </div>
    </div>
  )
}

function StatusLabel({
  status, expiryTitle,
}: {
  status: ReturnType<typeof statusMeta>
  expiryTitle?: string
}) {
  return (
    <span className="inline-flex shrink-0 items-center gap-1.5 text-xs text-muted-foreground" title={expiryTitle ? `${status.label} · ${expiryTitle}` : status.label}>
      <span className={cn('size-1.5 rounded-full', status.dot)} aria-hidden />
      <span>{status.label}</span>
    </span>
  )
}

function ScheduleSwitch({ cred, actions }: { cred: Credential; actions: CredentialActions }) {
  const { toggle } = actions

  return (
    <span className="relative inline-flex shrink-0 items-center justify-center">
      <Switch
        variant="success"
        checked={!cred.disabled}
        onCheckedChange={(enabled) => toggle.mutate(!enabled)}
        disabled={toggle.isPending}
        title={switchTitle(cred)}
        aria-label={`${cred.label}：${switchTitle(cred)}`}
        className={cn(toggle.isPending && 'opacity-0')}
      />
      {toggle.isPending && (
        <ArrowPathIcon className="absolute left-1/2 top-1/2 size-4 -translate-x-1/2 -translate-y-1/2 animate-spin text-muted-foreground" />
      )}
    </span>
  )
}

function MobileFact({
  label, children, className, title,
}: {
  label: string
  children: ReactNode
  className?: string
  title?: string
}) {
  return (
    <div className={cn('min-w-0 px-3 py-3', className)} title={title}>
      <dt className="text-[0.6875rem] font-medium text-muted-foreground">{label}</dt>
      <dd className="mt-1 min-w-0 truncate text-xs font-medium text-foreground">{children}</dd>
    </div>
  )
}

function ListQuotaMeter({
  label, util, reset,
}: {
  label: string
  util: number | null
  reset: number | null
}) {
  const emptyText = reset != null ? '已重置' : '未知'
  const emptyTitle = reset != null
    ? `${label}窗口已重置，之后暂无新请求`
    : `${label}额度暂无数据`

  if (util == null) {
    return (
      <div className="min-w-0" title={emptyTitle}>
        <div className="mb-1.5 flex items-center justify-between gap-2 text-xs">
          <span className="font-medium text-foreground/75">{label}</span>
          <span className="text-muted-foreground">{emptyText}</span>
        </div>
        <span className="block h-1.5 rounded-full bg-border/70" aria-hidden />
      </div>
    )
  }

  const percent = Math.min(100, Math.max(0, Math.round(util * 100)))
  const color = util >= 0.9 ? 'bg-bad' : util >= 0.7 ? 'bg-warn' : 'bg-ok'

  return (
    <div className="min-w-0" title={`${label}额度使用率 ${percent}%`}>
      <div className="mb-1.5 flex items-center justify-between gap-2 text-xs">
        <span className="font-medium text-foreground/75">{label}</span>
        <span className="tnum text-muted-foreground">{percent}%</span>
      </div>
      <div
        className="h-1.5 overflow-hidden rounded-full bg-border/80"
        role="progressbar"
        aria-label={`${label}额度`}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={percent}
      >
        <div className={cn('h-full rounded-full', color)} style={{ width: `${percent}%` }} />
      </div>
    </div>
  )
}
