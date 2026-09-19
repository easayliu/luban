import { memo, type ReactNode, useState } from 'react'
import {
  CalendarDaysIcon,
  ChevronDownIcon,
  ChevronUpIcon,
  EllipsisIcon,
  GlobeIcon,
  MessagesSquareIcon,
  SmartphoneIcon,
} from 'lucide-react'
import { type Credential } from '@/api/credentials'
import { localize, useI18n, type Language } from '@/lib/i18n'
import { CredentialDevicesDialog } from '@/components/credential-devices-dialog'
import { CredentialProxyDialog } from '@/components/credential-proxy-dialog'
import { CredentialRpmDialog } from '@/components/credential-rpm-dialog'
import { CredentialQuotaDialog } from '@/components/credential-quota-dialog'
import { CredentialUsageDialog } from '@/components/credential-usage-dialog'
import {
  ConnectivityTestDialog,
  CredentialMenuContent,
  DeferredMount,
  DeleteCredentialDialog,
  deviceUsageMeta,
  evaluateCredential,
  proxyDisplayLabel,
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
  cn,
  displayCredentialLabel,
  formatClockTime,
  formatCountdown,
  formatFullTime,
  formatTokens,
  formatUsd,
  relativeTime,
} from '@/lib/utils'

/**
 * 列宽预算（表格是 `table-fixed`，账号列吃掉剩余宽度）。
 *
 * 固定列合计：xl 992px（不含「最近使用」）、2xl 1120px（多出「最近使用」，两列用量各放宽
 * 一档）。改这里时两列用量与设备列是一组：它们之间只能互相匀，合计一动账号列就跟着缩。
 * 容器最大 88rem（见 `.page-frame`）：1280 宽的屏上表格约 1214px，账号列约 220px；
 * ≥1536 的屏约 1406px，账号列约 286px。
 * 「最近使用」只在 2xl 起显示——它是 12 列里信息量最低的一列，排序菜单里仍可按它排。
 * 每格内边距 p-2.5，各列的可用内容宽度 = 列宽 − 20px。
 */
