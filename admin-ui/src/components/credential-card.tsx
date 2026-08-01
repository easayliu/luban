import { useState } from 'react'
import {
  CalendarDaysIcon,
  CheckIcon,
  ChevronDownIcon,
  ClockIcon,
  EllipsisIcon,
  SmartphoneIcon,
  TriangleAlertIcon,
  XIcon,
} from 'lucide-react'
import { type Credential } from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import {
  cn,
  formatClockTime,
  formatFullTime,
  formatUsd,
  relativeTime,
} from '@/lib/utils'
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
  type CredentialStatusMeta,
  type QuotaFreshness,
} from '@/components/credential-shared'
import { CredentialDevicesDialog } from '@/components/credential-devices-dialog'
import { Avatar, AvatarFallback } from '@/components/ui/avatar'
import { Badge } from '@/components/ui/badge'
import { Button, buttonVariants } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'
import {
  Card,
  CardAction,
  CardDescription,
  CardFooter,
  CardHeader,
  CardPanel,
  CardTitle,
} from '@/components/ui/card'
import { Form } from '@/components/ui/form'
import { Input } from '@/components/ui/input'
import { Menu, MenuTrigger } from '@/components/ui/menu'
import {
  Meter,
  MeterIndicator,
  MeterLabel,
  MeterTrack,
  MeterValue,
} from '@/components/ui/meter'
import { Spinner } from '@/components/ui/spinner'
import { Switch } from '@/components/ui/switch'

