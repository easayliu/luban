import { memo, useState, type ElementType, type ReactNode } from 'react'
import {
  CalendarDaysIcon,
  CheckIcon,
  ClockIcon,
  BanIcon,
  Building2Icon,
  EllipsisIcon,
  GaugeIcon,
  GlobeIcon,
  MessagesSquareIcon,
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
  formatTokens,
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
  modelDenialSummary,
  proxyLabelParts,
  quotaLevel,
  isOrgAccount,
  METER_FILL,
  orgBadgeLabel,
  quotaPercentage,
  switchTitle,
  tierBadgeVariant,
  unifiedQuotaStatusLabel,
  useCredentialActions,
  type QuotaLevel,
  type QuotaWindowMeta,
} from '@/components/credential-shared'
import { CredentialDevicesDialog } from '@/components/credential-devices-dialog'
import { CredentialProxyDialog } from '@/components/credential-proxy-dialog'
import { CredentialRpmDialog } from '@/components/credential-rpm-dialog'
import { CredentialQuotaDialog } from '@/components/credential-quota-dialog'
import { CredentialUsageDialog } from '@/components/credential-usage-dialog'
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
import { Spinner } from '@/components/ui/spinner'
import { Switch } from '@/components/ui/switch'
import { Tooltip, TooltipPopup, TooltipTrigger } from '@/components/ui/tooltip'

/**
 * 页脚三格名额读数（设备 / 会话 / RPM）的定宽：2.25rem，数字左对齐。
 *
 * 读数若按内容伸缩，`1/10` 与 `0/100` 宽度不同，后面那格就跟着往左右挪——一列卡片叠下来，
 * 会话图标、RPM 图标各在各的位置上，竖着扫过去是锯齿状的。定宽之后每格等宽，卡片之间对齐，
 * 数字也都从同一处起读。2.25rem 够放 `0/100`（手机 12px 字号），再长才会把它撑开。
 * `@xs/card`（卡片窄于 320px）时放弃定宽，让这一行在极窄屏上还能自己收进去。
 */
const SLOT_WIDTH = '@xs/card:min-w-9'

/**
 * 名额占用 → 数字的颜色。空闲灰、健康绿、吃紧黄、占满红，判定见 [deviceUsageMeta]。
 *
 * 这个编码原来由实心徽章的底色承担。改成给数字本身上色是 Cloudflare 那套读数条的做法：
 * 实心胶囊留给状态（运行正常 / 封禁 / 套餐），计量值只着色不加底板——底板要 padding、要行高，
 * 四格并排就是页脚换行的直接原因。颜色编码一格没少，吃掉的高度没了。
 */
const SLOT_TEXT: Record<QuotaLevel, string> = {
  empty: 'text-muted-foreground',
  ok: 'text-success-foreground',
  warning: 'text-warning-foreground',
  critical: 'text-destructive-foreground',
}

/**
 * 页脚读数条里的一格：图标 + 一串数字，整格可点开对应的对话框。
 *
 * 刻意**不做成按钮**。按钮的高度由控件尺寸定（`h-9`＝36px），四格并排又把横向 padding 吃光，
 * 手机上只能换行——页脚一路涨到 96px，比它上面那块用量区还高。Cloudflare 的同类读数条走另一条路：
 * 读数是文本，高度由文本行高定，padding 只在容器上给一次；36–44px 的尺寸留给真正的控件
 * （这里就是最右那枚开关）。照这个口径，手机上页脚从 96px（换行）/ 56px（单行按钮）降到 38px。
 *
 * 触控不打折：视觉上是文本，命中区靠 `pointer-coarse:after:*` 以自身为中心撑到 44×44——
 * 仓库里 Badge / Button / Switch 用的同一套，指针设备上压根不生成，不占位也不挡相邻格。
 */
function FooterStat({
  icon: Icon,
  iconClassName,
  valueClassName,
  value,
  hint,
  ariaLabel,
  srLabel,
  onClick,
}: {
  icon: ElementType<{ className?: string }>
  /** 图标颜色＝名额策略（跟随默认 / 自定义 / 不限），与数字上的占用色是两回事。 */
  iconClassName?: string
  valueClassName?: string
  value: ReactNode
  hint: ReactNode
  ariaLabel: string
  srLabel?: string
  onClick: () => void
}) {
  return (
    <Tooltip>
      <TooltipTrigger
        className={cn(
          'relative flex min-w-0 shrink items-center gap-1 rounded-sm text-left font-medium text-xs tabular-nums outline-none',
          'transition-colors hover:underline hover:underline-offset-4 focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background',
          'pointer-coarse:after:absolute pointer-coarse:after:top-1/2 pointer-coarse:after:left-1/2 pointer-coarse:after:size-full pointer-coarse:after:min-h-11 pointer-coarse:after:min-w-11 pointer-coarse:after:-translate-x-1/2 pointer-coarse:after:-translate-y-1/2',
        )}
        onClick={onClick}
        aria-label={ariaLabel}
        aria-haspopup="dialog"
      >
        <Icon className={cn('size-3.5 shrink-0', iconClassName)} />
        <span className={cn('min-w-0 truncate', valueClassName)}>{value}</span>
        {srLabel && <span className="sr-only">{srLabel}</span>}
      </TooltipTrigger>
      <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">{hint}</TooltipPopup>
    </Tooltip>
  )
}