const COL = {
  /** 表格基类给带勾选框的格子 `has-[[role=checkbox]]:w-px`（table-auto 时的「缩到内容宽」），
      table-fixed 下会把这一列真压成 1px、勾选框叠到账号名上，这里按同等特异性写回 w-10。 */
  select: 'w-10 has-[[role=checkbox]]:w-10',
  account: 'w-auto',
  /** 开关 32 + 间距 8 + 状态徽标。最长的「Usage credits 生效中 / 待确认」单行要 105px，整列得
      168px 才放全，多数行却只有「运行正常」四个字，大半是空白。表格里让徽标换到两行（行高本来
      就是两行：账号名 + 添加时间），列宽收到 144px，状态文字一个不丢，见 ScheduleControl。 */
  schedule: 'w-36',
  /** 格子里只有 `P0`（约 18px），宽度是英文表头 `PRIORITY` + 排序箭头（约 86px）定的；
      表头允许越到下一列的内边距里（`TOTAL COST` 一直如此），收到 80px 仍读得全。 */
  priority: 'w-20',
  /** 「Max 20x」徽标 58px；组织账号的两枚徽标本来就换行排。 */
  tier: 'w-20',
  /**
   * 一格里排四样：用量摘要（最长 `1,633 · 245M · $188.30` 实测约 140px）、百分比 28px、
   * 进度条、重置倒计时 44px，分两行的依据见 [ListQuotaMeter] 里那段排版注释。
   *
   * 内容宽 = 列宽 − 20px：w-44 给 156px、2xl 的 w-48 给 172px。2xl 下除最长的那一条外都放得全，
   * xl 下四位数请求 + 三位美元的几行会截尾——精确值在悬浮提示里（delay=0）。再宽就只能动账号列了，
   * 那是身份列，不动。多要的 16px 由优先级、账号等级、RPM、累计花费（2xl 再加「最近使用」）
   * 各让 8px 匀出来，固定列合计不变。
   */
  quota5h: 'w-44 2xl:w-48',
  quota7d: 'w-44 2xl:w-48',
  /**
   * 只剩一枚 `2/5` 名额徽章——「跟随默认」那枚不再画（见 [devicePolicyMeta]），w-32 里有一半
   * 是空白。收到 w-24：英文表头 `DEVICES` 比同宽的 `LAST USED`、`TOTAL COST` 都短，那两列
   * 一直是这个宽度。
   */
  devices: 'w-24',
  rpm: 'w-18',
  recent: 'hidden w-22 2xl:table-cell',
  cost: 'w-22',
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
          'w-full px-0 text-2xs font-semibold uppercase tracking-[0.06em] sm:text-2xs',
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
          {sortable(t('设备 / 会话', 'Devices / Sessions'), 'devices')}
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
  const [quotaOpen, setQuotaOpen] = useState(false)
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
  // 模拟会话名额，同一套判定；与设备名额同一格上下两行、同一个对话框。
  const sessionEffectiveLimit = cred.session_limit_effective > 0 ? cred.session_limit_effective : '∞'
  const sessionPolicy = devicePolicyMeta(cred.session_limit, language)
  const sessionUsage = deviceUsageMeta(cred.session_count, cred.session_limit_effective)
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
                  {cred.proxy && (
                    <>
                      <span aria-hidden="true">·</span>
                      <Tooltip>
                        <TooltipTrigger
                          render={<button type="button" />}
                          className="inline-flex min-w-0 items-center gap-0.5 text-info-foreground"
                          onClick={() => setProxyOpen(true)}
                        >
                          <GlobeIcon className="size-3 shrink-0" />
                          <span className="min-w-0 truncate">{proxyDisplayLabel(cred.proxy)}</span>
                        </TooltipTrigger>
                        <TooltipPopup className="max-w-72 break-all">{cred.proxy}</TooltipPopup>
                      </Tooltip>
                    </>
                  )}
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
                  onQuotaPause={() => setQuotaOpen(true)}
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
                now={now}
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
                now={now}
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
                  {/* 同上：跟随默认不占这一行的宽度，见 [devicePolicyMeta]。 */}
                  {!policy.isDefault && <span className="text-muted-foreground">{policy.label}</span>}
                </Button>
              </MobileFact>
              <MobileFact label={t('模拟会话', 'Sessions')}>
                <Button
                  type="button"
                  size="xs"
                  variant="ghost"
                  onClick={() => setDevicesOpen(true)}
                  title={t(`查看模拟会话 · ${sessionPolicy.label}策略`, `View simulated sessions · ${sessionPolicy.label} policy`)}
                  aria-label={t(`查看 ${credentialLabel} 的模拟会话`, `View simulated sessions for ${credentialLabel}`)}
                >
                  <Badge variant={sessionUsage.variant} size="sm" className="tabular-nums">
                    {cred.session_count}/{sessionEffectiveLimit}
                  </Badge>
                  {!sessionPolicy.isDefault && <span className="text-muted-foreground">{sessionPolicy.label}</span>}
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
        <TableCell className={cn(COL.account, 'overflow-hidden')}>
          <div className="flex min-w-0 items-center">
            <div className="min-w-0 flex-1">
              {/* 账号名超出列宽就截断：table-fixed 下不截断会压到相邻列上。全名在 title 里。 */}
              <span className="block min-w-0 truncate font-semibold text-sm leading-snug" title={credentialLabel}>
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
                {cred.proxy && (
                  <>
                    <span aria-hidden="true">·</span>
                    <Tooltip>
                      <TooltipTrigger
                        render={<button type="button" />}
                        className="inline-flex min-w-0 items-center gap-0.5 text-info-foreground hover:underline"
                        onClick={() => setProxyOpen(true)}
                      >
                        <GlobeIcon className="size-3 shrink-0" />
                        <span className="min-w-0 truncate">{proxyDisplayLabel(cred.proxy)}</span>
                      </TooltipTrigger>
                      <TooltipPopup className="max-w-72 break-all">{cred.proxy}</TooltipPopup>
                    </Tooltip>
                  </>
                )}
              </span>
            </div>
          </div>
        </TableCell>
        <TableCell className={cn(COL.schedule, 'overflow-hidden')}>
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
            now={now}
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
            now={now}
            showLabel={false}
          />
        </TableCell>
        <TableCell className={COL.devices}>
          {/* 设备与模拟会话两种名额上下两行、各带自己的图标；两颗按钮开的是同一个对话框。 */}
          <div className="flex flex-col items-start gap-0.5">
            <Button
              type="button"
              size="xs"
              variant="ghost"
              onClick={() => setDevicesOpen(true)}
              title={t(`查看已绑定设备 · ${policy.label}策略`, `View bound devices · ${policy.label} policy`)}
              aria-haspopup="dialog"
            >
              <SmartphoneIcon className="size-3.5 text-muted-foreground" aria-hidden />
              {/* 计数底色随名额占用走（绿 / 黄 / 红），与卡片同一套判定，见 [deviceUsageMeta]。 */}
              <Badge variant={deviceUsage.variant} size="sm" className="tabular-nums">
                {cred.device_count}/{effectiveLimit}
              </Badge>
              {/* 跟随默认那一档不画徽章，见 [devicePolicyMeta]；策略仍写在按钮的悬浮提示里。 */}
              {!policy.isDefault && <Badge variant={policy.variant} size="sm">{policy.label}</Badge>}
            </Button>
            <Button
              type="button"
              size="xs"
              variant="ghost"
              onClick={() => setDevicesOpen(true)}
              title={t(`查看模拟会话 · ${sessionPolicy.label}策略`, `View simulated sessions · ${sessionPolicy.label} policy`)}
              aria-label={t(`查看 ${credentialLabel} 的模拟会话`, `View simulated sessions for ${credentialLabel}`)}
              aria-haspopup="dialog"
            >
              <MessagesSquareIcon className="size-3.5 text-muted-foreground" aria-hidden />
              <Badge variant={sessionUsage.variant} size="sm" className="tabular-nums">
                {cred.session_count}/{sessionEffectiveLimit}
              </Badge>
              {!sessionPolicy.isDefault && <Badge variant={sessionPolicy.variant} size="sm">{sessionPolicy.label}</Badge>}
            </Button>
          </div>
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
            onQuotaPause={() => setQuotaOpen(true)}
            onProxy={() => setProxyOpen(true)}
            onUsage={() => setUsageOpen(true)}
            onTest={() => setTesting(true)}
            onRequestDelete={() => setConfirmDelete(true)}
          />
        </TableCell>
      </TableRow>

      {/* 与卡片同一套：没点开过就不挂，见 DeferredMount。 */}
      <DeferredMount open={devicesOpen || usageOpen || confirmDelete || renameOpen || proxyOpen || rpmOpen || quotaOpen || testing}>
        <CredentialDevicesDialog
          cred={cred}
          open={devicesOpen}
          onOpenChange={setDevicesOpen}
          limit={actions.limit}
          sessionLimit={actions.sessionLimit}
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
        <CredentialQuotaDialog
          cred={cred}
          open={quotaOpen}
          onOpenChange={setQuotaOpen}
          quotaPause={actions.quotaPause}
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
  onQuotaPause,
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
  onQuotaPause: () => void
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
        onQuotaPause={onQuotaPause}
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
    <div className="flex shrink-0 flex-col items-end gap-3 xl:min-w-0 xl:flex-row xl:items-center xl:gap-2">
      <div className="flex shrink-0 items-center gap-2">
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
          className={cn(
            badgeVariants({ size: 'sm', variant: status.variant }),
            // 表格（xl 起）里「调度」列只有 9rem：徽标放开固定高度、允许换行，
            // 「Usage credits 生效中」拆成两行放全；卡片/移动端布局不受列宽约束，照旧单行。
            'min-w-0 max-w-full shrink xl:h-auto xl:whitespace-normal xl:py-0.5 xl:text-left',
          )}
          delay={status.kind === 'banned' || status.kind === 'token-invalid' ? 0 : undefined}
          aria-label={`${status.label}: ${status.detail}`}
          aria-live="polite"
        >
          {/* 两行还放不下才截断（兜底），完整文案在提示里。 */}
          <span className="min-w-0 truncate xl:line-clamp-2 xl:whitespace-normal">{status.label}</span>
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
  now,
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
  /** 页面时钟（30 秒一跳），重置倒计时靠它走，见 [formatCountdown]。 */
  now: number
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
        {/* 两行的排法与有用量时保持一致（见下面那条注释）：第一行说用量，第二行是这个窗口的时间
            维度——占位条虽然没有数据可画，位置与倒计时的落点仍与隔壁格子对得齐。 */}
        <div className="flex items-center gap-2">
          <div className="h-2 min-w-0 flex-1 bg-input" aria-hidden />
          {!showLabel && <QuotaCountdown reset={reset} now={now} />}
        </div>
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

  const title = t(`${label}用量 ${percentage}%`, `${label} usage ${percentage}%`)
  if (!showLabel) {
    // 表格那格 11rem（内容宽 156px）里要排四样东西，按「谁跟谁是一件事」分两行，而不是按大小塞：
    // 第一行是用量本身（摘要 + 百分比，两个都在回答「用掉多少」），第二行是这个窗口的时间维度
    // （进度条 + 还有多久重置）。
    //
    // 分法是按实测宽度定的：摘要最长 `1,633 · 245M · $188.30` 要 130px，百分比 28px、倒计时
    // 44px。倒计时若留在第一行，130 + 8 + 44 = 182px 放不下、摘要必被截断（上一版就是这样）；
    // 换成百分比同行是 166px，xl 下只有最长的那几条会截掉尾巴、2xl（内容宽 172px）一条都不截。
    // 进度条这边反而更宽：整行减去倒计时还有 104px（2xl 120px），比没有倒计时之前的 88px 还长。
    return (
      <Meter value={percentage} max={100} title={title}>
        <div className="flex min-w-0 items-baseline justify-between gap-2">
          <MeterLabel className="sr-only">{label}</MeterLabel>
          <SummaryValue hint={summaryTitle}>{usageSummary}</SummaryValue>
          <MeterValue className="shrink-0 font-medium text-xs leading-none">{() => `${percentage}%`}</MeterValue>
        </div>
        <div className="flex items-center gap-2">
          <MeterTrack className="min-w-0 flex-1">
            <MeterIndicator className={indicatorClass} />
          </MeterTrack>
          <QuotaCountdown reset={reset} now={now} />
        </div>
      </Meter>
    )
  }
  return (
    <Meter value={percentage} max={100} title={title}>
      <div className="flex items-center justify-between gap-2">
        <div className="flex min-w-0 items-baseline gap-1.5">
          <MeterLabel>{label}</MeterLabel>
        </div>
        <MeterValue className="font-medium leading-none">{() => `${percentage}%`}</MeterValue>
      </div>
      <MeterTrack>
        <MeterIndicator className={indicatorClass} />
      </MeterTrack>
      <ListQuotaDetails requests={requests} cost={cost} tokens={tokens} reset={reset} />
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
  // text-xs：表格那格只有 9rem，比 text-sm 多放约五个字符，`3,218 · 486M · $91.62` 刚好放全。
  const className = 'min-w-0 truncate font-medium text-foreground text-xs leading-none tabular-nums'
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

/**
 * 「还有多久重置」——与卡片上那枚倒计时同一个表达（见 credential-card 里的同一段）：`text-2xs`
 * 的次要色、同一套 [formatCountdown] 缩写，精确到分的绝对时刻放在 title 里。两种视图看同一个数
 * 时长得一样，从卡片切到表格不用重新认一遍。位置两边不同：卡片宽，进度条、百分比、倒计时三样
 * 挤一行仍留得下进度条；表格那格只有 11rem，百分比跟摘要走、倒计时跟进度条走，见那段排版注释。
 *
 * 倒计时靠页面那个 30 秒 tick 走（见 useNowSeconds），不会冻住；它受本地时钟偏差影响，只适合
 * 看个大概，要对准时刻的场合仍看 title 里的 [formatFullTime]。
 *
 * 已经重置过的窗口（`reset <= now`）不画：那不是「到期时间」而是一段过去，格子右上角的
 * 「已重置」已经说明了状态，具体时刻在整格的悬浮提示里。上游没报重置时刻的同理留空。
 */
function QuotaCountdown({ reset, now }: { reset: number | null; now: number }) {
  const { t, language } = useI18n()
  if (reset == null || reset <= now) return null
  return (
    <span
      className="shrink-0 whitespace-nowrap text-2xs text-muted-foreground tabular-nums"
      title={t(`${formatFullTime(reset, language)} 重置`, `Resets ${formatFullTime(reset, language)}`)}
    >
      {formatCountdown(reset, now)}
    </span>
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

/**
 * 名额策略的标签与配色。`isDefault`（跟随全局默认）是绝大多数账号的状态，列表里**不画**
 * 这枚徽章：一列里每行都挂着同一个词，读者得逐行确认它没变，而真正要一眼看出来的是
 * 「这个号被单独改过」。默认这一档只留在悬浮提示与设备对话框里——没有丢信息，只是不占位。
 */
function devicePolicyMeta(
  deviceLimit: number,
  language: Language,
): { label: string; variant: BadgeProps['variant']; isDefault: boolean } {
  if (deviceLimit === 0) {
    return { label: localize(language, '跟随默认', 'Default'), variant: 'secondary', isDefault: true }
  }
  if (deviceLimit < 0) {
    return { label: localize(language, '不限', 'Unlimited'), variant: 'outline', isDefault: false }
  }
  return { label: localize(language, '自定义', 'Custom'), variant: 'info', isDefault: false }
}
