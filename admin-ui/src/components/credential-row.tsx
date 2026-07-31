import { type ReactNode, useState } from 'react'
import {
  CalendarDaysIcon,
  ChevronDownIcon,
  ChevronUpIcon,
  EllipsisIcon,
} from 'lucide-react'
import { type Credential } from '@/api/credentials'
import { CredentialDevicesDialog } from '@/components/credential-devices-dialog'
import {
  ConnectivityTestDialog,
  CredentialMenuContent,
  DeleteCredentialDialog,
  evaluateCredential,
  quotaLevel,
  quotaPercentage,
  switchTitle,
  tierBadgeVariant,
  useCredentialActions,
  type CredentialActions,
  type CredentialStatusMeta,
  type QuotaFreshness,
  type SortDir,
  type SortKey,
} from '@/components/credential-shared'
import { Badge, badgeVariants, type BadgeProps } from '@/components/ui/badge'
import { Button, buttonVariants } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'
import {
  Dialog,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogPanel,
  DialogPopup,
  DialogTitle,
} from '@/components/ui/dialog'
import { Field, FieldLabel } from '@/components/ui/field'
import { Form } from '@/components/ui/form'
import { Input } from '@/components/ui/input'
import {
  Meter,
  MeterIndicator,
  MeterLabel,
  MeterTrack,
  MeterValue,
} from '@/components/ui/meter'
import { Spinner } from '@/components/ui/spinner'
import { Switch } from '@/components/ui/switch'
import { Menu, MenuTrigger } from '@/components/ui/menu'
import { TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { Tooltip, TooltipPopup, TooltipTrigger } from '@/components/ui/tooltip'
import { cn, formatFullTime, formatUsd, relativeTime } from '@/lib/utils'

const COL = {
  select: 'w-10',
  account: 'w-auto',
  schedule: 'w-32',
  priority: 'w-20',
  tier: 'w-24',
  quota5h: 'w-32',
  quota7d: 'w-32',
  devices: 'w-32',
  recent: 'w-24',
  cost: 'w-24',
  action: 'w-10',
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
        <TableHead className={cn(COL.select, selectable ? 'pl-4 pr-0' : 'p-0')}>
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
        <TableHead className={COL.action}>
          <span className="sr-only">操作</span>
        </TableHead>
      </TableRow>
    </TableHeader>
  )
}

