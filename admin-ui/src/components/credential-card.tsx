import { memo, useState, type ElementType, type ReactNode } from 'react'
import {
  CalendarDaysIcon,
  CheckIcon,
  ClockIcon,
  GaugeIcon,
  GlobeIcon,
  MessagesSquareIcon,
  SmartphoneIcon,
  TimerOffIcon,
  WalletCardsIcon,
  XIcon,
} from 'lucide-react'
import { priorityTierName, type Credential } from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import { useReadOnly } from '@/lib/role'
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
  AccountTierBadge,
  ConnectivityTestDialog,
  credentialDetailHref,
  CredentialActionsMenu,
  DeferredMount,
  DeleteCredentialDialog,
  deviceUsageMeta,
  evaluateCredential,
  fablePoolHint,
  fablePoolWindow,
  modelCooldownSummary,
  proxyMaskedUrl,
  useProxyName,
  quotaLevel,
  METER_FILL,
  quotaPercentage,
  switchTitle,
  unifiedQuotaStatusLabel,
  useCredentialActions,
  type QuotaLevel,
  type QuotaWindowMeta,
  CredentialOwner,
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
import {
  Meter,
  MeterIndicator,
  MeterLabel,
  MeterTrack,
  MeterValue,
} from '@/components/ui/meter'
import { Spinner } from '@/components/ui/spinner'
import { Switch } from '@/components/ui/switch'
import { Hint, Tooltip, TooltipPopup, TooltipTrigger } from '@/components/ui/tooltip'

/**
 * 页脚三格名额读数（设备 / 会话 / RPM）的定宽：2.25rem，数字左对齐。
 *
 * 读数若按内容伸缩，`1/10` 与 `0/100` 宽度不同，后面那格就跟着往左右挪——一列卡片叠下来，
 * 会话图标、RPM 图标各在各的位置上，竖着扫过去是锯齿状的。定宽之后每格等宽，卡片之间对齐，
 * 数字也都从同一处起读。2.25rem 够放 `0/100`（手机 12px 字号），再长才会把它撑开。
 * 卡片窄于 22rem（360 那档手机上卡片只有 328px）时放弃定宽：三格各多占的几像素加起来，正好把
 * 右边的累计费用挤成「$44.…」。窄卡上一列卡片本来也只有一张，竖向对齐无从谈起。
 */
