import { memo, useState } from 'react'
import {
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
  formatCompactNumber,
  formatCountdown,
  formatFullTime,
  formatUsd,
  relativeTime,
} from '@/lib/utils'
import {
  ConnectivityTestDialog,
  CredentialMenuContent,
  DeferredMount,
  DeleteCredentialDialog,
  deviceUsageMeta,
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
  type QuotaWindowMeta,
} from '@/components/credential-shared'
import { CredentialDevicesDialog } from '@/components/credential-devices-dialog'
import { CredentialProxyDialog } from '@/components/credential-proxy-dialog'
import { CredentialRpmDialog } from '@/components/credential-rpm-dialog'
import { CredentialUsageDialog } from '@/components/credential-usage-dialog'
import { Avatar, AvatarFallback } from '@/components/ui/avatar'
import { Badge, badgeVariants, type BadgeProps } from '@/components/ui/badge'
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

/**
 * memo 的收益在于「列表本身没变，但父组件重渲染了」这类情况：搜索框每敲一个字、
 * 勾选任意一行、翻页动画，都会重跑一遍工作区。配合稳定的 onSelectedChange 才生效。
 */
export const CredentialCard = memo(function CredentialCard({
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
  const [editing, setEditing] = useState(false)
  const [name, setName] = useState(cred.label)
  const [devicesOpen, setDevicesOpen] = useState(false)
  const [proxyOpen, setProxyOpen] = useState(false)
  const [rpmOpen, setRpmOpen] = useState(false)
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
  // 0 = 不限，此时页脚只显示 RPM 本身，不画分母、也不谈「打满」。
  const rpmLimit = cred.rpm_limit_effective
  const rpmFull = rpmLimit > 0 && cred.rpm >= rpmLimit
  // 设备名额占用的配色与说明：空闲灰 / 健康绿 / 吃紧黄 / 占满红，见 [deviceUsageMeta]。
  const deviceUsage = deviceUsageMeta(cred.device_count, cred.device_limit_effective)
  const deviceUsageHint = (() => {
    if (cred.device_count <= 0) {
      return t(
        '还没有设备绑定到这个账号。点击查看',
        'No devices are bound to this account yet. Click to view',
      )
    }
    if (cred.device_limit_effective <= 0) {
      return t(
        `已绑定 ${cred.device_count} 台设备，未设上限。点击查看`,
        `${cred.device_count} bound device(s), no limit set. Click to view`,
      )
    }
    if (deviceUsage.level === 'critical') {
      return t(
        `设备名额已占满（${cred.device_count}/${cred.device_limit_effective}）：新设备会被分到别的账号，全部占满时收到 429。点击查看`,
        `Device slots are full (${cred.device_count}/${cred.device_limit_effective}): new devices go to another account, and get a 429 once every account is full. Click to view`,
      )
    }
    return t(
      `已占用 ${cred.device_count}/${cred.device_limit_effective} 个设备名额。点击查看`,
      `${cred.device_count} of ${cred.device_limit_effective} device slots in use. Click to view`,
    )
  })()
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
          `账号已停用；${quotaSnapshotTime} 的用量快照记录了 Usage credits，当前不纳入调度风险统计`,
          `The account is disabled; the ${quotaSnapshotTime} usage snapshot recorded usage credits and is excluded from current scheduling-risk totals`,
        ),
      }
    }
    if (quota.overage === 'historical') {
      return {
        label: t('近期用过 Usage credits', 'Recently used usage credits'),
        variant: 'warning' as const,
        title: t(
          '最近的用量快照记录了 Usage credits，但相关用量窗口已经重置',
          'The latest usage snapshot recorded usage credits, but the related usage windows have reset',
        ),
      }
    }
    if (quota.overage === 'active' && status.kind !== 'overage') {
      return {
        label: t('Usage credits 生效中', 'Usage credits active'),
        variant: 'error' as const,
        title: t(
          `${quotaSnapshotTime} 的用量快照显示套餐用量已耗尽，正由 Usage credits 按标准 API 价放行请求`,
          `The ${quotaSnapshotTime} usage snapshot shows the plan's included usage exhausted and requests being served by usage credits at standard API rates`,
        ),
      }
    }
    if (quota.overage === 'unknown' && status.kind !== 'overage-unknown') {
      return {
        label: t('Usage credits 待确认', 'Usage credits unconfirmed'),
        variant: 'warning' as const,
        title: t(
          `${quotaSnapshotTime} 的用量快照记录了 Usage credits，当前状态仍需确认`,
          `The ${quotaSnapshotTime} usage snapshot recorded usage credits; the current state still needs confirmation`,
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
                    onCheckedChange={(checked) => onSelectedChange?.(cred.id, checked)}
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
                  <CardDescription className="mt-1 flex min-w-0 flex-wrap items-center gap-x-1.5 gap-y-0.5 text-2xs font-normal">
                    <span className="tabular-nums">#{cred.id}</span>
                    <span aria-hidden="true">·</span>
                    <span
                      className="inline-flex min-w-0 items-center gap-1"
                      title={t(
                        `添加于 ${formatFullTime(cred.created_at, language)}`,
                        `Added ${formatFullTime(cred.created_at, language)}`,
                      )}
                    >
                      <CalendarDaysIcon className="size-3 shrink-0" />
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
                  onRpmLimit={() => setRpmOpen(true)}
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
                size="sm"
                title={t(
                  `组织账号（${cred.org_type}）：用量由整个组织共享，与同档位的个人账号不是一回事`,
                  `Organisation account (${cred.org_type}): the usage is shared across the whole organisation, unlike a personal account on the same tier`,
                )}
              >
                {orgBadgeLabel(cred)}
              </Badge>
            )}
            {cred.tier && <Badge variant={tierBadgeVariant(cred.tier)} size="sm">{cred.tier}</Badge>}
            <Badge variant="outline" size="sm" title={t('调度优先级，数值越小越优先', 'Scheduling priority; lower values are scheduled first')}>
              P{cred.priority}
            </Badge>
          </div>

          <section aria-label={t(`${credentialLabel} 的用量限制`, `Usage limits for ${credentialLabel}`)} className="space-y-2">
            <div className="flex flex-wrap items-start justify-between gap-x-3 gap-y-1.5">
              <div className="flex flex-wrap items-center gap-2">
                <h4 className="font-medium text-xs text-muted-foreground">{t('用量限制', 'Usage limits')}</h4>
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
                <Tooltip>
                  <TooltipTrigger
                    render={<span />}
                    className="inline-flex items-center gap-1 text-2xs text-muted-foreground"
                  >
                    <ClockIcon className="size-3" />
                    {t(
                      `更新于 ${relativeTime(cred.quota.ts, now, language)}`,
                      `Updated ${relativeTime(cred.quota.ts, now, language)}`,
                    )}
                  </TooltipTrigger>
                  <TooltipPopup>{formatFullTime(cred.quota.ts, language)}</TooltipPopup>
                </Tooltip>
              ) : (
                <span className="text-2xs text-muted-foreground">{t('暂无数据', 'No data')}</span>
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
                    // 标签用 `5h`/`7d` 而不是「5 小时」：这一行现在还挤着进度条、百分比与
                    // 重置时刻，长标签会把进度条压没；完整称呼在读屏文本里。
                    label="5h"
                    windowVariant="info"
                    util={quota.h5.utilization}
                    reset={cred.quota.rl_5h_reset}
                    cost={cred.quota.cost_5h}
                    requests={cred.quota.requests_5h}
                    snapshotTs={cred.quota.ts}
                    now={now}
                  />
                )}
                {has7d && (
                  <QuotaMeter
                    credentialLabel={credentialLabel}
                    label="7d"
                    windowVariant="success"
                    util={quota.d7.utilization}
                    reset={cred.quota.rl_7d_reset}
                    cost={cred.quota.cost_7d}
                    requests={cred.quota.requests_7d}
                    snapshotTs={cred.quota.ts}
                    now={now}
                  />
                )}
              </div>
            ) : cred.quota ? (
              <p className="text-xs text-muted-foreground">{t('上游尚未返回用量窗口。', 'The upstream has not returned usage windows yet.')}</p>
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
                <TimerOffIcon className="size-3" />
                <span className="font-medium">{t('模型冷却', 'Model cooldown')}</span>
                <span>{modelCooldownSummary(cred, language)}</span>
              </p>
            )}
          </section>
        </CardPanel>

        <CardFooter className="mt-auto grid grid-cols-[minmax(0,1fr)_auto] items-center gap-3 border-t bg-muted/32 px-4 py-2.5 sm:py-3">
          <div className="flex min-w-0 items-center gap-2 @sm/card:gap-4">
            {/* 页脚这几项统一用 Tooltip 组件而不是原生 title：原生提示有约 1 秒延迟、
                触屏上完全出不来，样式也不受控，和卡片上方的状态提示不是一套东西。 */}
            <Tooltip>
              <TooltipTrigger
                className={cn(buttonVariants({ variant: 'ghost' }), 'w-fit max-w-full justify-start')}
                onClick={() => setDevicesOpen(true)}
                aria-label={t(`查看 ${credentialLabel} 的已绑定设备`, `View bound devices for ${credentialLabel}`)}
                aria-haspopup="dialog"
              >
                <SmartphoneIcon />
                {/* 计数本身带底色（绿 / 黄 / 红），颜色只看名额占用，见 [deviceUsageMeta]：
                    页脚这一行全是中性色数字，光靠 `3/5` 得逐个念才知道哪个号快满了。 */}
                <Badge variant={deviceUsage.variant} size="sm" className="tabular-nums">
                  {cred.device_count}/{effectiveLimit}
                </Badge>
                <Badge variant={devicePolicy.variant} size="sm">{devicePolicy.label}</Badge>
              </TooltipTrigger>
              <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
                {deviceUsageHint}
              </TooltipPopup>
            </Tooltip>
            <Separator orientation="vertical" className="h-5" />
            <Tooltip>
              <TooltipTrigger
                render={<span />}
                className="inline-flex shrink-0 items-center gap-1.5 whitespace-nowrap text-xs"
              >
                <WalletCardsIcon className="size-3.5 text-muted-foreground" aria-hidden />
                <span className="sr-only">{t('累计等价 API 费用', 'Cumulative equivalent API cost')}</span>
                <span className="font-medium tabular-nums">{formatUsd(cred.cost_total)}</span>
              </TooltipTrigger>
              <TooltipPopup>{t('累计等价 API 费用', 'Cumulative equivalent API cost')}</TooltipPopup>
            </Tooltip>
            {/* 只在有流量时出现：闲置号上一个恒为 0 的 RPM 只是噪声，还会挤掉本就紧张的页脚宽度。 */}
            {cred.rpm > 0 && (
              <>
                <Separator orientation="vertical" className="h-5" />
                <Tooltip>
                  <TooltipTrigger
                    render={<span />}
                    className="inline-flex shrink-0 items-center gap-2 whitespace-nowrap text-xs"
                  >
                    {/* 页脚里唯一的实时值（隔壁两个都是累计量），用呼吸点替掉图标把「活的」画出来。
                        绿色只落在这个 6px 点上：数值本身无好坏之分，颜色留给状态（运行正常 / 冷却）。 */}
                    <span className="relative flex size-1.5" aria-hidden>
                      <span className="absolute inline-flex size-full animate-ping rounded-full bg-success opacity-60 motion-reduce:hidden" />
                      <span className="relative inline-flex size-1.5 rounded-full bg-success" />
                    </span>
                    <span className="sr-only">{t('当前 RPM', 'Current RPM')}</span>
                    <span className="inline-flex items-baseline gap-1">
                      <span className={cn('font-medium tabular-nums', rpmFull && 'text-warning')}>
                        {cred.rpm}
                        {rpmLimit > 0 && <span className="text-muted-foreground">/{rpmLimit}</span>}
                      </span>
                      <span className="text-2xs text-muted-foreground tracking-wide">RPM</span>
                    </span>
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

        {/* 没点开过任何一个就一个都不挂：账号一多，这些常关的对话框全是白挂的组件树。 */}
        <DeferredMount open={proxyOpen || devicesOpen || usageOpen || confirmDelete || rpmOpen || testing}>
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
        </DeferredMount>
      </Card>
    </li>
  )
})

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
                    `${w.name}：上游明确报告 Usage credits（套餐用量耗尽后的按量计费用量）可用；它不是用量窗口，所以没有百分比`,
                    `${w.name}: the upstream explicitly reports usage credits (pay-as-you-go beyond the plan's included usage) as available; this is not a usage window, so it has no percentage`,
                  )
                : t(`用量窗口 ${w.name}`, `Usage limits window ${w.name}`),
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
        '上游正在限流该账号，请等待相关用量窗口恢复后再重试',
        'The upstream is rate-limiting this account; retry after the related usage window recovers',
      )
    : t(
        status === 'rejected'
          ? '上游拒绝了这次请求：该账号至少有一个用量窗口已耗尽'
          : '上游放行但已发出预警：该账号有用量窗口接近耗尽',
        status === 'rejected'
          ? 'The upstream rejected the request: at least one usage window for this account is exhausted'
          : 'The upstream allowed the request but issued a warning: a usage window is close to exhaustion',
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

/**
 * 额度里的一项事实（请求数、花费）：浅灰小块，值在前、单位在后。
 *
 * 做成块而不是「标签: 值」的文本对——卡片上这行要能一眼扫过去，标签在小字号下只是噪声，
 * 真要确认是什么，`title` 与读屏文本都写着全称。
 */
function QuotaFact({
  label,
  value,
  suffix,
  title,
}: {
  label: string
  value: string
  suffix?: string
  title?: string
}) {
  return (
    <div
      className={cn(
        badgeVariants({ variant: 'secondary', size: 'sm' }),
        'min-w-0 gap-0.5 font-normal',
      )}
      title={title ? `${label}: ${title}` : label}
    >
      <dt className="sr-only">{label}</dt>
      <dd className="truncate tabular-nums">{value}</dd>
      {suffix && <span className="text-muted-foreground" aria-hidden>{suffix}</span>}
    </div>
  )
}

function QuotaMeter({
  credentialLabel,
  label,
  windowVariant,
  util,
  reset,
  cost,
  requests,
  snapshotTs,
  now,
}: {
  credentialLabel: string
  label: string
  /** 窗口标签的固定配色（分类色，与占用无关）：5h 一色、7d 一色。 */
  windowVariant: BadgeProps['variant']
  util: number | null
  reset: number | null
  cost: number | null
  requests: number | null
  snapshotTs: number
  /** 页面时钟（30 秒一跳），倒计时靠它走，见 [formatCountdown]。 */
  now: number
}) {
  const { t, language, locale } = useI18n()
  // 窗口重置后上游那份 utilization 就作废了（[evaluateQuotaWindow] 把它抹成 null），此时
  // 这个窗口的用量确实归了零——直接按 0% 画，不再单独摆一句「已重置 / 暂无数据」。那句话
  // 占着和数据一样大的地方，说的却只是「这里没什么可看」。倒计时同理：没有未来的重置时刻
  // 就整段不出现，而不是留个「—」占位。
  const percentage = quotaPercentage(util) ?? 0
  const level = quotaLevel(util)
  const indicatorClass = level === 'critical'
    ? 'bg-destructive'
    : level === 'warning'
      ? 'bg-warning'
      : 'bg-success'

  const valueClass = level === 'critical'
    ? 'text-destructive'
    : level === 'warning'
      ? 'text-warning-foreground'
      : 'text-foreground'

  return (
    <Meter value={percentage} max={100} className="gap-1.5">
      {/* 数据先行、进度条随后：请求数与花费是「这个窗口里发生了什么」，百分比是「还剩多少」。
          两组分行排，比原先挤在一行的三列 dl 好扫——那一行里三个标签三个值交替出现，
          眼睛得逐个配对。 */}
      <dl className="flex min-w-0 flex-wrap items-center gap-1">
        <QuotaFact
          label={t('请求数', 'Requests')}
          value={requests == null ? '—' : formatCompactNumber(requests, locale)}
          title={requests == null ? undefined : requests.toLocaleString(locale)}
          suffix="req"
        />
        <QuotaFact
          label={t('等价 API 费用', 'Equivalent API cost')}
          value={cost == null ? '—' : formatUsd(cost)}
        />
      </dl>
      <div className="flex min-w-0 items-center gap-1.5">
        {/* 窗口名做成固定色的小标签（5h / 7d 各一色）：它是分类而不是状态，配色跟右边那组
            表示占用的红黄绿分开，两侧各管一件事。 */}
        <MeterLabel
          className={cn(badgeVariants({ variant: windowVariant, size: 'sm' }), 'shrink-0 tabular-nums')}
        >
          <span className="sr-only">{t(`${credentialLabel} 的 `, `${credentialLabel} `)}</span>
          {label}
          <span className="sr-only">{t('用量', 'usage')}</span>
        </MeterLabel>
        <MeterTrack className="h-1.5 min-w-6 flex-1 rounded-full">
          <MeterIndicator className={cn(indicatorClass, 'rounded-full')} />
        </MeterTrack>
        <MeterValue
          className={cn('shrink-0 font-medium text-xs', valueClass)}
          title={t(`快照于 ${formatFullTime(snapshotTs, language)}`, `Snapshot at ${formatFullTime(snapshotTs, language)}`)}
        >
          {() => `${percentage}%`}
        </MeterValue>
        {/* 距离重置还有多久。倒计时靠页面那个 30 秒 tick 走（见 useNowSeconds），不会冻住；
            精确到分秒的绝对时刻放在 title 里——倒计时受本地时钟偏差影响，只适合看个大概。 */}
        {reset != null && reset > now && (
          <span
            className="shrink-0 whitespace-nowrap text-2xs text-muted-foreground tabular-nums"
            title={t(`${formatFullTime(reset, language)} 重置`, `Resets ${formatFullTime(reset, language)}`)}
          >
            {formatCountdown(reset, now)}
          </span>
        )}
      </div>
    </Meter>
  )
}