export function CredentialCard({
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
  const [editing, setEditing] = useState(false)
  const [name, setName] = useState(cred.label)
  const [devicesOpen, setDevicesOpen] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [testing, setTesting] = useState(false)

  const actions = useCredentialActions(cred, () => setEditing(false))
  const { rename, toggle, limit } = actions
  const evaluation = evaluateCredential(cred, now, language)
  const { quota, status } = evaluation
  const initial = cred.label.trim().charAt(0).toUpperCase() || '?'
  const has5h = cred.quota?.rl_5h_utilization != null || cred.quota?.rl_5h_reset != null
  const has7d = cred.quota?.rl_7d_utilization != null || cred.quota?.rl_7d_reset != null
  const effectiveLimit = cred.device_limit_effective > 0 ? cred.device_limit_effective : '∞'
  const devicePolicy = cred.device_limit === 0
    ? { label: t('跟随默认', 'Default'), variant: 'secondary' as const }
    : cred.device_limit < 0
      ? { label: t('不限', 'Unlimited'), variant: 'outline' as const }
      : { label: t('自定义', 'Custom'), variant: 'info' as const }
  const titleId = `credential-card-title-${cred.id}`
  const statusDetailId = `credential-card-status-${cred.id}`
  const added = relativeTime(cred.created_at, now, language)
  const quotaSnapshotTime = cred.quota
    ? formatFullTime(cred.quota.ts, language)
    : t('未知时间', 'unknown time')
  const secondaryOverage = (() => {
    if (quota.overage === 'none') return null
    if (cred.disabled) {
      return {
        label: t('最近快照超额', 'Latest snapshot over limit'),
        variant: 'warning' as const,
        title: t(
          `账号已停用；${quotaSnapshotTime} 的额度快照记录了超额计费，当前不纳入调度风险统计`,
          `The account is disabled; the ${quotaSnapshotTime} quota snapshot recorded overage billing and is excluded from current scheduling-risk totals`,
        ),
      }
    }
    if (quota.overage === 'historical') {
      return {
        label: t('最近曾超额', 'Recently over limit'),
        variant: 'warning' as const,
        title: t(
          '最近的额度快照记录了超额计费，但相关额度窗口已经重置',
          'The latest quota snapshot recorded overage billing, but the related quota windows have reset',
        ),
      }
    }
    if (quota.overage === 'active' && status.kind !== 'overage') {
      return {
        label: t('超额计费', 'Overage billing'),
        variant: 'error' as const,
        title: t(
          `${quotaSnapshotTime} 的额度快照显示上游正以超额计费放行请求`,
          `The ${quotaSnapshotTime} quota snapshot shows the upstream allowing requests through overage billing`,
        ),
      }
    }
    if (quota.overage === 'unknown' && status.kind !== 'overage-unknown') {
      return {
        label: t('超额待确认', 'Overage unconfirmed'),
        variant: 'warning' as const,
        title: t(
          `${quotaSnapshotTime} 的额度快照记录了超额计费，当前状态仍需确认`,
          `The ${quotaSnapshotTime} quota snapshot recorded overage billing; the current state still needs confirmation`,
        ),
      }
    }
    return null
  })()

  return (
    <li className="min-w-0 h-full">
      <Card
        render={<article aria-labelledby={titleId} />}
        className={cn(
          '@container/card h-full overflow-hidden',
          selected && 'ring-2 ring-ring ring-offset-2 ring-offset-background',
        )}
      >
        <CardHeader className="p-4 pb-3 sm:p-5 sm:pb-4">
          <CardTitle className="min-w-0 text-sm leading-snug">
            {editing ? (
              <>
                <h3 id={titleId} className="sr-only">{cred.label}</h3>
                <Form
                  className="flex items-center gap-2"
                  onSubmit={(event) => {
                    event.preventDefault()
                    const nextName = name.trim()
                    if (nextName) rename.mutate(nextName)
                  }}
                >
                  <Input
                    value={name}
                    onChange={(event) => setName(event.target.value)}
                    autoFocus
                    aria-label={t('账号名称', 'Account name')}
                  />
                  <Button
                    type="submit"
                    size="icon"
                    variant="outline"
                    loading={rename.isPending}
                    disabled={!name.trim()}
                    aria-label={t('保存账号名称', 'Save account name')}
                  >
                    <CheckIcon />
                  </Button>
                  <Button
                    type="button"
                    size="icon"
                    variant="ghost"
                    aria-label={t('取消重命名', 'Cancel renaming')}
                    onClick={() => {
                      setEditing(false)
                      setName(cred.label)
                    }}
                  >
                    <XIcon />
                  </Button>
                </Form>
              </>
            ) : (
              <div className="flex min-w-0 items-center gap-3">
                {selectable && (
                  <Checkbox
                    checked={selected}
                    onCheckedChange={(checked) => onSelectedChange?.(checked)}
                    aria-label={t(`选择 ${cred.label}`, `Select ${cred.label}`)}
                  />
                )}
                <Avatar className="hidden @sm/card:flex" aria-hidden="true">
                  <AvatarFallback>{initial}</AvatarFallback>
                </Avatar>
                <div className="min-w-0 flex-1">
                  <h3
                    id={titleId}
                    className="line-clamp-2 break-all leading-snug @sm/card:line-clamp-1 @sm/card:truncate @sm/card:break-normal"
                    title={cred.label}
                  >
                    {cred.label}
                  </h3>
                  <CardDescription className="mt-1 flex min-w-0 flex-wrap items-center gap-x-1.5 gap-y-0.5 text-xs font-normal">
                    <span className="tabular-nums">#{cred.id}</span>
                    <span aria-hidden="true">·</span>
                    <span
                      className="inline-flex min-w-0 items-center gap-1"
                      title={t(
                        `添加于 ${formatFullTime(cred.created_at, language)}`,
                        `Added ${formatFullTime(cred.created_at, language)}`,
                      )}
                    >
                      <CalendarDaysIcon className="size-3.5 shrink-0" />
                      <span>{t(`添加于 ${added}`, `Added ${added}`)}</span>
                    </span>
                  </CardDescription>
                </div>
              </div>
            )}
          </CardTitle>

          {!editing && (
            <CardAction>
              <Menu modal={false}>
                <MenuTrigger
                  className={buttonVariants({ size: 'icon', variant: 'ghost' })}
                  aria-label={t(`打开 ${cred.label} 菜单`, `Open menu for ${cred.label}`)}
                >
                  <EllipsisIcon />
                </MenuTrigger>
                <CredentialMenuContent
                  cred={cred}
                  actions={actions}
                  onRename={() => {
                    setName(cred.label)
                    setEditing(true)
                  }}
                  onDeviceLimit={() => setDevicesOpen(true)}
                  onTest={() => setTesting(true)}
                  onRequestDelete={() => setConfirmDelete(true)}
                />
              </Menu>
            </CardAction>
          )}
        </CardHeader>

        <CardPanel className="space-y-4 px-4 pb-4 sm:px-5 sm:pb-5">
          <div className="flex flex-wrap items-center gap-2">
            <Badge
              variant={status.variant}
              aria-label={t(`${cred.label}：${status.label}`, `${cred.label}: ${status.label}`)}
              aria-describedby={status.attention ? statusDetailId : undefined}
            >
              {status.label}
            </Badge>
            {cred.tier && <Badge variant={tierBadgeVariant(cred.tier)}>{cred.tier}</Badge>}
            <Badge variant="outline" title={t('调度优先级，数值越小越优先', 'Scheduling priority; lower values are scheduled first')}>
              P{cred.priority}
            </Badge>
          </div>

          {status.attention && <AttentionSummary id={statusDetailId} status={status} />}

          <section aria-label={t(`${cred.label} 的额度使用`, `Quota usage for ${cred.label}`)} className="space-y-3">
            <div className="flex flex-wrap items-start justify-between gap-x-3 gap-y-2">
              <div className="flex flex-wrap items-center gap-2">
                <h4 className="font-medium text-sm">{t('额度使用', 'Quota usage')}</h4>
                {secondaryOverage && (
                  <Badge
                    variant={secondaryOverage.variant}
                    size="sm"
                    title={secondaryOverage.title}
                  >
                    {secondaryOverage.label}
                  </Badge>
                )}
              </div>
              {cred.quota ? (
                <span
                  className="inline-flex items-center gap-1 text-xs text-muted-foreground"
                  title={formatFullTime(cred.quota.ts, language)}
                >
                  <ClockIcon className="size-3.5" />
                  {t(
                    `更新于 ${relativeTime(cred.quota.ts, now, language)}`,
                    `Updated ${relativeTime(cred.quota.ts, now, language)}`,
                  )}
                </span>
              ) : (
                <span className="text-sm text-muted-foreground">{t('暂无数据', 'No data')}</span>
              )}
            </div>
            {cred.quota && (has5h || has7d) ? (
              <div className="grid gap-4 @sm/card:grid-cols-2 @sm/card:gap-5">
                {has5h && (
                  <QuotaMeter
                    credentialLabel={cred.label}
                    label={t('5 小时', '5 hours')}
                    util={quota.h5.utilization}
                    freshness={quota.h5.freshness}
                    reset={cred.quota.rl_5h_reset}
                    cost={cred.quota.cost_5h}
                    requests={cred.quota.requests_5h}
                    snapshotTs={cred.quota.ts}
                  />
                )}
                {has7d && (
                  <QuotaMeter
                    credentialLabel={cred.label}
                    label={t('7 天', '7 days')}
                    util={quota.d7.utilization}
                    freshness={quota.d7.freshness}
                    reset={cred.quota.rl_7d_reset}
                    cost={cred.quota.cost_7d}
                    requests={cred.quota.requests_7d}
                    snapshotTs={cred.quota.ts}
                  />
                )}
              </div>
            ) : cred.quota ? (
              <p className="text-sm text-muted-foreground">{t('上游尚未返回额度窗口。', 'The upstream has not returned quota windows yet.')}</p>
            ) : null}
          </section>
        </CardPanel>

        <CardFooter className="mt-auto flex-wrap justify-between gap-3 border-t bg-muted/32 px-4 py-3 sm:px-5 sm:py-4">
          <div className="flex min-w-0 flex-1 flex-wrap items-center gap-x-4 gap-y-2">
            <Button
              type="button"
              variant="ghost"
              onClick={() => setDevicesOpen(true)}
              title={t('查看已绑定设备', 'View bound devices')}
              aria-label={t(`查看 ${cred.label} 的已绑定设备`, `View bound devices for ${cred.label}`)}
              aria-haspopup="dialog"
            >
              <SmartphoneIcon />
              <span className="tabular-nums">{cred.device_count}/{effectiveLimit}</span>
              <Badge variant={devicePolicy.variant} size="sm">{devicePolicy.label}</Badge>
            </Button>
            <span className="whitespace-nowrap text-sm" title={t('累计等价 API 费用', 'Cumulative equivalent API cost')}>
              <span className="text-muted-foreground">{t('累计 ', 'Total ')}</span>
              <span className="font-medium tabular-nums">{formatUsd(cred.cost_total)}</span>
            </span>
          </div>
          <div className="flex items-center gap-2">
            {toggle.isPending && <Spinner />}
            <Switch
              checked={!cred.disabled}
              onCheckedChange={(enabled) => toggle.mutate(!enabled)}
              disabled={toggle.isPending}
              title={switchTitle(cred, language)}
              aria-label={`${cred.label}: ${switchTitle(cred, language)}`}
            />
          </div>
        </CardFooter>

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
    </li>
  )
}

function AttentionSummary({ id, status }: { id: string; status: CredentialStatusMeta }) {
  const { t } = useI18n()
  const [expanded, setExpanded] = useState(false)
  const expandable = status.detail.length > 52
  const isError = status.variant === 'error' || status.variant === 'destructive'

  return (
    <div
      id={id}
      className={cn(
        'flex items-start gap-2 rounded-lg px-3 py-2.5 text-sm',
        isError
          ? 'bg-destructive/8 text-destructive-foreground dark:bg-destructive/16'
          : 'bg-warning/8 text-warning-foreground dark:bg-warning/16',
      )}
      aria-live="polite"
      aria-atomic="true"
    >
      <TriangleAlertIcon className="mt-0.5 size-4 shrink-0" aria-hidden="true" />
      <div className="min-w-0 flex-1">
        <p className={cn('break-words leading-5', expandable && !expanded && 'line-clamp-2')}>
          {status.detail}
        </p>
        {expandable && (
          <Button
            type="button"
            size="xs"
            variant="ghost"
            className="mt-1 -ml-2"
            aria-expanded={expanded}
            onClick={() => setExpanded((current) => !current)}
          >
            {expanded ? t('收起说明', 'Show less') : t('查看完整说明', 'View full details')}
            <ChevronDownIcon className={cn('transition-transform', expanded && 'rotate-180')} />
          </Button>
        )}
      </div>
    </div>
  )
}

function QuotaMeter({
  credentialLabel,
  label,
  util,
  freshness,
  reset,
  cost,
  requests,
  snapshotTs,
}: {
  credentialLabel: string
  label: string
  util: number | null
  freshness: QuotaFreshness
  reset: number | null
  cost: number | null
  requests: number | null
  snapshotTs: number
}) {
  const { t, language, locale } = useI18n()
  if (util == null) {
    const expired = freshness === 'expired'
    const reason = expired && reset != null
      ? t(
          `窗口已在 ${formatFullTime(reset, language)} 重置，之后没有新请求`,
          `The window reset at ${formatFullTime(reset, language)} and has no newer requests`,
        )
      : t('上游未返回该窗口的额度信息', 'The upstream did not return quota data for this window')
    return (
      <div
        className="space-y-1"
        title={t(
          `${reason}。最后一次快照：${formatFullTime(snapshotTs, language)}`,
          `${reason}. Latest snapshot: ${formatFullTime(snapshotTs, language)}`,
        )}
      >
        <p className="font-medium text-sm">{label}</p>
        <p className="text-sm text-muted-foreground">
          {expired ? t('已重置，暂无新用量', 'Reset; no new usage') : t('暂无数据', 'No data')}
        </p>
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
    <Meter value={percentage} max={100}>
      <div className="flex items-center justify-between gap-2">
        <MeterLabel>
          <span className="sr-only">{t(`${credentialLabel} 的 `, `${credentialLabel} `)}</span>
          {label}
          <span className="sr-only">{t('额度使用率', 'quota usage')}</span>
        </MeterLabel>
        <MeterValue title={t(`快照于 ${formatFullTime(snapshotTs, language)}`, `Snapshot at ${formatFullTime(snapshotTs, language)}`)}>
          {() => `${percentage}%`}
        </MeterValue>
      </div>
      <MeterTrack>
        <MeterIndicator className={indicatorClass} />
      </MeterTrack>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground">
        <span>
          <span className="font-medium text-foreground tabular-nums">
            {requests == null ? '—' : requests.toLocaleString(locale)}
          </span>{' '}{t('次请求', requests === 1 ? 'request' : 'requests')}
          <span className="mx-1.5" aria-hidden="true">·</span>
          <span className="font-medium text-foreground">{cost == null ? '—' : formatUsd(cost)}</span>
        </span>
        {reset != null && (
          <span className="ml-auto" title={t(`${formatFullTime(reset, language)} 重置`, `Resets ${formatFullTime(reset, language)}`)}>
            {t(`${formatClockTime(reset, language)} 重置`, `Resets ${formatClockTime(reset, language)}`)}
          </span>
        )}
      </div>
    </Meter>
  )
}
