import { useState } from 'react'
import {
  CalendarDaysIcon,
  CheckIcon,
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
  isNearLimit,
  liveQuota,
  statusMeta,
  switchTitle,
  tierBadgeVariant,
  useCredentialActions,
} from '@/components/credential-shared'
import { CredentialDevicesDialog } from '@/components/credential-devices-dialog'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Avatar, AvatarFallback } from '@/components/ui/avatar'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
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
  selectable = false,
  selected = false,
  onSelectedChange,
}: {
  cred: Credential
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
  const { u5h, u7d } = liveQuota(cred)
  const nearLimit = isNearLimit(cred)
  const status = statusMeta(cred, nearLimit)
  const initial = cred.label.trim().charAt(0).toUpperCase() || '?'
  const has5h = cred.quota?.rl_5h_utilization != null
  const has7d = cred.quota?.rl_7d_utilization != null
  const effectiveLimit = cred.device_limit_effective > 0 ? cred.device_limit_effective : '∞'
  const devicePolicy = cred.device_limit === 0
    ? { label: '跟随默认', variant: 'secondary' as const }
    : cred.device_limit < 0
      ? { label: '不限', variant: 'outline' as const }
      : { label: '自定义', variant: 'info' as const }

  return (
    <Card
      className={cn(
        '@container/card h-full overflow-hidden',
        selected && 'ring-2 ring-ring ring-offset-2 ring-offset-background',
      )}
    >
      <CardHeader>
        <CardTitle className="min-w-0">
          {editing ? (
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
          ) : (
            <div className="flex min-w-0 items-center gap-3">
              {selectable && (
                <Checkbox
                  checked={selected}
                  onCheckedChange={(checked) => onSelectedChange?.(checked)}
                  aria-label={`选择 ${cred.label}`}
                />
              )}
              <Avatar>
                <AvatarFallback>{initial}</AvatarFallback>
              </Avatar>
              <div className="min-w-0 flex-1">
                <span className="block truncate" title={cred.label}>{cred.label}</span>
                <CardDescription className="mt-1 flex flex-wrap items-center gap-x-2 gap-y-1 font-normal">
                  <span className="font-mono">#{cred.id}</span>
                  <span aria-hidden>·</span>
                  <span
                    className="inline-flex items-center gap-1"
                    title={`添加于 ${formatFullTime(cred.created_at)}`}
                  >
                    <CalendarDaysIcon />
                    添加于 {relativeTime(cred.created_at)}
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
                render={<Button size="icon" variant="ghost" aria-label={`打开 ${cred.label} 菜单`} />}
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

      <CardPanel className="space-y-5">
        <div className="flex flex-wrap items-center gap-2">
          {cred.tier && <Badge variant={tierBadgeVariant(cred.tier)}>{cred.tier}</Badge>}
          <Badge variant="outline" title="调度优先级，数值越小越优先">
            P{cred.priority}
          </Badge>
          {cred.quota && (
            <span className="ml-auto text-xs text-muted-foreground" title={formatFullTime(cred.quota.ts)}>
              额度更新于 {relativeTime(cred.quota.ts)}
            </span>
          )}
        </div>

        {cred.ban_reason && (
          <Alert variant="error">
            <TriangleAlertIcon />
            <AlertTitle>账号认证异常</AlertTitle>
            <AlertDescription>
              <p className="line-clamp-2 break-words" title={cred.ban_reason}>{cred.ban_reason}</p>
            </AlertDescription>
          </Alert>
        )}

        <section aria-label="额度使用" className="space-y-3">
          <div className="flex items-center justify-between gap-3">
            <h3 className="font-medium text-sm">额度使用</h3>
            {!cred.quota && <span className="text-sm text-muted-foreground">暂无数据</span>}
          </div>
          {cred.quota && (has5h || has7d) ? (
            <div className="grid gap-5 @sm/card:grid-cols-2">
              {has5h && (
                <QuotaMeter
                  label="5 小时"
                  util={u5h}
                  reset={cred.quota.rl_5h_reset}
                  cost={cred.quota.cost_5h}
                  snapshotTs={cred.quota.ts}
                />
              )}
              {has7d && (
                <QuotaMeter
                  label="7 天"
                  util={u7d}
                  reset={cred.quota.rl_7d_reset}
                  cost={cred.quota.cost_7d}
                  snapshotTs={cred.quota.ts}
                />
              )}
            </div>
          ) : cred.quota ? (
            <p className="text-sm text-muted-foreground">上游尚未返回额度窗口。</p>
          ) : null}
        </section>
      </CardPanel>

      <CardFooter className="mt-auto flex-wrap justify-between gap-3 border-t bg-muted/32">
        <Button
          type="button"
          variant="ghost"
          onClick={() => setDevicesOpen(true)}
          title="查看已绑定设备"
          aria-haspopup="dialog"
        >
          <SmartphoneIcon />
          <span className="tabular-nums">{cred.device_count}/{effectiveLimit}</span>
          <Badge variant={devicePolicy.variant} size="sm">{devicePolicy.label}</Badge>
        </Button>
        <div className="flex items-center gap-2">
          <Badge variant={status.variant}>{status.label}</Badge>
          {toggle.isPending && <Spinner />}
          <Switch
            checked={!cred.disabled}
            onCheckedChange={(enabled) => toggle.mutate(!enabled)}
            disabled={toggle.isPending}
            title={switchTitle(cred)}
            aria-label={switchTitle(cred)}
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
  )
}

function QuotaMeter({
  label,
  util,
  reset,
  cost,
  snapshotTs,
}: {
  label: string
  util: number | null
  reset: number | null
  cost: number | null
  snapshotTs: number
}) {
  if (util == null) {
    const reason = reset != null
      ? `窗口已在 ${formatFullTime(reset)} 重置，之后没有新请求`
      : '上游未返回该窗口的额度信息'
    return (
      <div className="space-y-1" title={`${reason}。最后一次快照：${formatFullTime(snapshotTs)}`}>
        <p className="font-medium text-sm">{label}</p>
        <p className="text-sm text-muted-foreground">
          {reset != null ? '已重置，暂无新用量' : '暂无数据'}
        </p>
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
    <Meter value={percentage} max={100}>
      <div className="flex items-center justify-between gap-2">
        <MeterLabel>{label}</MeterLabel>
        <MeterValue title={`快照于 ${formatFullTime(snapshotTs)}`}>
          {() => `${percentage}%`}
        </MeterValue>
      </div>
      <MeterTrack>
        <MeterIndicator className={indicatorClass} />
      </MeterTrack>
      <div className="flex items-center justify-between gap-2 text-xs text-muted-foreground">
        <span>花费 <span className="font-medium text-foreground">{formatUsd(cost ?? 0)}</span></span>
        {reset != null && (
          <span title={`${formatFullTime(reset)} 重置`}>{formatClockTime(reset)} 重置</span>
        )}
      </div>
    </Meter>
  )
}
