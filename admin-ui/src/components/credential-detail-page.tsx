import { useEffect, useRef, useState, type ReactNode } from 'react'
import { keepPreviousData, useQuery } from '@tanstack/react-query'
import {
  ActivityIcon,
  BanIcon,
  Building2Icon,
  ChevronDownIcon,
  ChevronRightIcon,
  ChevronUpIcon,
  ClockIcon,
  GaugeIcon,
  InfoIcon,
  MessagesSquareIcon,
  PencilIcon,
  RefreshCwIcon,
  ScrollTextIcon,
  ShieldAlertIcon,
  SlidersHorizontalIcon,
  SmartphoneIcon,
  TimerOffIcon,
  UserRoundSearchIcon,
  WalletCardsIcon,
} from 'lucide-react'
import {
  listBanEvents,
  listCredentialDevices,
  listCredentialSessions,
  listCredentialUsage,
  type Credential,
} from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import { useMediaQuery } from '@/lib/use-media-query'
import {
  cn,
  displayCredentialLabel,
  extractError,
  formatCompactNumber,
  formatClockTime,
  formatCountdown,
  formatDuration,
  formatFullTime,
  formatTokens,
  formatUsd,
  localizeBackendMessage,
  relativeTime,
} from '@/lib/utils'
import { AppFooter } from '@/components/app-footer'
import { AppHeader, Breadcrumb, PreferencesMenu } from '@/components/app-header'
import { BanEventDetail, sourceLabel } from '@/components/ban-events-dialog'
import { ExtraWindows, verdictShownByMeter, visibleExtraWindows } from '@/components/credential-card'
import { CredentialDevicesDialog, DeviceList, SessionList } from '@/components/credential-devices-dialog'
import { CredentialProxyDialog } from '@/components/credential-proxy-dialog'
import { CredentialQuotaDialog } from '@/components/credential-quota-dialog'
import { RenameCredentialDialog } from '@/components/credential-row'
import { CredentialRpmDialog } from '@/components/credential-rpm-dialog'
import { CredentialStatsSection } from '@/components/credential-stats-section'
import {
  ConnectivityTestDialog,
  CredentialActionsMenu,
  DeferredMount,
  DeleteCredentialDialog,
  credentialExpiryMeta,
  deviceUsageMeta,
  evaluateCredential,
  fablePoolWindow,
  isOrgAccount,
  METER_FILL,
  orgBadgeLabel,
  proxyLabelParts,
  proxyMaskedUrl,
  quotaLevel,
  quotaPercentage,
  switchTitle,
  tierBadgeVariant,
  unifiedQuotaStatusLabel,
  useCredentialActions,
  type CredentialStatusMeta,
  type QuotaLevel,
} from '@/components/credential-shared'
import { CredentialUsageDialog, UsageCards, UsageTable } from '@/components/credential-usage-dialog'
import { useNowSeconds } from '@/components/credential-workspace'
import { OverviewMetric, OverviewMetricSkeleton } from '@/components/overview-metric'
import { SettingsGroup, SettingsRow } from '@/components/settings-group'
import { DetailSection as Section } from '@/components/detail-section'
import { RequestLookupDialog } from '@/components/request-lookup-dialog'
import { statusVariant } from '@/components/usage-shared'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Badge, type BadgeProps } from '@/components/ui/badge'
import { Button, buttonVariants } from '@/components/ui/button'
import {
  Empty,
  EmptyContent,
  EmptyDescription,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
} from '@/components/ui/empty'
import { Meter, MeterIndicator, MeterTrack } from '@/components/ui/meter'
import { Skeleton } from '@/components/ui/skeleton'
import { Spinner } from '@/components/ui/spinner'
import { Switch } from '@/components/ui/switch'
import { Tabs, TabsList, TabsPanel, TabsTab } from '@/components/ui/tabs'
import { Tooltip, TooltipPopup, TooltipTrigger } from '@/components/ui/tooltip'

/** 详情页里「最近请求」只取一页的这么多条；要翻页看全量走请求明细对话框。 */
const RECENT_USAGE_LIMIT = 10

/**
 * 详情页的轮询节奏与账号列表一致（30 秒）：页面上的额度、名额都来自同一份列表接口，
 * 这里另拉的流水、设备、会话若不跟着刷，一屏会出现两个不同时刻的数。
 */
const DETAIL_REFETCH_MS = 30_000

type Translate = (zh: string, en: string) => string

/** 状态徽章配色 → 页头那条说明的 Alert 配色；Alert 没有 plan / outline 这类纯分类色。 */
function alertVariantOf(variant: BadgeProps['variant']): 'error' | 'warning' | 'info' | 'default' {
  if (variant === 'error' || variant === 'destructive') return 'error'
  if (variant === 'warning') return 'warning'
  if (variant === 'info') return 'info'
  return 'default'
}

/**
 * 设备 / 会话 / RPM 上限的三态（>0 独立、0 跟随默认、<0 不限），写在读数格的小字里。
 * 生效上限已经是读数的分母，这里只交代它从哪来——配色同卡片页脚那枚图标：跟随默认、自定义、不限。
 */
function limitPolicyLabel(limit: number, t: Translate): string {
  if (limit === 0) return t('跟随默认', 'Default')
  if (limit < 0) return t('不限', 'Unlimited')
  return t('自定义', 'Custom')
}

/**
 * 上游 `representative-claim` 的原词 → 与上面几条进度条同名的称呼。认不出的原样显示（等宽），
 * 上游随时可能加新窗口，硬翻只会翻错；原词始终挂在悬浮提示里。
 */
function representativeLabel(claim: string, t: Translate): ReactNode {
  switch (claim) {
    case 'five_hour':
    case '5h':
      return t('5 小时窗口', '5-hour window')
    case 'seven_day':
    case '7d':
      return t('7 天窗口', '7-day window')
    case 'seven_day_overage_included':
    case '7d_oi':
      return t('fable 额度池', 'Fable pool')
    default:
      return <span className="font-mono text-xs">{claim}</span>
  }
}

/** 提前停调度阈值的三态（null 跟随全局、0 不停、1..100 独立）。 */
function pausePolicyText(pct: number | null, effective: number, t: Translate): string {
  const effectiveText = effective > 0 ? `${effective}%` : t('不停', 'off')
  if (pct == null) return t(`跟随全局（${effectiveText}）`, `Global (${effectiveText})`)
  if (pct === 0) return t('不停', 'Off')
  return t(`${pct}%（自定义）`, `${pct}% (custom)`)
}

export function CredentialDetailPage({
  id,
  credentials,
  isLoading,
  error,
  onRetry,
  onBack,
}: {
  id: number
  credentials: Credential[] | undefined
  isLoading: boolean
  error: unknown
  onRetry: () => void
  onBack: () => void
}) {
  const { t, language } = useI18n()
  // 详情页不另开接口：账号列表接口本来就带全了单个账号的字段，且已按 30 秒轮询，
  // 这里从同一份缓存里挑出这一个，列表页与详情页看到的永远是同一时刻的数。
  const cred = credentials?.find((item) => item.id === id)
  const credentialLabel = cred ? displayCredentialLabel(cred.label, language) : `#${id}`

  let content: ReactNode
  if (cred) {
    content = <CredentialDetail cred={cred} onDeleted={onBack} />
  } else if (isLoading && !credentials) {
    content = <DetailSkeleton />
  } else if (error && !credentials) {
    content = (
      <Alert variant="error">
        <AlertTitle>{t('账号信息读取失败', 'Failed to load the account')}</AlertTitle>
        <AlertDescription>
          <p className="break-words">{extractError(error, language)}</p>
          <Button type="button" size="sm" variant="destructive-outline" onClick={onRetry}>
            <RefreshCwIcon />
            {t('重试', 'Retry')}
          </Button>
        </AlertDescription>
      </Alert>
    )
  } else {
    content = (
      <Empty className="py-16">
        <EmptyHeader>
          <EmptyMedia variant="icon"><UserRoundSearchIcon /></EmptyMedia>
          <EmptyTitle className="text-base">{t('找不到这个账号', 'Account not found')}</EmptyTitle>
          <EmptyDescription>
            {t(
              `账号 #${id} 不存在，可能已被删除。`,
              `Account #${id} does not exist; it may have been deleted.`,
            )}
          </EmptyDescription>
        </EmptyHeader>
        <EmptyContent>
          <Button type="button" variant="outline" onClick={onBack}>{t('返回账号池', 'Back to account pool')}</Button>
        </EmptyContent>
      </Empty>
    )
  }

  return (
    <div className="app-shell flex min-h-dvh flex-col text-foreground">
      <AppHeader actions={<PreferencesMenu />} onNavigateHome={onBack} />
      <main className="page-frame relative flex-1 py-5 pb-8 sm:py-8 sm:pb-12">
        <div className="space-y-5 sm:space-y-7">
          <Breadcrumb
            current={credentialLabel}
            parent={t('账号池', 'Account pool')}
            onNavigateParent={onBack}
          />
          {content}
        </div>
      </main>
      <AppFooter />
    </div>
  )
}

