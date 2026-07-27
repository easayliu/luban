import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import {
  ArrowPathIcon, PencilIcon, CheckIcon, XMarkIcon, EllipsisHorizontalIcon,
  DevicePhoneMobileIcon, ExclamationTriangleIcon, ChevronDownIcon,
  CalendarDaysIcon, ClockIcon, WalletIcon,
} from '@heroicons/react/24/outline'
import { listCredentialDevices, unbindCredentialDevice, type Credential } from '@/api/credentials'
import { cn, extractError, formatDuration, formatUsd, relativeTime } from '@/lib/utils'
import {
  CredentialMenuContent, expiryMeta, inputToLimit, isAbnormal, isNearLimit, limitToInput,
  statusMeta, switchTitle, tierBadgeClass, useCredentialActions,
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

  const actions = useCredentialActions(
    cred,
    () => setEditing(false),
    () => setEditingLimit(false),
  )
  const { rename, toggle, limit } = actions

  // 额度接近上限（5h / 7d 任一 ≥90%）：卡片描边 + 角标提示。
  const quotaMax = Math.max(
    cred.quota?.rl_5h_utilization ?? 0,
    cred.quota?.rl_7d_utilization ?? 0,
  )
  const nearLimit = isNearLimit(cred)
  const initial = cred.label.trim().charAt(0).toUpperCase() || '?'
  const has5h = cred.quota?.rl_5h_utilization != null
  const has7d = cred.quota?.rl_7d_utilization != null
  const expiry = expiryMeta(cred)
  const status = statusMeta(cred, nearLimit)
  const abnormal = isAbnormal(cred)

  return (
    <Card
      className={cn(
        '@container/card group/card relative overflow-hidden rounded-2xl border-border/70 p-5 pl-[calc(1.25rem-3px)] shadow-card transition-all',
        'before:absolute before:inset-y-0 before:left-0 before:w-[3px] before:transition-colors',
        'hover:border-border hover:shadow-elev',
        cred.disabled && 'opacity-60',
        // 左侧状态轨：一眼分诊。正常态透明，异常态着色。
        status.rail,
        nearLimit && 'ring-1 ring-bad/20',
      )}
    >
      {/* 头部：头像 + 名称/徽章 + 开关/菜单 */}
      <div className="flex items-start gap-3.5">
        {selectable && (
          <input
            type="checkbox"
            checked={selected}
            onChange={(e) => onSelectedChange?.(e.target.checked)}
            className="mt-3 size-4 shrink-0 accent-primary"
            aria-label={`选择 ${cred.label}`}
          />
        )}
        <div className="relative shrink-0">
          <div
            className={cn(
              'grid size-10 place-items-center rounded-xl text-sm font-semibold',
              cred.disabled
                ? 'bg-muted text-muted-foreground'
                : 'bg-primary text-primary-foreground',
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
          />
        </div>

        <div className="min-w-0 flex-1">
          {editing ? (
            <form
              className="flex items-center gap-1.5"
              onSubmit={(e) => { e.preventDefault(); rename.mutate(name.trim()) }}
            >
              <Input value={name} onChange={(e) => setName(e.target.value)} autoFocus className="h-8 w-56" />
              <Button type="submit" size="icon" variant="ghost" className="h-8 w-8" disabled={rename.isPending}>
                {rename.isPending ? <ArrowPathIcon className="animate-spin" /> : <CheckIcon />}
              </Button>
              <Button type="button" size="icon" variant="ghost" className="h-8 w-8"
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
                <span className="truncate text-sm font-semibold tracking-tight">{cred.label}</span>
                <PencilIcon className="size-3 shrink-0 text-muted-foreground opacity-0 transition-opacity group-hover/name:opacity-100" />
              </button>
              {nearLimit && (
                <Badge variant="bad" className="shrink-0">
                  <ExclamationTriangleIcon className="size-3" />
                  额度将满 {Math.round(quotaMax * 100)}%
                </Badge>
              )}
            </div>
          )}

          {/* 元信息：固定两行，封禁/正常态布局一致。
              第一行 套餐 + 状态/有效期；第二行 #id · token。 */}
          <div className="mt-1.5 space-y-1.5 text-2xs text-muted-foreground">
            <div className="flex items-center gap-x-3 gap-y-1.5">
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
                title={cred.ban_reason ?? undefined}
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
        <div className="flex shrink-0 items-center gap-1.5">
          {/* 启用开关：健康态开=绿；封禁/过期等异常态转中性灰（避免绿开关与红状态灯语义冲突）。
              切换中显示加载圈占位，避免布局跳动。 */}
          <span className="relative inline-flex items-center">
            <Switch
              variant="success"
              checked={!cred.disabled}
              onCheckedChange={(on) => toggle.mutate(!on)}
              disabled={toggle.isPending}
              title={switchTitle(cred)}
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
              <Button size="icon" variant="ghost" className="size-8 text-muted-foreground">
                <EllipsisHorizontalIcon />
              </Button>
            </DropdownMenuTrigger>
            <CredentialMenuContent cred={cred} actions={actions} onRename={() => setEditing(true)} />
          </DropdownMenu>
        </div>
      </div>

      {/* 额度区：5h / 7d 订阅额度（缺失窗口不占位，仅一个时占满整行） */}
      {cred.quota && (has5h || has7d) && (
        <div className={cn('mt-4 grid gap-2.5', has5h && has7d && '@sm/card:grid-cols-2')}>
          {has5h && (
            <QuotaBar
              label="5 小时额度"
              util={cred.quota.rl_5h_utilization}
              reset={cred.quota.rl_5h_reset}
              cost={cred.quota.cost_5h}
            />
          )}
          {has7d && (
            <QuotaBar
              label="7 天额度"
              util={cred.quota.rl_7d_utilization}
              reset={cred.quota.rl_7d_reset}
              cost={cred.quota.cost_7d}
            />
          )}
        </div>
      )}

      {/* 底部：统计信息合并为一行（添加 / 最近使用 / 累计花费 / 设备）。设备可点击编辑上限。 */}
      <div className="mt-4 flex flex-wrap items-center gap-x-3.5 gap-y-1.5 border-t border-border/60 pt-3 text-2xs text-muted-foreground">
        <span
          className="inline-flex items-center gap-1"
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

        {/* 设备：非编辑态作为统计项，点击展开为上限输入框。
            上限三态——留空跟随全局默认（显示「默认」角标）、0 表示该账号不限、正数为独立上限。 */}
        {editingLimit ? (
          <form
            className="ml-auto inline-flex items-center gap-1.5"
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
              className="h-6 w-14 px-1.5 text-2xs"
              title="留空 = 跟随全局默认上限；0 = 该账号不限；正数 = 该账号独立上限"
            />
            <Button type="submit" size="icon" variant="ghost" className="size-6" disabled={limit.isPending}>
              {limit.isPending ? <ArrowPathIcon className="size-3 animate-spin" /> : <CheckIcon className="size-3" />}
            </Button>
            <Button type="button" size="icon" variant="ghost" className="size-6"
              onClick={() => { setEditingLimit(false); setLimitVal(limitToInput(cred.device_limit)) }}>
              <XMarkIcon className="size-3" />
            </Button>
          </form>
        ) : (
          <button
            onClick={() => { setLimitVal(limitToInput(cred.device_limit)); setEditingLimit(true) }}
            className="group/limit ml-auto inline-flex items-center gap-1 transition-colors hover:text-foreground"
            title={
              cred.device_limit === 0
                ? '跟随全局默认上限（在接入设置里改）；点击可为该账号单独设置'
                : '点击调整该账号的设备数上限（留空可改回跟随默认）'
            }
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
            <PencilIcon className="size-2.5 shrink-0 opacity-0 transition-opacity group-hover/limit:opacity-100" />
          </button>
        )}

        {/* 展开「已绑定设备」：上面的数字只说有几台，这里能看到具体是哪几台。
            单独一个箭头按钮，因为设备统计项本身的点击已经被「改上限」占用了。 */}
        <button
          onClick={() => setShowDevices((v) => !v)}
          className="inline-flex items-center transition-colors hover:text-foreground"
          title={showDevices ? '收起已绑定设备' : '查看已绑定的设备'}
          aria-expanded={showDevices}
          aria-label="已绑定设备"
        >
          <ChevronDownIcon
            className={cn('size-3 transition-transform', showDevices && 'rotate-180')}
          />
        </button>
      </div>

      {showDevices && <DeviceList credId={cred.id} />}
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
    <div className="mt-2.5 rounded-xl border border-border/60 bg-surface-2/40 px-3 py-2.5 text-2xs">
      {isPending ? (
        <span className="inline-flex items-center gap-1.5 text-muted-foreground">
          <ArrowPathIcon className="size-3 animate-spin" />读取设备列表…
        </span>
      ) : error ? (
        <span className="text-bad">{extractError(error)}</span>
      ) : data.length === 0 ? (
        <span className="text-muted-foreground">暂无活跃设备（绑定超时未活跃即自动释放名额）</span>
      ) : (
        <ul className="divide-y divide-border/50">
          {data.map((d) => (
            <li key={d.device_id} className="flex items-center gap-2 py-1 first:pt-0 last:pb-0">
              <DevicePhoneMobileIcon className="size-3 shrink-0 opacity-60" />
              <span className="min-w-0 truncate font-mono" title={d.device_id}>
                {d.device_id.slice(0, 12)}…
              </span>
              <span
                className="ml-auto shrink-0 tnum text-muted-foreground"
                title="该设备经此账号转发的累计请求数"
              >
                {d.request_count} 次
              </span>
              {/* 本账号花费。跨账号合计放 title：换过号的设备两个数会不一样，
                  行里塞两个金额只会看花眼。 */}
              <span
                className={cn(
                  'w-14 shrink-0 text-right tnum',
                  d.cost_usd > 0 ? 'text-foreground/80' : 'text-muted-foreground',
                )}
                title={
                  `该设备在本账号的累计花费 ${formatUsd(d.cost_usd)}` +
                  (d.cost_usd_all > d.cost_usd
                    ? `；含其它账号共 ${formatUsd(d.cost_usd_all)}（该设备曾绑到别的账号）`
                    : '') +
                  '。按用量日志统计，解绑重绑不会清零，故可能早于本次绑定'
                }
              >
                {formatUsd(d.cost_usd)}
              </span>
              <span
                className="w-14 shrink-0 text-right tnum text-muted-foreground"
                title={`首次绑定 ${new Date(d.created_at * 1000).toLocaleString()}`}
              >
                {relativeTime(d.last_seen_at)}
              </span>
              {/* 解绑：只是放掉这台设备占的名额，它下次请求会重新选号（名额没满时可能又回来），
                  所以不做二次确认；误点的代价仅是丢一次粘性。 */}
              <button
                onClick={() => unbind.mutate(d.device_id)}
                disabled={unbind.isPending}
                className="shrink-0 rounded p-0.5 text-muted-foreground opacity-0 transition-all hover:bg-bad-soft hover:text-bad focus-visible:opacity-100 disabled:opacity-40 group-hover/card:opacity-100"
                title="解除该设备的绑定（腾出一个设备名额；该设备下次请求会重新选号）"
                aria-label={`解绑设备 ${d.device_id}`}
              >
                {unbind.isPending && unbind.variables === d.device_id ? (
                  <ArrowPathIcon className="size-3 animate-spin" />
                ) : (
                  <XMarkIcon className="size-3" />
                )}
              </button>
            </li>
          ))}
        </ul>
      )}
    </div>
  )
}

/** 单个额度窗口条：标签 + 百分比 + 进度条 + 重置倒计时 + 本档已用金额。util 为空显示「未返回」。 */
function QuotaBar({
  label, util, reset, cost,
}: {
  label: string
  util: number | null
  reset: number | null
  cost: number | null
}) {
  if (util == null) {
    return (
      <div className="rounded-xl border border-dashed border-border/70 px-3 py-2.5 text-2xs text-muted-foreground">
        {label} · 暂无数据
      </div>
    )
  }
  const pct = Math.min(100, Math.max(0, Math.round(util * 100)))
  const critical = util >= 0.9
  const barColor = critical ? 'bg-bad' : util >= 0.7 ? 'bg-warn' : 'bg-ok'
  const pctColor = critical ? 'text-bad' : util >= 0.7 ? 'text-warn' : 'text-foreground'
  const remain = reset != null ? reset - Math.floor(Date.now() / 1000) : null
  return (
    <div className="rounded-xl border border-border/60 bg-surface-2/40 px-3 py-2.5">
      <div className="flex items-baseline justify-between gap-2">
        <span className="truncate text-2xs font-medium text-muted-foreground">{label}</span>
        <span className={cn('text-sm font-semibold tnum leading-none', pctColor)} title="额度使用率">
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
        {remain != null && remain > 0 && (
          <span className="tnum" title="额度重置倒计时">{formatDuration(remain)}后重置</span>
        )}
      </div>
    </div>
  )
}

function Dot() {
  return <span className="opacity-40">·</span>
}