const SLOT_WIDTH = '@min-[22rem]/card:min-w-9'

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
          // 图标与数字之间窄卡上收成 2px，理由见页脚处的注。
          'relative flex min-w-0 shrink items-center gap-0.5 rounded-sm text-left font-medium text-xs tabular-nums outline-none @min-[22rem]/card:gap-1',
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

  const readOnly = useReadOnly()
  const actions = useCredentialActions(cred, () => setEditing(false))
  const { rename, toggle, limit } = actions
  const evaluation = evaluateCredential(cred, now, language)
  const { quota, status } = evaluation
  const credentialLabel = displayCredentialLabel(cred.label, language)
  // 上游没报的窗口写「无此窗口」而不是摘掉，与列表视图（credential-row 的 ListQuotaMeter）同一口径，
  // 两列的位置在每张卡上都一样，见用量网格处的注。
  const has5h = quota.h5.reported
  const has7d = quota.d7.reported
  // fable 额度池（7d_oi）挂在 7d 那一列下面：同为 7 天周期，上下对着比；上游没报就不占位。
  const fablePool = fablePoolWindow(quota)
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
      ? t('此账号不限 RPM', 'this account has no RPM limit')
      : t(`此账号自定义上限为 ${cred.rpm_limit}`, `this account overrides the limit to ${cred.rpm_limit}`)
  const sessionEffectiveLimit = cred.session_limit_effective > 0 ? cred.session_limit_effective : '∞'
  // 设备名额占用的配色与说明：空闲灰 / 健康绿 / 吃紧黄 / 占满红，见 [deviceUsageMeta]。
  const deviceUsage = deviceUsageMeta(cred.device_count, cred.device_limit_effective)
  // 会话名额同一套判定与配色：按会话占名额的来访（设备上限不生效时的真实客户端、模拟路径上没有设备身份的）每个对话占一个，与设备分开计。
  const sessionUsage = deviceUsageMeta(cred.session_count, cred.session_limit_effective)
  const sessionPolicy = cred.session_limit === 0
    ? { label: t('跟随默认', 'Default'), className: 'text-muted-foreground' }
    : cred.session_limit < 0
      ? { label: t('不限', 'Unlimited'), className: 'text-foreground' }
      : { label: t('自定义', 'Custom'), className: 'text-info-foreground' }
  const sessionUsageHint = cred.session_limit_effective <= 0
    ? t(
        `${cred.session_count} 条活跃会话，未设上限。点击查看或清理`,
        `${cred.session_count} active session(s), no limit set. Click to view or clear`,
      )
    : sessionUsage.level === 'critical'
      ? t(
          `会话名额已占满（${cred.session_count}/${cred.session_limit_effective}）：新会话将分配到其他账号；所有账号均占满时，客户端将收到 429。点击查看或清理`,
          `Session slots are full (${cred.session_count}/${cred.session_limit_effective}): new sessions go to another account, and get a 429 once every account is full. Click to view or clear`,
        )
      : t(
          `已占用 ${cred.session_count}/${cred.session_limit_effective} 个会话名额（每个对话占用一个名额）。点击查看或清理`,
          `${cred.session_count} of ${cred.session_limit_effective} session slots in use (each conversation takes one). Click to view or clear`,
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
      ? t('此账号不限设备数', 'This account has no device limit')
      : t(`此账号自定义上限为 ${cred.device_limit}`, `This account overrides the limit to ${cred.device_limit}`)
  const deviceUsageHint = (() => {
    if (cred.device_count <= 0) {
      return t(
        `尚无设备绑定到此账号，${devicePolicyHint}。点击查看`,
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
        `设备名额已占满（${cred.device_count}/${cred.device_limit_effective}，${devicePolicyHint}）：新设备将分配到其他账号；所有账号均占满时，客户端将收到 429。点击查看`,
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
      `${parts.join(' · ')}。按公开价目表估算的等价 API 费用，并非账单金额。点击查看请求明细`,
      `${parts.join(' · ')}. Equivalent API cost estimated from the public price list, not a bill. Click to view the request log`,
    )
  })()
  const titleId = `credential-card-title-${cred.id}`
  // 所有需处理状态都用同一种渐进披露：卡片只显示状态，详情在悬浮提示里查看。
  // 避免同一条状态再渲染一块说明，把异常卡片单独撑高。
  const statusUsesTooltip = status.attention
  const added = relativeTime(cred.created_at, now, language)
  const proxyName = useProxyName()
  const proxyLabel = proxyName(cred)
  const quotaSnapshotTime = cred.quota
    ? formatFullTime(cred.quota.ts, language)
    : t('未知时间', 'unknown time')
  const secondaryOverage = (() => {
    if (quota.overage === 'none') return null
    if (cred.disabled) {
      return {
        label: t('快照含 Usage credits', 'Snapshot used usage credits'),
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
        {/* 间距按卡片**容器**宽度分两档，不跟全站那档按视口走的「手机 16 / ≥640 20」：
            窄于 27rem 的卡片（只有手机会）槽宽 16px、行距 12px，每一行都是逐像素算过的，
            多给就会把读数与页脚挤到截断；≥ 27rem 的卡片（桌面恒是）槽宽 20px、段落之间 16px，
            读数行已经改成纯文本、不再贴着宽度上限，多出的这点空间用来让各段之间透气。 */}
        <CardHeader className="p-4 pb-3 @min-[27rem]/card:px-5 @min-[27rem]/card:pt-5">
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
                  <Hint label={credentialLabel}>
                    <h3
                      id={titleId}
                      className="block min-w-0 truncate whitespace-nowrap leading-snug"
                    >
                      {/* 账号名即详情页入口：用真链接而不是按钮，中键 / ⌘ 点击能在新标签页打开。 */}
                      <a href={credentialDetailHref(cred.id)} className="rounded-sm underline-offset-4 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background">
                        {credentialLabel}
                      </a>
                    </h3>
                  </Hint>
                  <CardDescription className="mt-1 flex @min-[27rem]/card:mt-1.5 min-w-0 flex-wrap items-center gap-x-2 gap-y-0.5 text-xs font-normal">
                    <span className="tabular-nums">#{cred.id}</span>
                    {cred.owner && cred.owner !== 'admin' && <span aria-hidden="true">·</span>}
                    <CredentialOwner cred={cred} />
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
              <CredentialActionsMenu
                triggerClassName={buttonVariants({ size: 'icon', variant: 'ghost' })}
                triggerLabel={t(`打开 ${credentialLabel} 菜单`, `Open menu for ${credentialLabel}`)}
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
            </CardAction>
          )}
        </CardHeader>

        <CardPanel className="space-y-3 px-4 pb-4 @min-[27rem]/card:space-y-4 @min-[27rem]/card:px-5 @min-[27rem]/card:pb-5">
          {/* 这一行的徽章一律 `size="xs"`——整张卡片除标题外都是写死的 12px，不跟视口走，
              徽章得落在同一档才不会比下面「用量限制」大一号。默认档是 `text-sm sm:text-xs`，
              按 640px **视口**断点；而这张卡片走的是 `@sm/card` **容器**断点，两套不是一回事，
              手机或窄窗口下就露馅。`xs` 那档的取舍见 `ui/badge.tsx`。 */}
          {/* 徽章在左、可折行；快照时间单独一栏钉在右上角、不参与折行。原先两者同在一个 flex-wrap 里，
              徽章一多（尤其挂着代理名称时）时间就被挤到第二行最右，单独占一行。 */}
          <div className="flex items-start gap-2">
            <div className="flex min-w-0 flex-1 flex-wrap items-center gap-2">
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
              {/* 被拒的那个窗口下面已经画成红条（5h / 7d / fable）时，这枚徽章说的是同一件事，不挂；
                  只在上游点名的窗口没有进度条可看（未知窗口、老快照）时，它才是唯一的解释。 */}
              {cred.quota && !verdictShownByMeter(cred.quota.rl_representative, has5h, has7d, fablePool != null) && (
                <UpstreamVerdict quota={cred.quota} credentialLabel={credentialLabel} />
              )}
              <AccountTierBadge cred={cred} size="xs" />
              <Tooltip>
                <TooltipTrigger
                  className={cn(badgeVariants({ size: 'xs', variant: 'outline' }), 'cursor-help tabular-nums')}
                  delay={0}
                >
                  P{cred.priority}
                </TooltipTrigger>
                <TooltipPopup>
                  {t(`调度优先级 P${cred.priority} ${priorityTierName(cred.priority, t)}：同档分摊，跨档按先后顺序用尽`, `Priority P${cred.priority} ${priorityTierName(cred.priority, t)}: the same tier shares load, tiers are used up in order`)}
                </TooltipPopup>
              </Tooltip>
              {secondaryOverage && (
                <Tooltip>
                  <TooltipTrigger
                    className={cn(badgeVariants({ size: 'xs', variant: secondaryOverage.variant }), 'cursor-help')}
                    delay={0}
                  >
                    {secondaryOverage.label}
                  </TooltipTrigger>
                  <TooltipPopup className="max-w-80 whitespace-normal text-left leading-5">
                    {secondaryOverage.title}
                  </TooltipPopup>
                </Tooltip>
              )}
              {proxyLabel ? (
                // 代理名称排在最后、放在一个能伸缩的外壳里：外壳起步只要 4rem、再吃满这一行剩下的宽度，
                // 徽章本身按内容取宽、超出外壳才截断。直接把徽章放进 flex-wrap，名字一长它就整枚掉到
                // 第二行——卡片平白多一行；现在只有前面的徽章把这行占到不足 4rem 时它才折行。
                <div className="flex min-w-0 max-w-44 grow basis-16 @sm/card:max-w-72">
                  <Tooltip>
                    {/* 显示代理池里的名称，不铺 IP 与端口；不在池里的自定义地址才退回 `host:port`。
                        完整地址（脱敏）在 Tooltip 与出站代理对话框里。 */}
                    <TooltipTrigger
                      render={<button type="button" />}
                      className={cn(
                        badgeVariants({ size: 'xs', variant: 'outline' }),
                        'min-w-0 max-w-full cursor-pointer gap-1',
                      )}
                      onClick={() => setProxyOpen(true)}
                    >
                      <GlobeIcon className="size-3" />
                      <span className="min-w-0 truncate">{proxyLabel}</span>
                    </TooltipTrigger>
                    <TooltipPopup className="max-w-72 break-all">{proxyMaskedUrl(cred.proxy!)}</TooltipPopup>
                  </Tooltip>
                </div>
              ) : null}
            </div>
            {/* 用量快照的时间钉在徽章行最右。原先它独占一行「用量限制 … 更新于」，而下面的 5h / 7d
                自己就说明了那是什么，那一行只剩这个时间戳，白占 28px。窄卡上去掉「更新于」三个字，
                时钟图标已经说明它是什么。`h-4.5` 与 xs 徽章同高，徽章折行时它仍对齐第一行。 */}
            {cred.quota ? (
              <Tooltip>
                <TooltipTrigger
                  render={<span />}
                  className="inline-flex h-4.5 shrink-0 items-center gap-1 whitespace-nowrap text-xs text-muted-foreground"
                >
                  <ClockIcon className="size-3" />
                  <span className="@max-[27rem]/card:hidden">{t('更新于 ', 'Updated ')}</span>
                  {relativeTime(cred.quota.ts, now, language)}
                </TooltipTrigger>
                <TooltipPopup>
                  {t(`用量快照于 ${formatFullTime(cred.quota.ts, language)}`, `Usage snapshot at ${formatFullTime(cred.quota.ts, language)}`)}
                </TooltipPopup>
              </Tooltip>
            ) : (
              <span className="inline-flex h-4.5 shrink-0 items-center text-xs text-muted-foreground">{t('暂无用量数据', 'No usage data')}</span>
            )}
          </div>

          <section aria-label={t(`${credentialLabel} 的用量限制`, `Usage limits for ${credentialLabel}`)} className="space-y-2 empty:hidden">
            {cred.quota && (has5h || has7d || fablePool) ? (
              // **任何宽度下都是两列**，手机上也不摞成两行：5h 与 7d 是同一组数据的两个口径，
              // 并排才好比。只报了一个窗口时另一格也留着、写「无此窗口」（与列表视图同一口径）：
              // 否则那条进度条铺满整行，跟同一列其他卡片的 5h 条长短、百分比位置全都对不上。
              //
              // 排成三行的网格而不是两个各自上下排的列：第一行两组读数、第二行两条进度条、
              // 第三行 fable 子池（只挂在 7d 下面，5h 那格空着）。同一种东西永远在同一行，
              // fable 紧贴 7d 那条，两条长度可以直接比。
              //
              // 读数在上、进度条在下：请求数 / token / 费用是「这个窗口里发生了什么」，
              // 进度条是「还剩多少」，先读前者再看后者。
              //
              // 27rem 这条线：它正是 [CREDENTIAL_CARD_GRID_CLASS] 里卡片的最小宽度——桌面的卡片
              // 恒 ≥ 27rem，走宽版排法；只有手机上卡片被视口压到 27rem 以下，才切到窄版
              // （百分比与倒计时不再定宽、间距收紧，见 [QuotaBar]）。列间距宽版 24px、窄版 12px。
              <div className="grid grid-cols-2 gap-x-3 gap-y-2 @min-[27rem]/card:gap-x-6">
                {has5h ? (
                  <QuotaFacts
                    requests={cred.quota.requests_5h}
                    tokens={cred.quota.tokens_5h}
                    cost={cred.quota.cost_5h}
                  />
                ) : (
                  <span aria-hidden />
                )}
                {has7d ? (
                  <QuotaFacts
                    requests={cred.quota.requests_7d}
                    tokens={cred.quota.tokens_7d}
                    cost={cred.quota.cost_7d}
                  />
                ) : (
                  <span aria-hidden />
                )}
                {has5h ? (
                  <QuotaBar
                    credentialLabel={credentialLabel}
                    // 标签用 `5h`/`7d` 而不是「5 小时」：这一行还挤着进度条、百分比与
                    // 重置时刻，长标签会把进度条压没；完整称呼在读屏文本里。
                    label="5h"
                    util={quota.h5.utilization}
                    reset={cred.quota.rl_5h_reset}
                    snapshotTs={cred.quota.ts}
                    now={now}
                  />
                ) : (
                  <QuotaBarAbsent label="5h" />
                )}
                {has7d ? (
                  <QuotaBar
                    credentialLabel={credentialLabel}
                    label="7d"
                    // 下面挂着 fable 那条时，两条的窗口名格一起放宽到 `fable` 的宽度，条的起点才对齐。
                    labelClassName={fablePool ? 'w-8' : undefined}
                    util={quota.d7.utilization}
                    reset={cred.quota.rl_7d_reset}
                    snapshotTs={cred.quota.ts}
                    now={now}
                  />
                ) : (
                  <QuotaBarAbsent label="7d" labelClassName={fablePool ? 'w-8' : undefined} />
                )}
                {fablePool && (
                  <>
                    <span aria-hidden />
                    <FablePoolMeter window={fablePool} now={now} />
                  </>
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
                  '以下模型的额度池已用尽（上游返回 429），暂不参与账号选择；该账号的其余模型照常服务。冷却到期后自动恢复，也可在菜单中手动解除冷却',
                  'The quota pool for these models is exhausted (upstream 429), so they are temporarily skipped during account selection; this account keeps serving its other models. They recover automatically when due, or you can clear the cooldown from the menu',
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
                  '以下模型刚被上游限速（容量或请求速率），额度并未用尽。此类限制取决于出口或模型，而非账号，因此该账号照常参与账号选择。上游期望客户端按 retry-after 退避，而非停用账号',
                  'These models were just throttled upstream (capacity or request rate); no quota was exhausted. That kind of limit follows the egress or the model rather than the account, so this account keeps taking part in account selection. What upstream expects is the client backing off per retry-after, not the account being disabled',
                )}
              />
            )}
            {/* 「套餐不含」（Pro 号打 fable 那类）不在卡片上显示：那是套餐本身决定的、预期之内的事，
                不是故障，挂在卡片上只是噪声。记录与解除入口在详情页和菜单里。 */}
          </section>
        </CardPanel>

        {/* 页脚：设备名额、会话名额、当前 RPM ｜ 累计费用，最右是启停开关。 */}
        {/* 这是一条**读数条**而不是一排按钮，理由与尺寸账见 [FooterStat]：padding 只由容器给一次
            （`py-2`），四格之间只留 gap，行高由 text-xs 决定，手机上整条 38px。前三格都带分母、
            说的是「此刻占了多少」，费用没有分母、说的是「一共烧了多少」——两类量之间隔一道 1px 竖线
            分组，而不是靠间距暗示。四格加开关在 360px 屏上也是一行，不换行、不砍分母、不藏东西。 */}
        {/* 窄于 22rem 的卡片（360 那档手机，内容区只剩 294px）上，四格加开关按常规间距排正好顶满，
            RPM 到三位数或费用上百就被截成「2…」「$214.…」。所以窄卡上省出约 40px：格间距 8→6px、
            图标与数字间 4→2px，费用满 $100 时不显示分位（精确值在悬浮提示里）。 */}
        <CardFooter className="mt-auto flex items-center gap-1.5 border-t bg-muted/32 px-4 py-2 @min-[22rem]/card:gap-2 @sm/card:gap-3 @min-[27rem]/card:gap-4 @min-[27rem]/card:px-5 @min-[27rem]/card:py-3">
          {/* 设备按会话占名额时设备上限不生效，这一格不出现。 */}
          {cred.device_limit_applies && (
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
          )}
          {/* 会话名额，与设备名额并排、同一个对话框：图标颜色是策略，数字颜色是占用。 */}
          <FooterStat
            icon={MessagesSquareIcon}
            iconClassName={sessionPolicy.className}
            valueClassName={cn(SLOT_WIDTH, SLOT_TEXT[sessionUsage.level])}
            value={<>{cred.session_count}<SlotLimit limit={sessionEffectiveLimit} /></>}
            hint={sessionUsageHint}
            ariaLabel={t(`查看 ${credentialLabel} 的会话`, `View sessions for ${credentialLabel}`)}
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
                `当前 RPM ${cred.rpm}/${rpmLimit}：最近 60 秒经此账号转发的请求数（含失败请求），上限 ${rpmLimit} 条/分钟（${rpmPolicyHint}）。达到上限后，新请求将分流到其他账号，已绑定的设备将收到 429。点击调整`,
                `Current RPM ${cred.rpm}/${rpmLimit}: requests forwarded through this account in the last 60 seconds (failures included), limited to ${rpmLimit}/min (${rpmPolicyHint.toLowerCase()}). Once the limit is reached, new requests go to other accounts and already-bound devices get a 429. Click to adjust`,
              )
              : t(
                `当前 RPM ${cred.rpm}：最近 60 秒经此账号转发的请求数（含失败请求），${rpmPolicyHint}。点击调整`,
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
            value={cred.cost_total >= 100 ? (
              <>
                <span className="@max-[22rem]/card:hidden">{formatUsd(cred.cost_total)}</span>
                <span className="@min-[22rem]/card:hidden">${Math.round(cred.cost_total)}</span>
              </>
            ) : formatUsd(cred.cost_total)}
            hint={costHint}
            ariaLabel={t(`查看 ${credentialLabel} 的请求明细`, `View the request log for ${credentialLabel}`)}
            srLabel={t('累计等价 API 费用', 'Cumulative equivalent API cost')}
            onClick={() => setUsageOpen(true)}
          />
          {/* 开关钉在最右。 */}
          <div className="order-last ml-auto flex shrink-0 items-center gap-2">
            {toggle.isPending && <Spinner />}
            <Hint label={switchTitle(cred, language)}>
              <Switch
                checked={!cred.disabled}
                onCheckedChange={(enabled) => toggle.mutate(!enabled)}
                disabled={readOnly || toggle.isPending}
                aria-label={`${credentialLabel}: ${switchTitle(cred, language)}`}
              />
            </Hint>
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
    return { text: t('已拒绝', 'rejected'), bad: true }
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
/** [ExtraWindows] 实际会画出来的那几个窗口；调用方据此决定要不要给它留一块位置。 */
export function visibleExtraWindows(windows: QuotaWindowMeta[]): QuotaWindowMeta[] {
  return windows.filter((w) => (
    w.name.toLowerCase() !== '7d_oi'
    && (!isCapabilityWindow(w) || w.status === 'allowed' || w.status === 'allowed_warning')
  ))
}

export function ExtraWindows({ windows }: { windows: QuotaWindowMeta[] }) {
  const { t, language } = useI18n()
  const visibleWindows = visibleExtraWindows(windows)
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
                      `${w.name}：上游明确报告 Usage credits（套餐用量耗尽后的按量计费用量）可用；其并非用量窗口，因此不显示百分比`,
                      `${w.name}: the upstream explicitly reports usage credits (pay-as-you-go beyond the plan's included usage) as available; this is not a usage window, so it has no percentage`,
                    )
                  : t(`用量窗口 ${w.name}`, `Usage window ${w.name}`),
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
 * 上游点名的起约束窗口（`representative-claim`）是否已经画成了一条进度条。是的话，红条本身就
 * 说明了「哪里满了」，[UpstreamVerdict] 那枚徽章再说一遍就是重复。认不出的窗口名一律算没画。
 */
export function verdictShownByMeter(
  representative: string | null,
  has5h: boolean,
  has7d: boolean,
  hasFablePool: boolean,
): boolean {
  // 上游的 claim 名（`five_hour`）与窗口名（`5h`）两种写法都认：老快照里存的是哪种没有保证。
  switch (representative) {
    case 'five_hour':
    case '5h':
      return has5h
    case 'seven_day':
    case '7d':
      return has7d
    case 'seven_day_overage_included':
    case '7d_oi':
      return hasFablePool
    default:
      return false
  }
}

/**
 * 上游对**这个账号**的整体额度判决（`anthropic-ratelimit-unified-status`），
 * 以及它认为当前是哪个窗口在管事（`representative-claim`）。`allowed` 是常态，不占地方。
 *
 * 这个状态徽标是「5h / 7d 都没满，却被拒或动用了 Usage credits」时唯一能给出解释的东西：满掉的那个窗口
 * （实测多为超额池 `7d_oi`）后端只用来判冷却、并不落库，所以卡片上没有它的进度条可看，
 * 但上游的判决与它的名字是在快照里的。缺了这个状态，那种账号在界面上就是「一切正常却在烧钱」。
 */
export function UpstreamVerdict({
  quota,
  credentialLabel,
  size = 'xs',
}: {
  quota: NonNullable<Credential['quota']>
  credentialLabel: string
  /** 卡片里是 `xs`（整张卡写死 12px）；详情页页头的徽章是默认档，跟着传进来。 */
  size?: 'xs' | 'default'
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
        `上游报告当前起约束作用的是 ${quota.rl_representative} 窗口。若该窗口不属于 5h / 7d，则为未记录的窗口（通常是超额用量窗口），卡片上不显示对应的进度条`,
        `The upstream reports the ${quota.rl_representative} window as the binding constraint. If it is not one of the 5h / 7d windows, it is an unrecorded window (typically the extra usage window) and has no meter on this card`,
      )
    : null
  const detail = representativeDetail
    ? t(`${verdictTitle}。${representativeDetail}`, `${verdictTitle}. ${representativeDetail}`)
    : verdictTitle

  return (
    <Tooltip>
      <TooltipTrigger
        className={cn(
          badgeVariants({ size, variant: destructive ? 'error' : 'warning' }),
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
 * 额度里的一项事实（请求数、总 token、花费）：纯文本，值在前、单位在后。
 *
 * 不再是浅灰胶囊：一张卡上六块灰底小框挤在进度条旁边，读着杂；而且胶囊自带内边距，
 * 读着杂。改成与页脚读数条同一种读法：计量值只是文字，底板留给状态徽章。
 *
 * 提示用 `Tooltip` 组件而不是原生 `title`，且 `delay={0}`：原生提示要等约 1 秒才冒出来，
 * 而这三项的提示装的正是「这个数到底是什么、精确值多少」——等一秒才看见，等于没有。
 * 触屏上原生 `title` 更是压根不出。
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
        className="inline-flex min-w-0 cursor-help items-baseline gap-0.5 whitespace-nowrap"
      >
        <dt className="sr-only">{label}</dt>
        <dd className="tabular-nums">{value}</dd>
        {/* 窄卡上（< 27rem，只有手机）后缀隐掉，三项仍排得进一行；理由见 [QuotaFacts]。 */}
        {suffix && <span className="@max-[27rem]/card:hidden" aria-hidden>{suffix}</span>}
      </TooltipTrigger>
      <TooltipPopup className="max-w-72 whitespace-normal break-words text-left leading-5">
        {hint ? t(`${label}：${hint}`, `${label}: ${hint}`) : label}
      </TooltipPopup>
    </Tooltip>
  )
}

/**
 * 进度条上面那行读数：「128 req · 18.4M · $6.85」。
 *
 * 整行弱色：一眼要读到的是下面那条进度条的长度与百分比。token 那项不挂
 * `tok` 后缀——`397M` 与旁边的 `1947 req`、`$650.10` 靠形态就能分开。中间的点只是分隔，
 * 不进读屏。
 *
 * 卡片窄于 27rem（只有手机会）时一列约 150px，「128 req · 18.4M · $6.85」要 170px 装不下，
 * 间距收紧、`req` 后缀隐掉，「128 · 18.4M · $6.85」三个数靠形态就能分开（整数 / 带单位 /
 * 带 $），悬浮提示与读屏文本里全称照旧。`flex-wrap` 只是兜底：`$12345.67` 这种上界值真装不下
 * 时宁可折行也别盖到旁边那列。
 */
function QuotaFacts({
  requests,
  tokens,
  cost,
}: {
  requests: number | null
  /** 本窗口内用掉的总 token（官方 usage 四项之和，见 Quota.tokens_5h）。 */
  tokens: number | null
  cost: number | null
}) {
  const { t, locale } = useI18n()
  const dot = <span aria-hidden className="text-muted-foreground/60">·</span>
  return (
    <dl className="flex min-w-0 flex-wrap items-baseline gap-x-1.5 gap-y-0.5 text-xs text-muted-foreground @max-[27rem]/card:gap-x-1">
      <QuotaFact
        label={t('请求数', 'Requests')}
        value={requests == null ? '—' : formatCompactNumber(requests)}
        hint={requests == null ? undefined : requests.toLocaleString(locale)}
        suffix="req"
      />
      {dot}
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
      {dot}
      <QuotaFact
        label={t('等价 API 费用', 'Equivalent API cost')}
        value={cost == null ? '—' : formatUsd(cost)}
      />
    </dl>
  )
}

/** 窗口名那一格：定宽弱色文本，5h、7d、fable 几条的起点靠它对齐。 */
const QUOTA_LABEL_CLASS = 'w-5 shrink-0 font-medium text-muted-foreground text-xs tabular-nums'

/**
 * 上游没报的那个窗口：窗口名照旧占位，后面写「无此窗口」，与列表视图同一口径。
 * 行高与 [QuotaBar] 一致（都由 text-xs 的行高定），下面的 fable 那行因此不会错位。
 */
function QuotaBarAbsent({ label, labelClassName }: { label: string; labelClassName?: string }) {
  const { t } = useI18n()
  return (
    <div className="flex min-w-0 items-center gap-2 text-xs @max-[27rem]/card:gap-1.5">
      <span className={cn(QUOTA_LABEL_CLASS, labelClassName)}>{label}</span>
      <span className="min-w-0 truncate text-muted-foreground/70">{t('无此窗口', 'Not applicable')}</span>
    </div>
  )
}

function QuotaBar({
  credentialLabel,
  label,
  labelClassName,
  util,
  reset,
  snapshotTs,
  now,
}: {
  credentialLabel: string
  label: string
  /** 窗口名那一格的宽度，默认 `w-5`；7d 下面挂着 fable 那条时放宽，见调用处。 */
  labelClassName?: string
  util: number | null
  reset: number | null
  snapshotTs: number
  /** 页面时钟（30 秒一跳），倒计时靠它走，见 [formatCountdown]。 */
  now: number
}) {
  const { t, language } = useI18n()
  // 窗口重置后上游那份 utilization 就作废了（[evaluateQuotaWindow] 把它抹成 null），此时
  // 这个窗口的用量确实归了零——直接按 0% 画，不再单独摆一句「已重置 / 暂无数据」。那句话
  // 占着和数据一样大的地方，说的却只是「这里没什么可看」。倒计时同理：没有未来的重置时刻
  // 就不写字，也不留「—」——但那一格的**宽度**留着（空白），否则上下两条的尾巴会错开。
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
    <Meter value={percentage} max={100}>
      {/* 宽版是「5h ▬▬▬▬ 37% 2h13m」一行到底，定宽的几格合计 20+36+48 加三道间距 = 128px，
          进度条吃剩下的。窄于 27rem 时一列约 150px，照这套定宽条只剩二十几像素，所以窄版的
          百分比与倒计时改按内容取宽（`37%` 约 26px、`2h13m` 约 36px）、间距 8→6px，条能拿回
          约 50px。定宽本是为了上下摞着的两条（7d 与 fable）尾巴对齐；窄版上差这几像素看不出来。 */}
      <div className="flex min-w-0 items-center gap-2 @max-[27rem]/card:gap-1.5">
        {/* 窗口名是定宽的弱色文本，不是彩色胶囊：它只是"这条说的是哪个窗口"，一眼要认的是
            旁边那条的长度与颜色。实心胶囊在这张卡片上已经是状态的语言（运行正常 / 上游已拒），
            借给分类只会让一张卡片上五六块彩色抢同一份注意力。 */}
        <MeterLabel className={cn(QUOTA_LABEL_CLASS, labelClassName)}>
          <span className="sr-only">{t(`${credentialLabel} 的 `, `${credentialLabel} `)}</span>
          {label}
          <span className="sr-only">{t('用量', 'usage')}</span>
        </MeterLabel>
        <MeterTrack className="h-1.5 min-w-6 flex-1 rounded-full">
          {/* 填充色走共享的 [METER_FILL]：常态绿、吃紧琥珀、打满红，与设备 / 会话那几条
              计量条同一套档位配色。 */}
          <MeterIndicator className={cn(METER_FILL[level], 'rounded-full')} />
        </MeterTrack>
        {/* 百分比与倒计时都给定宽的一格、文字左对齐：`8%` 与 `100%` 宽度不同、倒计时又时有时无，
            两格若按内容伸缩，一列卡片里的进度条就一长一短、尾巴错开，看着像用量差别。 */}
        {/* 百分比说的是「快照那一刻」的占用，快照时刻本身挂在悬浮提示里——这个数越接近 100，
            越需要知道它是几小时前的。 */}
        <Tooltip>
          <TooltipTrigger render={<span />} delay={0} className="shrink-0 cursor-help">
            <MeterValue className={cn('block w-9 text-left font-medium text-xs tabular-nums @max-[27rem]/card:w-auto', valueClass)}>
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
              className="w-12 shrink-0 cursor-help whitespace-nowrap text-left text-xs text-muted-foreground tabular-nums @max-[27rem]/card:w-auto"
            >
              {formatCountdown(reset, now)}
            </TooltipTrigger>
            <TooltipPopup>
              {t(`${formatFullTime(reset, language)} 重置`, `Resets ${formatFullTime(reset, language)}`)}
            </TooltipPopup>
          </Tooltip>
        ) : (
          // 没有未来的重置时刻时不写字、也不补「—」，但**宽度留着**，理由同上。
          // 与列表里那格同一处理（见 QuotaCountdown）。
          <span className="w-12 shrink-0 @max-[27rem]/card:hidden" aria-hidden />
        )}
      </div>
    </Meter>
  )
}

/**
 * fable 额度池（`7d_oi`）那一条：只有进度条这一行，没有请求数 / token / 费用那排读数——上游只给
 * 使用率与重置时刻，配一排「—」会让人以为是数据缺了。
 *
 * 各格宽度与 [QuotaBar]逐一对齐（窗口名 `w-8`、百分比 `w-9`、倒计时 `w-12`，
 * 窄卡上后两格按内容取宽），挂在 7d 正下方时两条的起点、条尾都在同一条竖线上，长度可以直接比。
 * 条比上面细一档（h-1）：它是 7d 之下的一个子池，不是与 5h / 7d 平级的第三个窗口。
 *
 * 配色走同一套档位，但它满了只挡 fable，不进账号状态（见 [fablePoolWindow]）。
 */
function FablePoolMeter({ window: w, now }: { window: QuotaWindowMeta; now: number }) {
  const { t, language } = useI18n()
  const percentage = w.percentage ?? 0
  const level = quotaLevel(w.utilization)
  const rejected = w.status === 'rejected' || w.status === 'rate_limited'
  const valueClass = level === 'critical' || rejected
    ? 'text-destructive-foreground'
    : level === 'warning'
      ? 'text-warning-foreground'
      : 'text-foreground'
  return (
    <Tooltip>
      <TooltipTrigger render={<div />} delay={0} className="cursor-help">
        <Meter value={percentage} max={100}>
          <div className="flex min-w-0 items-center gap-2 @max-[27rem]/card:gap-1.5">
            <MeterLabel className="w-8 shrink-0 font-medium text-muted-foreground text-xs">fable</MeterLabel>
            <MeterTrack className="h-1 min-w-6 flex-1 rounded-full">
              <MeterIndicator className={cn(METER_FILL[rejected ? 'critical' : level], 'rounded-full')} />
            </MeterTrack>
            <MeterValue className={cn('block w-9 shrink-0 text-left font-medium text-xs tabular-nums @max-[27rem]/card:w-auto', valueClass)}>
              {() => `${percentage}%`}
            </MeterValue>
            {w.resetAt != null && w.resetAt > now ? (
              <span className="w-12 shrink-0 whitespace-nowrap text-left text-xs text-muted-foreground tabular-nums @max-[27rem]/card:w-auto">
                {formatCountdown(w.resetAt, now)}
              </span>
            ) : (
              <span className="w-12 shrink-0 @max-[27rem]/card:hidden" aria-hidden />
            )}
          </div>
        </Meter>
      </TooltipTrigger>
      <TooltipPopup className="max-w-80 whitespace-normal text-left leading-5">
        {fablePoolHint(w, language)}
        {rejected && t('。上游已拒绝：fable 暂不会分配到此账号', '. Rejected upstream: fable will not be routed to this account for now')}
      </TooltipPopup>
    </Tooltip>
  )
}