/**
 * 页头下面那条状态提示：**默认一行**——状态名 + 截断的一句说明 + 「详情」，点开才摊出完整说明
 * 与上游原话。
 *
 * 原来它把说明和上游原话一次全摊开：token 失效要占三行，封号的上游原话更长，把读数和正文往下推
 * 一大截，而这些话多数时候看一眼状态名就够了。常见的通知条（Cloudflare 控制台的 banner 也是）
 * 都是这个结构：一行说清「出了什么事」，细节按需展开。
 *
 * 停用原因与自动恢复时刻都归这条提示说，账号信息里不再各列一行：限流暂停的说明本身带恢复时刻；
 * 封号的说明就是上游原话；token 失效的说明是泛泛一句，上游原话放在展开后的灰底块里。
 * 「详情」只在真有东西可展开时出现：那一句被截断了（按实际渲染宽度量，窗口一变就重量），或者
 * 另有上游原话。桌面上一行放得下的说明再给一个「详情」，点开看到的还是同一句话。
 */
function StatusBanner({ cred, status }: { cred: Credential; status: CredentialStatusMeta }) {
  const { t, language } = useI18n()
  const [open, setOpen] = useState(false)
  const raw = cred.ban_reason && status.kind === 'token-invalid'
    ? localizeBackendMessage(cred.ban_reason, language)
    : null
  const detailRef = useRef<HTMLSpanElement>(null)
  const [truncated, setTruncated] = useState(false)
  useEffect(() => {
    const el = detailRef.current
    if (!el) return
    const measure = () => setTruncated(el.scrollWidth > el.clientWidth + 1)
    measure()
    const observer = new ResizeObserver(measure)
    observer.observe(el)
    return () => observer.disconnect()
  }, [status.detail, open])
  const expandable = raw != null || truncated || open
  const panelId = `status-banner-detail-${cred.id}`
  return (
    <Alert variant={alertVariantOf(status.variant)} className="py-2.5">
      <InfoIcon />
      <div className="flex min-w-0 items-center gap-2">
        <AlertTitle className="shrink-0">{status.label}</AlertTitle>
        {!open && (
          <span ref={detailRef} className="min-w-0 flex-1 truncate text-muted-foreground" title={status.detail}>{status.detail}</span>
        )}
        {expandable && (
          <Button
            type="button"
            size="xs"
            variant="ghost"
            className="-my-1 ml-auto shrink-0 text-muted-foreground"
            aria-expanded={open}
            aria-controls={panelId}
            onClick={() => setOpen((v) => !v)}
          >
            {open ? t('收起', 'Less') : t('详情', 'Details')}
            <ChevronDownIcon className={cn('transition-transform', open && 'rotate-180')} />
          </Button>
        )}
      </div>
      {open && (
        <AlertDescription id={panelId} className="mt-1 break-words">
          <p>{status.detail}</p>
          {raw && (
            <div className="space-y-1">
              <p className="text-xs font-medium">{t('上游原话', 'Upstream message')}</p>
              <p className="rounded-md bg-muted/64 px-2.5 py-2 font-mono text-xs [overflow-wrap:anywhere]">{raw}</p>
            </div>
          )}
        </AlertDescription>
      )}
    </Alert>
  )
}

type DetailTab = 'overview' | 'stats' | 'requests' | 'bindings' | 'bans'

/** 手机分页签的断点，与 Tailwind 的 `sm`、⋯ 底部面板、对话框贴底是同一条线（40rem）。 */
const MOBILE_QUERY = '(max-width: 39.98rem)'

/**
 * 页头与六格读数之下的正文。
 *
 * 桌面整页铺开（各块按行对齐，见 overview 里那段）；**手机上改成五个页签**：同样的内容在 375px
 * 上一路摞下来有四千多像素，想看封号记录得先滑过图表、流水、设备三大块。页头、状态提示和六格
 * 读数留在页签上面常驻，它们是「这个号现在怎么样」，切到哪一页都该看得见；页签栏吸顶，
 * 滑到哪儿都能换页。页签只切换挂载哪一块，数据查询各块自己管，切走再切回来命中缓存。
 */
function DetailBody({
  overview,
  stats,
  requests,
  bindings,
  bans,
}: {
  overview: ReactNode
  stats: ReactNode
  requests: ReactNode
  bindings: ReactNode
  bans: ReactNode
}) {
  const { t } = useI18n()
  const mobile = useMediaQuery(MOBILE_QUERY)
  const [tab, setTab] = useState<DetailTab>('overview')
  const barRef = useRef<HTMLDivElement>(null)
  if (!mobile) {
    return (
      <>
        {overview}
        {stats}
        {requests}
        {bindings}
        {bans}
      </>
    )
  }
  // 页签不挂数量角标：正上方六格读数里就有设备、会话与被封停次数，「设备 5」还是设备加会话的
  // 合计，与读数「2/3」对不上，反倒要人去想这 5 是怎么来的。
  const tabs: { key: DetailTab; label: string }[] = [
    { key: 'overview', label: t('概览', 'Overview') },
    { key: 'stats', label: t('统计', 'Stats') },
    { key: 'requests', label: t('请求', 'Requests') },
    { key: 'bindings', label: t('设备', 'Devices') },
    { key: 'bans', label: t('封号', 'Bans') },
  ]
  const panels: Record<DetailTab, ReactNode> = { overview, stats, requests, bindings, bans }
  const change = (value: unknown) => {
    if (typeof value !== 'string' || !tabs.some((item) => item.key === value)) return
    setTab(value as DetailTab)
    // 已经滑过了页签栏就把新页签的开头带回栏下面，否则换页后停在一段莫名其妙的中间。
    const bar = barRef.current
    if (bar) {
      const stuckAt = bar.getBoundingClientRect().top + window.scrollY
      const headerHeight = parseFloat(getComputedStyle(bar).top) || 0
      if (window.scrollY > stuckAt - headerHeight) {
        window.scrollTo({ top: stuckAt - headerHeight, behavior: 'instant' })
      }
    }
  }
  return (
    <Tabs value={tab} onValueChange={change} className="gap-5">
      <div
        ref={barRef}
        // 吸在顶栏下面（顶栏 3.5rem + 刘海），底色与页面同色，滑过的内容从它底下走。
        className="sticky top-[calc(3.5rem+env(safe-area-inset-top))] z-10 -mx-4 bg-surface-page px-4 py-2"
      >
        <TabsList className="w-full" aria-label={t('账号详情分区', 'Account detail sections')}>
          {tabs.map((item) => (
            <TabsTab key={item.key} value={item.key} className="min-w-0 px-1">
              <span className="truncate">{item.label}</span>
            </TabsTab>
          ))}
        </TabsList>
      </div>
      {tabs.map((item) => (
        <TabsPanel key={item.key} value={item.key} className="space-y-5">
          {tab === item.key && panels[item.key]}
        </TabsPanel>
      ))}
    </Tabs>
  )
}

