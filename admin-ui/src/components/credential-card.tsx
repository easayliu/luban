import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import {
  ArrowPathIcon, PencilIcon, CheckIcon, XMarkIcon, EllipsisHorizontalIcon,
  DevicePhoneMobileIcon, ChevronDownIcon, ClockIcon, WalletIcon, ExclamationTriangleIcon,
} from '@heroicons/react/24/outline'
import { listCredentialDevices, unbindCredentialDevice, type Credential } from '@/api/credentials'
import {
  cn, copyText, extractError, formatClockTime, formatFullTime, formatUsd, relativeTime,
} from '@/lib/utils'
import {
  ConnectivityTestDialog, credentialExpiryMeta, type CredentialActions, CredentialMenuContent,
  DeleteCredentialDialog, inputToLimit, isAbnormal, isNearLimit, limitToInput, liveQuota,
  statusMeta, switchTitle, tierBadgeClass, useCredentialActions,
} from '@/components/credential-shared'
import { Card } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { DropdownMenu, DropdownMenuTrigger } from '@/components/ui/dropdown-menu'
import {
  Dialog, DialogBody, DialogContent, DialogDescription, DialogHeader, DialogTitle,
} from '@/components/ui/dialog'

export function CredentialCard({
  cred, selectable = false, selected = false, onSelectedChange,
}: {
  cred: Credential
  /** 批量模式：卡片显示勾选框。 */
  selectable?: boolean
  selected?: boolean
  onSelectedChange?: (next: boolean) => void
}) {
  const [editing, setEditing] = useState(false)
  const [name, setName] = useState(cred.label)
  const [editingLimit, setEditingLimit] = useState(false)
  const [limitVal, setLimitVal] = useState(limitToInput(cred.device_limit))
  // 设备明细放到弹窗里，避免多账号网格被单张展开卡片撑出大块空洞。
  const [devicesOpen, setDevicesOpen] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [testing, setTesting] = useState(false)

  const actions = useCredentialActions(
    cred,
    () => setEditing(false),
    () => setEditingLimit(false),
  )
  const { rename, toggle, limit } = actions

  // 额度接近上限（5h / 7d 任一 ≥90%）：用于状态标签与卡片描边。
  // 用 liveQuota 而非原始快照——窗口已重置的百分比是上个周期的，不该再触发告警。
  const { u5h, u7d } = liveQuota(cred)
  const nearLimit = isNearLimit(cred)
  const initial = cred.label.trim().charAt(0).toUpperCase() || '?'
  // 窗口已重置的仍然渲染额度条（由 QuotaBar 显示成「已重置」），只是不参与告警。
  const has5h = cred.quota?.rl_5h_utilization != null
  const has7d = cred.quota?.rl_7d_utilization != null
  const expiry = credentialExpiryMeta(cred)
  const status = statusMeta(cred, nearLimit)
  const abnormal = isAbnormal(cred)
  const effectiveDeviceLimit = cred.device_limit_effective > 0 ? cred.device_limit_effective : '∞'
  const deviceFull = cred.device_limit_effective > 0
    && cred.device_count >= cred.device_limit_effective

  return (
    <Card
      className={cn(
        '@container/card group/card relative flex h-full flex-col overflow-hidden rounded-xl border-border/80 bg-card p-0 shadow-card transition-[border-color,box-shadow,background-color]',
        'before:absolute before:inset-y-0 before:left-0 before:w-[3px] before:transition-colors',
        'hover:border-foreground/20 hover:shadow-panel',
        cred.disabled && 'bg-muted/25',
        selected && 'border-primary/50 ring-1 ring-primary/15',
        // 左侧状态轨：一眼分诊。正常态透明，异常态着色。
        status.rail,
      )}
    >
      {/* 第一阅读层：账号身份与健康状态。批量模式下整个头像区域都可点。 */}
      <div className="p-3.5 pl-[1.125rem] sm:p-4 sm:pl-5">
        <div className="grid grid-cols-[2.5rem_minmax(0,1fr)_2.5rem] items-start gap-2.5 sm:gap-3">
          {selectable ? (
            <label
              className={cn(
                'grid size-10 cursor-pointer place-items-center rounded-full border border-border',
                selected && 'bg-primary/5',
              )}
            >
              <input
                type="checkbox"
                checked={selected}
                onChange={(e) => onSelectedChange?.(e.target.checked)}
                className="size-4 rounded border-border accent-primary"
                aria-label={`选择 ${cred.label}`}
              />
            </label>
          ) : (
            <div
              className={cn(
                'grid size-10 place-items-center rounded-full border border-border text-xs font-semibold',
                cred.disabled ? 'bg-muted/70 text-muted-foreground/70' : 'bg-muted text-foreground',
              )}
              aria-hidden
            >
              {initial}
            </div>
          )}

          {editing ? (
            <form
              className="col-span-2 grid min-w-0 grid-cols-[minmax(0,1fr)_2.5rem_2.5rem] items-center gap-1"
              onSubmit={(e) => { e.preventDefault(); rename.mutate(name.trim()) }}
            >
              <Input
                value={name}
                onChange={(e) => setName(e.target.value)}
                autoFocus
                className="h-10 min-w-0 px-2.5 text-sm"
                aria-label="账号名称"
              />
              <Button
                type="submit"
                size="icon"
                variant="ghost"
                className="size-10"
                disabled={rename.isPending}
                aria-label="保存账号名称"
              >
                {rename.isPending ? <ArrowPathIcon className="animate-spin" /> : <CheckIcon />}
              </Button>
              <Button
                type="button"
                size="icon"
                variant="ghost"
                className="size-10"
                aria-label="取消重命名"
                onClick={() => { setEditing(false); setName(cred.label) }}
              >
                <XMarkIcon />
              </Button>
            </form>
          ) : (
            <>
              <div className="min-w-0">
                <div
                  className="line-clamp-2 break-words text-base font-semibold leading-5 tracking-tight @sm/card:line-clamp-1"
                  title={cred.label}
                >
                  {cred.label}
                </div>

                <div className="mt-2 flex flex-wrap items-center gap-x-2 gap-y-1.5 text-xs text-muted-foreground">
                  {/* 配色与 statusMeta 同源：access_token 的有效期不参与判色，token 到点会在
                      下次使用时自动刷新，而 `expires_in <= 300` 正是后端的刷新窗口。 */}
                  <Badge
                    variant={
                      cred.ban_reason
                        ? 'bad'
                        : cred.disabled
                          ? 'outline'
                          : cred.rate_limited_secs > 0 || nearLimit
                            ? 'warn'
                            : 'ok'
                    }
                    className={cn('h-5 px-1.5 py-0 text-xs', cred.disabled && 'text-muted-foreground')}
                  >
                    {status.label}
                  </Badge>
                  <span className="font-mono tnum" title={`账号 ID：${cred.id}`}>
                    #{cred.id}
                  </span>
                  {cred.tier && (
                    <Badge
                      variant="outline"
                      className={cn('h-5 max-w-full truncate px-1.5 py-0 text-xs', tierBadgeClass(cred.tier))}
                      title={cred.tier}
                    >
                      {cred.tier}
                    </Badge>
                  )}
                  <span className="font-mono tnum" title="调度优先级，数值小者优先">
                    P{cred.priority}
                  </span>
                </div>
              </div>

              {/* 菜单里会打开 Dialog。禁用菜单自身的 modal 指针锁，避免它与 Dialog 的
                  pointer-events 恢复互相覆盖，导致关闭测试弹窗后整页无法再次点击。 */}
              <DropdownMenu modal={false}>
                <DropdownMenuTrigger asChild>
                  <Button size="icon" variant="ghost" className="-mr-2 -mt-2 size-10 text-muted-foreground" aria-label={`打开 ${cred.label} 菜单`}>
                    <EllipsisHorizontalIcon />
                  </Button>
                </DropdownMenuTrigger>
                <CredentialMenuContent
                  cred={cred}
                  actions={actions}
                  onRename={() => setEditing(true)}
                  onDeviceLimit={() => { setLimitVal(limitToInput(cred.device_limit)); setEditingLimit(true) }}
                  onTest={() => setTesting(true)}
                  onRequestDelete={() => setConfirmDelete(true)}
                />
              </DropdownMenu>
            </>
          )}
        </div>
        {cred.ban_reason && !editing && (
          <div
            className="mt-3 flex min-h-9 items-start gap-2 rounded-md border border-bad/20 bg-bad-soft/80 px-2.5 py-2 text-bad"
            role="alert"
          >
            <ExclamationTriangleIcon className="mt-0.5 size-3.5 shrink-0" aria-hidden />
            <p
              className="line-clamp-2 min-w-0 flex-1 break-words text-xs leading-4 @sm/card:line-clamp-1"
              title={cred.ban_reason}
            >
              {cred.ban_reason}
            </p>
          </div>
        )}
      </div>

      {/* 额度是巡检主信息：显式标出快照新鲜度，窄卡上下堆叠，宽卡再并排。 */}
      <div className="mx-3 mb-2 flex items-center justify-between gap-3 sm:mx-4">
        <h3 className="text-xs font-semibold text-foreground">额度使用</h3>
        {cred.quota && (
          <span className="text-2xs text-muted-foreground" title={formatFullTime(cred.quota.ts)}>
            更新于 {relativeTime(cred.quota.ts)}
          </span>
        )}
      </div>
      {cred.quota && (has5h || has7d) && (
        <div className={cn('mx-3 grid overflow-hidden rounded-lg border border-border/70 bg-muted/20 sm:mx-4', has5h && has7d && '@sm/card:grid-cols-2')}>
          {has5h && (
            <QuotaBar
              label="5 小时"
              util={u5h}
              reset={cred.quota.rl_5h_reset}
              cost={cred.quota.cost_5h}
              snapshotTs={cred.quota.ts}
              className={cn(has7d && 'border-b border-border/70 @sm/card:border-b-0 @sm/card:border-r')}
            />
          )}
          {has7d && (
            <QuotaBar
              label="7 天"
              util={u7d}
              reset={cred.quota.rl_7d_reset}
              cost={cred.quota.cost_7d}
              snapshotTs={cred.quota.ts}
            />
          )}
        </div>
      )}
      {(!cred.quota || (!has5h && !has7d)) && (
        <div className="mx-3 grid min-h-28 place-items-center rounded-lg border border-border/70 bg-muted/20 px-3 text-xs text-muted-foreground sm:mx-4">
          暂无额度数据
        </div>
      )}

      {/* 次级事实区：窄卡先突出设备容量，再把最近使用与花费并排；宽卡恢复三列。 */}
      {editingLimit ? (
        <div className="mx-3 mt-4 rounded-lg border border-border/70 bg-muted/25 p-3 sm:mx-4">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <div>
              <div className="text-sm font-medium">设备上限</div>
              <p className="mt-0.5 text-xs text-muted-foreground">留空 = 默认，0 = 不限</p>
            </div>
            <form className="flex w-full items-center justify-end gap-1 sm:w-auto" onSubmit={(e) => { e.preventDefault(); limit.mutate(inputToLimit(limitVal)) }}>
              <Input
                type="number"
                min={0}
                value={limitVal}
                onChange={(e) => setLimitVal(e.target.value)}
                autoFocus
                placeholder="默认"
                className="h-9 w-20 px-2 text-xs"
                aria-label="设备上限"
              />
              <Button type="submit" size="icon" variant="ghost" className="size-9" disabled={limit.isPending} aria-label="保存设备上限">
                {limit.isPending ? <ArrowPathIcon className="size-3 animate-spin" /> : <CheckIcon className="size-3" />}
              </Button>
              <Button type="button" size="icon" variant="ghost" className="size-9" aria-label="取消修改设备上限"
                onClick={() => { setEditingLimit(false); setLimitVal(limitToInput(cred.device_limit)) }}>
                <XMarkIcon className="size-3" />
              </Button>
            </form>
          </div>
        </div>
      ) : (
        <div className="mx-3 mt-4 grid min-h-16 grid-cols-2 border-y border-border/70 sm:mx-4 @sm/card:grid-cols-3">
          <Button
            type="button"
            variant="ghost"
            onClick={() => setDevicesOpen(true)}
            className="col-span-2 h-auto min-h-16 min-w-0 justify-between whitespace-normal rounded-none border-b border-border/70 px-2.5 py-2.5 text-left hover:bg-muted/25 @sm/card:col-span-1 @sm/card:border-b-0 @sm/card:border-r"
            title="查看已绑定设备"
            aria-haspopup="dialog"
            aria-label={`查看已绑定设备，当前 ${cred.device_count} 台`}
          >
            <span className="min-w-0">
              <span className="flex items-center gap-1.5 text-xs font-normal text-muted-foreground">
                <DevicePhoneMobileIcon className="size-3.5" />
                设备
                {cred.device_limit === 0 && <span className="rounded bg-muted px-1 text-2xs">默认</span>}
              </span>
              <span
                className={cn(
                  'mt-1 block truncate text-sm font-semibold tnum text-foreground',
                  deviceFull && 'text-warn',
                )}
              >
                {cred.device_count}/{effectiveDeviceLimit}
              </span>
            </span>
            <ChevronDownIcon className="-rotate-90 size-3.5 text-muted-foreground" />
          </Button>
          <div className="min-w-0 border-r border-border/70 px-2.5 py-2.5" title="最近一次转发使用">
            <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
              <ClockIcon className="size-3.5" />
              最近使用
            </div>
            <div className="mt-1 truncate text-sm font-semibold tnum text-foreground">
              {cred.last_used != null ? relativeTime(cred.last_used) : '未使用'}
            </div>
          </div>
          <div className="min-w-0 px-2.5 py-2.5" title="该账号历史累计等价 API 费用（按官方定价估算）">
            <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
              <WalletIcon className="size-3.5" />
              累计花费
            </div>
            <div className="mt-1 truncate text-sm font-semibold tnum text-foreground">{formatUsd(cred.cost_total)}</div>
          </div>
        </div>
      )}

      {/* 最少留一档间距，同时吃掉同一网格行的剩余高度，让异常卡与普通卡的底栏齐平。 */}
      <div className="min-h-4 flex-1" aria-hidden />

      {/* 真实有效期与账号开关独占底栏，不再把封禁/冷却误写成“有效期”。 */}
      <div className="flex min-h-14 items-center justify-between gap-3 border-t border-border/70 bg-muted/10 px-3 py-3 sm:px-4">
        <div className="min-w-0">
          <div className="text-xs text-muted-foreground">凭证有效期</div>
          <div className={cn('mt-1 flex min-w-0 items-center gap-1.5 text-sm font-medium', expiry.className)} title={expiry.title}>
            <ClockIcon className="size-3.5 shrink-0" />
            <span className="truncate">{expiry.text}</span>
          </div>
        </div>
        <div className="flex shrink-0 items-center gap-3">
          <span className="text-right">
            <span className="block text-xs text-muted-foreground">账号开关</span>
            <span className={cn('mt-1 block text-sm font-medium', cred.disabled ? 'text-muted-foreground' : 'text-foreground')}>
              {cred.disabled ? '已关闭' : '已开启'}
            </span>
          </span>
          <span className="relative inline-flex items-center">
            <Switch
              variant="success"
              checked={!cred.disabled}
              onCheckedChange={(on) => toggle.mutate(!on)}
              disabled={toggle.isPending}
              title={switchTitle(cred)}
              aria-label={switchTitle(cred)}
              className={cn(
                "relative after:absolute after:-inset-x-1 after:-inset-y-3 after:content-['']",
                toggle.isPending && 'opacity-0',
                abnormal && 'data-[state=checked]:bg-muted-foreground/50',
              )}
            />
            {toggle.isPending && (
              <ArrowPathIcon className="absolute left-1/2 top-1/2 size-4 -translate-x-1/2 -translate-y-1/2 animate-spin text-muted-foreground" />
            )}
          </span>
        </div>
      </div>

      <DeviceListDialog
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

/** 设备明细独立成弹窗，保持多账号卡片网格的高度与阅读节奏稳定。 */
function DeviceListDialog({
  cred, open, onOpenChange, limit,
}: {
  cred: Credential
  open: boolean
  onOpenChange: (open: boolean) => void
  limit: CredentialActions['limit']
}) {
  const [editingLimit, setEditingLimit] = useState(false)
  const [limitVal, setLimitVal] = useState(limitToInput(cred.device_limit))
  const effectiveLimit = cred.device_limit_effective > 0 ? cred.device_limit_effective : '∞'

  const handleOpenChange = (next: boolean) => {
    if (!next) {
      setEditingLimit(false)
      setLimitVal(limitToInput(cred.device_limit))
    }
    onOpenChange(next)
  }

  const startEditingLimit = () => {
    setLimitVal(limitToInput(cred.device_limit))
    setEditingLimit(true)
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent className="max-w-lg">
        <DialogHeader>
          <DialogTitle>
            <DevicePhoneMobileIcon className="size-4 text-muted-foreground" />
            已绑定设备
          </DialogTitle>
          <DialogDescription className="truncate" title={cred.label}>
            {cred.label} · 当前 {cred.device_count}/{effectiveLimit} 台
          </DialogDescription>
        </DialogHeader>
        <DialogBody className="p-0 sm:p-0">
          <div className="border-b border-border/70 bg-muted/20 px-4 py-3 sm:px-5">
            {editingLimit ? (
              <form
                className="flex flex-wrap items-center gap-2"
                onSubmit={(event) => {
                  event.preventDefault()
                  limit.mutate(inputToLimit(limitVal), {
                    onSuccess: () => setEditingLimit(false),
                  })
                }}
              >
                <label htmlFor={`dialog-device-limit-${cred.id}`} className="mr-auto min-w-32">
                  <span className="block text-sm font-medium">设备上限</span>
                  <span className="mt-0.5 block text-xs text-muted-foreground">留空 = 默认，0 = 不限</span>
                </label>
                <Input
                  id={`dialog-device-limit-${cred.id}`}
                  type="number"
                  min={0}
                  value={limitVal}
                  onChange={(event) => setLimitVal(event.target.value)}
                  autoFocus
                  placeholder="默认"
                  className="h-10 w-24 px-2 text-sm sm:h-9"
                />
                <Button
                  type="submit"
                  size="icon"
                  variant="ghost"
                  className="size-10 sm:size-9"
                  disabled={limit.isPending}
                  aria-label="保存设备上限"
                >
                  {limit.isPending
                    ? <ArrowPathIcon className="size-4 animate-spin" />
                    : <CheckIcon className="size-4" />}
                </Button>
                <Button
                  type="button"
                  size="icon"
                  variant="ghost"
                  className="size-10 sm:size-9"
                  disabled={limit.isPending}
                  aria-label="取消修改设备上限"
                  onClick={() => {
                    setEditingLimit(false)
                    setLimitVal(limitToInput(cred.device_limit))
                  }}
                >
                  <XMarkIcon className="size-4" />
                </Button>
              </form>
            ) : (
              <div className="flex items-center justify-between gap-3">
                <div>
                  <div className="text-xs text-muted-foreground">设备容量</div>
                  <div className="mt-0.5 text-sm font-medium tnum">
                    {cred.device_count}/{effectiveLimit} 台
                  </div>
                </div>
                <Button
                  type="button"
                  size="sm"
                  variant="ghost"
                  className="h-10 px-3 text-xs sm:h-9"
                  onClick={startEditingLimit}
                >
                  <PencilIcon className="size-3.5" />
                  调整设备上限
                </Button>
              </div>
            )}
          </div>
          {/* 关闭弹窗时直接卸载，避免多账号页面预取所有设备列表。 */}
          {open && <DeviceList credId={cred.id} />}
        </DialogBody>
      </DialogContent>
    </Dialog>
  )
}

/**
 * 某账号当前绑定的设备明细。只在弹窗打开时挂载，故列表按需拉取。
 *
 * 口径与卡片「设备 x/y」的 x 完全一致（后端按同一个绑定 TTL 过滤）。
 */
function DeviceList({ credId }: { credId: number }) {
  const qc = useQueryClient()
  const { data, isPending, error } = useQuery({
    queryKey: ['credential-devices', credId],
    queryFn: () => listCredentialDevices(credId),
  })

  // 手动解绑：连带刷新账号列表，卡片上的「设备 x/y」要立刻跟着掉一台。
  const unbind = useMutation({
    mutationFn: (deviceId: string) => unbindCredentialDevice(credId, deviceId),
    onSuccess: () => {
      toast.success('已解绑')
      qc.invalidateQueries({ queryKey: ['credential-devices', credId] })
      qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (e) => toast.error('解绑失败', { description: extractError(e) }),
  })

  return (
    <div className="px-4 pb-4 text-xs sm:px-5">
      {isPending ? (
        <span className="inline-flex items-center gap-1.5 py-4 text-muted-foreground" role="status">
          <ArrowPathIcon className="size-3 animate-spin" />读取设备列表…
        </span>
      ) : error ? (
        <span className="block break-words py-4 text-bad">{extractError(error)}</span>
      ) : data.length === 0 ? (
        <span className="block py-4 text-muted-foreground">暂无活跃设备</span>
      ) : (
        <ul className="divide-y divide-border">
          {data.map((d) => (
            <li key={d.device_id} className="py-3">
              <div className="flex items-start gap-2.5">
                <span className="grid size-8 shrink-0 place-items-center rounded-full border border-border bg-muted text-muted-foreground">
                  <DevicePhoneMobileIcon className="size-4" />
                </span>
                <div className="min-w-0 flex-1">
                  <Button
                    type="button"
                    size="sm"
                    variant="link"
                    className="h-5 max-w-full justify-start truncate p-0 font-mono text-xs font-medium"
                    title={`${d.device_id}（点击复制）`}
                    aria-label={`复制设备 ID ${d.device_id}`}
                    onClick={async () => {
                      const ok = await copyText(d.device_id)
                      if (ok) toast.success('已复制 device_id')
                      else toast.error('复制失败', { description: d.device_id })
                    }}
                  >
                    {d.device_id}
                  </Button>
                  <span
                    className="mt-1 block tnum text-2xs text-muted-foreground"
                    title={`首次绑定 ${new Date(d.created_at * 1000).toLocaleString()}`}
                  >
                    最近活跃 {relativeTime(d.last_seen_at)}
                  </span>
                </div>
                <Button
                  size="icon"
                  variant="ghost"
                  className="size-9 shrink-0 text-bad hover:text-bad sm:size-8"
                  onClick={() => unbind.mutate(d.device_id)}
                  disabled={unbind.isPending}
                  title="解绑设备"
                  aria-label={`解绑设备 ${d.device_id}`}
                >
                  {unbind.isPending && unbind.variables === d.device_id
                    ? <ArrowPathIcon className="size-3 animate-spin" />
                    : <XMarkIcon className="size-3" />}
                </Button>
              </div>
              <div className="mt-2 grid grid-cols-2 gap-2 sm:grid-cols-3">
                <DeviceStat label="请求" value={`${d.request_count} 次`} />
                <DeviceStat label="本账号花费" value={formatUsd(d.cost_usd)} />
                {d.cost_usd_all > d.cost_usd && (
                  <DeviceStat label="全部账号花费" value={formatUsd(d.cost_usd_all)} className="col-span-2 sm:col-span-1" />
                )}
              </div>
            </li>
          ))}
        </ul>
      )}
    </div>
  )
}

function DeviceStat({ label, value, className }: { label: string; value: string; className?: string }) {
  return (
    <div className={cn('min-w-0 py-1', className)}>
      <div className="text-2xs text-muted-foreground">{label}</div>
      <div className="mt-0.5 font-medium tnum text-foreground">{value}</div>
    </div>
  )
}

/**
 * 单个额度窗口条：标签 + 百分比 + 进度条 + 重置时刻 + 本档已用金额。
 *
 * `util` 传的是 [`liveQuota`] 的结果而非原始快照：窗口过了重置时刻就置空，这里退化成
 * 一行说明。此时进度条上的旧百分比、以及按旧窗口起点累加出来的花费，都已经不成立了，
 * 与其画一根 95% 的红条再在旁边写「已重置」，不如干脆不画。
 */
function QuotaBar({
  label, util, reset, cost, snapshotTs, className,
}: {
  label: string
  util: number | null
  reset: number | null
  cost: number | null
  className?: string
  /** 快照对应的请求时间（Unix 秒），用于说明这份数据有多旧。 */
  snapshotTs: number
}) {
  if (util == null) {
    const reason = reset != null
      ? `窗口已在 ${formatFullTime(reset)} 重置，之后该账号没有新请求，因此没有新数据`
      : '上游未返回该窗口的额度信息'
    return (
      <div
        className={cn('flex min-h-28 flex-col justify-center px-3.5 py-3.5 text-xs text-muted-foreground', className)}
        title={`${reason}。最后一次快照：${formatFullTime(snapshotTs)}`}
      >
        {label} · {reset != null ? '已重置，暂无新用量' : '暂无数据'}
      </div>
    )
  }
  const pct = Math.min(100, Math.max(0, Math.round(util * 100)))
  const critical = util >= 0.9
  const barColor = critical ? 'bg-bad' : util >= 0.7 ? 'bg-warn' : 'bg-ok'
  const pctColor = critical ? 'text-bad' : util >= 0.7 ? 'text-warn' : 'text-foreground'
  return (
    <div className={cn('min-h-28 px-3.5 py-3.5', className)}>
      <div className="flex items-baseline justify-between gap-2">
        <span className="truncate text-xs font-medium text-muted-foreground">{label}</span>
        {/* 百分比只在有请求经过时才刷新，标注快照时间，免得把很旧的数当成实时值。 */}
        <span
          className={cn('text-xl font-semibold tnum leading-none', pctColor)}
          title={`额度使用率 · 快照于 ${relativeTime(snapshotTs)}（${formatFullTime(snapshotTs)}）`}
        >
          {pct}
          <span className="ml-px text-xs font-medium text-muted-foreground">%</span>
        </span>
      </div>
      <div
        className="mt-2.5 h-2 overflow-hidden rounded-full bg-border/80"
        role="progressbar"
        aria-valuenow={pct}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-label={label}
      >
        <div
          className={cn(
            'h-full rounded-full transition-[width] duration-500 ease-out',
            barColor,
            // 临界时轻微条纹动画，强化「快满了」的紧迫感。
            critical && 'bg-[length:0.75rem_0.75rem] bg-[image:repeating-linear-gradient(45deg,transparent,transparent_4px,rgba(255,255,255,0.22)_4px,rgba(255,255,255,0.22)_8px)]',
          )}
          style={{ width: `${pct}%` }}
        />
      </div>
      <div className="mt-2 flex items-baseline justify-between gap-2 text-xs text-muted-foreground">
        <span title="本周期内已消耗的等价 API 费用">
          花费 <span className="tnum font-medium text-foreground/80">{formatUsd(cost ?? 0)}</span>
        </span>
        {reset != null && (
          <span className="whitespace-nowrap tnum" title={`${formatFullTime(reset)} 重置`}>
            {formatClockTime(reset)} 重置
          </span>
        )}
      </div>
    </div>
  )
}
