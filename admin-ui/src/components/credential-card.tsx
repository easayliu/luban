import { useState } from 'react'
import {
  ActivityIcon,
  CalendarDaysIcon,
  CheckIcon,
  ClockIcon,
  EllipsisIcon,
  SmartphoneIcon,
  TimerOffIcon,
  WalletCardsIcon,
  XIcon,
} from 'lucide-react'
import { type Credential } from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import {
  cn,
  displayCredentialLabel,
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
  modelCooldownSummary,
  quotaLevel,
  isOrgAccount,
  orgBadgeLabel,
  quotaPercentage,
  switchTitle,
  tierBadgeVariant,
  unifiedQuotaStatusLabel,
  useCredentialActions,
  type QuotaFreshness,
  type QuotaWindowMeta,
} from '@/components/credential-shared'
import { CredentialDevicesDialog } from '@/components/credential-devices-dialog'
import { CredentialProxyDialog } from '@/components/credential-proxy-dialog'
import { CredentialUsageDialog } from '@/components/credential-usage-dialog'
import { Avatar, AvatarFallback } from '@/components/ui/avatar'
import { Badge, badgeVariants } from '@/components/ui/badge'
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
import { Separator } from '@/components/ui/separator'
import { Spinner } from '@/components/ui/spinner'
import { Switch } from '@/components/ui/switch'
import { Tooltip, TooltipPopup, TooltipTrigger } from '@/components/ui/tooltip'

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
  const [proxyOpen, setProxyOpen] = useState(false)
  const [usageOpen, setUsageOpen] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [testing, setTesting] = useState(false)

  const actions = useCredentialActions(cred, () => setEditing(false))
  const { rename, toggle, limit } = actions
  const evaluation = evaluateCredential(cred, now, language)
  const { quota, status } = evaluation
  const credentialLabel = displayCredentialLabel(cred.label, language)
  const initial = credentialLabel.trim().charAt(0).toUpperCase() || '?'
  // 只渲染上游真报过的窗口。卡片是弹性布局，没有的那个直接不占位；表格那边列宽固定，
  // 摘不掉，所以改成显式的「无此窗口」，见 credential-row 的 ListQuotaMeter。
  const has5h = quota.h5.reported
  const has7d = quota.d7.reported
  const effectiveLimit = cred.device_limit_effective > 0 ? cred.device_limit_effective : '∞'
  const devicePolicy = cred.device_limit === 0
    ? { label: t('跟随默认', 'Default'), variant: 'secondary' as const }
    : cred.device_limit < 0
      ? { label: t('不限', 'Unlimited'), variant: 'outline' as const }
      : { label: t('自定义', 'Custom'), variant: 'info' as const }
  const titleId = `credential-card-title-${cred.id}`
  // 所有需处理状态都用同一种渐进披露：卡片只显示状态，详情在悬浮提示里查看。
  // 避免同一条状态再渲染一块说明，把异常卡片单独撑高。
  const statusUsesTooltip = status.attention
  const added = relativeTime(cred.created_at, now, language)
  const quotaSnapshotTime = cred.quota
    ? formatFullTime(cred.quota.ts, language)
    : t('未知时间', 'unknown time')
  const secondaryOverage = (() => {
    if (quota.overage === 'none') return null
    if (cred.disabled) {
      return {
        label: t('快照有 Usage credits', 'Snapshot used usage credits'),
        variant: 'warning' as const,
        title: t(
          `账号已停用；${quotaSnapshotTime} 的额度快照记录了 Usage credits，当前不纳入调度风险统计`,
          `The account is disabled; the ${quotaSnapshotTime} quota snapshot recorded usage credits and is excluded from current scheduling-risk totals`,
        ),
      }
    }
    if (quota.overage === 'historical') {
      return {
        label: t('近期用过 Usage credits', 'Recently used usage credits'),
        variant: 'warning' as const,
        title: t(
          '最近的额度快照记录了 Usage credits，但相关额度窗口已经重置',
          'The latest quota snapshot recorded usage credits, but the related quota windows have reset',
        ),
      }
    }
    if (quota.overage === 'active' && status.kind !== 'overage') {
      return {
        label: t('Usage credits 生效中', 'Usage credits active'),
        variant: 'error' as const,
        title: t(
          `${quotaSnapshotTime} 的额度快照显示套餐用量已耗尽，正由 Usage credits 按标准 API 价放行请求`,
          `The ${quotaSnapshotTime} quota snapshot shows the plan's included usage exhausted and requests being served by usage credits at standard API rates`,
        ),
      }
    }
    if (quota.overage === 'unknown' && status.kind !== 'overage-unknown') {
      return {
        label: t('Usage credits 待确认', 'Usage credits unconfirmed'),
        variant: 'warning' as const,
        title: t(
          `${quotaSnapshotTime} 的额度快照记录了 Usage credits，当前状态仍需确认`,
          `The ${quotaSnapshotTime} quota snapshot recorded usage credits; the current state still needs confirmation`,
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
        <CardHeader className="p-4 pb-3">
          <CardTitle className="min-w-0 text-sm leading-snug">
            {editing ? (
              <>
                <h3 id={titleId} className="sr-only">{credentialLabel}</h3>
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
                    aria-label={t(`选择 ${credentialLabel}`, `Select ${credentialLabel}`)}
                  />
                )}
                <Avatar className="hidden @sm/card:flex" aria-hidden="true">
                  <AvatarFallback>{initial}</AvatarFallback>
                </Avatar>
                <div className="min-w-0 flex-1">
                  <h3
                    id={titleId}
                    className="block min-w-0 truncate whitespace-nowrap leading-snug"
                    title={credentialLabel}
                  >
                    {credentialLabel}
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
                  aria-label={t(`打开 ${credentialLabel} 菜单`, `Open menu for ${credentialLabel}`)}
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
                  onProxy={() => setProxyOpen(true)}
                  onUsage={() => setUsageOpen(true)}
                  onTest={() => setTesting(true)}
                  onRequestDelete={() => setConfirmDelete(true)}
                />
              </Menu>
            </CardAction>
          )}
        </CardHeader>

        <CardPanel className="space-y-3 px-4 pb-3 sm:pb-4">
          <div className="flex flex-wrap items-center gap-2">
            {statusUsesTooltip ? (
              <Tooltip>
                <TooltipTrigger
                  className={cn(badgeVariants({ variant: status.variant }), 'cursor-help')}
                  delay={0}
                  aria-label={t(
                    `${credentialLabel}：${status.label}。${status.detail}`,
                    `${credentialLabel}: ${status.label}. ${status.detail}`,
                  )}
                  aria-live="polite"
                >
                  {status.label}
                </TooltipTrigger>
                <TooltipPopup
                  side="bottom"
                  align="start"
                  className="max-w-80 whitespace-normal break-words text-left leading-5"
                >
                  {status.detail}
                </TooltipPopup>
              </Tooltip>
            ) : (
              <Badge
                variant={status.variant}
                aria-label={t(`${credentialLabel}：${status.label}`, `${credentialLabel}: ${status.label}`)}
              >
                {status.label}
              </Badge>
            )}
            {cred.quota && (
              <UpstreamVerdict quota={cred.quota} credentialLabel={credentialLabel} />
            )}
            {isOrgAccount(cred) && (
              <Badge
                variant="warning"
                title={t(
                  `组织账号（${cred.org_type}）：额度由整个组织共享，与同档位的个人账号不是一回事`,
                  `Organisation account (${cred.org_type}): the quota is shared across the whole organisation, unlike a personal account on the same tier`,
                )}
              >
                {orgBadgeLabel(cred)}
              </Badge>
            )}
            {cred.tier && <Badge variant={tierBadgeVariant(cred.tier)}>{cred.tier}</Badge>}
            <Badge variant="outline" title={t('调度优先级，数值越小越优先', 'Scheduling priority; lower values are scheduled first')}>
              P{cred.priority}
            </Badge>
          </div>

          <section aria-label={t(`${credentialLabel} 的额度使用`, `Quota usage for ${credentialLabel}`)} className="space-y-2">
            <div className="flex flex-wrap items-start justify-between gap-x-3 gap-y-1.5">
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
              // 只有一个窗口时不留空半格：分两列却只填一格，看起来像另一半加载失败了。
              <div
                className={cn(
                  'grid gap-3',
                  has5h && has7d && '@sm/card:grid-cols-2 @sm/card:gap-4',
                )}
              >
                {has5h && (
                  <QuotaMeter
                    credentialLabel={credentialLabel}
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
                    credentialLabel={credentialLabel}
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
            {quota.extraWindows.length > 0 && <ExtraWindows windows={quota.extraWindows} />}
            {evaluation.modelCooling && (
              <p
                className="flex flex-wrap items-center gap-x-1.5 text-warning-foreground text-xs"
                title={t(
                  '这些模型刚被上游 429（多为容量限制或超额池满），暂时不参与选号；该账号的其余模型照常服务。到点自动恢复，也可在菜单里手动解除冷却',
                  'These models were just rate-limited upstream (usually capacity limits or an exhausted overage pool) and are temporarily skipped during account selection; this account keeps serving its other models. They recover automatically, or you can clear the cooldown from the menu',
                )}
              >
                <TimerOffIcon className="size-3.5" />
                <span className="font-medium">{t('模型冷却', 'Model cooldown')}</span>
                <span>{modelCooldownSummary(cred, language)}</span>
              </p>
            )}
          </section>
        </CardPanel>

        <CardFooter className="mt-auto grid grid-cols-[minmax(0,1fr)_auto] items-center gap-3 border-t bg-muted/32 px-4 py-2.5 sm:py-3">
          <div className="flex min-w-0 items-center gap-2 @sm/card:gap-4">
            <Button
              type="button"
              variant="ghost"
              className="w-fit max-w-full justify-start"
              onClick={() => setDevicesOpen(true)}
              title={t('查看已绑定设备', 'View bound devices')}
              aria-label={t(`查看 ${credentialLabel} 的已绑定设备`, `View bound devices for ${credentialLabel}`)}
              aria-haspopup="dialog"
            >
              <SmartphoneIcon />
              <span className="tabular-nums">{cred.device_count}/{effectiveLimit}</span>
              <Badge variant={devicePolicy.variant} size="sm">{devicePolicy.label}</Badge>
            </Button>
            <Separator orientation="vertical" className="h-5" />
            <span
              className="inline-flex shrink-0 items-center gap-1.5 whitespace-nowrap text-sm"
              title={t('累计等价 API 费用', 'Cumulative equivalent API cost')}
            >
              <WalletCardsIcon className="size-4 text-muted-foreground" aria-hidden />
              <span className="sr-only">{t('累计等价 API 费用', 'Cumulative equivalent API cost')}</span>
              <span className="font-medium tabular-nums">{formatUsd(cred.cost_total)}</span>
            </span>
            {/* 只在有流量时出现：闲置号上一个恒为 0 的 RPM 只是噪声，还会挤掉本就紧张的页脚宽度。 */}
            {cred.rpm > 0 && (
              <>
                <Separator orientation="vertical" className="h-5" />
                <span
                  className="inline-flex shrink-0 items-center gap-1.5 whitespace-nowrap text-sm"
                  title={t(
                    '当前 RPM：最近 60 秒经这个账号转发的请求数（含失败的）',
                    'Current RPM: requests forwarded through this account in the last 60 seconds (failures included)',
                  )}
                >
                  <ActivityIcon className="size-4 text-muted-foreground" aria-hidden />
                  <span className="sr-only">{t('当前 RPM', 'Current RPM')}</span>
                  <span className="font-medium tabular-nums">{cred.rpm}</span>
                  <span className="text-muted-foreground text-xs">RPM</span>
                </span>
              </>
            )}
          </div>
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
        </CardFooter>

        <CredentialProxyDialog
          cred={cred}
          open={proxyOpen}
          onOpenChange={setProxyOpen}
          proxy={actions.proxy}
        />
        <CredentialDevicesDialog
          cred={cred}
          open={devicesOpen}
          onOpenChange={setDevicesOpen}
          limit={limit}
        />
        <CredentialUsageDialog cred={cred} open={usageOpen} onOpenChange={setUsageOpen} />
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

/**
 * 这个「窗口」其实是个**可用性标记**而不是用量窗口。
 *
 * 上游的 `anthropic-ratelimit-unified-overage-status` 报的是「这个账号的 Usage credits
 * 能不能用」。Usage credits 是 Anthropic 官方术语（旧称 extra usage）：套餐包含的用量
 * 用完后不拦你，而是切成按标准 API 价的按量计费继续跑。
 *
 * `rejected` = 不可用，已知两种成因，而**我们区分不了**——上游只给状态词，成因没有对应的头：
 * `org_level_disabled`（没开启，见 proxy.rs `rate_limit_scope` 的抓包样例）与
 * `out_of_credits`（额度用光）。两种情况都没有可展示的可用能力，所以界面直接隐藏。
 *
 * 它没有 utilization，也没有 reset，和 `7d_oi` 那种真有用量的超额**池**是两回事——
 * 后者的 `rejected` 才是「这个池子满了/被拒了」。
 *
 * 判据用「没有 utilization 且名字里有 overage、但不是 `_oi` 结尾的池子」，而不是死等
 * `name === 'overage'`：窗口名是上游说了算的，将来多个 `overage_xxx` 也该走同一套解释。
 */
function isCapabilityWindow(w: QuotaWindowMeta): boolean {
  return w.rawUtilization == null && w.name.includes('overage') && !w.name.endsWith('_oi')
}

/**
 * 把上游的状态词翻成人话。**同一个 `rejected` 在两类窗口上含义完全不同**，所以不能共用一句：
 *
 * - 可用性标记（见 [`isCapabilityWindow`]）：只有 `allowed` / `allowed_warning` 才会渲染。
 *   `rejected` 可能是没开启，也可能是额度已用光，上游没有给成因；对用户而言都表示当前
 *   没有可用的 Usage credits，因此直接隐藏，不占用额度区域。
 * - 用量窗口（`7d_oi` 之类）：`rejected`/`rate_limited` = 这个池子确实被拒了，标红。
 *
 * 认不出的状态词原样显示，不猜——上游随时可能加新词，硬翻只会翻错。
 */
function windowStatusLabel(
  w: QuotaWindowMeta,
  t: (zh: string, en: string) => string,
): { text: string; bad: boolean } | null {
  const status = w.status
  if (!status) return null
  if (isCapabilityWindow(w)) {
    if (status === 'allowed' || status === 'allowed_warning') {
      return { text: t('可用', 'Available'), bad: false }
    }
    return null
  }
  if (status === 'rejected' || status === 'rate_limited') {
    return { text: t('已拒', 'rejected'), bad: true }
  }
  // 用量窗口的 allowed 是常态，不占地方。
  return null
}

/**
 * 5h / 7d 之外的窗口。`7d_oi` 已由卡片状态栏里的 Usage credits / 上游判定表达，
 * 这里不再重复显示；其余未来出现的额外窗口仍保留，避免静默丢失新类型。
 *
 * 刻意画成一行紧凑标签而不是第三、第四条进度条：这些窗口没有配套的窗口内费用与请求数
 * （那要靠 reset 反推窗口起点去聚合流水，只有 5h/7d 做得到），撑成同规格的进度条会让人
 * 以为下面那两个数字也是它的。窗口名原样显示，不翻译；仅过滤已被状态栏覆盖的 `7d_oi`。
 *
 * **但状态词要按窗口的种类翻译**，见 [`windowStatusLabel`]：同一个 `rejected` 在用量窗口上
 * 是「这个池子满了」，在 `overage` 那个可用性标记上却是「Usage credits 用不了」。
 */
function ExtraWindows({ windows }: { windows: QuotaWindowMeta[] }) {
  const { t, language } = useI18n()
  const visibleWindows = windows.filter((w) => (
    w.name.toLowerCase() !== '7d_oi'
    && (!isCapabilityWindow(w) || w.status === 'allowed' || w.status === 'allowed_warning')
  ))
  if (visibleWindows.length === 0) return null

  return (
    <div className="flex flex-wrap items-center gap-x-3 gap-y-1 text-xs text-muted-foreground">
      {visibleWindows.map((w) => {
        const pct = w.percentage
        const badge = windowStatusLabel(w, t)
        return (
          <span
            key={w.name}
            className="inline-flex items-center gap-1"
            title={[
              isCapabilityWindow(w)
                ? t(
                    `${w.name}：上游明确报告 Usage credits（套餐用量耗尽后的按量计费额度）可用；它不是用量窗口，所以没有百分比`,
                    `${w.name}: the upstream explicitly reports usage credits (pay-as-you-go beyond the plan's included usage) as available; this is not a usage window, so it has no percentage`,
                  )
                : t(`额度窗口 ${w.name}`, `Quota window ${w.name}`),
              w.status && t(`上游原值 ${w.status}`, `upstream raw value ${w.status}`),
              w.resetAt != null && t(
                `${formatFullTime(w.resetAt, language)} 重置`,
                `resets ${formatFullTime(w.resetAt, language)}`,
              ),
              !isCapabilityWindow(w) && t(
                '该窗口没有专用的窗口内费用与请求数统计',
                'this window has no per-window cost or request breakdown',
              ),
            ].filter(Boolean).join(' · ')}
          >
            <span className="font-medium text-foreground">{w.name}</span>
            {/* 没有 utilization 的窗口不摆百分比位：一个 `—` 会让人以为「数据缺失」，
                而开关式窗口本来就没有用量可言。 */}
            {pct != null && (
              <span className={cn('tabular-nums', badge?.bad && 'text-destructive-foreground font-medium')}>
                {pct}%
              </span>
            )}
            {badge && (
              <span className={badge.bad ? 'text-destructive-foreground' : undefined}>
                {badge.text}
              </span>
            )}
            {w.resetAt != null && (
              <span>{t(`· ${formatClockTime(w.resetAt, language)} 重置`, `· resets ${formatClockTime(w.resetAt, language)}`)}</span>
            )}
          </span>
        )
      })}
    </div>
  )
}

/**
 * 上游对**这个账号**的整体额度判决（`anthropic-ratelimit-unified-status`），
 * 以及它认为当前是哪个窗口在管事（`representative-claim`）。`allowed` 是常态，不占地方。
 *
 * 这个状态徽标是「5h / 7d 都没满，却被拒或动用了 Usage credits」时唯一能给出解释的东西：满掉的那个窗口
 * （实测多为超额池 `7d_oi`）后端只用来判冷却、并不落库，所以卡片上没有它的进度条可看，
 * 但上游的判决与它的名字是在快照里的。缺了这个状态，那种账号在界面上就是「一切正常却在烧钱」。
 */
function UpstreamVerdict({
  quota,
  credentialLabel,
}: {
  quota: NonNullable<Credential['quota']>
  credentialLabel: string
}) {
  const { t, language } = useI18n()
  const status = quota.unified_status
  if (!status || status === 'allowed') return null
  const destructive = status === 'rejected' || status === 'rate_limited'
  const statusLabel = unifiedQuotaStatusLabel(status, language)
  const badgeLabel = t(`上游 · ${statusLabel}`, `Upstream · ${statusLabel}`)
  const verdictTitle = status === 'rate_limited'
    ? t(
        '上游正在限流该账号，请等待相关额度窗口恢复后再重试',
        'The upstream is rate-limiting this account; retry after the related quota window recovers',
      )
    : t(
        status === 'rejected'
          ? '上游拒绝了这次请求：该账号至少有一个额度窗口已耗尽'
          : '上游放行但已发出预警：该账号有额度窗口接近耗尽',
        status === 'rejected'
          ? 'The upstream rejected the request: at least one quota window for this account is exhausted'
          : 'The upstream allowed the request but issued a warning: a quota window is close to exhaustion',
      )
  const representativeDetail = quota.rl_representative
    ? t(
        `上游称当前起约束作用的是 ${quota.rl_representative} 窗口。若它不在 5h / 7d 里，说明这是一个未被记录的窗口（多为超额池），卡片上没有对应的进度条`,
        `The upstream reports the ${quota.rl_representative} window as the binding constraint. If it is not among the 5h / 7d windows, it is an unrecorded window (typically the overage pool) and has no meter on this card`,
      )
    : null
  const detail = representativeDetail
    ? t(`${verdictTitle}。${representativeDetail}`, `${verdictTitle}. ${representativeDetail}`)
    : verdictTitle

  return (
    <Tooltip>
      <TooltipTrigger
        className={cn(
          badgeVariants({ variant: destructive ? 'error' : 'warning' }),
          'cursor-help',
        )}
        delay={0}
        aria-label={t(
          `${credentialLabel}：${badgeLabel}。${detail}`,
          `${credentialLabel}: ${badgeLabel}. ${detail}`,
        )}
      >
        {badgeLabel}
      </TooltipTrigger>
      <TooltipPopup
        side="bottom"
        align="start"
        className="max-w-80 whitespace-normal break-words text-left leading-5"
      >
        {detail}
      </TooltipPopup>
    </Tooltip>
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
        className="flex w-full flex-col gap-1.5"
        title={t(
          `${reason}。最后一次快照：${formatFullTime(snapshotTs, language)}`,
          `${reason}. Latest snapshot: ${formatFullTime(snapshotTs, language)}`,
        )}
      >
        <div className="flex items-center justify-between gap-2">
          <p className="font-medium text-sm">{label}</p>
          <span className="shrink-0 text-xs text-muted-foreground">
            {expired ? t('已重置', 'Reset') : t('暂无数据', 'No data')}
          </span>
        </div>
        <div className="h-1.5 w-full bg-input" aria-hidden />
        <div className="flex min-w-0 items-center justify-between gap-2 text-xs text-muted-foreground">
          <span>{expired ? t('暂无新用量', 'No new usage') : t('等待上游返回', 'Waiting for upstream')}</span>
          {expired && reset != null && (
            <span
              className="shrink-0 whitespace-nowrap tabular-nums"
              title={t(`${formatFullTime(reset, language)} 已重置`, `Reset at ${formatFullTime(reset, language)}`)}
            >
              {t(`重置于 ${formatClockTime(reset, language)}`, `Reset ${formatClockTime(reset, language)}`)}
            </span>
          )}
        </div>
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
    <Meter value={percentage} max={100} className="gap-1.5">
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
      <MeterTrack className="h-1.5">
        <MeterIndicator className={indicatorClass} />
      </MeterTrack>
      <dl className="flex min-w-0 items-baseline justify-between gap-1 text-xs leading-none text-muted-foreground @sm/card:text-[10px] @lg/card:gap-2 @lg/card:text-xs">
        <div className="inline-flex min-w-0 items-baseline gap-0.5 whitespace-nowrap @lg/card:gap-1">
          <dt
            className="shrink-0"
            title={t('请求数', requests === 1 ? 'Request' : 'Requests')}
          >
            {t('请求', 'Req')}
          </dt>
          <dd className="shrink-0 tabular-nums">
            {requests == null ? '—' : requests.toLocaleString(locale)}
          </dd>
        </div>
        <div className="inline-flex min-w-0 items-baseline gap-0.5 whitespace-nowrap @lg/card:gap-1">
          <dt className="shrink-0">{t('花费', 'Cost')}</dt>
          <dd className="shrink-0 tabular-nums">
            {cost == null ? '—' : formatUsd(cost)}
          </dd>
        </div>
        <div className="inline-flex min-w-0 items-baseline gap-0.5 whitespace-nowrap @lg/card:gap-1">
          <dt className="shrink-0">{t('重置', 'Reset')}</dt>
          <dd
            className="shrink-0 tabular-nums"
            title={reset != null
              ? t(`${formatFullTime(reset, language)} 重置`, `Resets ${formatFullTime(reset, language)}`)
              : undefined}
          >
            {reset == null ? '—' : formatClockTime(reset, language)}
          </dd>
        </div>
      </dl>
    </Meter>
  )
}