/**
 * 邮箱式的账号名在 `@` 前留一个断点：窄屏折行时断成「local / @domain」，而不是在单词中间
 * 随便切一刀（`yahoo.co` / `m`）。其余名字原样，实在太长由外层 `overflow-wrap:anywhere` 兜底。
 */
function BreakableLabel({ label }: { label: string }) {
  const at = label.indexOf('@')
  if (at <= 0) return <>{label}</>
  return <>{label.slice(0, at)}<wbr />{label.slice(at)}</>
}

/** 账号列表还没回来时的占位：与真页面同构（页头、六格读数、两栏），数据到了不跳版。 */
function DetailSkeleton() {
  const { t } = useI18n()
  return (
    <div aria-busy="true" aria-label={t('正在读取账号信息', 'Loading account')} className="space-y-5 sm:space-y-7">
      <div className="space-y-3">
        <Skeleton className="h-8 w-64 max-w-full" />
        <Skeleton className="h-5 w-80 max-w-full" />
      </div>
      <div className="grid grid-cols-2 overflow-hidden rounded-xl border bg-card lg:grid-cols-6">
        <OverviewMetricSkeleton className="border-r border-b lg:border-b-0" />
        <OverviewMetricSkeleton className="border-b lg:border-r lg:border-b-0" />
        <OverviewMetricSkeleton className="border-r border-b lg:border-b-0" />
        <OverviewMetricSkeleton className="border-b lg:border-r lg:border-b-0" />
        <OverviewMetricSkeleton className="border-r" />
        <OverviewMetricSkeleton />
      </div>
      <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_22rem]">
        <Skeleton className="h-72 rounded-2xl" />
        <Skeleton className="h-72 rounded-2xl" />
      </div>
      <Skeleton className="h-56 rounded-2xl" />
    </div>
  )
}

function CredentialDetail({ cred, onDeleted }: { cred: Credential; onDeleted: () => void }) {
  const { t, language } = useI18n()
  const now = useNowSeconds()
  const [renaming, setRenaming] = useState(false)
  const [name, setName] = useState(cred.label)
  const [devicesOpen, setDevicesOpen] = useState(false)
  const [proxyOpen, setProxyOpen] = useState(false)
  const [rpmOpen, setRpmOpen] = useState(false)
  const [quotaOpen, setQuotaOpen] = useState(false)
  const [usageOpen, setUsageOpen] = useState(false)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [testing, setTesting] = useState(false)

  const actions = useCredentialActions(cred, () => setRenaming(false))
  const { toggle, remove } = actions
  const evaluation = evaluateCredential(cred, now, language)
  const { status } = evaluation
  const credentialLabel = displayCredentialLabel(cred.label, language)

  // 删掉之后这一页就没有对象了，直接回账号池，而不是停在一张「找不到这个账号」上。
  useEffect(() => {
    if (remove.isSuccess) onDeleted()
  }, [remove.isSuccess, onDeleted])

  useEffect(() => {
    const previousTitle = document.title
    document.title = `${credentialLabel} · Luban`
    return () => {
      document.title = previousTitle
    }
  }, [credentialLabel])

  const openRename = () => {
    setName(cred.label)
    setRenaming(true)
  }

  return (
    <>
      {/* 独立页面的页头：与设置页同一套层级（面包屑 → 页标题 → 分组），不套账号池首页那张卡片。
          标题下一行是这个号的身份徽章，操作在右侧；窄屏整块换到标题下面。 */}
      <section aria-labelledby="credential-detail-title" className="flex flex-wrap items-start justify-between gap-x-6 gap-y-4">
        <div className="min-w-0 flex-1 basis-80 space-y-2.5">
          <div className="flex min-w-0 items-baseline gap-2">
            <h1
              // 窄屏上折行显示全名（邮箱常有 30 多个字符，截断就认不出是哪个号）；sm 起一行放得下，照常截断。
              className="min-w-0 text-xl font-semibold tracking-tight [overflow-wrap:anywhere] sm:truncate sm:text-2xl"
              id="credential-detail-title"
              title={credentialLabel}
            >
              <BreakableLabel label={credentialLabel} />
            </h1>
            <span className="shrink-0 text-sm text-muted-foreground tabular-nums max-sm:hidden">#{cred.id}</span>
            {/* 名称与 ID 就是页标题本身，账号信息里不再重复列；重命名入口跟着标题走。
                手机上藏掉：标题常折成两行，铅笔被挤到右上角孤零零一枚，而 ⋯ 面板里就有「重命名」。 */}
            <Button
              type="button"
              size="icon-xs"
              variant="ghost"
              className="shrink-0 self-center max-sm:hidden"
              aria-label={t('重命名', 'Rename')}
              title={t('重命名', 'Rename')}
              onClick={openRename}
            >
              <PencilIcon />
            </Button>
          </div>
          <div className="flex flex-wrap items-center gap-2">
            <span className="text-sm text-muted-foreground tabular-nums sm:hidden">#{cred.id}</span>
            {/* 需处理的状态由下面那条提示条说（它的标题就是状态名），这里再挂一枚同名徽章就是同一句话
                说两遍；正常、已停用这类没有提示条的状态才在这里露面。上游判定同理不放页头，
                用量限制那一块有「上游判定 / 起约束的窗口」，被拒的那条进度条也标着。 */}
            {!status.attention && <Badge variant={status.variant} aria-live="polite">{status.label}</Badge>}
            {isOrgAccount(cred) && (
              <Badge variant="outline">
                <Building2Icon className="size-3" />
                {orgBadgeLabel(cred)}
              </Badge>
            )}
            {cred.tier && <Badge variant={tierBadgeVariant(cred.tier)}>{cred.tier}</Badge>}
          </div>
        </div>
        {/* 手机上操作整行铺开：连通性测试拉宽成主按钮，⋯ 与启停开关贴右。 */}
        <div className="flex shrink-0 items-center gap-2 max-sm:w-full">
          <Button type="button" variant="outline" className="max-sm:flex-1" onClick={() => setTesting(true)}>
            <ActivityIcon />
            {t('连通性测试', 'Connectivity test')}
          </Button>
          <CredentialActionsMenu
            triggerClassName={buttonVariants({ size: 'icon', variant: 'outline' })}
            triggerLabel={t(`打开 ${credentialLabel} 菜单`, `Open menu for ${credentialLabel}`)}
              cred={cred}
              actions={actions}
              onRename={openRename}
              onDeviceLimit={() => setDevicesOpen(true)}
              onRpmLimit={() => setRpmOpen(true)}
              onQuotaPause={() => setQuotaOpen(true)}
              onProxy={() => setProxyOpen(true)}
              onUsage={() => setUsageOpen(true)}
              onTest={() => setTesting(true)}
              onRequestDelete={() => setConfirmDelete(true)}
              showDetail={false}
          />
          {/* 启停开关与卡片页脚同一处理：钉在最右，旁边只在切换中挂一枚 Spinner。 */}
          <div className="flex items-center gap-2 pl-1">
            {toggle.isPending && <Spinner />}
            <Switch
              checked={!cred.disabled}
              onCheckedChange={(enabled) => toggle.mutate(!enabled)}
              disabled={toggle.isPending}
              title={switchTitle(cred, language)}
              aria-label={`${credentialLabel}: ${switchTitle(cred, language)}`}
            />
          </div>
        </div>
      </section>

      {/* 卡片上需处理的状态只挂在徽章的悬浮提示里（卡片要省高度）；详情页有地方，直接摊开说。 */}
      {status.attention && <StatusBanner cred={cred} status={status} />}

      <StatsRow
        cred={cred}
        now={now}
        onDevices={() => setDevicesOpen(true)}
        onRpm={() => setRpmOpen(true)}
        onUsage={() => setUsageOpen(true)}
      />

      <DetailBody
        overview={(
          <>
            {/* 按行对齐而不是按栏：两栏各自往下堆，只要内容长短不一，底边就永远对不齐。
                这一行只放内容量相近的两块，并让两张卡等高（网格默认 stretch + 卡片 h-full）；
                其余各块铺满整宽，调度配置那几行本来就是设置页「说明在左、控件在右」的整行排法。 */}
            <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_22rem]">
              <QuotaSection cred={cred} now={now} />
              <InfoSection cred={cred} now={now} />
            </div>
            <ScheduleSection
              cred={cred}
              onProxy={() => setProxyOpen(true)}
              onQuota={() => setQuotaOpen(true)}
            />
          </>
        )}
        stats={<CredentialStatsSection cred={cred} />}
        requests={<RecentUsageSection cred={cred} onViewAll={() => setUsageOpen(true)} />}
        bindings={<BindingsSection cred={cred} onManage={() => setDevicesOpen(true)} />}
        bans={<BanEventsSection cred={cred} />}
      />

      <DeferredMount open={renaming || proxyOpen || devicesOpen || usageOpen || confirmDelete || rpmOpen || quotaOpen || testing}>
        <RenameCredentialDialog
          cred={cred}
          actions={actions}
          name={name}
          onNameChange={setName}
          open={renaming}
          onOpenChange={setRenaming}
        />
        <CredentialProxyDialog cred={cred} open={proxyOpen} onOpenChange={setProxyOpen} proxy={actions.proxy} />
        <CredentialRpmDialog cred={cred} open={rpmOpen} onOpenChange={setRpmOpen} rpmLimit={actions.rpmLimit} />
        <CredentialQuotaDialog cred={cred} open={quotaOpen} onOpenChange={setQuotaOpen} quotaPause={actions.quotaPause} />
        <CredentialDevicesDialog
          cred={cred}
          open={devicesOpen}
          onOpenChange={setDevicesOpen}
          limit={actions.limit}
          sessionLimit={actions.sessionLimit}
        />
        <CredentialUsageDialog cred={cred} open={usageOpen} onOpenChange={setUsageOpen} />
        <DeleteCredentialDialog cred={cred} actions={actions} open={confirmDelete} onOpenChange={setConfirmDelete} />
        <ConnectivityTestDialog cred={cred} open={testing} onOpenChange={setTesting} />
      </DeferredMount>
    </>
  )
}

