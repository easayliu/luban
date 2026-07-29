import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import {
  ArrowPathIcon, PencilIcon, CheckIcon, XMarkIcon, EllipsisHorizontalIcon,
  DevicePhoneMobileIcon, ExclamationTriangleIcon, ChevronDownIcon,
  CalendarDaysIcon, ClockIcon, WalletIcon,
} from '@heroicons/react/24/outline'
import { listCredentialDevices, unbindCredentialDevice, type Credential } from '@/api/credentials'
import {
  cn, copyText, extractError, formatClockTime, formatFullTime, formatUsd, relativeTime,
} from '@/lib/utils'
import {
  CredentialMenuContent, DeleteCredentialDialog, expiryMeta, inputToLimit, isAbnormal,
  isNearLimit, limitToInput, liveQuota, statusMeta, switchTitle, tierBadgeClass,
  useCredentialActions,
} from '@/components/credential-shared'
import { Card } from '@/components/ui/card'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { DropdownMenu, DropdownMenuTrigger } from '@/components/ui/dropdown-menu'

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
  // 已绑定设备明细：默认收起，展开时才挂载 DeviceList（也才发请求）。
  const [showDevices, setShowDevices] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)

  const actions = useCredentialActions(
    cred,
    () => setEditing(false),
    () => setEditingLimit(false),
  )
  const { rename, toggle, limit } = actions

  // 额度接近上限（5h / 7d 任一 ≥90%）：卡片描边 + 角标提示。
  // 用 liveQuota 而非原始快照——窗口已重置的百分比是上个周期的，不该再触发告警。
  const { u5h, u7d } = liveQuota(cred)
  const quotaMax = Math.max(u5h ?? 0, u7d ?? 0)
  const nearLimit = isNearLimit(cred)
  const initial = cred.label.trim().charAt(0).toUpperCase() || '?'
  // 窗口已重置的仍然渲染额度条（由 QuotaBar 显示成「已重置」），只是不参与告警。
  const has5h = cred.quota?.rl_5h_utilization != null
  const has7d = cred.quota?.rl_7d_utilization != null
  const expiry = expiryMeta(cred)
  const status = statusMeta(cred, nearLimit)
  const abnormal = isAbnormal(cred)

  return (
    <Card
      className={cn(
        '@container/card group/card relative overflow-hidden rounded-lg border-border bg-card p-3 pl-[calc(0.75rem-3px)] shadow-card transition-[border-color,box-shadow] sm:p-5 sm:pl-[calc(1.25rem-3px)]',
        'before:absolute before:inset-y-0 before:left-0 before:w-[3px] before:transition-colors',
        'hover:border-foreground/15',
        cred.disabled && 'opacity-60',
        // 左侧状态轨：一眼分诊。正常态透明，异常态着色。
        status.rail,
        nearLimit && 'ring-1 ring-bad/20',
      )}
    >
      {/* 头部：头像 + 名称/徽章 + 开关/菜单 */}
      <div className="flex items-start gap-2.5 sm:gap-3.5">
        {selectable && (
          <input
            type="checkbox"
            checked={selected}
            onChange={(e) => onSelectedChange?.(e.target.checked)}
            className="mt-3 size-4 shrink-0 rounded border-border accent-primary"
            aria-label={`选择 ${cred.label}`}
          />
        )}
        <div className="relative shrink-0">
          <div
            className={cn(
              'grid size-9 place-items-center rounded-md text-xs font-semibold sm:size-10 sm:text-sm',
              cred.disabled
                ? 'bg-muted text-muted-foreground'
                : 'bg-primary text-primary-foreground shadow-sm',
            )}
            aria-hidden
          >
            {initial}
          </div>
          {/* 状态灯：绿=正常 红=异常 琥珀=将满/将过期 灰=停用，环切合卡片底色。 */}
          <span
            className={cn(
              'absolute -bottom-0.5 -right-0.5 size-3 rounded-full ring-2 ring-card',
              status.dot,
            )}
            title={status.label}
            aria-label={status.label}
            role="img"
          />
        </div>

        <div className="min-w-0 flex-1">
          {editing ? (
            <form
              className="flex min-w-0 items-center gap-1"
              onSubmit={(e) => { e.preventDefault(); rename.mutate(name.trim()) }}
            >
              <Input value={name} onChange={(e) => setName(e.target.value)} autoFocus className="h-8 min-w-0 flex-1 px-2 text-xs sm:w-56 sm:flex-none sm:px-3 sm:text-sm" aria-label="账号名称" />
              <Button type="submit" size="icon" variant="ghost" className="h-8 w-8" disabled={rename.isPending} aria-label="保存账号名称">
                {rename.isPending ? <ArrowPathIcon className="animate-spin" /> : <CheckIcon />}
              </Button>
              <Button type="button" size="icon" variant="ghost" className="h-8 w-8" aria-label="取消重命名"
                onClick={() => { setEditing(false); setName(cred.label) }}>
                <XMarkIcon />
              </Button>
            </form>
          ) : (
            <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
              <button
                onClick={() => setEditing(true)}
                className="group/name inline-flex min-w-0 items-center gap-1.5"
                title="点击重命名"
              >
                <span className="truncate text-[0.8125rem] font-semibold tracking-tight sm:text-sm">{cred.label}</span>
                {/* pointer-fine 才隐藏：触屏没有 hover 态，藏起来等于这个入口不存在。 */}
                <PencilIcon className="size-3 shrink-0 text-muted-foreground transition-opacity pointer-fine:opacity-0 pointer-fine:group-hover/name:opacity-100" />
              </button>
              {nearLimit && (
                <Badge variant="bad" className="shrink-0">
                  <ExclamationTriangleIcon className="size-3" />
                  额度 {Math.round(quotaMax * 100)}%
                </Badge>
              )}
            </div>
          )}

          {/* 元信息：固定两行，封禁/正常态布局一致。
              第一行 套餐 + 状态/有效期；第二行 #id · token。 */}
          <div className="mt-1.5 space-y-1.5 text-2xs text-muted-foreground">
            <div className="flex items-center gap-x-2 gap-y-1.5 sm:gap-x-3">
              {cred.tier && (
                <Badge
                  variant="outline"
                  className={cn('h-5 shrink-0 gap-1 px-2 py-0 text-2xs font-medium', tierBadgeClass(cred.tier))}
                >
                  {cred.tier}
                </Badge>
              )}
              {/* 调度优先级：只读展示，修改入口在 ⋯ 菜单（低频配置）。 */}
              <Badge
                variant="outline"
                className="h-5 shrink-0 gap-1 px-2 py-0 font-mono text-2xs font-medium text-muted-foreground"
                title="调度优先级（数值小者优先），在 ⋯ 菜单中调整"
              >
                P{cred.priority}
              </Badge>
              <span
                className={cn('inline-flex min-w-0 items-center gap-1', expiry.className)}
                title={expiry.title}
              >
                <ClockIcon className="size-3 shrink-0" />
                <span className="truncate">{expiry.text}</span>
              </span>
            </div>
            <div className="flex items-center gap-1 font-mono">
              <span className="tnum shrink-0">#{cred.id}</span>
              <Dot />
              <span className="min-w-0 truncate" title="refresh_token（脱敏）">
                {cred.token_hint}
              </span>
            </div>
          </div>
        </div>

        {/* 右上控制：启用开关 + 溢出菜单 */}
        <div className="flex shrink-0 items-center gap-0.5 sm:gap-1.5">
          {/* 启用开关：健康态开=绿；封禁/过期等异常态转中性灰（避免绿开关与红状态灯语义冲突）。
              切换中显示加载圈占位，避免布局跳动。 */}
          <span className="relative inline-flex items-center">
            <Switch
              variant="success"
              checked={!cred.disabled}
              onCheckedChange={(on) => toggle.mutate(!on)}
              disabled={toggle.isPending}
              title={switchTitle(cred)}
              aria-label={switchTitle(cred)}
              className={cn(
                toggle.isPending && 'opacity-0',
                // 封禁/过期等异常态：开关转中性灰，不用健康绿，避免与红状态灯冲突。
                abnormal && 'data-[state=checked]:bg-muted-foreground/50',
              )}
            />
            {toggle.isPending && (
              <ArrowPathIcon className="absolute left-1/2 top-1/2 size-4 -translate-x-1/2 -translate-y-1/2 animate-spin text-muted-foreground" />
            )}
          </span>
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button size="icon" variant="ghost" className="size-8 text-muted-foreground" aria-label={`打开 ${cred.label} 菜单`}>
                <EllipsisHorizontalIcon />
              </Button>
            </DropdownMenuTrigger>
            <CredentialMenuContent
              cred={cred}
              actions={actions}
              onRename={() => setEditing(true)}
              onRequestDelete={() => setConfirmDelete(true)}
            />
          </DropdownMenu>
        </div>
      </div>

      {/* 额度区：5h / 7d 订阅额度（缺失窗口不占位，仅一个时占满整行） */}
      {cred.quota && (has5h || has7d) && (
        <div className={cn('mt-3 grid gap-2.5 sm:mt-4', has5h && has7d && '@sm/card:grid-cols-2')}>
          {has5h && (
            <QuotaBar
              label="5 小时额度"
              util={u5h}
              reset={cred.quota.rl_5h_reset}
              cost={cred.quota.cost_5h}
              snapshotTs={cred.quota.ts}
            />
          )}
          {has7d && (
            <QuotaBar
              label="7 天额度"
              util={u7d}
              reset={cred.quota.rl_7d_reset}
              cost={cred.quota.cost_7d}
              snapshotTs={cred.quota.ts}
            />
          )}
        </div>
      )}

      {/* 底部：统计信息合并为一行（添加 / 最近使用 / 累计花费 / 设备）。设备可点击编辑上限。 */}
      <div className="mt-3 flex flex-wrap items-center gap-x-3 gap-y-1.5 border-t border-border/60 pt-3 text-2xs text-muted-foreground sm:mt-4 sm:gap-x-3.5">
        <span
          className="hidden items-center gap-1 sm:inline-flex"
          title={`添加于 ${new Date(cred.created_at * 1000).toLocaleString()}`}
        >
          <CalendarDaysIcon className="size-3 shrink-0 opacity-70" />
          {relativeTime(cred.created_at)}
        </span>
        <span className="inline-flex items-center gap-1" title="最近一次转发使用">
          <ClockIcon className="size-3 shrink-0 opacity-70" />
          {cred.last_used != null ? relativeTime(cred.last_used) : '未使用'}
        </span>
        <span
          className="inline-flex items-center gap-1"
          title="该账号历史累计等价 API 费用（按官方定价估算）"
        >
          <WalletIcon className="size-3 shrink-0 opacity-70" />
          <span className="tnum">{formatUsd(cred.cost_total)}</span>
        </span>

        {/* 设备：摘要按钮展开明细，右侧铅笔单独编辑上限。
            上限三态——留空跟随全局默认（显示「默认」角标）、0 表示该账号不限、正数为独立上限。 */}
        {editingLimit ? (
          <form
            className="ml-auto flex w-full items-center justify-end gap-1.5 sm:w-auto"
            onSubmit={(e) => { e.preventDefault(); limit.mutate(inputToLimit(limitVal)) }}
          >
            <DevicePhoneMobileIcon className="size-3 shrink-0 opacity-70" />
            <Input
              type="number"
              min={0}
              value={limitVal}
              onChange={(e) => setLimitVal(e.target.value)}
              autoFocus
              placeholder="默认"
              className="h-9 w-20 px-2 text-xs sm:h-6 sm:w-14 sm:px-1.5 sm:text-2xs"
              title="留空 = 跟随全局默认上限；0 = 该账号不限；正数 = 该账号独立上限"
              aria-label="设备上限"
            />
            <Button type="submit" size="icon" variant="ghost" className="size-9 sm:size-6" disabled={limit.isPending} aria-label="保存设备上限">
              {limit.isPending ? <ArrowPathIcon className="size-3 animate-spin" /> : <CheckIcon className="size-3" />}
            </Button>
            <Button type="button" size="icon" variant="ghost" className="size-9 sm:size-6" aria-label="取消修改设备上限"
              onClick={() => { setEditingLimit(false); setLimitVal(limitToInput(cred.device_limit)) }}>
              <XMarkIcon className="size-3" />
            </Button>
          </form>
        ) : (
          <div className="ml-auto inline-flex h-9 items-center overflow-hidden rounded-md border border-border bg-background shadow-sm sm:h-7">
            <button
              onClick={() => setShowDevices((v) => !v)}
              className="inline-flex h-full items-center gap-1.5 px-2.5 transition-colors hover:bg-muted hover:text-foreground"
              title={showDevices ? '收起已绑定设备' : '查看已绑定设备'}
              aria-expanded={showDevices}
              aria-label={`${showDevices ? '收起' : '展开'}已绑定设备，当前 ${cred.device_count} 台`}
            >
              <DevicePhoneMobileIcon className="size-3 shrink-0 opacity-70" />
              <span className="tnum">
                设备 {cred.device_count}/
                {cred.device_limit_effective > 0 ? cred.device_limit_effective : '∞'}
              </span>
              {cred.device_limit === 0 && (
                <span className="rounded bg-muted px-1 text-[0.625rem] leading-4 text-muted-foreground">
                  默认
                </span>
              )}
              <ChevronDownIcon className={cn('size-3 transition-transform', showDevices && 'rotate-180')} />
            </button>
            <button
              onClick={() => { setLimitVal(limitToInput(cred.device_limit)); setEditingLimit(true) }}
              className="grid size-9 shrink-0 place-items-center border-l border-border text-muted-foreground transition-colors hover:bg-muted hover:text-foreground sm:size-7"
              title="调整设备上限"
              aria-label="调整设备上限"
            >
              <PencilIcon className="size-3" />
            </button>
          </div>
        )}
      </div>

      {showDevices && <DeviceList credId={cred.id} />}

      <DeleteCredentialDialog
        cred={cred}
        actions={actions}
        open={confirmDelete}
        onOpenChange={setConfirmDelete}
      />
    </Card>
  )
}

