import { memo, type ReactNode, useState } from 'react'
import {
  CalendarDaysIcon,
  ChevronDownIcon,
  ChevronUpIcon,
  EllipsisIcon,
} from 'lucide-react'
import { type Credential } from '@/api/credentials'
import { localize, useI18n, type Language } from '@/lib/i18n'
import { CredentialDevicesDialog } from '@/components/credential-devices-dialog'
import { CredentialProxyDialog } from '@/components/credential-proxy-dialog'
import { CredentialRpmDialog } from '@/components/credential-rpm-dialog'
import { CredentialUsageDialog } from '@/components/credential-usage-dialog'
import {
  ConnectivityTestDialog,
  CredentialMenuContent,
  DeferredMount,
  DeleteCredentialDialog,
  deviceUsageMeta,
  evaluateCredential,
  quotaLevel,
  isOrgAccount,
  orgBadgeLabel,
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
  cn, displayCredentialLabel, formatClockTime, formatFullTime, formatTokens, formatUsd, relativeTime,
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
  rpm: 'w-20',
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
  // 数值列表头跟着单元格右对齐：数字右对齐后个位数落在同一条线上，
  // 一列扫下来能直接比大小，这也是表格里数值列的通行排法。
  const sortable = (label: string, key: SortKey, numeric = false) => {
    const active = sort === key
    const Arrow = active && dir === 'asc' ? ChevronUpIcon : ChevronDownIcon

    return (
      <Button
        type="button"
        size="xs"
        variant="ghost"
        onClick={() => onSortChange(key)}
        className={cn(
          'w-full px-0 sm:text-sm',
          numeric ? 'justify-end text-right' : 'justify-start text-left',
        )}
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
          {sortable(t('5h 用量', '5h usage'), 'usage5h')}
        </TableHead>
        <TableHead className={COL.quota7d} {...sortProps('usage7d')}>
          {sortable(t('7d 用量', '7d usage'), 'usage7d')}
        </TableHead>
        <TableHead className={COL.devices} {...sortProps('devices')}>
          {sortable(t('设备', 'Devices'), 'devices')}
        </TableHead>
        <TableHead className={cn(COL.rpm, 'text-right')} {...sortProps('rpm')}>
          {sortable(t('RPM', 'RPM'), 'rpm', true)}
        </TableHead>
        <TableHead className={COL.recent} {...sortProps('recent')}>
          {sortable(t('最近使用', 'Last used'), 'recent')}
        </TableHead>
        <TableHead className={cn(COL.cost, 'text-right')} {...sortProps('cost')}>
          {sortable(t('累计花费', 'Total cost'), 'cost', true)}
        </TableHead>
        <TableHead className={COL.action}>
          <span className="sr-only">{t('操作', 'Actions')}</span>
        </TableHead>
      </TableRow>
    </TableHeader>
  )
}

export const CredentialRow = memo(function CredentialRow({
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
  /** 收 id 而不是每张卡现做一个闭包，回调引用才能稳定，memo 才拦得住重渲染。 */
  onSelectedChange?: (id: number, next: boolean) => void
}) {
  const { t, language } = useI18n()
  const [devicesOpen, setDevicesOpen] = useState(false)
  const [proxyOpen, setProxyOpen] = useState(false)
  const [rpmOpen, setRpmOpen] = useState(false)
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
  // 设备名额占用的配色：满了红、快满了黄、不限中性，与卡片共用 [deviceUsageMeta]。
  const deviceUsage = deviceUsageMeta(cred.device_count, cred.device_limit_effective)
  // 0 = 不限，此时不显示分母也不谈「打满」。
  const rpmLimit = cred.rpm_limit_effective
  const rpmFull = rpmLimit > 0 && cred.rpm >= rpmLimit
  const added = relativeTime(cred.created_at, now, language)

  return (
    <>
      <TableRow className="xl:hidden" data-state={selected ? 'selected' : undefined}>
        <TableCell colSpan={12} className="w-full max-w-0 whitespace-normal p-0">
          <article className="min-w-0 space-y-3 p-3 sm:space-y-4 sm:p-5">
            <div className="flex items-start gap-3">
              {selectable && (
                <Checkbox
                  checked={selected}
                  onCheckedChange={(checked) => onSelectedChange?.(cred.id, checked)}
                  className="mt-2"
                  aria-label={t(`选择 ${credentialLabel}`, `Select ${credentialLabel}`)}
                />
              )}
              <div className="min-w-0 flex-1">
                <h3 className="min-w-0 truncate font-semibold text-sm leading-snug" title={credentialLabel}>
                  {credentialLabel}
                </h3>
                <p className="mt-1 flex min-w-0 flex-wrap items-center gap-x-1 gap-y-0.5 text-xs text-muted-foreground">
                  <CalendarDaysIcon />
                  <span className="min-w-0 break-all tabular-nums">#{cred.id}</span>
                  <span aria-hidden="true">·</span>
                  <Tooltip>
                    <TooltipTrigger render={<span />} className="min-w-0">
                      {t(`添加于 ${added}`, `Added ${added}`)}
                    </TooltipTrigger>
                    <TooltipPopup>{formatFullTime(cred.created_at, language)}</TooltipPopup>
                  </Tooltip>
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
                  onRpmLimit={() => setRpmOpen(true)}
                  onProxy={() => setProxyOpen(true)}
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
                tokens={cred.quota?.tokens_5h ?? null}
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
                tokens={cred.quota?.tokens_7d ?? null}
                reported={quota.d7.reported}
                hasSnapshot={quota.hasSnapshot}
              />
            </div>

            <dl className="grid grid-cols-2 gap-3 border-t pt-3 sm:grid-cols-3 sm:gap-4 sm:pt-4">
              <MobileFact label={t('优先级', 'Priority')}><span className="tabular-nums">P{cred.priority}</span></MobileFact>
              <MobileFact label={t('账号等级', 'Tier')}>
                <span className="flex flex-wrap items-center gap-1">
                  {isOrgAccount(cred) && (
                    <Badge variant="warning" size="sm">{orgBadgeLabel(cred)}</Badge>
                  )}
                  {cred.tier
                    ? <Badge variant={tierBadgeVariant(cred.tier)} size="sm">{cred.tier}</Badge>
                    : !isOrgAccount(cred) && '—'}
                </span>
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
                  <Badge variant={deviceUsage.variant} size="sm" className="tabular-nums">
                    {cred.device_count}/{effectiveLimit}
                  </Badge>
                  <span className="text-muted-foreground">{policy.label}</span>
                </Button>
              </MobileFact>
              <MobileFact label={t('当前 RPM', 'Current RPM')}>
                <span
                  className={cn('tabular-nums', rpmFull && 'text-warning')}
                  title={t(
                    '最近 60 秒经这个账号转发的请求数（含失败的）',
                    'Requests forwarded through this account in the last 60 seconds (failures included)',
                  )}
                >
                  {cred.rpm > 0 ? cred.rpm : '—'}
                  {rpmLimit > 0 && <span className="text-muted-foreground">/{rpmLimit}</span>}
                </span>
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
              onCheckedChange={(checked) => onSelectedChange?.(cred.id, checked)}
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
                <Tooltip>
                  <TooltipTrigger render={<span />} className="min-w-0">
                    {t(`添加于 ${added}`, `Added ${added}`)}
                  </TooltipTrigger>
                  <TooltipPopup>{formatFullTime(cred.created_at, language)}</TooltipPopup>
                </Tooltip>
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
          <span className="flex flex-wrap items-center gap-1">
            {isOrgAccount(cred) && (
              <Badge
                variant="warning"
                title={t(
                  `组织账号（${cred.org_type}）：用量由整个组织共享`,
                  `Organisation account (${cred.org_type}): the usage is shared across the whole organisation`,
                )}
              >
                {orgBadgeLabel(cred)}
              </Badge>
            )}
            {cred.tier
              ? <Badge variant={tierBadgeVariant(cred.tier)}>{cred.tier}</Badge>
              : !isOrgAccount(cred) && <span className="text-muted-foreground">—</span>}
          </span>
        </TableCell>
        <TableCell className={COL.quota5h}>
          <ListQuotaMeter
            label="5h"
            util={u5h}
            freshness={quota.h5.freshness}
            reset={cred.quota?.rl_5h_reset ?? null}
            cost={cred.quota?.cost_5h ?? null}
            requests={cred.quota?.requests_5h ?? null}
            tokens={cred.quota?.tokens_5h ?? null}
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
            tokens={cred.quota?.tokens_7d ?? null}
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
            {/* 计数底色随名额占用走（绿 / 黄 / 红），与卡片同一套判定，见 [deviceUsageMeta]。 */}
            <Badge variant={deviceUsage.variant} size="sm" className="tabular-nums">
              {cred.device_count}/{effectiveLimit}
            </Badge>
            <Badge variant={policy.variant} size="sm">{policy.label}</Badge>
          </Button>
        </TableCell>
        <TableCell className={cn(COL.rpm, 'text-right')}>
          {/* 闲置账号占了大半，0 一律显示成「—」：一列排开的 0 会把真正有流量的那几行淹掉。
              配了上限就带上分母——两个数同一个 60 秒窗口，直接比得出还剩多少余量。 */}
          <Tooltip>
            <TooltipTrigger
              render={<span />}
              className={cn(
                'tabular-nums text-sm',
                cred.rpm > 0 ? 'font-medium' : 'text-muted-foreground',
                rpmFull && 'text-warning',
              )}
            >
              {cred.rpm > 0 ? cred.rpm : '—'}
              {rpmLimit > 0 && <span className="text-muted-foreground">/{rpmLimit}</span>}
            </TooltipTrigger>
            <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
              {rpmLimit > 0
                ? t(
                  `当前 RPM：最近 60 秒经这个账号转发的请求数（含失败的）。上限 ${rpmLimit} 条/分钟，打满后新请求分流到别的账号，已绑定的设备收到 429。`,
                  `Current RPM: requests forwarded through this account in the last 60 seconds (failures included). Limited to ${rpmLimit}/min; once full, new requests spill to another account and already-bound devices get a 429.`,
                )
                : t(
                  '当前 RPM：最近 60 秒经这个账号转发的请求数（含失败的）',
                  'Current RPM: requests forwarded through this account in the last 60 seconds (failures included)',
                )}
            </TooltipPopup>
          </Tooltip>
        </TableCell>
        <TableCell className={COL.recent}>
          {cred.last_used != null ? relativeTime(cred.last_used, now, language) : t('未使用', 'Never used')}
        </TableCell>
        <TableCell className={cn(COL.cost, 'text-right')}>
          <Tooltip>
            <TooltipTrigger render={<span />} className="tabular-nums font-medium text-sm">
              {formatUsd(cred.cost_total)}
            </TooltipTrigger>
            <TooltipPopup>{t('累计等价 API 费用', 'Cumulative equivalent API cost')}</TooltipPopup>
          </Tooltip>
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
            onRpmLimit={() => setRpmOpen(true)}
            onProxy={() => setProxyOpen(true)}
            onUsage={() => setUsageOpen(true)}
            onTest={() => setTesting(true)}
            onRequestDelete={() => setConfirmDelete(true)}
          />
        </TableCell>
      </TableRow>

      {/* 与卡片同一套：没点开过就不挂，见 DeferredMount。 */}
      <DeferredMount open={devicesOpen || usageOpen || confirmDelete || renameOpen || proxyOpen || rpmOpen || testing}>
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
        <CredentialProxyDialog
          cred={cred}
          open={proxyOpen}
          onOpenChange={setProxyOpen}
          proxy={actions.proxy}
        />
        <CredentialRpmDialog
          cred={cred}
          open={rpmOpen}
          onOpenChange={setRpmOpen}
          rpmLimit={actions.rpmLimit}
        />
        <ConnectivityTestDialog cred={cred} open={testing} onOpenChange={setTesting} />
      </DeferredMount>
    </>
  )
})

function CredentialRowActionsMenu({
  cred,
  actions,
  onRename,
  onDeviceLimit,
  onRpmLimit,
  onProxy,
  onUsage,
  onTest,
  onRequestDelete,
}: {
  cred: Credential
  actions: CredentialActions
  onRename: () => void
  onDeviceLimit: () => void
  onRpmLimit: () => void
  onProxy: () => void
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
        onRpmLimit={onRpmLimit}
        onProxy={onProxy}
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
          delay={status.kind === 'banned' || status.kind === 'token-invalid' ? 0 : undefined}
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
  tokens,
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
  /** 本窗口内用掉的总 token（官方 usage 四项之和，见 Quota.tokens_5h）。 */
  tokens: number | null
  /** 上游是否报告过这个窗口；见 QuotaWindowMeta.reported。 */
  reported: boolean
  /** 该账号是否已有额度快照；用于把「还没数据」和「无此窗口」分开。 */
  hasSnapshot: boolean
  showLabel?: boolean
}) {
  const { t, language, locale } = useI18n()
  // 表格那份摘要（showLabel=false）挤在 8rem 的格子里，三个数只能各留数字：`M`/`K` 标着 token、
  // `$` 标着钱，唯一没单位的就是最左边的请求数，它的含义写在 title 里（见下面的 summaryTitle）。
  const usageSummary = requests == null
    ? '—'
    : `${requests.toLocaleString(locale)} · ${tokens == null ? '—' : formatTokens(tokens)} · ${cost == null ? '—' : formatUsd(cost)}`
  const summaryTitle = requests == null
    ? undefined
    : t(
        `${label}本周期：${requests.toLocaleString(locale)} 次请求 · ${tokens == null ? '—' : `${tokens.toLocaleString(locale)} token`} · ${cost == null ? '—' : formatUsd(cost)}`,
        `${label} this period: ${requests.toLocaleString(locale)} ${requests === 1 ? 'request' : 'requests'} · ${tokens == null ? '—' : `${tokens.toLocaleString(locale)} tokens`} · ${cost == null ? '—' : formatUsd(cost)}`,
      )

  // 有快照却从没报过这个窗口 = 这个账号的额度模型里没有它，再等也不会出现。
  // 表格的列摘不掉（列宽固定、表头常驻），所以必须在格子里把原因说出来，
  // 而不是留一条和「还没跑过请求」长得一模一样的空进度条。
  if (hasSnapshot && !reported) {
    return (
      <div
        className="flex w-full flex-col gap-2"
        title={t(
          `上游从未为该账号返回 ${label} 窗口，说明它的用量模型里没有这个窗口（不是数据缺失）`,
          `The upstream has never returned a ${label} window for this account, meaning its usage model has no such window (this is not missing data)`,
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
      : t(`${label}用量暂无数据`, `No ${label} usage data`)
    return (
      <div
        className="flex w-full flex-col gap-2"
        title={emptyDetail}
      >
        <div className="flex items-center justify-between gap-2">
          <div className="flex min-w-0 items-baseline gap-1.5">
            <span className={cn('font-medium text-sm', !showLabel && 'sr-only')}>{label}</span>
            {!showLabel && (
              <SummaryValue hint={summaryTitle}>{expired ? '—' : usageSummary}</SummaryValue>
            )}
          </div>
          <span className="shrink-0 text-xs text-muted-foreground">{emptyLabel}</span>
        </div>
        <div className="h-2 w-full bg-input" aria-hidden />
        {showLabel && !expired && (requests != null || cost != null || tokens != null || reset != null) && (
          <ListQuotaDetails requests={requests} cost={cost} tokens={tokens} reset={reset} />
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
    <Meter value={percentage} max={100} title={t(`${label}用量 ${percentage}%`, `${label} usage ${percentage}%`)}>
      <div className="flex items-center justify-between gap-2">
        <div className="flex min-w-0 items-baseline gap-1.5">
          <MeterLabel className={cn(!showLabel && 'sr-only')}>{label}</MeterLabel>
          {!showLabel && <SummaryValue hint={summaryTitle}>{usageSummary}</SummaryValue>}
        </div>
        <MeterValue className="font-medium leading-none">{() => `${percentage}%`}</MeterValue>
      </div>
      <MeterTrack>
        <MeterIndicator className={indicatorClass} />
      </MeterTrack>
      {showLabel && <ListQuotaDetails requests={requests} cost={cost} tokens={tokens} reset={reset} />}
    </Meter>
  )
}

/**
 * 表格里那一格的用量摘要（`128 · 18.4M · $6.85`）。
 *
 * 格子只有 8rem，三个数各留数字、还常被 `truncate` 切掉尾巴，所以提示是这里唯一能看到
 * 「哪个数是什么、精确值多少」的地方——必须立刻出（`delay={0}`），原生 `title` 那一秒
 * 等下来就没人再等了。没有摘要可说时（该窗口连请求数都没有）不挂提示，免得冒一个空气泡。
 */
function SummaryValue({ hint, children }: { hint?: string; children: ReactNode }) {
  const className = 'min-w-0 truncate font-medium text-foreground text-sm leading-none tabular-nums'
  if (!hint) return <span className={className}>{children}</span>
  return (
    <Tooltip>
      <TooltipTrigger render={<span />} delay={0} className={className}>
        {children}
      </TooltipTrigger>
      <TooltipPopup className="max-w-72 whitespace-normal break-words text-left leading-5">
        {hint}
      </TooltipPopup>
    </Tooltip>
  )
}

function ListQuotaDetails({
  requests,
  cost,
  tokens,
  reset,
}: {
  requests: number | null
  cost: number | null
  tokens: number | null
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
      {/* token 与「重置」同一行，不另起一行：这块要在窄屏两列里塞两个窗口，多一行就把
          整张卡片撑高一截。左边是本窗口用掉的总 token（只给数字，量纲由 K/M 表达，
          全称在读屏文本与悬浮提示里），右边仍是重置时刻。 */}
      {/* 这两项的提示同样走 Tooltip 组件 + delay 0：原生 title 要等约 1 秒，而这里装的是
          精确 token 数与精确重置时刻——都是「想确认一下」才去悬浮的东西，等一秒等于没有。 */}
      <div className="col-span-2 flex min-w-0 items-baseline justify-between gap-2 text-[11px] text-muted-foreground">
        <div className="flex min-w-0 items-baseline gap-1">
          <dt className="sr-only">{t('总 token', 'Total tokens')}</dt>
          <dd className="min-w-0">
            <Tooltip>
              <TooltipTrigger
                render={<span />}
                delay={0}
                className="whitespace-nowrap font-medium text-foreground tabular-nums"
              >
                {tokens == null ? '—' : formatTokens(tokens)}
              </TooltipTrigger>
              <TooltipPopup className="max-w-72 whitespace-normal break-words text-left leading-5">
                {tokens == null
                  ? t('本周期总 token：暂无数据', 'Total tokens this period: no data')
                  : t(
                    `本周期总 token ${tokens.toLocaleString(locale)}（输入 + 输出 + 缓存写 + 缓存读，官方 usage 口径，不加权）`,
                    `${tokens.toLocaleString(locale)} tokens this period (input + output + cache write + cache read, per the official usage fields, unweighted)`,
                  )}
              </TooltipPopup>
            </Tooltip>
          </dd>
        </div>
        <div className="flex min-w-0 items-baseline gap-1">
          <dt className="whitespace-nowrap">{t('重置', 'Reset')}</dt>
          <dd className="min-w-0">
            {reset == null ? (
              <span className="whitespace-nowrap tabular-nums">—</span>
            ) : (
              <Tooltip>
                <TooltipTrigger render={<span />} delay={0} className="whitespace-nowrap tabular-nums">
                  {formatClockTime(reset, language)}
                </TooltipTrigger>
                <TooltipPopup>
                  {t(`${formatFullTime(reset, language)} 重置`, `Resets ${formatFullTime(reset, language)}`)}
                </TooltipPopup>
              </Tooltip>
            )}
          </dd>
        </div>
      </div>
    </dl>
  )
}

function devicePolicyMeta(deviceLimit: number, language: Language): { label: string; variant: BadgeProps['variant'] } {
  if (deviceLimit === 0) return { label: localize(language, '跟随默认', 'Default'), variant: 'secondary' }
  if (deviceLimit < 0) return { label: localize(language, '不限', 'Unlimited'), variant: 'outline' }
  return { label: localize(language, '自定义', 'Custom'), variant: 'info' }
}