/** 名额占用档位 → OverviewMetric 的色调：只有吃紧 / 占满才上色，常态不着色（理由见 OverviewMetric）。 */
function levelTone(level: QuotaLevel): 'bad' | 'warn' | 'neutral' {
  if (level === 'critical') return 'bad'
  if (level === 'warning') return 'warn'
  return 'neutral'
}

/**
 * 页头下面那排六格：名额三格 + 费用 + 最近使用 + 封号次数。单格复用 OverviewMetric（图标、字号、
 * 告警色与首页概览一致），外面是这一页自己的一条读数带。
 *
 * 前四格点开的是与卡片页脚同一批对话框（带角标），详情页是卡片的放大版，入口不该换地方。
 */
function StatsRow({
  cred,
  now,
  onDevices,
  onRpm,
  onUsage,
}: {
  cred: Credential
  now: number
  onDevices: () => void
  onRpm: () => void
  onUsage: () => void
}) {
  const { t, language, locale } = useI18n()
  const ratio = (count: number, limit: number) =>
    `${count.toLocaleString(locale)}/${limit > 0 ? limit.toLocaleString(locale) : '∞'}`
  return (
    <section
      aria-label={t('账号概览', 'Account overview')}
      className="grid grid-cols-2 overflow-hidden rounded-xl border bg-card shadow-xs/5 lg:grid-cols-6"
    >
      <OverviewMetric
        className="border-r border-b lg:border-b-0"
        label={t('当前 RPM', 'Current RPM')}
        value={ratio(cred.rpm, cred.rpm_limit_effective)}
        status={limitPolicyLabel(cred.rpm_limit, t)}
        statusHint={t('最近 60 秒经该账号转发的请求数（含失败请求） / 生效上限。点击调整上限', 'Requests forwarded in the last 60 seconds (failures included) / effective limit. Click to adjust the limit')}
        icon={GaugeIcon}
        tone={levelTone(deviceUsageMeta(cred.rpm, cred.rpm_limit_effective).level)}
        opensDetail
        onClick={onRpm}
      />
      <OverviewMetric
        className="border-b lg:border-r lg:border-b-0"
        label={t('设备', 'Devices')}
        value={ratio(cred.device_count, cred.device_limit_effective)}
        status={limitPolicyLabel(cred.device_limit, t)}
        statusHint={t('已绑定设备 / 生效上限。点击查看设备或调整上限', 'Bound devices / effective limit. Click to view devices or adjust the limit')}
        icon={SmartphoneIcon}
        tone={levelTone(deviceUsageMeta(cred.device_count, cred.device_limit_effective).level)}
        opensDetail
        onClick={onDevices}
      />
      <OverviewMetric
        className="border-r border-b lg:border-b-0"
        label={t('模拟会话', 'Sessions')}
        value={ratio(cred.session_count, cred.session_limit_effective)}
        status={limitPolicyLabel(cred.session_limit, t)}
        statusHint={t('活跃模拟会话 / 生效上限。点击查看会话或调整上限', 'Active simulated sessions / effective limit. Click to view sessions or adjust the limit')}
        icon={MessagesSquareIcon}
        tone={levelTone(deviceUsageMeta(cred.session_count, cred.session_limit_effective).level)}
        opensDetail
        onClick={onDevices}
      />
      <OverviewMetric
        className="border-b lg:border-r lg:border-b-0"
        label={t('累计费用', 'Total cost')}
        value={formatUsd(cred.cost_total)}
        statusHint={t('按公开价目表估算的等价 API 费用，不是账单金额。点击查看请求明细', 'Equivalent API cost estimated from the public price list, not a bill. Click to view the request log')}
        icon={WalletCardsIcon}
        tone="neutral"
        opensDetail
        onClick={onUsage}
      />
      <OverviewMetric
        className="border-r"
        label={t('最近使用', 'Last used')}
        value={cred.last_used ? relativeTime(cred.last_used, now, language) : t('从未使用', 'Never')}
        statusHint={cred.last_used ? formatFullTime(cred.last_used, language) : undefined}
        icon={ClockIcon}
        tone="neutral"
      />
      <OverviewMetric
        label={t('被封停次数', 'Auto-disables')}
        value={cred.ban_count.toLocaleString(locale)}
        statusHint={t('自动封停的累计次数，解封不清零', 'Cumulative automatic disables; re-enabling does not reset it')}
        icon={ShieldAlertIcon}
        tone={cred.ban_count > 0 ? 'bad' : 'neutral'}
      />
    </section>
  )
}