/**
 * 某账号当前绑定的设备明细。只在展开时挂载，故列表是按需拉取的。
 *
 * 口径与上方「设备 x/y」的 x 完全一致（后端按同一个绑定 TTL 过滤），条数必然对得上；
 * 超时未活跃的绑定既不占名额也不在这里出现。
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
    <div className="mt-3 animate-in overflow-hidden rounded-md border border-border bg-muted/20 text-xs fade-in-0 slide-in-from-top-1 duration-200 motion-reduce:animate-none">
      <div className="flex items-center justify-between border-b border-border bg-muted/30 px-3 py-2">
        <span className="font-medium text-foreground">已绑定设备</span>
        {!isPending && !error && (
          <span className="tnum text-2xs text-muted-foreground">{data.length} 台</span>
        )}
      </div>
      {isPending ? (
        <span className="inline-flex items-center gap-1.5 p-3 text-muted-foreground" role="status">
          <ArrowPathIcon className="size-3 animate-spin" />读取设备列表…
        </span>
      ) : error ? (
        <span className="block p-3 text-bad">{extractError(error)}</span>
      ) : data.length === 0 ? (
        <span className="block p-3 text-muted-foreground">暂无活跃设备</span>
      ) : (
        <ul className="divide-y divide-border">
          {data.map((d) => (
            <li key={d.device_id} className="bg-card p-3">
              <div className="flex items-start gap-2.5">
                <span className="grid size-8 shrink-0 place-items-center rounded-md bg-muted text-muted-foreground">
                  <DevicePhoneMobileIcon className="size-4" />
                </span>
                <div className="min-w-0 flex-1">
                  <button
                    className="block max-w-full truncate text-left font-mono text-xs font-medium transition-colors hover:text-foreground"
                    title={`${d.device_id}（点击复制）`}
                    aria-label={`复制设备 ID ${d.device_id}`}
                    onClick={async () => {
                      const ok = await copyText(d.device_id)
                      if (ok) toast.success('已复制 device_id')
                      else toast.error('复制失败', { description: d.device_id })
                    }}
                  >
                    {d.device_id}
                  </button>
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
    <div className={cn('rounded-md border border-border/60 bg-muted/30 px-2.5 py-2', className)}>
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
  label, util, reset, cost, snapshotTs,
}: {
  label: string
  util: number | null
  reset: number | null
  cost: number | null
  /** 快照对应的请求时间（Unix 秒），用于说明这份数据有多旧。 */
  snapshotTs: number
}) {
  if (util == null) {
    const reason = reset != null
      ? `窗口已在 ${formatFullTime(reset)} 重置，之后该账号没有新请求，因此没有新数据`
      : '上游未返回该窗口的额度信息'
    return (
      <div
        className="rounded-md border border-dashed border-border/70 px-3 py-2.5 text-2xs text-muted-foreground"
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
    <div className="rounded-md border border-border/60 bg-muted/30 px-3 py-2.5">
      <div className="flex items-baseline justify-between gap-2">
        <span className="truncate text-2xs font-medium text-muted-foreground">{label}</span>
        {/* 百分比只在有请求经过时才刷新，标注快照时间，免得把很旧的数当成实时值。 */}
        <span
          className={cn('text-sm font-semibold tnum leading-none', pctColor)}
          title={`额度使用率 · 快照于 ${relativeTime(snapshotTs)}（${formatFullTime(snapshotTs)}）`}
        >
          {pct}
          <span className="ml-px text-2xs font-medium text-muted-foreground">%</span>
        </span>
      </div>
      <div
        className="mt-2 h-1.5 overflow-hidden rounded-full bg-border/80"
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
      <div className="mt-1.5 flex items-baseline justify-between gap-2 text-2xs text-muted-foreground">
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

function Dot() {
  return <span className="opacity-40">·</span>
}
