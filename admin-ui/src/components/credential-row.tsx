import { type ReactNode, useState } from 'react'
import {
  CalendarDaysIcon,
  ChevronDownIcon,
  ChevronUpIcon,
} from 'lucide-react'
import { type Credential } from '@/api/credentials'
import { CredentialDevicesDialog } from '@/components/credential-devices-dialog'
import {
  liveQuota,
  isNearLimit,
  statusMeta,
  switchTitle,
  tierBadgeVariant,
  useCredentialActions,
  type CredentialActions,
  type SortDir,
  type SortKey,
} from '@/components/credential-shared'
import { Badge, type BadgeProps } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'
import {
  Meter,
  MeterIndicator,
  MeterLabel,
  MeterTrack,
  MeterValue,
} from '@/components/ui/meter'
import { Spinner } from '@/components/ui/spinner'
import { Switch } from '@/components/ui/switch'
import { TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { Tooltip, TooltipPopup, TooltipTrigger } from '@/components/ui/tooltip'
import { cn, formatFullTime, formatUsd, relativeTime } from '@/lib/utils'

const COL = {
  select: 'w-3',
  account: 'w-[22%]',
  schedule: 'w-[10%]',
  priority: 'w-[7%]',
  tier: 'w-[9%]',
  quota5h: 'w-[12%]',
  quota7d: 'w-[11%]',
  devices: 'w-[9%]',
  recent: 'w-[12%]',
  cost: 'w-[8%]',
} as const

export function CredentialListHeader({
  selectable,
  sort,
  dir,
  onSortChange,
  allSelected,
  onSelectAll,
}: {
  selectable?: boolean
  sort: SortKey
  dir: SortDir
  onSortChange: (key: SortKey) => void
  allSelected?: boolean
  onSelectAll?: (next: boolean) => void
}) {
  const sortable = (label: string, key: SortKey) => {
    const active = sort === key
    const Arrow = active && dir === 'asc' ? ChevronUpIcon : ChevronDownIcon

    return (
      <Button
        type="button"
        size="xs"
        variant="ghost"
        onClick={() => onSortChange(key)}
        className="w-full justify-start px-0 text-left sm:text-sm"
        title={active ? `按${label}排序（点击切换升降序）` : `按${label}排序`}
      >
        {label}
        <Arrow className={cn(!active && 'opacity-0')} />
      </Button>
    )
  }
  const sortProps = (key: SortKey) =>
    sort === key ? ({ 'aria-sort': dir === 'asc' ? 'ascending' : 'descending' } as const) : {}

  return (
    <TableHeader className="hidden xl:table-header-group">
      <TableRow>
        <TableHead className={cn(COL.select, selectable ? 'w-10 pl-4 pr-0' : 'p-0')}>
          {selectable && (
            <Checkbox
              checked={!!allSelected}
              onCheckedChange={(checked) => onSelectAll?.(checked)}
              aria-label="全选当前筛选结果"
            />
          )}
        </TableHead>
        <TableHead className={COL.account} {...sortProps('name')}>
          {sortable('账号', 'name')}
        </TableHead>
        <TableHead className={COL.schedule}>调度</TableHead>
        <TableHead className={COL.priority} {...sortProps('priority')}>
          {sortable('优先级', 'priority')}
        </TableHead>
        <TableHead className={COL.tier} {...sortProps('tier')}>
          {sortable('账号等级', 'tier')}
        </TableHead>
        <TableHead className={COL.quota5h} {...sortProps('usage5h')}>
          {sortable('5h 额度', 'usage5h')}
        </TableHead>
        <TableHead className={COL.quota7d} {...sortProps('usage7d')}>
          {sortable('7d 额度', 'usage7d')}
        </TableHead>
        <TableHead className={COL.devices} {...sortProps('devices')}>
          {sortable('设备', 'devices')}
        </TableHead>
        <TableHead className={COL.recent} {...sortProps('recent')}>
          {sortable('最近使用', 'recent')}
        </TableHead>
        <TableHead className={COL.cost} {...sortProps('cost')}>
          {sortable('累计花费', 'cost')}
        </TableHead>
      </TableRow>
    </TableHeader>
  )
}

export function CredentialRow({
  cred,
  selectable = false,
  selected = false,
  onSelectedChange,
}: {
  cred: Credential
  selectable?: boolean
  selected?: boolean
  onSelectedChange?: (next: boolean) => void
}) {
  const [devicesOpen, setDevicesOpen] = useState(false)
  const actions = useCredentialActions(cred)
  const { u5h, u7d } = liveQuota(cred)
  const effectiveLimit = cred.device_limit_effective > 0 ? cred.device_limit_effective : '∞'
  const policy = devicePolicyMeta(cred.device_limit)
  const added = relativeTime(cred.created_at)

  return (
    <>
      <TableRow className="xl:hidden" data-state={selected ? 'selected' : undefined}>
        <TableCell colSpan={10} className="whitespace-normal p-0">
          <article className="space-y-4 p-4 sm:p-5">
            <div className="flex items-start gap-3">
              {selectable && (
                <Checkbox
                  checked={selected}
                  onCheckedChange={(checked) => onSelectedChange?.(checked)}
                  className="mt-2"
                  aria-label={`选择 ${cred.label}`}
                />
              )}
              <div className="min-w-0 flex-1">
                <div className="flex min-w-0 items-center gap-2">
                  <h3 className="min-w-0 break-all font-semibold text-sm leading-snug" title={cred.label}>{cred.label}</h3>
                  <span className="shrink-0 text-xs text-muted-foreground tabular-nums">#{cred.id}</span>
                </div>
                <p
                  className="mt-1 inline-flex items-center gap-1 text-xs text-muted-foreground"
                  title={`添加于 ${formatFullTime(cred.created_at)}`}
                >
                  <CalendarDaysIcon />
                  添加于 {added}
                </p>
              </div>
              <ScheduleControl cred={cred} actions={actions} />
            </div>

            <div className="grid grid-cols-2 gap-4 border-t pt-4">
              <ListQuotaMeter
                label="5h"
                util={u5h}
                reset={cred.quota?.rl_5h_reset ?? null}
                cost={cred.quota?.cost_5h ?? null}
              />
              <ListQuotaMeter
                label="7d"
                util={u7d}
                reset={cred.quota?.rl_7d_reset ?? null}
                cost={cred.quota?.cost_7d ?? null}
              />
            </div>

            <dl className="grid grid-cols-2 gap-4 border-t pt-4 sm:grid-cols-3">
              <MobileFact label="优先级"><span className="tabular-nums">P{cred.priority}</span></MobileFact>
              <MobileFact label="账号等级">
                {cred.tier
                  ? <Badge variant={tierBadgeVariant(cred.tier)} size="sm">{cred.tier}</Badge>
                  : '—'}
              </MobileFact>
              <MobileFact label="设备">
                <Button
                  type="button"
                  size="xs"
                  variant="ghost"
                  onClick={() => setDevicesOpen(true)}
                  title={`查看已绑定设备 · ${policy.label}策略`}
                  aria-label={`查看 ${cred.label} 的已绑定设备`}
                >
                  <span className="tabular-nums">{cred.device_count}/{effectiveLimit} · {policy.label}</span>
                </Button>
              </MobileFact>
            </dl>
          </article>
        </TableCell>
      </TableRow>

      <TableRow className="hidden xl:table-row" data-state={selected ? 'selected' : undefined}>
        <TableCell className={cn(COL.select, selectable ? 'w-10 pl-4 pr-0' : 'p-0')}>
          {selectable && (
            <Checkbox
              checked={selected}
              onCheckedChange={(checked) => onSelectedChange?.(checked)}
              aria-label={`选择 ${cred.label}`}
            />
          )}
        </TableCell>
        <TableCell className={cn(COL.account, 'whitespace-normal')}>
          <div className="flex min-w-0 items-center">
            <div className="min-w-0 flex-1">
              <div className="flex min-w-0 items-baseline gap-2">
                <span className="min-w-0 break-all font-semibold text-sm leading-snug" title={cred.label}>
                  {cred.label}
                </span>
                <span className="shrink-0 text-xs text-muted-foreground tabular-nums">#{cred.id}</span>
              </div>
              <span className="text-xs text-muted-foreground" title={`添加于 ${formatFullTime(cred.created_at)}`}>
                添加于 {added}
              </span>
            </div>
          </div>
        </TableCell>
        <TableCell className={COL.schedule}>
          <ScheduleControl cred={cred} actions={actions} />
        </TableCell>
        <TableCell className={COL.priority}>
          <span className="font-semibold text-sm tabular-nums" title="数值越小，调度优先级越高">
            P{cred.priority}
          </span>
        </TableCell>
        <TableCell className={COL.tier}>
          {cred.tier
            ? <Badge variant={tierBadgeVariant(cred.tier)}>{cred.tier}</Badge>
            : <span className="text-muted-foreground">—</span>}
        </TableCell>
        <TableCell className={COL.quota5h}>
          <ListQuotaMeter
            label="5h"
            util={u5h}
            reset={cred.quota?.rl_5h_reset ?? null}
            cost={cred.quota?.cost_5h ?? null}
            showLabel={false}
          />
        </TableCell>
        <TableCell className={COL.quota7d}>
          <ListQuotaMeter
            label="7d"
            util={u7d}
            reset={cred.quota?.rl_7d_reset ?? null}
            cost={cred.quota?.cost_7d ?? null}
            showLabel={false}
          />
        </TableCell>
        <TableCell className={COL.devices}>
          <Button
            type="button"
            size="xs"
            variant="ghost"
            onClick={() => setDevicesOpen(true)}
            title={`查看已绑定设备 · ${policy.label}策略`}
            aria-haspopup="dialog"
          >
            <span className="tabular-nums">{cred.device_count}/{effectiveLimit}</span>
            <Badge variant={policy.variant} size="sm">{policy.label}</Badge>
          </Button>
        </TableCell>
        <TableCell className={COL.recent}>
          {cred.last_used != null ? relativeTime(cred.last_used) : '未使用'}
        </TableCell>
        <TableCell className={COL.cost}>
          <span className="tabular-nums font-medium text-sm" title="累计等价 API 费用">
            {formatUsd(cred.cost_total)}
          </span>
        </TableCell>
      </TableRow>

      <CredentialDevicesDialog
        cred={cred}
        open={devicesOpen}
        onOpenChange={setDevicesOpen}
        limit={actions.limit}
      />
    </>
  )
}

function ScheduleControl({
  cred,
  actions,
}: {
  cred: Credential
  actions: CredentialActions
}) {
  const { toggle } = actions
  const nearLimit = isNearLimit(cred)
  const status = statusMeta(cred, nearLimit)
  const statusDetail = cred.ban_reason
    || (cred.disabled
      ? '账号已停用，不参与调度'
      : cred.rate_limited_secs > 0
        ? `账号约 ${Math.max(1, Math.ceil(cred.rate_limited_secs / 60))} 分钟后恢复调度`
        : nearLimit
          ? '5 小时或 7 天额度使用率已达到 90%'
          : '账号运行正常，可参与调度')

  return (
    <div className="flex shrink-0 flex-col items-end gap-3 xl:flex-row xl:items-center xl:gap-2">
      <div className="flex items-center gap-2">
        {toggle.isPending && <Spinner />}
        <Switch
          checked={!cred.disabled}
          onCheckedChange={(enabled) => toggle.mutate(!enabled)}
          disabled={toggle.isPending}
          title={switchTitle(cred)}
          aria-label={`${cred.label}：${switchTitle(cred)}`}
        />
      </div>
      <Tooltip>
        <TooltipTrigger
          render={
            <Badge
              render={<button type="button" />}
              size="sm"
              variant={status.variant}
              aria-label={`${status.label}：${statusDetail}`}
              aria-live="polite"
            />
          }
        >
          {status.label}
        </TooltipTrigger>
        <TooltipPopup className="max-w-72 break-words">{statusDetail}</TooltipPopup>
      </Tooltip>
    </div>
  )
}

function MobileFact({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="min-w-0">
      <dt className="text-xs text-muted-foreground">{label}</dt>
      <dd className="mt-1 min-w-0 truncate font-medium text-sm">{children}</dd>
    </div>
  )
}

function ListQuotaMeter({
  label,
  util,
  reset,
  cost,
  showLabel = true,
}: {
  label: string
  util: number | null
  reset: number | null
  cost: number | null
  showLabel?: boolean
}) {
  if (util == null) {
    const emptyLabel = reset != null ? '已重置' : '暂无数据'
    return (
      <div
        className="flex w-full flex-col gap-2"
        title={reset != null ? `${label}窗口已重置，之后暂无新请求` : `${label}额度暂无数据`}
      >
        <div className="flex items-center justify-between gap-2">
          <div className="flex min-w-0 items-baseline gap-1.5">
            <span className={cn('font-medium text-sm', !showLabel && 'sr-only')}>{label}</span>
            <span
              className={cn(
                'min-w-0 truncate tabular-nums',
                showLabel
                  ? 'text-xs text-muted-foreground'
                  : 'font-medium text-foreground text-sm leading-none',
              )}
              title={`${label}本周期花费 ${cost == null ? '暂无数据' : formatUsd(cost)}`}
            >
              {cost == null ? '—' : formatUsd(cost)}
            </span>
          </div>
          <span className="shrink-0 text-xs text-muted-foreground">{emptyLabel}</span>
        </div>
        <div className="h-2 w-full bg-input" aria-hidden />
      </div>
    )
  }

  const percentage = Math.min(100, Math.max(0, Math.round(util * 100)))
  const indicatorClass = util >= 0.9
    ? 'bg-destructive'
    : util >= 0.7
      ? 'bg-warning'
      : 'bg-success'

  return (
    <Meter value={percentage} max={100} title={`${label}额度使用率 ${percentage}%`}>
      <div className="flex items-center justify-between gap-2">
        <div className="flex min-w-0 items-baseline gap-1.5">
          <MeterLabel className={cn(!showLabel && 'sr-only')}>{label}</MeterLabel>
          <span
            className={cn(
              'min-w-0 truncate tabular-nums',
              showLabel
                ? 'text-xs text-muted-foreground'
                : 'font-medium text-foreground text-sm leading-none',
            )}
            title={`${label}本周期花费 ${cost == null ? '暂无数据' : formatUsd(cost)}`}
          >
            {cost == null ? '—' : formatUsd(cost)}
          </span>
        </div>
        <MeterValue className="font-medium leading-none">{() => `${percentage}%`}</MeterValue>
      </div>
      <MeterTrack>
        <MeterIndicator className={indicatorClass} />
      </MeterTrack>
    </Meter>
  )
}

function devicePolicyMeta(deviceLimit: number): { label: string; variant: BadgeProps['variant'] } {
  if (deviceLimit === 0) return { label: '跟随默认', variant: 'secondary' }
  if (deviceLimit < 0) return { label: '不限', variant: 'outline' }
  return { label: '自定义', variant: 'info' }
}