/** 额度一节：5h / 7d 两个窗口各一整行（卡片上挤在半格里的那几项在这里摊开）、额外窗口、上游判定、模型限制。 */
function QuotaSection({ cred, now }: { cred: Credential; now: number }) {
  const { t, language } = useI18n()
  const evaluation = evaluateCredential(cred, now, language)
  const { quota } = evaluation
  const snapshot = cred.quota
  // fable 专用额度池（7d_oi），见 fablePoolWindow。
  const fablePool = fablePoolWindow(quota)
  // 「上游判定 / 起约束的窗口 / Usage credits」三格只在有话可说时出现：正常号那一行是
  // 「已放行 · — · 未知」，全是默认值，读完一无所获。被拒、预警、上游点名了起约束的窗口、
  // 正在烧 Usage credits，任一成立才画。
  // 页头提示条正在说 Usage credits（生效中 / 待确认）时，这里不再重复那一格。
  const overageInBanner = evaluation.status.kind === 'overage' || evaluation.status.kind === 'overage-unknown'
  // 上游点名的起约束窗口已经画成进度条（被拒的那条红着、标着「已拒绝」）时，「上游判定 · 起约束的
  // 窗口」两格说的就是那条红条，不再另起一行重复；与卡片上不挂「上游 · 已拒绝」同一条规则。
  const verdictOnMeter = snapshot != null && verdictShownByMeter(
    snapshot.rl_representative, quota.h5.reported, quota.d7.reported, fablePool != null,
  )
  const upstreamNotable = snapshot != null && (
    (!verdictOnMeter && (
      (snapshot.unified_status != null && snapshot.unified_status !== 'allowed')
      || snapshot.rl_representative != null
    ))
    || (snapshot.overage_in_use === true && !overageInBanner)
  )
  const extraVisible = visibleExtraWindows(quota.extraWindows).length > 0
  const cooldowns = cred.rate_limited_models ?? []
  const denials = cred.denied_models ?? []
  const clear = useCredentialActions(cred).cooldown
  const overageText = snapshot?.overage_in_use == null
    ? t('未知', 'Unknown')
    : snapshot.overage_in_use
      ? t('正在使用', 'In use')
      : t('未使用', 'Not in use')

  return (
    <Section
      icon={GaugeIcon}
      title={t('用量限制', 'Usage limits')}
      className="min-w-0 lg:h-full"
      panelClassName="lg:flex-1"
      description={snapshot
        ? t('来自上游限流头的最新快照。', 'Latest snapshot from the upstream rate-limit headers.')
        : t('还没有额度快照：该账号转发过带限流头的请求后才会出现。', 'No usage snapshot yet: it appears once the account forwards a request that carries rate-limit headers.')}
      mobileDescription={snapshot ? undefined : t('还没有额度快照', 'No usage snapshot yet')}
      // 更新时刻挂在标题右侧，与卡片「用量限制 … 🕐 更新于 刚刚」同一个位置：它是这一整块数据的
      // 时间戳，不是一句说明。原来在手机上它单独占标题下一整行（四个字），桌面上又埋在一长句说明里。
      action={snapshot ? (
        <Tooltip>
          <TooltipTrigger
            render={<span />}
            delay={0}
            className="inline-flex h-6 cursor-help items-center gap-1 text-xs text-muted-foreground tabular-nums"
          >
            <ClockIcon aria-hidden className="size-3.5" />
            {t(`更新于 ${relativeTime(snapshot.ts, now, language)}`, `Updated ${relativeTime(snapshot.ts, now, language)}`)}
          </TooltipTrigger>
          <TooltipPopup>{t(`快照于 ${formatFullTime(snapshot.ts, language)}`, `Snapshot at ${formatFullTime(snapshot.ts, language)}`)}</TooltipPopup>
        </Tooltip>
      ) : undefined}
    >
      <div className="divide-y">
        {snapshot && (quota.h5.reported || quota.d7.reported || fablePool) ? (
          <div className="grid gap-5 p-4 sm:p-5 md:grid-cols-2">
            {quota.h5.reported && (
              <WindowDetail
                label={t('5 小时窗口', '5-hour window')}
                util={quota.h5.utilization}
                reset={snapshot.rl_5h_reset}
                requests={snapshot.requests_5h}
                tokens={snapshot.tokens_5h}
                cost={snapshot.cost_5h}
                now={now}
              />
            )}
            {(quota.d7.reported || fablePool) && (
              // fable 额度池挂在 7d 下面，与卡片、列表同一规则：同为 7 天周期，上下对着比。
              <div className="min-w-0 space-y-4">
                {quota.d7.reported && (
                  <WindowDetail
                    label={t('7 天窗口', '7-day window')}
                    util={quota.d7.utilization}
                    reset={snapshot.rl_7d_reset}
                    requests={snapshot.requests_7d}
                    tokens={snapshot.tokens_7d}
                    cost={snapshot.cost_7d}
                    now={now}
                  />
                )}
                {fablePool && (
                  <WindowDetail
                    label={(
                      <span className="inline-flex items-center gap-1.5">
                        {t('fable 额度池', 'Fable pool')}
                        <span className="font-mono text-xs font-normal text-muted-foreground">7d_oi</span>
                        <InfoHint>
                          {t(
                            '上游只报使用率与重置时刻，没有按窗口统计的请求数与费用；fable 的用量看用量统计的按模型拆分。满了只影响 fable，账号其余模型照常。',
                            'Upstream reports only utilisation and reset, with no per-window request or cost totals; see fable usage in the by-model breakdown. When full only fable is affected.',
                          )}
                        </InfoHint>
                      </span>
                    )}
                    util={fablePool.utilization}
                    reset={fablePool.resetAt}
                    rejected={fablePool.status === 'rejected' || fablePool.status === 'rate_limited'}
                    now={now}
                  />
                )}
              </div>
            )}
          </div>
        ) : snapshot ? (
          <p className="p-4 text-sm text-muted-foreground sm:p-5">{t('上游尚未返回用量窗口。', 'The upstream has not returned usage windows yet.')}</p>
        ) : null}

        {snapshot && (upstreamNotable || extraVisible) && (
          <div className="space-y-3 p-4 sm:p-5">
            {upstreamNotable && (
            <dl className="grid grid-cols-2 gap-x-4 gap-y-3 text-sm sm:grid-cols-3">
              {!verdictOnMeter && (
              <>
              <Fact label={t('上游判定', 'Upstream verdict')}>
                {snapshot.unified_status ? unifiedQuotaStatusLabel(snapshot.unified_status, language) : '—'}
              </Fact>
              <Fact label={t('起约束的窗口', 'Binding window')}>
                {snapshot.rl_representative ? (
                  <span title={snapshot.rl_representative}>{representativeLabel(snapshot.rl_representative, t)}</span>
                ) : '—'}
              </Fact>
              </>
              )}
              {!overageInBanner && (
                <Fact label="Usage credits">
                  <span className={snapshot.overage_in_use ? 'font-medium text-destructive-foreground' : undefined}>{overageText}</span>
                </Fact>
              )}
            </dl>
            )}
            {extraVisible && <ExtraWindows windows={quota.extraWindows} />}
          </div>
        )}

        {(cooldowns.length > 0 || denials.length > 0) && (
          <div className="space-y-3 p-4 sm:p-5">
            <div className="flex flex-wrap items-center justify-between gap-2">
              <h3 className="flex items-center gap-1.5 font-medium text-sm">
                {t('模型限制', 'Model restrictions')}
                {/* 解释收进 ⓘ：看懂一次就够了，不必每次都占一行。 */}
                <InfoHint>
                  {t(
                    '只影响列出的模型：选号时这些模型绕开该账号，其余模型照常服务。',
                    'Only the listed models are affected: they skip this account during selection while its other models keep serving.',
                  )}
                </InfoHint>
              </h3>
              <Button
                type="button"
                size="xs"
                variant="outline"
                loading={clear.isPending}
                onClick={() => clear.mutate()}
              >
                <TimerOffIcon />
                {denials.length > 0
                  ? t('解除冷却与模型限制', 'Clear cooldown & model blocks')
                  : t('解除冷却', 'Clear cooldown')}
              </Button>
            </div>
            <ul className="space-y-1.5">
              {cooldowns.map((m) => (
                <li key={`cd-${m.model}`} className="flex flex-wrap items-center gap-x-3 gap-y-1 rounded-lg border px-3 py-2 text-sm">
                  <span className="font-mono text-xs">{m.model}</span>
                  <Badge size="sm" variant={m.gated ? 'warning' : 'secondary'}>
                    {m.gated ? t('冷却中', 'Cooling down') : t('刚被限速', 'Throttled')}
                  </Badge>
                  <span className="text-xs text-muted-foreground">
                    {t(`剩余 ${formatDuration(m.secs, language)}`, `${formatDuration(m.secs, language)} left`)}
                    {!m.gated && t('，仍参与选号', '; still selectable')}
                  </span>
                </li>
              ))}
              {denials.map((d) => (
                <li key={`dn-${d.model}`} className="space-y-1 rounded-lg border px-3 py-2 text-sm">
                  <div className="flex flex-wrap items-center gap-x-3 gap-y-1">
                    <span className="font-mono text-xs">{d.model}</span>
                    <Badge size="sm" variant="secondary"><BanIcon className="size-3" />{t('套餐不含', 'Not in plan')}</Badge>
                    <span className="text-xs text-muted-foreground">
                      {t(`学到于 ${formatFullTime(d.learned_at, language)}`, `Learned ${formatFullTime(d.learned_at, language)}`)}
                      {' · '}
                      {d.expires_at
                        ? t(`${formatFullTime(d.expires_at, language)} 自动失效`, `expires ${formatFullTime(d.expires_at, language)}`)
                        : t('直到手动解除', 'until cleared')}
                    </span>
                  </div>
                  <ExpandableText text={d.reason} label={t('原话', 'Raw')} />
                </li>
              ))}
            </ul>
          </div>
        )}
      </div>
    </Section>
  )
}

