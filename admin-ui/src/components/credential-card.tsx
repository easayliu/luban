import { useState } from 'react'
import {
  CheckIcon,
  ChevronDownIcon,
  ClockIcon,
  EllipsisIcon,
  SmartphoneIcon,
  TriangleAlertIcon,
  XIcon,
} from 'lucide-react'
import { type Credential } from '@/api/credentials'
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
  const [editing, setEditing] = useState(false)
  const [name, setName] = useState(cred.label)
  const [devicesOpen, setDevicesOpen] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [testing, setTesting] = useState(false)

  const actions = useCredentialActions(cred, () => setEditing(false))
  const { rename, toggle, limit } = actions
  const evaluation = evaluateCredential(cred, now)
  const { quota, status } = evaluation
  const initial = cred.label.trim().charAt(0).toUpperCase() || '?'
  const has5h = cred.quota?.rl_5h_utilization != null || cred.quota?.rl_5h_reset != null
  const has7d = cred.quota?.rl_7d_utilization != null || cred.quota?.rl_7d_reset != null
  const effectiveLimit = cred.device_limit_effective > 0 ? cred.device_limit_effective : '∞'
  const devicePolicy = cred.device_limit === 0
    ? { label: '跟随默认', variant: 'secondary' as const }
    : cred.device_limit < 0
      ? { label: '不限', variant: 'outline' as const }
      : { label: '自定义', variant: 'info' as const }
  const titleId = `credential-card-title-${cred.id}`
  const statusDetailId = `credential-card-status-${cred.id}`
  const lastUsed = cred.last_used == null
    ? { label: '尚未使用', title: '该账号尚未转发过请求' }
    : {
        label: `使用于 ${relativeTime(cred.last_used, now)}`,
        title: `最近使用于 ${formatFullTime(cred.last_used)}`,
      }
  const quotaSnapshotTime = cred.quota ? formatFullTime(cred.quota.ts) : '未知时间'
  const secondaryOverage = (() => {
    if (quota.overage === 'none') return null
    if (cred.disabled) {
      return {
        label: '最近快照超额',
        variant: 'warning' as const,
        title: `账号已停用；${quotaSnapshotTime} 的额度快照记录了超额计费，当前不纳入调度风险统计`,
      }
    }
    if (quota.overage === 'historical') {
      return {
        label: '最近曾超额',
        variant: 'warning' as const,
        title: '最近的额度快照记录了超额计费，但相关额度窗口已经重置',
      }
    }
    if (quota.overage === 'active' && status.kind !== 'overage') {
      return {
        label: '超额计费',
        variant: 'error' as const,
        title: `${quotaSnapshotTime} 的额度快照显示上游正以超额计费放行请求`,
      }
    }
    if (quota.overage === 'unknown' && status.kind !== 'overage-unknown') {
      return {
        label: '超额待确认',
        variant: 'warning' as const,
        title: `${quotaSnapshotTime} 的额度快照记录了超额计费，当前状态仍需确认`,
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
                    aria-label="账号名称"
                  />
                  <Button
                    type="submit"
                    size="icon"
                    variant="outline"
                    loading={rename.isPending}
                    disabled={!name.trim()}
                    aria-label="保存账号名称"
                  >
                    <CheckIcon />
                  </Button>
                  <Button
                    type="button"
                    size="icon"
                    variant="ghost"
                    aria-label="取消重命名"
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
                    aria-label={`选择 ${cred.label}`}
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
                  <CardDescription className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 text-xs font-normal">
                    <span className="tabular-nums">#{cred.id}</span>
                    <span aria-hidden>·</span>
                    <span className="inline-flex items-center gap-1" title={lastUsed.title}>
                      <ClockIcon className="size-3.5" />
                      {lastUsed.label}
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
                  aria-label={`打开 ${cred.label} 菜单`}
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
              aria-label={`${cred.label}：${status.label}`}
              aria-describedby={status.attention ? statusDetailId : undefined}
            >
              {status.label}
            </Badge>
            {cred.tier && <Badge variant={tierBadgeVariant(cred.tier)}>{cred.tier}</Badge>}
            <Badge variant="outline" title="调度优先级，数值越小越优先">
              P{cred.priority}
            </Badge>
          </div>

          {status.attention && <AttentionSummary id={statusDetailId} status={status} />}

          <section aria-label={`${cred.label} 的额度使用`} className="space-y-3">
            <div className="flex flex-wrap items-start justify-between gap-x-3 gap-y-2">
              <div className="flex flex-wrap items-center gap-2">
                <h4 className="font-medium text-sm">额度使用</h4>
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
                  title={formatFullTime(cred.quota.ts)}
                >
                  <ClockIcon className="size-3.5" />
                  更新于 {relativeTime(cred.quota.ts, now)}
                </span>
              ) : (
                <span className="text-sm text-muted-foreground">暂无数据</span>
              )}
            </div>
            {cred.quota && (has5h || has7d) ? (
              <div className="grid gap-4 @sm/card:grid-cols-2 @sm/card:gap-5">
                {has5h && (
                  <QuotaMeter
                    credentialLabel={cred.label}
                    label="5 小时"
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
                    label="7 天"
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
              <p className="text-sm text-muted-foreground">上游尚未返回额度窗口。</p>
            ) : null}
          </section>
        </CardPanel>

        <CardFooter className="mt-auto flex-wrap justify-between gap-3 border-t bg-muted/32 px-4 py-3 sm:px-5 sm:py-4">
          <div className="flex min-w-0 flex-1 flex-wrap items-center gap-x-4 gap-y-2">
            <Button
              type="button"
              variant="ghost"
              onClick={() => setDevicesOpen(true)}
              title="查看已绑定设备"
              aria-label={`查看 ${cred.label} 的已绑定设备`}
              aria-haspopup="dialog"
            >
              <SmartphoneIcon />
              <span className="tabular-nums">{cred.device_count}/{effectiveLimit}</span>
              <Badge variant={devicePolicy.variant} size="sm">{devicePolicy.label}</Badge>
            </Button>
            <span className="whitespace-nowrap text-sm" title="累计等价 API 费用">
              <span className="text-muted-foreground">累计 </span>
              <span className="font-medium tabular-nums">{formatUsd(cred.cost_total)}</span>
            </span>
          </div>
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
            {expanded ? '收起说明' : '查看完整说明'}
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
  if (util == null) {
    const expired = freshness === 'expired'
    const reason = expired && reset != null
      ? `窗口已在 ${formatFullTime(reset)} 重置，之后没有新请求`
      : '上游未返回该窗口的额度信息'
    return (
      <div className="space-y-1" title={`${reason}。最后一次快照：${formatFullTime(snapshotTs)}`}>
        <p className="font-medium text-sm">{label}</p>
        <p className="text-sm text-muted-foreground">
          {expired ? '已重置，暂无新用量' : '暂无数据'}
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
          <span className="sr-only">{credentialLabel} 的 </span>
          {label}
          <span className="sr-only">额度使用率</span>
        </MeterLabel>
        <MeterValue title={`快照于 ${formatFullTime(snapshotTs)}`}>
          {() => `${percentage}%`}
        </MeterValue>
      </div>
      <MeterTrack>
        <MeterIndicator className={indicatorClass} />
      </MeterTrack>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground">
        <span>
          <span className="font-medium text-foreground tabular-nums">
            {requests == null ? '—' : requests.toLocaleString('zh-CN')}
          </span>{' '}次请求
          <span className="mx-1.5" aria-hidden="true">·</span>
          <span className="font-medium text-foreground">{cost == null ? '—' : formatUsd(cost)}</span>
        </span>
        {reset != null && (
          <span className="ml-auto" title={`${formatFullTime(reset)} 重置`}>
            {formatClockTime(reset)} 重置
          </span>
        )}
      </div>
    </Meter>
  )
}