/** 分母（`/5`、`/∞`）压成弱色：一眼先读到的该是当前值，上限是背景信息。 */
function SlotLimit({ limit }: { limit: number | string }) {
  return <span className="font-normal text-muted-foreground">/{limit}</span>
}

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
  const [quotaOpen, setQuotaOpen] = useState(false)
  const [usageOpen, setUsageOpen] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [testing, setTesting] = useState(false)

  const actions = useCredentialActions(cred, () => setEditing(false))
  const { rename, toggle, limit } = actions
  const evaluation = evaluateCredential(cred, now, language)
  const { quota, status } = evaluation
  const credentialLabel = displayCredentialLabel(cred.label, language)
  // 只渲染上游真报过的窗口。卡片是弹性布局，没有的那个直接不占位；表格那边列宽固定，
  // 摘不掉，所以改成显式的「无此窗口」，见 credential-row 的 ListQuotaMeter。
  const has5h = quota.h5.reported
  const has7d = quota.d7.reported
  const effectiveLimit = cred.device_limit_effective > 0 ? cred.device_limit_effective : '∞'
  // 0 = 不限，此时页脚只显示 RPM 本身，不画分母、也不谈「打满」。
  const rpmLimit = cred.rpm_limit_effective
  // RPM 徽章的配色与策略，与设备 / 会话两枚同一套判定，三枚并排读法一致。
  const rpmUsage = deviceUsageMeta(cred.rpm, rpmLimit)
  const rpmPolicy = cred.rpm_limit === 0
    ? { label: t('跟随默认', 'Default'), className: 'text-muted-foreground' }
    : cred.rpm_limit < 0
      ? { label: t('不限', 'Unlimited'), className: 'text-foreground' }
      : { label: t('自定义', 'Custom'), className: 'text-info-foreground' }
  const rpmPolicyHint = cred.rpm_limit === 0
    ? t('上限跟随全局默认', 'the limit follows the global default')
    : cred.rpm_limit < 0
      ? t('这个账号不限 RPM', 'this account has no RPM limit')
      : t(`这个账号自定义了上限 ${cred.rpm_limit}`, `this account overrides the limit to ${cred.rpm_limit}`)
  const sessionEffectiveLimit = cred.session_limit_effective > 0 ? cred.session_limit_effective : '∞'
  // 设备名额占用的配色与说明：空闲灰 / 健康绿 / 吃紧黄 / 占满红，见 [deviceUsageMeta]。
  const deviceUsage = deviceUsageMeta(cred.device_count, cred.device_limit_effective)
  // 模拟会话名额同一套判定与配色：走模拟路径、没有设备身份的来访按会话占名额，与设备分开计。
  const sessionUsage = deviceUsageMeta(cred.session_count, cred.session_limit_effective)
  const sessionPolicy = cred.session_limit === 0
    ? { label: t('跟随默认', 'Default'), className: 'text-muted-foreground' }
    : cred.session_limit < 0
      ? { label: t('不限', 'Unlimited'), className: 'text-foreground' }
      : { label: t('自定义', 'Custom'), className: 'text-info-foreground' }
  const sessionUsageHint = cred.session_limit_effective <= 0
    ? t(
        `${cred.session_count} 条活跃模拟会话，未设上限。点击查看或清理`,
        `${cred.session_count} active simulated session(s), no limit set. Click to view or clear`,
      )
    : sessionUsage.level === 'critical'
      ? t(
          `模拟会话名额已占满（${cred.session_count}/${cred.session_limit_effective}）：新会话会被分到别的账号，全部占满时收到 429。点击查看或清理`,
          `Session slots are full (${cred.session_count}/${cred.session_limit_effective}): new sessions go to another account, and get a 429 once every account is full. Click to view or clear`,
        )
      : t(
          `已占用 ${cred.session_count}/${cred.session_limit_effective} 个模拟会话名额（走模拟路径、没有设备身份的来访按会话占名额）。点击查看或清理`,
          `${cred.session_count} of ${cred.session_limit_effective} simulated session slots in use (requests on the simulation path without a device identity take one per session). Click to view or clear`,
        )
  // 名额策略不再占页脚的横向宽度（那点宽度让给右边的 RPM 数字），改成给前面那枚手机图标上色：
  // 淡灰＝跟随全局默认，蓝＝这个账号单独改过上限，深色＝不限设备数（旁边的分母就是 `∞`）。
  // 三档都躲开绿 / 黄 / 红：那三色紧挨着就是名额占用徽章的语义，同色不同义最容易读错。
  // 颜色只是提个醒，谁是谁全写在 [devicePolicyHint]、悬浮提示和读屏文本里。
  const devicePolicy = cred.device_limit === 0
    ? { label: t('跟随默认', 'Default'), className: 'text-muted-foreground' }
    : cred.device_limit < 0
      ? { label: t('不限', 'Unlimited'), className: 'text-foreground' }
      : { label: t('自定义', 'Custom'), className: 'text-info-foreground' }
  const devicePolicyHint = cred.device_limit === 0
    ? t('名额上限跟随全局默认', 'The slot limit follows the global default')
    : cred.device_limit < 0
      ? t('这个账号不限设备数', 'This account has no device limit')
      : t(`这个账号自定义了上限 ${cred.device_limit}`, `This account overrides the limit to ${cred.device_limit}`)
  const deviceUsageHint = (() => {
    if (cred.device_count <= 0) {
      return t(
        `还没有设备绑定到这个账号，${devicePolicyHint}。点击查看`,
        `No devices are bound to this account yet; ${devicePolicyHint.toLowerCase()}. Click to view`,
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
        `设备名额已占满（${cred.device_count}/${cred.device_limit_effective}，${devicePolicyHint}）：新设备会被分到别的账号，全部占满时收到 429。点击查看`,
        `Device slots are full (${cred.device_count}/${cred.device_limit_effective}; ${devicePolicyHint.toLowerCase()}): new devices go to another account, and get a 429 once every account is full. Click to view`,
      )
    }
    return t(
      `已占用 ${cred.device_count}/${cred.device_limit_effective} 个设备名额，${devicePolicyHint}。点击查看`,
      `${cred.device_count} of ${cred.device_limit_effective} device slots in use; ${devicePolicyHint.toLowerCase()}. Click to view`,
    )
  })()
  /**
   * 页脚那枚金额的悬浮提示：主数是累计，后面补上 5h / 7d 两个窗口的费用。
   *
   * 页脚只放得下一个数，而「这号一共烧了多少」与「这一阵烧得快不快」是两个问题：前者决定要不要
   * 再养着它，后者才解释今天为什么慢。累计当主数（任何时候都有值，不依赖上游快照），两个窗口的
   * 值放进提示里——它们在上面的用量区各自有 pill，这里只是免去上下对照。
   *
   * 末尾那句必须留着：这个数是按公开价目表拿 token 估的，不是账单，两者对不上是正常的。
   */
  const costHint = (() => {
    const parts = [t(`累计 ${formatUsd(cred.cost_total)}`, `Total ${formatUsd(cred.cost_total)}`)]
    if (cred.quota?.cost_5h != null) parts.push(`5h ${formatUsd(cred.quota.cost_5h)}`)
    if (cred.quota?.cost_7d != null) parts.push(`7d ${formatUsd(cred.quota.cost_7d)}`)
    return t(
      `${parts.join(' · ')}。按公开价目表估算的等价 API 费用，不是账单金额。点击查看请求明细`,
      `${parts.join(' · ')}. Equivalent API cost estimated from the public price list, not a bill. Click to view the request log`,
    )
  })()
  const titleId = `credential-card-title-${cred.id}`
  // 所有需处理状态都用同一种渐进披露：卡片只显示状态，详情在悬浮提示里查看。
  // 避免同一条状态再渲染一块说明，把异常卡片单独撑高。
  const statusUsesTooltip = status.attention
  const added = relativeTime(cred.created_at, now, language)
  const proxyLabel = cred.proxy ? proxyLabelParts(cred.proxy) : null
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
    // historical：超额池窗口已重置，情况已经结束，不再显示徽章——避免与「运行正常」矛盾。
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
        {/* 账号卡片的槽宽固定 16px，不跟全站那档「手机 16 / ≥640 20」走：
            这张卡在网格里通常只有 300–600px 宽，里面「5h 1 req 24.8K tok $0.018 … 0%」
            那一行是逐字算过的，两侧各多 4px 就会把 `$0.018` 挤到第二行。
            页面级卡片（工作区、设置面板）才用 20px。 */}
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
                <div className="min-w-0 flex-1">
                  <h3
                    id={titleId}
                    className="block min-w-0 truncate whitespace-nowrap leading-snug"
                    title={credentialLabel}
                  >
                    {credentialLabel}
                  </h3>
                  <CardDescription className="mt-1 flex min-w-0 flex-wrap items-center gap-x-2 gap-y-0.5 text-xs font-normal">
                    <span className="tabular-nums">#{cred.id}</span>
                    <span aria-hidden="true">·</span>
                    <Tooltip>
                      <TooltipTrigger
                        render={<span />}
                        className="inline-flex min-w-0 items-center gap-1"
                      >
                        <CalendarDaysIcon className="size-3 shrink-0" />
                        <span>{t(`添加于 ${added}`, `Added ${added}`)}</span>
                      </TooltipTrigger>
                      <TooltipPopup>
                        {formatFullTime(cred.created_at, language)}
                      </TooltipPopup>
                    </Tooltip>
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
                  onQuotaPause={() => setQuotaOpen(true)}
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
          {/* 这一行的徽章一律 `size="xs"`——整张卡片除标题外都是写死的 12px，不跟视口走，
              徽章得落在同一档才不会比下面「用量限制」大一号。默认档是 `text-sm sm:text-xs`，
              按 640px **视口**断点；而这张卡片走的是 `@sm/card` **容器**断点，两套不是一回事，
              手机或窄窗口下就露馅。`xs` 那档的取舍见 `ui/badge.tsx`。 */}
          <div className="flex flex-wrap items-center gap-2">
            {statusUsesTooltip ? (
              <Tooltip>
                <TooltipTrigger
                  className={cn(badgeVariants({ size: 'xs', variant: status.variant }), 'cursor-help')}
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
                size="xs"
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
              <Tooltip>
                <TooltipTrigger
                  className={cn(badgeVariants({ size: 'xs', variant: 'outline' }), 'cursor-help')}
                  delay={0}
                >
                  {/* 图标不是装饰：org_type 与 tier 常常都叫 `Team`，两枚都成了描边胶囊之后
                      光看文字分不出哪个是「组织账号」哪个是「套餐档位」。 */}
                  <Building2Icon className="size-3" />
                  {orgBadgeLabel(cred)}
                </TooltipTrigger>
                <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
                  {t(
                    `组织账号（${cred.org_type}）：用量由整个组织共享，与同档位的个人账号不是一回事`,
                    `Organisation account (${cred.org_type}): the usage is shared across the whole organisation, unlike a personal account on the same tier`,
                  )}
                </TooltipPopup>
              </Tooltip>
            )}
            {cred.tier && <Badge size="xs" variant={tierBadgeVariant(cred.tier)}>{cred.tier}</Badge>}
            <Tooltip>
              <TooltipTrigger
                className={cn(badgeVariants({ size: 'xs', variant: 'outline' }), 'cursor-help tabular-nums')}
                delay={0}
              >
                P{cred.priority}
              </TooltipTrigger>
              <TooltipPopup>
                {t('调度优先级，数值越小越优先', 'Scheduling priority; lower values are scheduled first')}
              </TooltipPopup>
            </Tooltip>
            {proxyLabel ? (
              <Tooltip>
                {/* 手机上这枚最长：`socks5h://…` 连协议带主机常有 190px，胶囊行一挤就自己独占一行。
                    窄容器下藏掉协议段只留 `host:port`，再给一个上限截断长域名——完整 URL 在
                    Tooltip 与出站代理对话框里，一点不丢。 */}
                <TooltipTrigger
                  render={<button type="button" />}
                  className={cn(
                    badgeVariants({ size: 'xs', variant: 'outline' }),
                    'min-w-0 max-w-44 cursor-pointer gap-1 @sm/card:max-w-72',
                  )}
                  onClick={() => setProxyOpen(true)}
                >
                  <GlobeIcon className="size-3" />
                  <span className="min-w-0 truncate">
                    {proxyLabel.scheme ? (
                      <span className="hidden @sm/card:inline">{proxyLabel.scheme}</span>
                    ) : null}
                    {proxyLabel.host}
                  </span>
                </TooltipTrigger>
                <TooltipPopup className="max-w-72 break-all">{cred.proxy}</TooltipPopup>
              </Tooltip>
            ) : null}
          </div>

          <section aria-label={t(`${credentialLabel} 的用量限制`, `Usage limits for ${credentialLabel}`)} className="space-y-2">
            <div className="flex flex-wrap items-start justify-between gap-x-3 gap-y-1.5">
              <div className="flex flex-wrap items-center gap-2">
                <h4 className="font-medium text-xs text-muted-foreground">{t('用量限制', 'Usage limits')}</h4>
                {secondaryOverage && (
                  <Tooltip>
                    <TooltipTrigger
                      className={cn(
                        // 同上：它紧挨着 `text-xs` 的「用量限制」标题。
                        badgeVariants({ size: 'xs', variant: secondaryOverage.variant }),
                        'cursor-help',
                      )}
                      delay={0}
                    >
                      {secondaryOverage.label}
                    </TooltipTrigger>
                    <TooltipPopup className="max-w-80 whitespace-normal text-left leading-5">
                      {secondaryOverage.title}
                    </TooltipPopup>
                  </Tooltip>
                )}
              </div>
              {cred.quota ? (
                <Tooltip>
                  <TooltipTrigger
                    render={<span />}
                    className="inline-flex items-center gap-1 text-xs text-muted-foreground"
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
                <span className="text-xs text-muted-foreground">{t('暂无数据', 'No data')}</span>
              )}
            </div>
            {cred.quota && (has5h || has7d) ? (
              // 只有一个窗口时不留空半格：分两列却只填一格，看起来像另一半加载失败了。
              //
              // 断点回到 `@sm/card`（24rem / 384px），与这套胶囊排法是配套的：
              // 三枚胶囊「1947 req · 397M · $650.10」约 168px（token 那格不带 `tok` 后缀、
              // 胶囊自带内边距所以 `gap-x-2` 就够），一列 (384-32-16)/2 = 176px 装得下。
              // 之前把断点推到 512px，是因为当时那版带 ` tok` 后缀又用 `gap-x-3`，
              // 一列要 204px；排法换回来之后不必再推，卡片也不会平白高一行。
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
                    util={quota.h5.utilization}
                    reset={cred.quota.rl_5h_reset}
                    cost={cred.quota.cost_5h}
                    requests={cred.quota.requests_5h}
                    tokens={cred.quota.tokens_5h}
                    snapshotTs={cred.quota.ts}
                    now={now}
                  />
                )}
                {has7d && (
                  <QuotaMeter
                    credentialLabel={credentialLabel}
                    label="7d"
                    util={quota.d7.utilization}
                    reset={cred.quota.rl_7d_reset}
                    cost={cred.quota.cost_7d}
                    requests={cred.quota.requests_7d}
                    tokens={cred.quota.tokens_7d}
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
              <ModelStateLine
                icon={TimerOffIcon}
                tone="text-warning-foreground"
                label={t('模型冷却', 'Model cooldown')}
                detail={modelCooldownSummary(cred, language)}
                hint={t(
                  '这些模型的额度池已满（上游 429），暂时不参与选号；该账号的其余模型照常服务。到点自动恢复，也可在菜单里手动解除冷却',
                  'The overage pool for these models is exhausted (upstream 429), so they are temporarily skipped during account selection; this account keeps serving its other models. They recover automatically, or you can clear the cooldown from the menu',
                )}
              />
            )}
            {/* 与上面那条**不是**一回事，故分开显示：这一档只是刚撞过一发限速，号仍在调度池里。
                合并成「模型冷却」会让人以为这个号已经不干活了，从而跑去查一个根本不存在的故障。 */}
            {evaluation.modelThrottled && (
              <ModelStateLine
                icon={TimerOffIcon}
                tone="text-muted-foreground"
                label={t('刚被限速', 'Recently throttled')}
                detail={modelCooldownSummary(cred, language, false)}
                hint={t(
                  '这些模型刚被上游限速（容量或请求速率），额度并没有用完。这种限制跟着出口或模型走、不跟着账号走，所以该账号照常参与选号——上游要的是客户端按 retry-after 退避，不是把号停掉',
                  'These models were just throttled upstream (capacity or request rate); no quota was exhausted. That kind of limit follows the egress or the model rather than the account, so this account keeps taking part in selection — what upstream wants is the client backing off per retry-after, not an account being parked',
                )}
              />
            )}
            {/* 第三档：上游说这个套餐压根不含这些模型。它不会自己过去，所以措辞不能是「冷却」。 */}
            {evaluation.modelDenied && (
              <ModelStateLine
                icon={BanIcon}
                tone="text-muted-foreground"
                label={t('套餐不含', 'Not in plan')}
                detail={modelDenialSummary(cred, language)}
                hint={t(
                  '上游判定该账号的套餐不含这些模型（回了 429 却没有任何额度窗口、且组织未开 extra usage），选号时这些模型绕开它，其余模型照常。连通性测试通过、等级变化或菜单里手动解除都会清掉这条记录',
                  'Upstream reported that this account’s plan does not include these models (a 429 with no quota window at all and extra usage disabled for the org), so they skip this account during selection; its other models keep serving. A passing connectivity test, a tier change, or clearing from the menu removes the mark',
                )}
              />
            )}
          </section>
        </CardPanel>

        {/* 页脚：设备名额、模拟会话名额、当前 RPM ｜ 累计费用，最右是启停开关。 */}
        {/* 这是一条**读数条**而不是一排按钮，理由与尺寸账见 [FooterStat]：padding 只由容器给一次
            （`py-2`），四格之间只留 gap，行高由 text-xs 决定，手机上整条 38px。前三格都带分母、
            说的是「此刻占了多少」，费用没有分母、说的是「一共烧了多少」——两类量之间隔一道 1px 竖线
            分组，而不是靠间距暗示。四格加开关在 360px 屏上也是一行，不换行、不砍分母、不藏东西。 */}
        <CardFooter className="mt-auto flex items-center gap-2 border-t bg-muted/32 px-4 py-2 @sm/card:gap-3">
          <FooterStat
            icon={SmartphoneIcon}
            iconClassName={devicePolicy.className}
            valueClassName={cn(SLOT_WIDTH, SLOT_TEXT[deviceUsage.level])}
            value={<>{cred.device_count}<SlotLimit limit={effectiveLimit} /></>}
            hint={deviceUsageHint}
            ariaLabel={t(`查看 ${credentialLabel} 的已绑定设备`, `View bound devices for ${credentialLabel}`)}
            srLabel={devicePolicy.label}
            onClick={() => setDevicesOpen(true)}
          />
          {/* 模拟会话名额，与设备名额并排、同一个对话框：图标颜色是策略，数字颜色是占用。 */}
          <FooterStat
            icon={MessagesSquareIcon}
            iconClassName={sessionPolicy.className}
            valueClassName={cn(SLOT_WIDTH, SLOT_TEXT[sessionUsage.level])}
            value={<>{cred.session_count}<SlotLimit limit={sessionEffectiveLimit} /></>}
            hint={sessionUsageHint}
            ariaLabel={t(`查看 ${credentialLabel} 的模拟会话`, `View simulated sessions for ${credentialLabel}`)}
            srLabel={sessionPolicy.label}
            onClick={() => setDevicesOpen(true)}
          />
          {/* 当前 RPM 与两格名额同一副面孔，点开的是 RPM 上限对话框。分母是生效上限、不限时 ∞。 */}
          <FooterStat
            icon={GaugeIcon}
            iconClassName={rpmPolicy.className}
            valueClassName={cn(SLOT_WIDTH, SLOT_TEXT[rpmUsage.level])}
            value={<>{cred.rpm}<SlotLimit limit={rpmLimit > 0 ? rpmLimit : '∞'} /></>}
            hint={rpmLimit > 0
              ? t(
                `当前 RPM ${cred.rpm}/${rpmLimit}：最近 60 秒经这个账号转发的请求数（含失败的），上限 ${rpmLimit} 条/分钟（${rpmPolicyHint}）。打满后新请求分流到别的账号，已绑定的设备收到 429。点击调整`,
                `Current RPM ${cred.rpm}/${rpmLimit}: requests forwarded through this account in the last 60 seconds (failures included), limited to ${rpmLimit}/min (${rpmPolicyHint.toLowerCase()}). Once full, new requests spill to another account and already-bound devices get a 429. Click to adjust`,
              )
              : t(
                `当前 RPM ${cred.rpm}：最近 60 秒经这个账号转发的请求数（含失败的），${rpmPolicyHint}。点击调整`,
                `Current RPM ${cred.rpm}: requests forwarded through this account in the last 60 seconds (failures included); ${rpmPolicyHint.toLowerCase()}. Click to adjust`,
              )}
            ariaLabel={t(`调整 ${credentialLabel} 的 RPM 上限`, `Adjust the RPM limit for ${credentialLabel}`)}
            srLabel={`${t('当前 RPM', 'Current RPM')} · ${rpmPolicy.label}`}
            onClick={() => setRpmOpen(true)}
          />
          {/* 名额与费用之间的分组竖线：1px、与文字同高，不占高度。 */}
          <span aria-hidden="true" className="h-3 w-px shrink-0 bg-border" />
          {/* 累计费用：同一副面孔，但**不编占用色**——费用既没有分母也没有阈值，套上绿 / 黄 / 红
              会被当成告警读。只分「有」与「没有」：0 压成弱色，一列卡片扫下来烧了钱的那几张自己跳出来。 */}
          <FooterStat
            icon={WalletCardsIcon}
            iconClassName="text-muted-foreground"
            valueClassName={cred.cost_total > 0 ? 'text-foreground' : 'text-muted-foreground'}
            value={formatUsd(cred.cost_total)}
            hint={costHint}
            ariaLabel={t(`查看 ${credentialLabel} 的请求明细`, `View the request log for ${credentialLabel}`)}
            srLabel={t('累计等价 API 费用', 'Cumulative equivalent API cost')}
            onClick={() => setUsageOpen(true)}
          />
          {/* 开关钉在最右。 */}
          <div className="order-last ml-auto flex shrink-0 items-center gap-2">
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
        <DeferredMount open={proxyOpen || devicesOpen || usageOpen || confirmDelete || rpmOpen || quotaOpen || testing}>
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
          <CredentialDevicesDialog
            cred={cred}
            open={devicesOpen}
            onOpenChange={setDevicesOpen}
            limit={limit}
            sessionLimit={actions.sessionLimit}
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
          <Tooltip key={w.name}>
            <TooltipTrigger
              render={<span />}
              delay={0}
              className="inline-flex cursor-help items-center gap-1"
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
            </TooltipTrigger>
            {/* 这一整段解释原来只挂在原生 `title` 上：手机上完全看不到，而额外窗口是什么、
                为什么没有百分比、上游原值是哪个词，全在这里。 */}
            <TooltipPopup className="max-w-80 whitespace-normal text-left leading-5">
              {[
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
            </TooltipPopup>
          </Tooltip>
        )
      })}
    </div>
  )
}