function WindowDetail({
  label,
  util,
  reset,
  requests,
  tokens,
  cost,
  rejected = false,
  now,
}: {
  label: ReactNode
  util: number | null
  reset: number | null
  /** 只有 5h / 7d 有这三项（后端按窗口起点聚合）；不传则不摆这三格，只留重置时刻。 */
  requests?: number | null
  tokens?: number | null
  cost?: number | null
  /** 上游对这个窗口的判决是 rejected / rate_limited。 */
  rejected?: boolean
  now: number
}) {
  const hasFacts = requests !== undefined || tokens !== undefined || cost !== undefined
  const { t, language, locale } = useI18n()
  // 窗口已重置时 utilization 被抹成 null，此刻用量确实归零，按 0% 画（同卡片 QuotaMeter）。
  const percentage = quotaPercentage(util) ?? 0
  const level = quotaLevel(util)
  const resetDue = reset != null && reset > now
  // 与卡片 QuotaMeter 同一口径：常态前景色，吃紧琥珀、打满红，文字走 `-foreground` 那一支。
  const valueClass = level === 'critical'
    ? 'text-destructive-foreground'
    : level === 'warning'
      ? 'text-warning-foreground'
      : 'text-foreground'
  return (
    <Meter value={percentage} max={100} className="gap-2.5">
      <div className="flex items-baseline justify-between gap-3">
        <span className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 font-medium text-sm">
          <span>{label}</span>
          {rejected && <Badge size="sm" variant="error">{t('已拒绝', 'Rejected')}</Badge>}
        </span>
        <span className={cn('font-semibold text-lg tabular-nums', valueClass)}>
          {percentage}%
        </span>
      </div>
      <MeterTrack className="h-2 rounded-full">
        <MeterIndicator className={cn(METER_FILL[level], 'rounded-full')} />
      </MeterTrack>
      {/* 重置时刻跟在条下面一行说完：「16:54 重置 · 1h 39m」。原来它是右下角一格事实，完整日期
          加倒计时在窄屏上要折两行，三个窗口叠起来每个都高出一截；日期到分钟的精确值放 title。 */}
      {/* 上游没给重置时刻就不写这一行；已经过了的写「已重置」，不再摆一句「没有待到的重置时刻」。 */}
      {reset != null && (
      <p className="text-xs text-muted-foreground tabular-nums" title={formatFullTime(reset, language)}>
        {resetDue
          ? t(
              `${formatClockTime(reset, language)} 重置 · ${formatCountdown(reset, now)}`,
              `Resets ${formatClockTime(reset, language)} · ${formatCountdown(reset, now)}`,
            )
          : t(`${formatClockTime(reset, language)} 已重置`, `Reset at ${formatClockTime(reset, language)}`)}
      </p>
      )}
      {hasFacts && (
        <dl className="grid grid-cols-3 gap-x-3 text-sm">
          <Fact label={t('请求数', 'Requests')}>
            {requests == null ? '—' : requests.toLocaleString(locale)}
          </Fact>
          <Fact label="Token">
            {tokens == null ? '—' : (
              <span title={tokens.toLocaleString(locale)}>{formatTokens(tokens)}</span>
            )}
          </Fact>
          <Fact label={t('费用', 'Cost')}>
            {cost == null ? '—' : formatUsd(cost)}
          </Fact>
        </dl>
      )}
    </Meter>
  )
}

/** 标题旁的 ⓘ：解释性的话收进悬浮提示，看懂一次就不必每次占一行。触屏上点一下即出。 */
function InfoHint({ children }: { children: ReactNode }) {
  const { t } = useI18n()
  return (
    <Tooltip>
      <TooltipTrigger
        render={<button type="button" />}
        delay={0}
        aria-label={t('说明', 'Explanation')}
        className="relative inline-flex rounded-full text-muted-foreground outline-none transition-colors hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring pointer-coarse:after:absolute pointer-coarse:after:-inset-3"
      >
        <InfoIcon aria-hidden className="size-3.5" />
      </TooltipTrigger>
      <TooltipPopup className="max-w-72 whitespace-normal text-left font-normal leading-5">{children}</TooltipPopup>
    </Tooltip>
  )
}

/**
 * 上游原话这类长文本：默认一行截断，末尾「原话 ⌄」展开。与页头状态提示同一套——原话要能查，
 * 但不该每次都摊成一大段（手机上一条「套餐不含」的原话就是五行）。一行放得下时不给按钮。
 */
function ExpandableText({ text, label }: { text: string; label: string }) {
  const { t } = useI18n()
  const [open, setOpen] = useState(false)
  const ref = useRef<HTMLParagraphElement>(null)
  const [truncated, setTruncated] = useState(false)
  useEffect(() => {
    const el = ref.current
    if (!el || open) return
    const measure = () => setTruncated(el.scrollWidth > el.clientWidth + 1)
    measure()
    const observer = new ResizeObserver(measure)
    observer.observe(el)
    return () => observer.disconnect()
  }, [text, open])
  return (
    <div className="flex min-w-0 items-start gap-2">
      <p
        ref={ref}
        className={cn(
          'min-w-0 flex-1 font-mono text-xs text-muted-foreground',
          open ? '[overflow-wrap:anywhere]' : 'truncate',
        )}
        title={open ? undefined : text}
      >
        {text}
      </p>
      {(truncated || open) && (
        <button
          type="button"
          className="inline-flex shrink-0 items-center gap-0.5 rounded-sm text-xs text-muted-foreground outline-none hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring"
          aria-expanded={open}
          onClick={() => setOpen((v) => !v)}
        >
          {open ? t('收起', 'Less') : label}
          <ChevronDownIcon aria-hidden className={cn('size-3.5 transition-transform', open && 'rotate-180')} />
        </button>
      )}
    </div>
  )
}

function Fact({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="min-w-0">
      <dt className="text-xs text-muted-foreground">{label}</dt>
      <dd className="mt-0.5 min-w-0 break-words tabular-nums">{children}</dd>
    </div>
  )
}

