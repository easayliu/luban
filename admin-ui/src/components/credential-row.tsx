import { type ReactNode, useState } from 'react'
import {
  CalendarDaysIcon,
  ChevronDownIcon,
  ChevronUpIcon,
  EllipsisIcon,
} from 'lucide-react'
import { type Credential } from '@/api/credentials'
import { localize, useI18n, type Language } from '@/lib/i18n'
import { CredentialDevicesDialog } from '@/components/credential-devices-dialog'
import { CredentialUsageDialog } from '@/components/credential-usage-dialog'
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
import {
  cn, displayCredentialLabel, formatClockTime, formatFullTime, formatUsd, relativeTime,
} from '@/lib/utils'

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
  const { t } = useI18n()
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
        title={active
          ? t(`按${label}排序（点击切换升降序）`, `Sort by ${label} (click to reverse direction)`)
          : t(`按${label}排序`, `Sort by ${label}`)}
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
              aria-label={t('全选当前筛选结果', 'Select all filtered results')}
            />
          )}
        </TableHead>
        <TableHead className={COL.account} {...sortProps('name')}>
          {sortable(t('账号', 'Account'), 'name')}
        </TableHead>
        <TableHead className={COL.schedule}>{t('调度', 'Scheduling')}</TableHead>
        <TableHead className={COL.priority} {...sortProps('priority')}>
          {sortable(t('优先级', 'Priority'), 'priority')}
        </TableHead>
        <TableHead className={COL.tier} {...sortProps('tier')}>
          {sortable(t('账号等级', 'Tier'), 'tier')}
        </TableHead>
        <TableHead className={COL.quota5h} {...sortProps('usage5h')}>
          {sortable(t('5h 额度', '5h quota'), 'usage5h')}
        </TableHead>
        <TableHead className={COL.quota7d} {...sortProps('usage7d')}>
          {sortable(t('7d 额度', '7d quota'), 'usage7d')}
        </TableHead>
        <TableHead className={COL.devices} {...sortProps('devices')}>
          {sortable(t('设备', 'Devices'), 'devices')}
        </TableHead>
        <TableHead className={COL.recent} {...sortProps('recent')}>
          {sortable(t('最近使用', 'Last used'), 'recent')}
        </TableHead>
        <TableHead className={COL.cost} {...sortProps('cost')}>
          {sortable(t('累计花费', 'Total cost'), 'cost')}
        </TableHead>
        <TableHead className={COL.action}>
          <span className="sr-only">{t('操作', 'Actions')}</span>
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
  const { t, language } = useI18n()
  const [devicesOpen, setDevicesOpen] = useState(false)
  const [usageOpen, setUsageOpen] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [renameOpen, setRenameOpen] = useState(false)
  const [renameName, setRenameName] = useState(cred.label)
  const [testing, setTesting] = useState(false)
  const actions = useCredentialActions(cred)
  const evaluation = evaluateCredential(cred, now, language)
  const { quota } = evaluation
  const credentialLabel = displayCredentialLabel(cred.label, language)
  const u5h = quota.h5.utilization
  const u7d = quota.d7.utilization
  const effectiveLimit = cred.device_limit_effective > 0 ? cred.device_limit_effective : '∞'
  const policy = devicePolicyMeta(cred.device_limit, language)
  const added = relativeTime(cred.created_at, now, language)

  return (
    <>
      <TableRow className="xl:hidden" data-state={selected ? 'selected' : undefined}>
        <TableCell colSpan={11} className="w-full max-w-0 whitespace-normal p-0">
          <article className="min-w-0 space-y-3 p-3 sm:space-y-4 sm:p-5">
            <div className="flex items-start gap-3">
              {selectable && (
                <Checkbox
                  checked={selected}
                  onCheckedChange={(checked) => onSelectedChange?.(checked)}
                  className="mt-2"
                  aria-label={t(`选择 ${credentialLabel}`, `Select ${credentialLabel}`)}
                />
              )}
              <div className="min-w-0 flex-1">
                <h3 className="min-w-0 truncate font-semibold text-sm leading-snug" title={credentialLabel}>
                  {credentialLabel}
                </h3>
                <p
                  className="mt-1 flex min-w-0 flex-wrap items-center gap-x-1 gap-y-0.5 text-xs text-muted-foreground"
                  title={t(
                    `添加于 ${formatFullTime(cred.created_at, language)}`,
                    `Added ${formatFullTime(cred.created_at, language)}`,
                  )}
                >
                  <CalendarDaysIcon />
                  <span className="min-w-0 break-all tabular-nums">#{cred.id}</span>
                  <span aria-hidden="true">·</span>
                  <span className="min-w-0">{t(`添加于 ${added}`, `Added ${added}`)}</span>
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
                  onUsage={() => setUsageOpen(true)}
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
                reported={quota.h5.reported}
                hasSnapshot={quota.hasSnapshot}
              />
              <ListQuotaMeter
                label="7d"
                util={u7d}
                freshness={quota.d7.freshness}
                reset={cred.quota?.rl_7d_reset ?? null}
                cost={cred.quota?.cost_7d ?? null}
                requests={cred.quota?.requests_7d ?? null}
                reported={quota.d7.reported}
                hasSnapshot={quota.hasSnapshot}
              />
            </div>

            <dl className="grid grid-cols-2 gap-3 border-t pt-3 sm:grid-cols-3 sm:gap-4 sm:pt-4">
              <MobileFact label={t('优先级', 'Priority')}><span className="tabular-nums">P{cred.priority}</span></MobileFact>
              <MobileFact label={t('账号等级', 'Tier')}>
                {cred.tier
                  ? <Badge variant={tierBadgeVariant(cred.tier)} size="sm">{cred.tier}</Badge>
                  : '—'}
              </MobileFact>
              <MobileFact label={t('设备', 'Devices')}>
                <Button
                  type="button"
                  size="xs"
                  variant="ghost"
                  onClick={() => setDevicesOpen(true)}
                  title={t(`查看已绑定设备 · ${policy.label}策略`, `View bound devices · ${policy.label} policy`)}
                  aria-label={t(`查看 ${credentialLabel} 的已绑定设备`, `View bound devices for ${credentialLabel}`)}
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
              aria-label={t(`选择 ${credentialLabel}`, `Select ${credentialLabel}`)}
            />
          )}
        </TableCell>
        <TableCell className={cn(COL.account, 'whitespace-nowrap')}>
          <div className="flex min-w-0 items-center">
            <div className="min-w-0 flex-1">
              <span className="block min-w-0 whitespace-nowrap font-semibold text-sm leading-snug" title={credentialLabel}>
                {credentialLabel}
              </span>
              <span className="mt-1 flex min-w-0 flex-wrap items-center gap-x-1 gap-y-0.5 text-xs text-muted-foreground">
                <span className="min-w-0 break-all tabular-nums">#{cred.id}</span>
                <span aria-hidden="true">·</span>
                <span className="min-w-0" title={t(`添加于 ${formatFullTime(cred.created_at, language)}`, `Added ${formatFullTime(cred.created_at, language)}`)}>
                  {t(`添加于 ${added}`, `Added ${added}`)}
                </span>
              </span>
            </div>
          </div>
        </TableCell>
        <TableCell className={COL.schedule}>
          <ScheduleControl cred={cred} actions={actions} status={evaluation.status} />
        </TableCell>
        <TableCell className={COL.priority}>
          <span className="font-semibold text-sm tabular-nums" title={t('数值越小，调度优先级越高', 'Lower values have higher scheduling priority')}>
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
            reported={quota.h5.reported}
            hasSnapshot={quota.hasSnapshot}
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
            reported={quota.d7.reported}
            hasSnapshot={quota.hasSnapshot}
            showLabel={false}
          />
        </TableCell>
        <TableCell className={COL.devices}>
          <Button
            type="button"
            size="xs"
            variant="ghost"
            onClick={() => setDevicesOpen(true)}
            title={t(`查看已绑定设备 · ${policy.label}策略`, `View bound devices · ${policy.label} policy`)}
            aria-haspopup="dialog"
          >
            <span className="tabular-nums">{cred.device_count}/{effectiveLimit}</span>
            <Badge variant={policy.variant} size="sm">{policy.label}</Badge>
          </Button>
        </TableCell>
        <TableCell className={COL.recent}>
          {cred.last_used != null ? relativeTime(cred.last_used, now, language) : t('未使用', 'Never used')}
        </TableCell>
        <TableCell className={COL.cost}>
          <span className="tabular-nums font-medium text-sm" title={t('累计等价 API 费用', 'Cumulative equivalent API cost')}>
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
            onUsage={() => setUsageOpen(true)}
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
      <CredentialUsageDialog cred={cred} open={usageOpen} onOpenChange={setUsageOpen} />
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
  onUsage,
  onTest,
  onRequestDelete,
}: {
  cred: Credential
  actions: CredentialActions
  onRename: () => void
  onDeviceLimit: () => void
  onUsage: () => void
  onTest: () => void
  onRequestDelete: () => void
}) {
  const { t, language } = useI18n()
  const credentialLabel = displayCredentialLabel(cred.label, language)
  return (
    <Menu modal={false}>
      <MenuTrigger
        className={buttonVariants({ size: 'icon-xs', variant: 'ghost' })}
        aria-label={t(`打开 ${credentialLabel} 操作菜单`, `Open actions for ${credentialLabel}`)}
        title={t('账号操作', 'Account actions')}
      >
        <EllipsisIcon />
      </MenuTrigger>
      <CredentialMenuContent
        cred={cred}
        actions={actions}
        onRename={onRename}
        onDeviceLimit={onDeviceLimit}
        onUsage={onUsage}
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
  const { t } = useI18n()
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
            <DialogTitle>{t('重命名账号', 'Rename account')}</DialogTitle>
            <DialogDescription>
              {t('修改列表中显示的账号名称，不会变更上游凭证。', 'Change the account name shown in the list without modifying the upstream credential.')}
            </DialogDescription>
          </DialogHeader>
          <DialogPanel>
            <Field>
              <FieldLabel htmlFor={`credential-name-${cred.id}`}>{t('账号名称', 'Account name')}</FieldLabel>
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
              {t('取消', 'Cancel')}
            </Button>
            <Button
              type="submit"
              loading={actions.rename.isPending}
              disabled={!normalizedName || unchanged}
            >
              {t('保存', 'Save')}
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
  const { language } = useI18n()
  const { toggle } = actions
  const credentialLabel = displayCredentialLabel(cred.label, language)

  return (
    <div className="flex shrink-0 flex-col items-end gap-3 xl:flex-row xl:items-center xl:gap-2">
      <div className="flex items-center gap-2">
        {toggle.isPending && <Spinner />}
        <Switch
          checked={!cred.disabled}
          onCheckedChange={(enabled) => toggle.mutate(!enabled)}
          disabled={toggle.isPending}
          title={switchTitle(cred, language)}
          aria-label={`${credentialLabel}: ${switchTitle(cred, language)}`}
        />
      </div>
      <Tooltip>
        <TooltipTrigger
          className={badgeVariants({ size: 'sm', variant: status.variant })}
          delay={status.kind === 'banned' ? 0 : undefined}
          aria-label={`${status.label}: ${status.detail}`}
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
  reported,
  hasSnapshot,
  showLabel = true,
}: {
  label: string
  util: number | null
  freshness: QuotaFreshness
  reset: number | null
  cost: number | null
  requests: number | null
  /** 上游是否报告过这个窗口；见 QuotaWindowMeta.reported。 */
  reported: boolean
  /** 该账号是否已有额度快照；用于把「还没数据」和「无此窗口」分开。 */
  hasSnapshot: boolean
  showLabel?: boolean
}) {
  const { t, language, locale } = useI18n()
  const usageSummary = requests == null
    ? '—'
    : t(
        `${requests.toLocaleString(locale)} 次 · ${cost == null ? '—' : formatUsd(cost)}`,
        `${requests.toLocaleString(locale)} ${requests === 1 ? 'request' : 'requests'} · ${cost == null ? '—' : formatUsd(cost)}`,
      )

  // 有快照却从没报过这个窗口 = 这个账号的额度模型里没有它，再等也不会出现。
  // 表格的列摘不掉（列宽固定、表头常驻），所以必须在格子里把原因说出来，
  // 而不是留一条和「还没跑过请求」长得一模一样的空进度条。
  if (hasSnapshot && !reported) {
    return (
      <div
        className="flex w-full flex-col gap-2"
        title={t(
          `上游从未为该账号返回 ${label} 窗口，说明它的额度模型里没有这个窗口（不是数据缺失）`,
          `The upstream has never returned a ${label} window for this account, meaning its quota model has no such window (this is not missing data)`,
        )}
      >
        <div className="flex items-center justify-between gap-2">
          <div className="flex min-w-0 items-baseline gap-1.5">
            <span className={cn('font-medium text-sm', !showLabel && 'sr-only')}>{label}</span>
            <span
              className={cn(
                'min-w-0 truncate tabular-nums text-muted-foreground',
                showLabel ? 'text-xs' : 'text-sm leading-none',
              )}
            >
              —
            </span>
          </div>
          <span className="shrink-0 text-xs text-muted-foreground">{t('无此窗口', 'Not applicable')}</span>
        </div>
      </div>
    )
  }

  if (util == null) {
    const expired = freshness === 'expired'
    const emptyLabel = expired ? t('已重置', 'Reset') : t('暂无数据', 'No data')
    const emptyDetail = expired && reset != null
      ? t(
          `${label}窗口已于 ${formatFullTime(reset, language)} 重置，之后暂无新请求`,
          `${label} window reset at ${formatFullTime(reset, language)}; there are no newer requests`,
        )
      : t(`${label}额度暂无数据`, `No ${label} quota data`)
    return (
      <div
        className="flex w-full flex-col gap-2"
        title={emptyDetail}
      >
        <div className="flex items-center justify-between gap-2">
          <div className="flex min-w-0 items-baseline gap-1.5">
            <span className={cn('font-medium text-sm', !showLabel && 'sr-only')}>{label}</span>
            {!showLabel && (
              <span
                className="min-w-0 truncate font-medium text-foreground text-sm leading-none tabular-nums"
                title={t(`${label}本周期请求数与花费：${usageSummary}`, `${label} requests and cost this period: ${usageSummary}`)}
              >
                {expired ? '—' : usageSummary}
              </span>
            )}
          </div>
          <span className="shrink-0 text-xs text-muted-foreground">{emptyLabel}</span>
        </div>
        <div className="h-2 w-full bg-input" aria-hidden />
        {showLabel && !expired && (requests != null || cost != null || reset != null) && (
          <ListQuotaDetails requests={requests} cost={cost} reset={reset} />
        )}
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
    <Meter value={percentage} max={100} title={t(`${label}额度使用率 ${percentage}%`, `${label} quota usage ${percentage}%`)}>
      <div className="flex items-center justify-between gap-2">
        <div className="flex min-w-0 items-baseline gap-1.5">
          <MeterLabel className={cn(!showLabel && 'sr-only')}>{label}</MeterLabel>
          {!showLabel && (
            <span
              className="min-w-0 truncate font-medium text-foreground text-sm leading-none tabular-nums"
              title={t(`${label}本周期请求数与花费：${usageSummary}`, `${label} requests and cost this period: ${usageSummary}`)}
            >
              {usageSummary}
            </span>
          )}
        </div>
        <MeterValue className="font-medium leading-none">{() => `${percentage}%`}</MeterValue>
      </div>
      <MeterTrack>
        <MeterIndicator className={indicatorClass} />
      </MeterTrack>
      {showLabel && <ListQuotaDetails requests={requests} cost={cost} reset={reset} />}
    </Meter>
  )
}

function ListQuotaDetails({
  requests,
  cost,
  reset,
}: {
  requests: number | null
  cost: number | null
  reset: number | null
}) {
  const { t, language, locale } = useI18n()
  const formattedRequests = requests == null
    ? '—'
    : t(
        `${requests.toLocaleString(locale)} 次`,
        `${requests.toLocaleString(locale)} req`,
      )

  return (
    <dl className="grid min-w-0 grid-cols-2 gap-x-2 gap-y-1">
      <div className="min-w-0">
        <dt className="sr-only">{t('请求', requests === 1 ? 'Request' : 'Requests')}</dt>
        <dd className="whitespace-nowrap font-medium text-xs tabular-nums">
          {formattedRequests}
        </dd>
      </div>
      <div className="min-w-0 text-right">
        <dt className="sr-only">{t('花费', 'Cost')}</dt>
        <dd className="whitespace-nowrap font-medium text-xs tabular-nums">
          {cost == null ? '—' : formatUsd(cost)}
        </dd>
      </div>
      <div className="col-span-2 flex min-w-0 items-baseline justify-between gap-2 text-[11px] text-muted-foreground">
        <dt className="whitespace-nowrap">{t('重置', 'Reset')}</dt>
        <dd
          className="whitespace-nowrap tabular-nums"
          title={reset == null
            ? undefined
            : t(`${formatFullTime(reset, language)} 重置`, `Resets ${formatFullTime(reset, language)}`)}
        >
          {reset == null ? '—' : formatClockTime(reset, language)}
        </dd>
      </div>
    </dl>
  )
}

function devicePolicyMeta(deviceLimit: number, language: Language): { label: string; variant: BadgeProps['variant'] } {
  if (deviceLimit === 0) return { label: localize(language, '跟随默认', 'Default'), variant: 'secondary' }
  if (deviceLimit < 0) return { label: localize(language, '不限', 'Unlimited'), variant: 'outline' }
  return { label: localize(language, '自定义', 'Custom'), variant: 'info' }
}