export function CredentialRow({
  cred,
  now,
  selectable = false,
  selected = false,
  onSelectedChange,
}: {
  cred: Credential
  now: number
  selectable?: boolean
  selected?: boolean
  onSelectedChange?: (next: boolean) => void
}) {
  const [devicesOpen, setDevicesOpen] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [renameOpen, setRenameOpen] = useState(false)
  const [renameName, setRenameName] = useState(cred.label)
  const [testing, setTesting] = useState(false)
  const actions = useCredentialActions(cred)
  const evaluation = evaluateCredential(cred, now)
  const { quota } = evaluation
  const u5h = quota.h5.utilization
  const u7d = quota.d7.utilization
  const effectiveLimit = cred.device_limit_effective > 0 ? cred.device_limit_effective : '∞'
  const policy = devicePolicyMeta(cred.device_limit)
  const added = relativeTime(cred.created_at, now)

  return (
    <>
      <TableRow className="xl:hidden" data-state={selected ? 'selected' : undefined}>
        <TableCell colSpan={11} className="whitespace-normal p-0">
          <article className="space-y-3 p-3 sm:space-y-4 sm:p-5">
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
                <h3 className="min-w-0 truncate font-semibold text-sm leading-snug" title={cred.label}>
                  {cred.label}
                </h3>
                <p
                  className="mt-1 flex min-w-0 flex-wrap items-center gap-x-1 gap-y-0.5 text-xs text-muted-foreground"
                  title={`添加于 ${formatFullTime(cred.created_at)}`}
                >
                  <CalendarDaysIcon />
                  <span className="min-w-0 break-all tabular-nums">#{cred.id}</span>
                  <span aria-hidden="true">·</span>
                  <span className="min-w-0">添加于 {added}</span>
                </p>
              </div>
              <div className="flex shrink-0 items-start gap-1">
                <ScheduleControl cred={cred} actions={actions} status={evaluation.status} />
                <CredentialRowActionsMenu
                  cred={cred}
                  actions={actions}
                  onRename={() => {
                    setRenameName(cred.label)
                    setRenameOpen(true)
                  }}
                  onDeviceLimit={() => setDevicesOpen(true)}
                  onTest={() => setTesting(true)}
                  onRequestDelete={() => setConfirmDelete(true)}
                />
              </div>
            </div>

            <div className="grid grid-cols-2 gap-3 border-t pt-3 sm:gap-4 sm:pt-4">
              <ListQuotaMeter
                label="5h"
                util={u5h}
                freshness={quota.h5.freshness}
                reset={cred.quota?.rl_5h_reset ?? null}
                cost={cred.quota?.cost_5h ?? null}
                requests={cred.quota?.requests_5h ?? null}
              />
              <ListQuotaMeter
                label="7d"
                util={u7d}
                freshness={quota.d7.freshness}
                reset={cred.quota?.rl_7d_reset ?? null}
                cost={cred.quota?.cost_7d ?? null}
                requests={cred.quota?.requests_7d ?? null}
              />
            </div>

            <dl className="grid grid-cols-2 gap-3 border-t pt-3 sm:grid-cols-3 sm:gap-4 sm:pt-4">
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
        <TableCell className={cn(COL.select, selectable ? 'pl-4 pr-0' : 'p-0')}>
          {selectable && (
            <Checkbox
              checked={selected}
              onCheckedChange={(checked) => onSelectedChange?.(checked)}
              aria-label={`选择 ${cred.label}`}
            />
          )}
        </TableCell>
        <TableCell className={cn(COL.account, 'whitespace-nowrap')}>
          <div className="flex min-w-0 items-center">
            <div className="min-w-0 flex-1">
              <span className="block min-w-0 whitespace-nowrap font-semibold text-sm leading-snug" title={cred.label}>
                {cred.label}
              </span>
              <span className="mt-1 flex min-w-0 flex-wrap items-center gap-x-1 gap-y-0.5 text-xs text-muted-foreground">
                <span className="min-w-0 break-all tabular-nums">#{cred.id}</span>
                <span aria-hidden="true">·</span>
                <span className="min-w-0" title={`添加于 ${formatFullTime(cred.created_at)}`}>
                  添加于 {added}
                </span>
              </span>
            </div>
          </div>
        </TableCell>
        <TableCell className={COL.schedule}>
          <ScheduleControl cred={cred} actions={actions} status={evaluation.status} />
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
            freshness={quota.h5.freshness}
            reset={cred.quota?.rl_5h_reset ?? null}
            cost={cred.quota?.cost_5h ?? null}
            requests={cred.quota?.requests_5h ?? null}
            showLabel={false}
          />
        </TableCell>
        <TableCell className={COL.quota7d}>
          <ListQuotaMeter
            label="7d"
            util={u7d}
            freshness={quota.d7.freshness}
            reset={cred.quota?.rl_7d_reset ?? null}
            cost={cred.quota?.cost_7d ?? null}
            requests={cred.quota?.requests_7d ?? null}
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
          {cred.last_used != null ? relativeTime(cred.last_used, now) : '未使用'}
        </TableCell>
        <TableCell className={COL.cost}>
          <span className="tabular-nums font-medium text-sm" title="累计等价 API 费用">
            {formatUsd(cred.cost_total)}
          </span>
        </TableCell>
        <TableCell className={cn(COL.action, 'text-right')}>
          <CredentialRowActionsMenu
            cred={cred}
            actions={actions}
            onRename={() => {
              setRenameName(cred.label)
              setRenameOpen(true)
            }}
            onDeviceLimit={() => setDevicesOpen(true)}
            onTest={() => setTesting(true)}
            onRequestDelete={() => setConfirmDelete(true)}
          />
        </TableCell>
      </TableRow>

      <CredentialDevicesDialog
        cred={cred}
        open={devicesOpen}
        onOpenChange={setDevicesOpen}
        limit={actions.limit}
      />
      <DeleteCredentialDialog
        cred={cred}
        actions={actions}
        open={confirmDelete}
        onOpenChange={setConfirmDelete}
      />
      <RenameCredentialDialog
        cred={cred}
        actions={actions}
        name={renameName}
        onNameChange={setRenameName}
        open={renameOpen}
        onOpenChange={setRenameOpen}
      />
      <ConnectivityTestDialog cred={cred} open={testing} onOpenChange={setTesting} />
    </>
  )
}