/** 最近几条流水；要翻页、看全量走请求明细对话框（它有锚点翻页，这里只看最新一页）。 */
function RecentUsageSection({ cred, onViewAll }: { cred: Credential; onViewAll: () => void }) {
  const { t, language, locale } = useI18n()
  const credentialLabel = displayCredentialLabel(cred.label, language)
  const wide = useMediaQuery('(min-width: 64rem)')
  const [lookupId, setLookupId] = useState<string | null>(null)
  const usage = useQuery({
    queryKey: ['credential-usage-recent', cred.id],
    queryFn: () => listCredentialUsage(cred.id, { limit: RECENT_USAGE_LIMIT }),
    refetchInterval: DETAIL_REFETCH_MS,
    placeholderData: keepPreviousData,
  })
  const rows = usage.data?.logs ?? []
  const noteId = `credential-detail-usage-note-${cred.id}`

  return (
    <Section
      icon={ScrollTextIcon}
      title={t('最近请求', 'Recent requests')}
      description={usage.data
        ? t(
            `近 30 天共 ${usage.data.total.toLocaleString(locale)} 条、花费 ${formatUsd(usage.data.total_cost)}；这里显示最新 ${RECENT_USAGE_LIMIT} 条。`,
            `${usage.data.total.toLocaleString(locale)} requests costing ${formatUsd(usage.data.total_cost)} in the last 30 days; showing the newest ${RECENT_USAGE_LIMIT}.`,
          )
        : t('流水仅保留最近 30 天。', 'Logs are retained for 30 days.')}
      mobileDescription={usage.data
        ? t(
            `近 30 天 ${usage.data.total.toLocaleString(locale)} 条 · ${formatUsd(usage.data.total_cost)}`,
            `${usage.data.total.toLocaleString(locale)} in 30 days · ${formatUsd(usage.data.total_cost)}`,
          )
        : undefined}
      action={(
        <>
          <Button
            type="button"
            size="icon-sm"
            variant="ghost"
            disabled={usage.isFetching}
            aria-label={t('刷新', 'Refresh')}
            onClick={() => { void usage.refetch() }}
          >
            <RefreshCwIcon className={usage.isFetching ? 'animate-spin' : undefined} />
          </Button>
          <Button type="button" size="sm" variant="outline" onClick={onViewAll}>
            {t('查看全部', 'View all')}
            <ChevronRightIcon />
          </Button>
        </>
      )}
      panelClassName="p-3 sm:p-4"
    >
      <span id={noteId} className="sr-only">{t('最近请求', 'Recent requests')}</span>
      {usage.isPending ? (
        <div className="space-y-2">
          {Array.from({ length: 4 }, (_, index) => <Skeleton key={index} className="h-9 w-full" />)}
        </div>
      ) : usage.error ? (
        <Alert variant="error">
          <AlertTitle>{t('请求明细读取失败', 'Failed to load request log')}</AlertTitle>
          <AlertDescription className="break-words">{extractError(usage.error, language)}</AlertDescription>
        </Alert>
      ) : rows.length === 0 ? (
        <p className="py-6 text-center text-sm text-muted-foreground">
          {t('此账号转发一次请求后就会出现在这里。', 'Requests forwarded through this account will show up here.')}
        </p>
      ) : wide ? (
        <UsageTable
          rows={rows}
          credentialLabel={credentialLabel}
          descriptionId={noteId}
          loading={usage.isFetching}
          onLookup={setLookupId}
        />
      ) : (
        <UsageCards rows={rows} credentialLabel={credentialLabel} loading={usage.isFetching} onLookup={setLookupId} scroll={false} />
      )}
      {lookupId && (
        <RequestLookupDialog
          open
          initialId={lookupId}
          onOpenChange={(next) => { if (!next) setLookupId(null) }}
        />
      )}
    </Section>
  )
}

/**
 * 设备与模拟会话两份列表直接摊在页面上。查询键与名额对话框共用，
 * 在这里解绑或在对话框里解绑，两边读的是同一份缓存。
 */
function BindingsSection({ cred, onManage }: { cred: Credential; onManage: () => void }) {
  const { t } = useI18n()
  const devices = useQuery({
    queryKey: ['credential-devices', cred.id],
    queryFn: () => listCredentialDevices(cred.id),
    refetchInterval: DETAIL_REFETCH_MS,
  })
  const sessions = useQuery({
    queryKey: ['credential-sessions', cred.id],
    queryFn: () => listCredentialSessions(cred.id),
    refetchInterval: DETAIL_REFETCH_MS,
  })
  return (
    <Section
      icon={SmartphoneIcon}
      title={t('设备与会话', 'Devices & sessions')}
      description={t(
        '带设备身份的客户端按设备占名额，走模拟路径、没有设备身份的来访按会话占名额，两者互不相干。',
        'Clients with a device identity take a device slot; simulated requests without one take a session slot. The two are independent.',
      )}
      action={(
        <Button type="button" size="sm" variant="outline" onClick={onManage}>
          {t('调整上限', 'Adjust limits')}
        </Button>
      )}
      // `[&>*]:min-w-0`：网格项默认 `min-width: auto`，一行不换行的 `sim:` 设备 id（64 位 hex）
      // 会把整个网格撑宽，手机上整页跟着能横着拖。对话框里是块级流式布局，所以那边没这个问题。
      panelClassName="grid gap-6 p-4 sm:p-5 xl:grid-cols-2 [&>*]:min-w-0"
    >
      <DeviceList
        credId={cred.id}
        data={devices.data}
        isPending={devices.isPending}
        isFetching={devices.isFetching}
        error={devices.error}
        onRetry={() => { void devices.refetch() }}
      />
      <SessionList
        credId={cred.id}
        data={sessions.data}
        isPending={sessions.isPending}
        isFetching={sessions.isFetching}
        error={sessions.error}
        onRetry={() => { void sessions.refetch() }}
      />
    </Section>
  )
}

/** 该账号自己的封号事件；展开一条就是封号记录对话框里同一份取证详情。 */
function BanEventsSection({ cred }: { cred: Credential }) {
  const { t, language } = useI18n()
  const [expanded, setExpanded] = useState<number | null>(null)
  const events = useQuery({
    // 封号计数进 key：计数一变就是多了一条事件，跟着重取；平时不轮询。
    queryKey: ['ban-events', cred.id, cred.ban_count],
    queryFn: () => listBanEvents({ cred_id: cred.id, limit: 50 }),
  })
  const rows = events.data ?? []

  return (
    <Section
      icon={ShieldAlertIcon}
      title={t('封号记录', 'Ban events')}
      description={t(
        '每次自动封停记录一条，解封或删除账号都不会删除。展开可看上游原话与封前 7 天的流水。',
        'One row per automatic disable; re-enabling or deleting the account never removes it. Expand a row for the upstream message and the 7 days of logs before it.',
      )}
    >
      {events.isPending ? (
        <div className="flex justify-center py-8"><Spinner /></div>
      ) : events.error ? (
        <div className="p-4 sm:p-5">
          <Alert variant="error">
            <AlertTitle>{t('读取失败', 'Failed to load')}</AlertTitle>
            <AlertDescription className="break-words">{extractError(events.error, language)}</AlertDescription>
          </Alert>
        </div>
      ) : rows.length === 0 ? (
        <p className="p-4 text-sm text-muted-foreground sm:p-5">{t('该账号没有被自动封停过。', 'This account has never been auto-disabled.')}</p>
      ) : (
        <ul className="divide-y">
          {rows.map((ev) => {
            const isOpen = expanded === ev.id
            return (
              <li key={ev.id}>
                <button
                  type="button"
                  aria-expanded={isOpen}
                  className="flex w-full min-w-0 items-start gap-2 px-4 py-3 text-left transition-colors hover:bg-accent/40 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring sm:px-5"
                  onClick={() => setExpanded(isOpen ? null : ev.id)}
                >
                  <span className="mt-0.5 shrink-0 text-muted-foreground" aria-hidden>
                    {isOpen ? <ChevronDownIcon className="size-4" /> : <ChevronRightIcon className="size-4" />}
                  </span>
                  <span className="min-w-0 flex-1 space-y-1">
                    <span className="flex flex-wrap items-center gap-2 text-sm">
                      <span className="tabular-nums">{formatFullTime(ev.ts, language)}</span>
                      <Badge size="sm" variant="outline">{sourceLabel(ev.source, t)}</Badge>
                      {ev.status != null && <Badge size="sm" variant={statusVariant(ev.status)}>{ev.status}</Badge>}
                      {/* 手机上固定单独一行（basis-full），不随宽度在徽章后面随机折行；sm 起跟在徽章后面。 */}
                      <span className="text-xs text-muted-foreground tabular-nums max-sm:basis-full">
                        {t(
                          `封前 7 天 ${formatCompactNumber(ev.requests_7d)} 次 · 入站 ${ev.devices_7d} → 出站 ${ev.devices_out_7d} 台`,
                          `7d before: ${formatCompactNumber(ev.requests_7d)} req · ${ev.devices_7d} in → ${ev.devices_out_7d} out`,
                        )}
                      </span>
                    </span>
                    <span className="line-clamp-2 block break-all font-mono text-xs text-muted-foreground">{ev.reason}</span>
                  </span>
                </button>
                {isOpen && <BanEventDetail ev={ev} />}
              </li>
            )
          })}
        </ul>
      )}
    </Section>
  )
}

