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
import { cn, formatFullTime, formatUsd, relativeTime } from '@/lib/utils'

const COL = {
  select: 'w-3',
  account: 'w-[22%] text-left',
  schedule: 'w-[8%] text-center',
  priority: 'w-[7%] text-center',
  tier: 'w-[9%] text-left',
  quota: 'w-[25%] text-left',
  devices: 'w-[9%] text-left',
  recent: 'w-[12%] text-left',
  cost: 'w-[8%] text-right',
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
  const sortable = (label: string, key: SortKey, align: 'left' | 'center' | 'right' = 'left') => {
    const active = sort === key
    const Arrow = active && dir === 'asc' ? ChevronUpIcon : ChevronDownIcon

    return (
      <Button
        type="button"
        size="xs"
        variant="ghost"
        onClick={() => onSortChange(key)}
        className={cn(
          'w-full justify-start',
          align === 'center' && 'justify-center',
          align === 'right' && 'justify-end',
        )}
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
        <TableHead className={cn(COL.account, 'px-3')} {...sortProps('name')}>
          {sortable('账号', 'name')}
        </TableHead>
        <TableHead className={cn(COL.schedule, 'px-2')}>调度</TableHead>
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
        <TableCell colSpan={9} className="whitespace-normal p-0">
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
                  <h3 className="truncate font-semibold text-sm" title={cred.label}>{cred.label}</h3>
                  <span className="shrink-0 font-mono text-xs text-muted-foreground">#{cred.id}</span>
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
              <ListQuotaMeter label="5h" util={u5h} reset={cred.quota?.rl_5h_reset ?? null} />
              <ListQuotaMeter label="7d" util={u7d} reset={cred.quota?.rl_7d_reset ?? null} />
            </div>

            <dl className="grid grid-cols-2 gap-4 border-t pt-4 sm:grid-cols-3">
              <MobileFact label="优先级"><span className="font-mono">P{cred.priority}</span></MobileFact>
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
        <TableCell className={cn(COL.account, 'whitespace-normal px-3 py-3')}>
          <div className="flex min-w-0 items-center">
            <div className="min-w-0 flex-1">
              <div className="flex min-w-0 items-center gap-2">
                <span className="truncate font-semibold text-sm" title={cred.label}>{cred.label}</span>
                <span className="shrink-0 font-mono text-xs text-muted-foreground">#{cred.id}</span>
              </div>
              <span className="text-xs text-muted-foreground" title={`添加于 ${formatFullTime(cred.created_at)}`}>
                添加于 {added}
              </span>
            </div>
          </div>
        </TableCell>
        <TableCell className={cn(COL.schedule, 'px-2 py-3 text-center')}>
          <ScheduleControl cred={cred} actions={actions} />
        </TableCell>
        <TableCell className={cn(COL.priority, 'px-2 py-3 text-center')}>
          <span className="font-mono font-semibold text-sm" title="数值越小，调度优先级越高">
            P{cred.priority}
          </span>
        </TableCell>
        <TableCell className={cn(COL.tier, 'px-2 py-3')}>
          {cred.tier
            ? <Badge variant={tierBadgeVariant(cred.tier)}>{cred.tier}</Badge>
            : <span className="text-muted-foreground">—</span>}
        </TableCell>
        <TableCell className={cn(COL.quota, 'px-3 py-3')}>
          <div className="grid grid-cols-2 gap-4">
            <ListQuotaMeter label="5h" util={u5h} reset={cred.quota?.rl_5h_reset ?? null} />
            <ListQuotaMeter label="7d" util={u7d} reset={cred.quota?.rl_7d_reset ?? null} />
          </div>
        </TableCell>
        <TableCell className={cn(COL.devices, 'px-2 py-3')}>
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
        <TableCell className={cn(COL.recent, 'px-2 py-3 text-sm')}>
          {cred.last_used != null ? relativeTime(cred.last_used) : '未使用'}
        </TableCell>
        <TableCell className={cn(COL.cost, 'px-3 py-3 text-right')}>
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
  return (
    <div className="flex shrink-0 items-center justify-center gap-2">
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
}: {
  label: string
  util: number | null
  reset: number | null
}) {
  if (util == null) {
    return (
      <div title={reset != null ? `${label}窗口已重置，之后暂无新请求` : `${label}额度暂无数据`}>
        <p className="font-medium text-sm">{label}</p>
        <p className="text-xs text-muted-foreground">{reset != null ? '已重置' : '暂无数据'}</p>
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
        <MeterLabel>{label}</MeterLabel>
        <MeterValue>{() => `${percentage}%`}</MeterValue>
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