/**
 * 「模型冷却 / 刚被限速 / 套餐不含」三档共用的一行：图标 + 词 + 受影响的模型，整行悬浮出解释。
 *
 * 解释原先挂在原生 `title` 上——要等约一秒才冒出来，触屏上压根不出。而这三行真正的差别
 * （这个号还在不在调度池里、要不要动手）全写在那段解释里，看不到就等于三行长得一样。
 * 换成与卡片其余部分同一套 Tooltip，`delay={0}`。
 */
function ModelStateLine({
  icon: Icon,
  tone,
  label,
  detail,
  hint,
}: {
  icon: ElementType<{ className?: string }>
  /** 整行的文字色：冷却是琥珀（要留意），另外两档是弱色（知会即可）。 */
  tone: string
  label: string
  detail: string
  hint: string
}) {
  return (
    <Tooltip>
      <TooltipTrigger
        render={<p />}
        delay={0}
        className={cn('flex cursor-help flex-wrap items-center gap-x-2 text-xs', tone)}
      >
        <Icon className="size-3" />
        <span className="font-medium">{label}</span>
        <span>{detail}</span>
      </TooltipTrigger>
      <TooltipPopup className="max-w-80 whitespace-normal text-left leading-5">{hint}</TooltipPopup>
    </Tooltip>
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
  if (!status || status === 'allowed' || status === 'allowed_warning') return null
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
          badgeVariants({ size: 'xs', variant: destructive ? 'error' : 'warning' }),
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
 * 额度里的一项事实（请求数、总 token、花费）：浅灰小块，值在前、单位在后。
 *
 * 做成块而不是「标签: 值」的文本对——卡片上这行要能一眼扫过去，标签在小字号下只是噪声，
 * 真要确认是什么，悬浮提示与读屏文本都写着全称。
 *
 * 提示用 `Tooltip` 组件而不是原生 `title`，且 `delay={0}`：原生提示要等约 1 秒才冒出来，
 * 而这三块的提示装的正是「这个数到底是什么、精确值多少」——等一秒才看见，等于没有。
 * 触屏上原生 `title` 更是压根不出。理由同页脚那三项，见上面 CardFooter 处的注。
 */
function QuotaFact({
  label,
  value,
  suffix,
  hint,
}: {
  label: string
  value: string
  suffix?: string
  /** 提示里跟在标签后面的明细（精确值、口径说明）；不传则只显示标签。 */
  hint?: string
}) {
  const { t } = useI18n()
  return (
    <Tooltip>
      <TooltipTrigger
        render={<div />}
        delay={0}
        className={cn(
          // `sm` 那档桌面上是 10px，压在 12px 的「用量限制」标题下面矮一截，
          // 且低于 kumo 字号梯子的下限（`Text` 的 size 只到 xs＝12px）。换成 `xs` 之后
          // 两端都是 12px，圆角也从 4px 回到 2px，跟上面那行胶囊对齐。
          badgeVariants({ size: 'xs', variant: 'secondary' }),
          'min-w-0 gap-0.5 font-normal',
        )}
      >
        <dt className="sr-only">{label}</dt>
        <dd className="truncate tabular-nums">{value}</dd>
        {suffix && <span className="text-muted-foreground" aria-hidden>{suffix}</span>}
      </TooltipTrigger>
      <TooltipPopup className="max-w-72 whitespace-normal break-words text-left leading-5">
        {hint ? t(`${label}：${hint}`, `${label}: ${hint}`) : label}
      </TooltipPopup>
    </Tooltip>
  )
}

function QuotaMeter({
  credentialLabel,
  label,
  util,
  reset,
  cost,
  requests,
  tokens,
  snapshotTs,
  now,
}: {
  credentialLabel: string
  label: string
  util: number | null
  reset: number | null
  cost: number | null
  requests: number | null
  /** 本窗口内用掉的总 token（官方 usage 四项之和，见 Quota.tokens_5h）。 */
  tokens: number | null
  snapshotTs: number
  /** 页面时钟（30 秒一跳），倒计时靠它走，见 [formatCountdown]。 */
  now: number
}) {
  const { t, language, locale } = useI18n()
  // 窗口重置后上游那份 utilization 就作废了（[evaluateQuotaWindow] 把它抹成 null），此时
  // 这个窗口的用量确实归了零——直接按 0% 画，不再单独摆一句「已重置 / 暂无数据」。那句话
  // 占着和数据一样大的地方，说的却只是「这里没什么可看」。倒计时同理：没有未来的重置时刻
  // 就不写字，也不留「—」——但那一格的**宽度**留着（空白），否则 5h 与 7d 两条的尾巴会错开。
  const percentage = quotaPercentage(util) ?? 0
  const level = quotaLevel(util)
  // 文字一律用 `-foreground` 那一支：`--destructive` 是给填充用的底色，拿来写字在浅底上
  // 对比度不够、暗色下又偏暗（见 index.css 里两支的注）。旁边的 warning 本来就用对了。
  const valueClass = level === 'critical'
    ? 'text-destructive-foreground'
    : level === 'warning'
      ? 'text-warning-foreground'
      : 'text-foreground'

  return (
    <Meter value={percentage} max={100} className="gap-2">
      {/* 数据先行、进度条随后：请求数与花费是「这个窗口里发生了什么」，百分比是「还剩多少」。
          两组分行排，比原先挤在一行的三列 dl 好扫——那一行里三个标签三个值交替出现，
          眼睛得逐个配对。 */}
      {/* 进度条上面这一行：三个事实是 `secondary` 胶囊（见 QuotaFact），不是裸文本。
          胶囊自带内边距，所以间距用 `gap-x-2` 而不是 `gap-x-3`；token 那格也不挂 `tok`
          后缀——`397M` 与旁边的 `1947 req`、`$650.10` 靠形态就能分开，挂上后缀一列要多 24px，
          `@sm/card` 下（一列 176px）会把第三枚挤到第二行，两条进度条跟着一上一下错开。 */}
      <dl className="flex min-w-0 flex-wrap items-baseline gap-x-2 gap-y-0.5">
        <QuotaFact
          label={t('请求数', 'Requests')}
          value={requests == null ? '—' : formatCompactNumber(requests, locale)}
          hint={requests == null ? undefined : requests.toLocaleString(locale)}
          suffix="req"
        />
        {/* 费用是按价目表估的、token 是上游实报的，两个数**不成正比**：缓存读按 ×0.1 计价，
            重度吃缓存的号「token 一大堆、花费很少」。所以两项并列而不是只留其中一个。 */}
        <QuotaFact
          label={t('总 token', 'Total tokens')}
          value={tokens == null ? '—' : formatTokens(tokens)}
          hint={tokens == null
            ? undefined
            : t(
              `${tokens.toLocaleString(locale)}（输入 + 输出 + 缓存写 + 缓存读，官方 usage 口径，不加权）`,
              `${tokens.toLocaleString(locale)} (input + output + cache write + cache read, per the official usage fields, unweighted)`,
            )}
        />
        <QuotaFact
          label={t('等价 API 费用', 'Equivalent API cost')}
          value={cost == null ? '—' : formatUsd(cost)}
        />
      </dl>
      <div className="flex min-w-0 items-center gap-2">
        {/* 窗口名是定宽的弱色文本，不是彩色胶囊：它只是"这条说的是哪个窗口"，一眼要认的是
            旁边那条的长度与颜色。实心胶囊在这张卡片上已经是状态的语言（运行正常 / 上游已拒），
            借给分类只会让一张卡片上五六块彩色抢同一份注意力。定宽 1.25rem 让 5h、7d 两条的
            起点对齐。 */}
        <MeterLabel
          className="w-5 shrink-0 font-medium text-muted-foreground text-xs tabular-nums"
        >
          <span className="sr-only">{t(`${credentialLabel} 的 `, `${credentialLabel} `)}</span>
          {label}
          <span className="sr-only">{t('用量', 'usage')}</span>
        </MeterLabel>
        <MeterTrack className="h-1.5 min-w-6 flex-1 rounded-full">
          {/* 填充色走共享的 [METER_FILL]：常态绿、吃紧琥珀、打满红，与设备 / 会话那几条
              计量条同一套档位配色。v0.3.142 曾把这条的常态单独改成 marine 蓝，只有这一处
              与别处不同，现在归位。 */}
          <MeterIndicator className={cn(METER_FILL[level], 'rounded-full')} />
        </MeterTrack>
        {/* 百分比与倒计时都给定宽的一格、文字左对齐：一张卡上下摞着 5h 与 7d 两条，`8%` 与
            `100%` 宽度不同、倒计时又时有时无，两格若按内容伸缩，两条进度条就一长一短、尾巴
            错开，看着像两个窗口的用量差别。留白不补，条尾因此永远在同一条竖线上。 */}
        {/* 百分比说的是「快照那一刻」的占用，快照时刻本身挂在悬浮提示里（原先是原生 `title`，
            手机上根本出不来——而这个数越接近 100，越需要知道它是几小时前的）。 */}
        <Tooltip>
          <TooltipTrigger render={<span />} delay={0} className="shrink-0 cursor-help">
            <MeterValue className={cn('block w-9 text-left font-medium text-xs tabular-nums', valueClass)}>
              {() => `${percentage}%`}
            </MeterValue>
          </TooltipTrigger>
          <TooltipPopup>
            {t(`快照于 ${formatFullTime(snapshotTs, language)}`, `Snapshot at ${formatFullTime(snapshotTs, language)}`)}
          </TooltipPopup>
        </Tooltip>
        {/* 距离重置还有多久。倒计时靠页面那个 30 秒 tick 走（见 useNowSeconds），不会冻住；
            精确到分秒的绝对时刻在悬浮提示里——倒计时受本地时钟偏差影响，只适合看个大概。 */}
        {reset != null && reset > now ? (
          <Tooltip>
            <TooltipTrigger
              render={<span />}
              delay={0}
              className="w-12 shrink-0 cursor-help whitespace-nowrap text-left text-xs text-muted-foreground tabular-nums"
            >
              {formatCountdown(reset, now)}
            </TooltipTrigger>
            <TooltipPopup>
              {t(`${formatFullTime(reset, language)} 重置`, `Resets ${formatFullTime(reset, language)}`)}
            </TooltipPopup>
          </Tooltip>
        ) : (
          // 没有未来的重置时刻时不写字、也不补「—」，但**宽度留着**：否则 5h 与 7d 两条的
          // 尾巴会错开，看着像两个窗口的用量差别。与列表里那格同一处理（见 QuotaCountdown）。
          <span className="w-12 shrink-0" aria-hidden />
        )}
      </div>
    </Meter>
  )
}