/** 与用量限制并排的那块：账号本身的静态信息（组织、时间、凭证）。名称、ID、套餐都在页头上，这里不重复。 */
function InfoSection({ cred, now }: { cred: Credential; now: number }) {
  const { t, language } = useI18n()
  const expiry = credentialExpiryMeta(cred, language)
  return (
    <Section icon={InfoIcon} title={t('账号信息', 'Account')} className="min-w-0 lg:h-full" panelClassName="lg:flex-1">
      <dl className="divide-y">
        {/* 只在组织账号上列：个人号这一格要么是「—」，要么是 claude_max 这类与页头套餐徽章同义的原值；
            组织号的原值（claude_team / claude_enterprise）比页头那枚「Team」多一层信息。 */}
        {isOrgAccount(cred) && (
          <InfoRow label={t('组织类型', 'Organisation type')}>
            <span className="font-mono text-xs">{cred.org_type}</span>
          </InfoRow>
        )}
        <InfoRow label={t('添加时间', 'Added')}>
          {formatFullTime(cred.created_at, language)}
          <span className="block text-xs text-muted-foreground">{relativeTime(cred.created_at, now, language)}</span>
        </InfoRow>
        <InfoRow label={t('最近更新', 'Updated')}>{formatFullTime(cred.updated_at, language)}</InfoRow>
        <InfoRow label="access token">
          <span title={expiry.title}>{expiry.text}</span>
          <span className="mt-0.5 block font-mono text-xs text-muted-foreground">{cred.token_hint}</span>
        </InfoRow>
      </dl>
    </Section>
  )
}

function InfoRow({ label, children }: { label: string; children: ReactNode }) {
  return (
    <div className="grid grid-cols-[6.5rem_minmax(0,1fr)] items-baseline gap-3 px-4 py-2.5 text-sm sm:px-5">
      <dt className="text-xs text-muted-foreground">{label}</dt>
      <dd className="min-w-0">{children}</dd>
    </div>
  )
}

/**
 * 调度配置直接用设置页的 SettingsGroup / SettingsRow：一行一项、当前值在说明位、控件在右。
 *
 * 只放页头读数里**没有**的项。设备 / 会话 / RPM 三项上限原来这里也各有一行，与读数格的
 * 「当前 / 上限」重复——读数格本身就能点开调整，这里不再重复列。
 */
function ScheduleSection({
  cred,
  onProxy,
  onQuota,
}: {
  cred: Credential
  onProxy: () => void
  onQuota: () => void
}) {
  const { t } = useI18n()
  const { prio } = useCredentialActions(cred)
  const mobile = useMediaQuery(MOBILE_QUERY)
  const edit = (onClick: () => void) => (
    <Button type="button" size="sm" variant="outline" onClick={onClick}>{t('修改', 'Edit')}</Button>
  )
  const priorityButtons = (
    <>
      <Button
        type="button"
        size="icon-sm"
        variant="outline"
        disabled={prio.isPending}
        aria-label={t('提高优先级', 'Increase priority')}
        title={t(`提高到 P${cred.priority - 1}`, `Raise to P${cred.priority - 1}`)}
        onClick={() => prio.mutate(cred.priority - 1)}
      >
        <ChevronUpIcon />
      </Button>
      <Button
        type="button"
        size="icon-sm"
        variant="outline"
        disabled={prio.isPending}
        aria-label={t('降低优先级', 'Decrease priority')}
        title={t(`降低到 P${cred.priority + 1}`, `Lower to P${cred.priority + 1}`)}
        onClick={() => prio.mutate(cred.priority + 1)}
      >
        <ChevronDownIcon />
      </Button>
    </>
  )
  if (mobile) {
    // 手机上 SettingsRow 会把「修改」按钮换到说明下面单独一行，三项配置占掉大半屏。改成与 ⋯ 底部
    // 面板「设置」组同一副面孔：名称在左、当前值在右、末尾一枚 ›，整行可点。
    const pause = (pct: number) => (pct > 0 ? `${pct}%` : t('不停', 'off'))
    const row = (label: string, value: string, onClick: () => void) => (
      <button
        type="button"
        className="flex min-h-12 w-full items-center gap-3 px-4 text-left text-sm outline-none transition-colors active:bg-accent focus-visible:bg-accent"
        onClick={onClick}
      >
        <span className="shrink-0 font-medium">{label}</span>
        <span className="min-w-0 flex-1 truncate text-right text-xs text-muted-foreground tabular-nums">{value}</span>
        <ChevronRightIcon aria-hidden className="size-4 shrink-0 text-muted-foreground/64" />
      </button>
    )
    return (
      <SettingsGroup icon={SlidersHorizontalIcon} title={t('调度配置', 'Scheduling')}>
        {row(t('出站代理', 'Outbound proxy'), cred.proxy ? proxyLabelParts(cred.proxy).host : t('直连', 'Direct'), onProxy)}
        {row(
          t('提前停调度', 'Early pause'),
          `5h ${pause(cred.quota_pause_pct_effective)} · 7d ${pause(cred.quota_pause_pct_7d_effective)}`,
          onQuota,
        )}
        <div className="flex min-h-12 items-center gap-3 px-4 text-sm">
          <span className="min-w-0 flex-1 font-medium">{t('调度优先级', 'Priority')}</span>
          <span className="text-xs text-muted-foreground tabular-nums">P{cred.priority}</span>
          {priorityButtons}
        </div>
      </SettingsGroup>
    )
  }
  return (
    <SettingsGroup
      icon={SlidersHorizontalIcon}
      title={t('调度配置', 'Scheduling')}
      description={t('设备、会话与 RPM 上限在页头读数里点开调整。', 'Adjust the device, session and RPM limits from the readouts at the top.')}
    >
      <SettingsRow
        label={t('出站代理', 'Outbound proxy')}
        description={cred.proxy
          ? <span className="break-all font-mono text-xs">{proxyMaskedUrl(cred.proxy)}</span>
          : t('直连', 'Direct')}
      >
        {edit(onProxy)}
      </SettingsRow>
      <SettingsRow
        label={t('提前停调度阈值', 'Early pause threshold')}
        description={t(
          `5h ${pausePolicyText(cred.quota_pause_pct, cred.quota_pause_pct_effective, t)} · 7d ${pausePolicyText(cred.quota_pause_pct_7d, cred.quota_pause_pct_7d_effective, t)}`,
          `5h ${pausePolicyText(cred.quota_pause_pct, cred.quota_pause_pct_effective, t)} · 7d ${pausePolicyText(cred.quota_pause_pct_7d, cred.quota_pause_pct_7d_effective, t)}`,
        )}
      >
        {edit(onQuota)}
      </SettingsRow>
      <SettingsRow
        label={t('调度优先级', 'Priority')}
        description={t(`当前 P${cred.priority}，数值越小越优先`, `Currently P${cred.priority}; lower values are scheduled first`)}
      >
        {priorityButtons}
      </SettingsRow>
    </SettingsGroup>
  )
}