function CredentialRowActionsMenu({
  cred,
  actions,
  onRename,
  onDeviceLimit,
  onTest,
  onRequestDelete,
}: {
  cred: Credential
  actions: CredentialActions
  onRename: () => void
  onDeviceLimit: () => void
  onTest: () => void
  onRequestDelete: () => void
}) {
  return (
    <Menu modal={false}>
      <MenuTrigger
        className={buttonVariants({ size: 'icon-xs', variant: 'ghost' })}
        aria-label={`打开 ${cred.label} 操作菜单`}
        title="账号操作"
      >
        <EllipsisIcon />
      </MenuTrigger>
      <CredentialMenuContent
        cred={cred}
        actions={actions}
        onRename={onRename}
        onDeviceLimit={onDeviceLimit}
        onTest={onTest}
        onRequestDelete={onRequestDelete}
      />
    </Menu>
  )
}

function RenameCredentialDialog({
  cred,
  actions,
  name,
  onNameChange,
  open,
  onOpenChange,
}: {
  cred: Credential
  actions: CredentialActions
  name: string
  onNameChange: (name: string) => void
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const normalizedName = name.trim()
  const unchanged = normalizedName === cred.label.trim()

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogPopup className="sm:max-w-md" showCloseButton={false}>
        <Form
          className="contents"
          onSubmit={(event) => {
            event.preventDefault()
            if (!normalizedName || unchanged) return
            actions.rename.mutate(normalizedName, {
              onSuccess: () => onOpenChange(false),
            })
          }}
        >
          <DialogHeader>
            <DialogTitle>重命名账号</DialogTitle>
            <DialogDescription>
              修改列表中显示的账号名称，不会变更上游凭证。
            </DialogDescription>
          </DialogHeader>
          <DialogPanel>
            <Field>
              <FieldLabel htmlFor={`credential-name-${cred.id}`}>账号名称</FieldLabel>
              <Input
                id={`credential-name-${cred.id}`}
                value={name}
                onChange={(event) => onNameChange(event.target.value)}
                autoFocus
              />
            </Field>
          </DialogPanel>
          <DialogFooter>
            <Button
              type="button"
              variant="outline"
              disabled={actions.rename.isPending}
              onClick={() => onOpenChange(false)}
            >
              取消
            </Button>
            <Button
              type="submit"
              loading={actions.rename.isPending}
              disabled={!normalizedName || unchanged}
            >
              保存
            </Button>
          </DialogFooter>
        </Form>
      </DialogPopup>
    </Dialog>
  )
}

function ScheduleControl({
  cred,
  actions,
  status,
}: {
  cred: Credential
  actions: CredentialActions
  status: CredentialStatusMeta
}) {
  const { toggle } = actions

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
          className={badgeVariants({ size: 'sm', variant: status.variant })}
          aria-label={`${status.label}：${status.detail}`}
          aria-live="polite"
        >
          {status.label}
        </TooltipTrigger>
        <TooltipPopup className="max-w-72 break-words">{status.detail}</TooltipPopup>
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
  freshness,
  reset,
  cost,
  requests,
  showLabel = true,
}: {
  label: string
  util: number | null
  freshness: QuotaFreshness
  reset: number | null
  cost: number | null
  requests: number | null
  showLabel?: boolean
}) {
  const usageSummary = requests == null
    ? '—'
    : `${requests.toLocaleString('zh-CN')} 次 · ${cost == null ? '—' : formatUsd(cost)}`

  if (util == null) {
    const expired = freshness === 'expired'
    const emptyLabel = expired ? '已重置' : '暂无数据'
    const emptyDetail = expired && reset != null
      ? `${label}窗口已于 ${formatFullTime(reset)} 重置，之后暂无新请求`
      : `${label}额度暂无数据`
    return (
      <div
        className="flex w-full flex-col gap-2"
        title={emptyDetail}
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
              title={`${label}本周期请求数与花费：${usageSummary}`}
            >
              {expired ? '—' : usageSummary}
            </span>
          </div>
          <span className="shrink-0 text-xs text-muted-foreground">{emptyLabel}</span>
        </div>
        <div className="h-2 w-full bg-input" aria-hidden />
      </div>
    )
  }

  const percentage = quotaPercentage(util) ?? 0
  const level = quotaLevel(util)
  const indicatorClass = level === 'critical'
    ? 'bg-destructive'
    : level === 'warning'
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
            title={`${label}本周期请求数与花费：${usageSummary}`}
          >
            {usageSummary}
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
