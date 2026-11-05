import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  DatabaseZapIcon,
  LayoutGridIcon,
  ListFilterIcon,
  ListIcon,
  PlusIcon,
  ActivityIcon,
  RadioIcon,
  ReceiptIcon,
  RefreshCwIcon,
  SearchIcon,
  ShieldCheckIcon,
  SmartphoneIcon,
  TimerIcon,
  TriangleAlertIcon,
} from 'lucide-react'
import { useQuery } from '@tanstack/react-query'
import type { Credential } from '@/api/credentials'
import { getMetrics } from '@/api/metrics'
import { getBilling } from '@/api/billing'
import { listUsers } from '@/api/users'
import { listProxies } from '@/api/proxies'
import { BatchActionsBar } from '@/components/batch-actions-bar'
import { CacheHitSparkline, cacheSplitText } from '@/components/cache-hit-chart'
import { CacheHitTrendDialog, useCacheSeries } from '@/components/cache-hit-trend-dialog'
import { CredentialCard } from '@/components/credential-card'
import { CredentialLoadingState } from '@/components/credential-loading'
import { ownerFacets, type CredentialOwnerFilter } from '@/components/credential-owner-filter'
import { proxyBucket, proxyFacets, type CredentialProxyFilter } from '@/components/credential-proxy-filter'
import {
  SORTS,
  CREDENTIAL_CARD_GRID_CLASS,
  SORT_DIR_DEFAULT,
  evaluateCredential,
  isAccountBan,
  isSubscriptionPause,
  planKey,
  sortCreds,
  type CredentialEvaluation,
  type PlanKey,
  type SortDir,
  type SortKey,
  useDevicesBySession,
} from '@/components/credential-shared'
import { CredentialListHeader, CredentialRow } from '@/components/credential-row'
import { LiveTrafficMetric, OverviewMetric, OverviewMetricSkeleton } from '@/components/overview-metric'
import {
  TtftSparkline,
  TtftTrendDialog,
  formatMs,
  formatTokensPerSec,
  useTtftSeries,
} from '@/components/ttft-trend-dialog'
import { getRejections, type CacheSeriesPoint, type TtftSeriesPoint } from '@/api/metrics'
import { PaginationBar } from '@/components/pagination-bar'
import { ErrorState } from '@/components/state-placeholders'
import { Badge } from '@/components/ui/badge'
import { Button, buttonVariants } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import {
  Empty,
  EmptyContent,
  EmptyDescription,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
} from '@/components/ui/empty'
import {
  ActiveFilterChips,
  ToolbarFacetFilter,
  ToolbarMenuSelect,
  ToolbarSearch,
  type ActiveFilterChip,
  type FacetFilterOption,
  type FacetFilterSection,
} from '@/components/toolbar-controls'
import { Popover, PopoverPopup, PopoverTrigger } from '@/components/ui/popover'
import {
  Select, SelectGroup, SelectItem, SelectPopup, SelectSeparator, SelectTrigger, SelectValue,
} from '@/components/ui/select'
import { Skeleton } from '@/components/ui/skeleton'
import { Table, TableBody, TableCaption } from '@/components/ui/table'
import { Toggle } from '@/components/ui/toggle'
import { ToggleGroup, ToggleGroupItem, ToggleGroupSeparator } from '@/components/ui/toggle-group'
import { Toolbar, ToolbarGroup, ToolbarSeparator } from '@/components/ui/toolbar'
import { Tooltip, TooltipPopup, TooltipTrigger } from '@/components/ui/tooltip'
import { useI18n, type Language } from '@/lib/i18n'
import { ReadOnlyScope, useCanManageUsers, useMe, useReadOnly, useSeesWholePool } from '@/lib/role'
import { useDebounced } from '@/lib/use-debounced'
import { cacheHitRate, cn, displayCredentialLabel, formatPercent, formatUsd } from '@/lib/utils'

export type CredentialFilterKey =
  | 'all'
  | 'schedulable'
  | 'attention'
  | 'enabled'
  | 'disabled'
  | 'banned'
  | 'tokenInvalid'
  | 'subscriptionInactive'
  | 'nearLimit'
  | 'cooldown'
  | 'hasDevice'
  | 'deviceFull'

/**
 * 套餐筛选与上面那组状态筛选**是两个维度，各自独立**：一个说「这个号现在怎么样」，另一个说
 * 「这个号是什么档位」。合进同一个单选列表的话，「Pro 里有哪些需要处理」这种最常问的问题就
 * 提不出来——选了 Pro 就丢掉了状态条件。故单开一列，两个菜单同时生效（取交集）。
 */
/**
 * 套餐筛选值：`all`、某个档位，或 `team:<套餐名>`——团队 / 企业号再按库里的套餐全称细分
 * （`Team Standard`、`Team Premium`、`Enterprise`…）。全称取自数据本身，上游出了新的席位档
 * 不用改代码就能列出来。
 */
export type CredentialTierFilterKey = 'all' | PlanKey | `team:${string}`

const TEAM_TIER_PREFIX = 'team:'

/** 团队 / 企业号的细分名：库里的套餐全称（已带组织前缀，如 `Team Standard`）。 */
function teamSubtier(credential: Pick<Credential, 'tier'>): string {
  return (credential.tier ?? '').trim()
}

function matchTier(credential: Pick<Credential, 'tier'>, tier: CredentialTierFilterKey): boolean {
  if (tier === 'all') return true
  const plan = planKey(credential.tier)
  if (tier.startsWith(TEAM_TIER_PREFIX)) return plan === 'team' && teamSubtier(credential) === tier.slice(TEAM_TIER_PREFIX.length)
  return plan === tier
}

export function parseTierFilter(raw: string): CredentialTierFilterKey | null {
  if (raw.startsWith(TEAM_TIER_PREFIX) && raw.length > TEAM_TIER_PREFIX.length) return raw as CredentialTierFilterKey
  return (CREDENTIAL_TIER_FILTER_KEYS as readonly string[]).includes(raw) ? raw as CredentialTierFilterKey : null
}

export type CredentialViewMode = 'card' | 'list'

export const CREDENTIAL_PAGE_SIZES = [10, 20, 50] as const
export type CredentialPageSize = (typeof CREDENTIAL_PAGE_SIZES)[number]

export const CREDENTIAL_VIEW_MODES = ['card', 'list'] as const

type LocalizedLabel = readonly [chinese: string, english: string]

/**
 * 状态筛选按含义分组（调度结论、账号状态、用量、设备），组间只用分隔线隔开，不加组标题：
 * 与 Linear / GitHub 的下拉一致，十来项的列表靠分隔线就能读出层次，标题只会把菜单撑高。
 */
type FilterGroup = 'overview' | 'schedule' | 'status' | 'usage' | 'device'

/** 状态下拉里各组的小标题；「全部」那组不加。 */
const FILTER_GROUP_LABELS: Partial<Record<FilterGroup, LocalizedLabel>> = {
  schedule: ['调度', 'Scheduling'],
  status: ['账号状态', 'Account status'],
  usage: ['用量', 'Usage'],
  device: ['设备', 'Devices'],
}

const FILTERS: {
  key: CredentialFilterKey
  group: FilterGroup
  label: LocalizedLabel
  match: (evaluation: CredentialEvaluation) => boolean
}[] = [
  { key: 'all', group: 'overview', label: ['全部', 'All'], match: () => true },
  {
    key: 'schedulable',
    group: 'schedule',
    label: ['可调度', 'Schedulable'],
    match: (evaluation) => evaluation.schedulable,
  },
  {
    key: 'attention',
    group: 'schedule',
    label: ['需处理', 'Needs attention'],
    match: (evaluation) => evaluation.needsAttention,
  },
  { key: 'enabled', group: 'status', label: ['已启用', 'Enabled'], match: ({ credential }) => !credential.disabled },
  {
    key: 'disabled',
    group: 'status',
    label: ['手动停用', 'Manually disabled'],
    // 只匹配人工手动关掉的号：排除封禁、Token 失效（有 ban_reason）和限流暂停（有 resume_at），
    // 那几种各有各的 tab，混在一起会让计数虚高、点开找不到预期的号。
    match: ({ credential }) =>
      credential.disabled && !credential.ban_reason && credential.resume_at == null,
  },
  {
    key: 'banned',
    group: 'status',
    label: ['已封禁', 'Banned'],
    match: ({ credential }) =>
      !!credential.ban_reason && credential.resume_at == null
      && !isSubscriptionPause(credential.ban_reason) && isAccountBan(credential.ban_reason),
  },
  {
    key: 'tokenInvalid',
    group: 'status',
    label: ['token 失效', 'Token expired'],
    match: ({ credential }) =>
      !!credential.ban_reason && credential.resume_at == null
      && !isSubscriptionPause(credential.ban_reason) && !isAccountBan(credential.ban_reason),
  },
  {
    key: 'subscriptionInactive',
    group: 'status',
    label: ['订阅未生效', 'Subscription inactive'],
    match: ({ credential }) =>
      !!credential.ban_reason && credential.resume_at == null && isSubscriptionPause(credential.ban_reason),
  },
  { key: 'nearLimit', group: 'usage', label: ['用量风险', 'Usage risk'], match: (evaluation) => evaluation.quotaRisk },
  {
    key: 'cooldown',
    group: 'usage',
    label: ['冷却中', 'Cooling down'],
    // 三档都收：resume_at（额度用尽落库暂停）、rate_limited_secs（进程内账号级冷却）、
    // modelCooling（模型级冷却）。区别由卡片上的状态与提示分别说明。
    match: (evaluation) =>
      evaluation.credential.resume_at != null
      || (!evaluation.credential.disabled
        && (evaluation.credential.rate_limited_secs > 0 || evaluation.modelCooling)),
  },
  {
    key: 'hasDevice',
    group: 'device',
    label: ['已绑定设备', 'Has bound devices'],
    match: ({ credential }) => credential.device_count > 0,
  },
  {
    key: 'deviceFull',
    group: 'device',
    label: ['设备已满', 'Device limit reached'],
    match: ({ credential }) =>
      credential.device_limit_effective > 0
      && credential.device_count >= credential.device_limit_effective,
  },
]

/**
 * 档位选项按**从高到低**排，与 [`tierRank`] 的口径一致——下拉里读到的顺序就是套餐的贵贱顺序，
 * 不必再去对照徽标颜色。档位名是上游的商品名，中英文一样，故不做翻译。
 */
const TIER_FILTERS: { key: 'all' | PlanKey; label: LocalizedLabel }[] = [
  { key: 'all', label: ['全部套餐', 'All plans'] },
  { key: 'max20x', label: ['Max 20x', 'Max 20x'] },
  { key: 'max5x', label: ['Max 5x', 'Max 5x'] },
  // 上游偶尔只给 `claude_max` 这种不带倍率的写法，单列一档收着，否则它会掉进「未知」里。
  { key: 'max', label: ['Max（倍率未知）', 'Max (multiplier unknown)'] },
  { key: 'team', label: ['Team / Enterprise', 'Team / Enterprise'] },
  { key: 'pro', label: ['Pro', 'Pro'] },
  { key: 'free', label: ['Free', 'Free'] },
  { key: 'unknown', label: ['未知', 'Unknown'] },
]

/** 套餐下拉在这些档位前加分隔线：全部 | Max 各档 | 团队 / 企业 | Pro、Free | 未知。 */
const TIER_GROUP_STARTS = new Set<CredentialTierFilterKey>(['max20x', 'team', 'pro', 'unknown'])

const SORT_LABELS: Record<SortKey, LocalizedLabel> = {
  priority: ['优先级', 'Priority'],
  status: ['状态', 'Status'],
  name: ['名称', 'Name'],
  tier: ['套餐', 'Plan'],
  usage5h: ['5h 使用率', '5h usage'],
  usage7d: ['7d 使用率', '7d usage'],
  devices: ['设备数', 'Devices'],
  sessions: ['会话数', 'Sessions'],
  rpm: ['当前 RPM', 'Current RPM'],
  cost: ['累计费用', 'Total cost'],
  recent: ['最近使用', 'Last used'],
  created: ['添加时间', 'Date added'],
}

/** 排序字段按含义分组：账号属性 / 用量与活跃 / 名额。 */
const SORT_GROUPS: readonly (readonly SortKey[])[] = [
  ['priority', 'status', 'name', 'tier', 'created'],
  ['usage5h', 'usage7d', 'rpm', 'cost', 'recent'],
  ['devices', 'sessions'],
]

/** 下拉里的一项：名称靠左，计数靠右、弱化，不做染色。 */
/** 能不能改由后端按权限给（`editable`）：代理的列表里下属的号只读。 */
const owns = (credential: Credential) => credential.editable

/** 计数为 0 的项调暗而不隐藏：状态是一组固定选项，位置不跳；「已封禁 0」本身也是信息。 */
function FacetOption({ label, count, nested = false, empty = false }: { label: string; count: string; nested?: boolean; empty?: boolean }) {
  return (
    <span className={cn('flex min-w-0 flex-1 items-center justify-between gap-4', nested && 'ps-3', empty && 'text-muted-foreground/70')}>
      <span className="min-w-0 truncate">{label}</span>
      <span className="tnum text-xs text-muted-foreground">{count}</span>
    </span>
  )
}

export const CREDENTIAL_FILTER_KEYS = FILTERS.map((filter) => filter.key)
export const CREDENTIAL_TIER_FILTER_KEYS = TIER_FILTERS.map((item) => item.key)

export function preferredInitialCredentialView(): CredentialViewMode {
  return typeof window !== 'undefined' && window.matchMedia('(min-width: 80rem)').matches
    ? 'list'
    : 'card'
}

/** 额度 reset 与相对时间都依赖当前时刻；30 秒 tick 与接口刷新节奏一致。 */
export function useNowSeconds(): number {
  const [now, setNow] = useState(() => Math.floor(Date.now() / 1000))

  useEffect(() => {
    const update = () => setNow(Math.floor(Date.now() / 1000))
    const onVisibilityChange = () => {
      if (!document.hidden) update()
    }
    const interval = window.setInterval(update, 30_000)
    window.addEventListener('focus', update)
    document.addEventListener('visibilitychange', onVisibilityChange)
    return () => {
      window.clearInterval(interval)
      window.removeEventListener('focus', update)
      document.removeEventListener('visibilitychange', onVisibilityChange)
    }
  }, [])

  return now
}


/**
 * 搜索匹配的字段。除名称和 #id 外还收了套餐、组织类型与当前状态文案——
 * 「max」「team」「已封禁 / banned」这类词是排查时最先想敲进去的，只匹配名称会全部落空。
 * 状态用的是界面上那句本地化文案，所见即可搜。
 */
function matchQuery(evaluation: CredentialEvaluation, query: string, language: Language): boolean {
  const value = query.trim().toLowerCase()
  if (!value) return true
  const credential = evaluation.credential
  if (`#${credential.id}`.includes(value) || String(credential.id) === value) return true
  return [
    credential.label,
    displayCredentialLabel(credential.label, language),
    credential.tier ?? '',
    credential.org_type ?? '',
    credential.owner ?? '',
    evaluation.status.label,
  ].some((field) => field.toLowerCase().includes(value))
}

interface CredentialWorkspaceData {
  credentials?: Credential[]
  isLoading: boolean
  isError: boolean
  isRefetchError: boolean
  isFetching: boolean
  error?: unknown
}

interface CredentialWorkspaceState {
  query: string
  filter: CredentialFilterKey
  tier: CredentialTierFilterKey
  owner: CredentialOwnerFilter
  proxy: CredentialProxyFilter
  sort: SortKey
  dir: SortDir
  view: CredentialViewMode
  selected: Set<number>
  page: number
  pageSize: CredentialPageSize
}

interface CredentialWorkspaceActions {
  onQueryChange: (value: string) => void
  onFilterChange: (value: CredentialFilterKey) => void
  onTierChange: (value: CredentialTierFilterKey) => void
  onOwnerChange: (value: CredentialOwnerFilter) => void
  onProxyChange: (value: CredentialProxyFilter) => void
  onSortChange: (key: SortKey, dir: SortDir) => void
  onViewChange: (value: CredentialViewMode) => void
  onSelectedChange: (value: Set<number>) => void
  onPageChange: (value: number) => void
  onPageSizeChange: (value: CredentialPageSize) => void
  onRetry: () => void
  onAdd: () => void
  /** 代理和用户点「今日费用」那一格：去费用页看明细。 */
  onOpenBilling?: () => void
}

export interface CredentialWorkspaceProps {
  data: CredentialWorkspaceData
  state: CredentialWorkspaceState
  actions: CredentialWorkspaceActions
}

function WorkspaceToolbarSkeleton() {
  return (
    <div
      className="grid w-full grid-cols-3 items-stretch gap-2 sm:flex sm:flex-row sm:flex-wrap sm:items-center xl:justify-end"
      aria-hidden="true"
    >
      <Skeleton className="h-9 max-sm:col-span-3 sm:h-8 sm:min-w-56 sm:flex-1 xl:max-w-64" />
      <div className="grid min-w-0 grid-cols-3 gap-1 max-sm:col-span-3 sm:flex">
        <Skeleton className="h-9 min-w-0 sm:h-8 sm:w-28" />
        <Skeleton className="h-9 min-w-0 sm:h-8 sm:w-20" />
        <Skeleton className="h-9 min-w-0 sm:h-8 sm:w-28" />
      </div>
    </div>
  )
}

/**
 * 「显示」按钮：视图（卡片 / 列表）、排序字段与方向收在一处。它们决定「怎么摆」，与状态、筛选
 * 这些「看哪些号」分开；视图与排序大多设一次就不动（都记在 localStorage），常驻工具条占地方。
 * 按钮上仍写着当前排序（「添加时间 ↓」），收起来也看得出是怎么排的。列表视图点表头排序照旧。
 */
function DisplayOptions({
  view,
  onViewChange,
  sort,
  dir,
  sortGroups,
  sortLabel,
  onSortKeyChange,
  onSortDirChange,
}: {
  view: CredentialViewMode
  onViewChange: (value: CredentialViewMode) => void
  sort: SortKey
  dir: SortDir
  sortGroups: { key: SortKey; label: string }[][]
  sortLabel: string
  onSortKeyChange: (key: SortKey) => void
  onSortDirChange: (dir: SortDir) => void
}) {
  const { t } = useI18n()
  const arrow = dir === 'asc' ? '↑' : '↓'
  const viewLabel = view === 'card' ? t('卡片视图', 'Card view') : t('列表视图', 'List view')
  const sortItems = sortGroups.flat().map((item) => ({ value: item.key, label: item.label }))
  const row = 'grid grid-cols-[3.5rem_minmax(0,1fr)] items-center gap-2'
  const rowLabel = 'text-xs font-medium text-muted-foreground'
  return (
    <Popover>
      <PopoverTrigger
        aria-label={t(
          `显示：${viewLabel}，按${sortLabel}${dir === 'asc' ? '升序' : '降序'}`,
          `Display: ${viewLabel}, sorted by ${sortLabel} ${dir === 'asc' ? 'ascending' : 'descending'}`,
        )}
        className={cn(
          buttonVariants({ variant: 'outline' }),
          'w-full min-w-0 justify-between max-sm:[&_svg]:hidden sm:w-auto',
        )}
      >
        {view === 'card' ? <LayoutGridIcon /> : <ListIcon />}
        <span className="min-w-0 truncate max-[22rem]:hidden">{sortLabel} {arrow}</span>
        <span className="hidden shrink-0 max-[22rem]:inline">{t('排序', 'Sort')} {arrow}</span>
      </PopoverTrigger>
      <PopoverPopup align="end" className="w-72">
        <div className="space-y-3">
          <div className={row}>
            <span className={rowLabel}>{t('视图', 'View')}</span>
            <ToggleGroup
              value={[view]}
              onValueChange={(values) => {
                const next = values[values.length - 1]
                if (next === 'card' || next === 'list') onViewChange(next)
              }}
              variant="outline"
              size="sm"
              className="w-full"
              aria-label={t('账号视图', 'Account view')}
            >
              <ToggleGroupItem value="card" className="flex-1">
                <LayoutGridIcon />
                {t('卡片', 'Cards')}
              </ToggleGroupItem>
              <ToggleGroupSeparator />
              <ToggleGroupItem value="list" className="flex-1">
                <ListIcon />
                {t('列表', 'List')}
              </ToggleGroupItem>
            </ToggleGroup>
          </div>
          <div className={row}>
            <span className={rowLabel}>{t('排序', 'Sort')}</span>
            <Select items={sortItems} value={sort} onValueChange={(next) => next && onSortKeyChange(next as SortKey)}>
              <SelectTrigger size="sm" aria-label={t('排序字段', 'Sort field')}><SelectValue /></SelectTrigger>
              <SelectPopup alignItemWithTrigger={false}>
                {sortGroups.map((items, index) => (
                  <SelectGroup key={items[0]?.key ?? index}>
                    {index > 0 && <SelectSeparator />}
                    {items.map((item) => <SelectItem key={item.key} value={item.key}>{item.label}</SelectItem>)}
                  </SelectGroup>
                ))}
              </SelectPopup>
            </Select>
          </div>
          <div className={row}>
            <span className={rowLabel}>{t('方向', 'Order')}</span>
            <ToggleGroup
              value={[dir]}
              onValueChange={(values) => {
                const next = values[values.length - 1]
                if (next === 'asc' || next === 'desc') onSortDirChange(next)
              }}
              variant="outline"
              size="sm"
              className="w-full"
              aria-label={t('排序方向', 'Sort order')}
            >
              <ToggleGroupItem value="asc" className="flex-1">{t('升序', 'Ascending')} ↑</ToggleGroupItem>
              <ToggleGroupSeparator />
              <ToggleGroupItem value="desc" className="flex-1">{t('降序', 'Descending')} ↓</ToggleGroupItem>
            </ToggleGroup>
          </div>
        </div>
      </PopoverPopup>
    </Popover>
  )
}

/**
 * 账号页唯一的工作区组件。真实页面和离线预览共同使用这棵组件树，避免概览、工具栏、
 * 列表与分页在两处独立演进后产生视觉和交互差异。
 */
export function CredentialWorkspace({ data, state, actions }: CredentialWorkspaceProps) {
  const { language, locale, t } = useI18n()
  // 访客不给勾选：勾了只能接批量操作，而批量操作全是改动。
  const readOnly = useReadOnly()
  const {
    credentials,
    isLoading,
    isError,
    isRefetchError,
    isFetching,
    error,
  } = data
  const {
    query,
    filter,
    tier,
    owner,
    proxy,
    sort,
    dir,
    view,
    selected,
    page,
    pageSize,
  } = state
  const pool = credentials ?? []
  const debouncedQuery = useDebounced(query)
  const now = useNowSeconds()
  // 实时指标单独轮询，10 秒一次：全局 RPM 与在途并发都是秒级变化的量，跟着账号列表那份
  // 30 秒的节奏走就成了「一直在看十几秒前的现场」。这个接口只有两条查询，拉得起。
  //
  // 实时流量人人都有：管理员与访客看全池，代理和用户看自己名下的号（后端按身份收窄）。缓存命中率、
  // 首字时延与拒绝统计是全池口径，只给管理员与访客：代理和用户只看得到自己名下的号，这几个接口
  // 后端回 403，查询干脆不发，格子也不出现。
  const seesWholePool = useSeesWholePool()
  const me = useMe()
  const metricsQuery = useQuery({
    queryKey: ['metrics'],
    queryFn: getMetrics,
    refetchInterval: 10_000,
    enabled: !!me.data,
  })
  // 今日费用：今天（本地零点起）的等价费用，走分层账单接口，口径与费用页一致。只给代理和用户
  // （概览里那一格，本人名下的号）；管理员与访客的页头不放金额，看费用去费用页。一分钟一拉——账单
  // 要扫当天的预聚合，没必要跟实时流量一样 10 秒一次。成员只认查回来的真身份：占位数据的 id 是 0，
  // 拿它去查会被后端当成别人。
  const memberId = !seesWholePool && me.data && !me.isPlaceholderData ? me.data.id : null
  const todayStart = (() => {
    const d = new Date()
    return Math.floor(new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime() / 1000)
  })()
  const todayCostQuery = useQuery({
    queryKey: ['billing-today', memberId, todayStart],
    queryFn: () => getBilling({ from: todayStart, to: todayStart + 86400, by: 'day', owner_id: memberId! }),
    refetchInterval: 60_000,
    enabled: memberId != null,
  })
  // 两枚质量卡片各拉两条线：近 24 小时逐小时（迷你线 + 近 1 小时的主数）与近 7 天（基线）。
  // 主数是「现在」，基线是「平时」——7 天平均看不出今天有没有变慢，一比就看出来了。
  const cacheSeries = useCacheSeries('24h', seesWholePool)
  const cacheBaseline = useCacheSeries('7d', seesWholePool)
  const [cacheTrendOpen, setCacheTrendOpen] = useState(false)
  const ttftSeries = useTtftSeries('24h', seesWholePool)
  const ttftBaseline = useTtftSeries('7d', seesWholePool)
  const [ttftTrendOpen, setTtftTrendOpen] = useState(false)
  // 近 1 小时没请求就退回 24 小时，24 小时也没有再退回 7 天，标签跟着写清是哪个窗口。
  const cacheNow = (() => {
    const pick = (p: CacheSeriesPoint | null, label: [string, string]) =>
      p && p.input_tokens > 0 ? { p, label } : null
    return (
      pick(cacheSeries.recent, ['近 1h', 'last 1h'])
      ?? pick(cacheSeries.summary, ['近 24h', 'last 24h'])
      ?? pick(cacheBaseline.summary, ['近 7d', 'last 7d'])
    )
  })()
  const cacheRate = cacheNow ? cacheHitRate(cacheNow.p.input_tokens, cacheNow.p.cached_tokens) : null
  const cacheBase = cacheBaseline.summary && cacheBaseline.summary.input_tokens > 0
    ? cacheHitRate(cacheBaseline.summary.input_tokens, cacheBaseline.summary.cached_tokens)
    : null
  const ttftNow = (() => {
    const pick = (p: TtftSeriesPoint | null, label: [string, string]) =>
      p && p.count > 0 ? { p, label } : null
    return (
      pick(ttftSeries.recent, ['近 1h', 'last 1h'])
      ?? pick(ttftSeries.summary, ['近 24h', 'last 24h'])
      ?? pick(ttftBaseline.summary, ['近 7d', 'last 7d'])
    )
  })()
  const ttftBase = ttftBaseline.summary && ttftBaseline.summary.count > 0 ? ttftBaseline.summary : null
  // 相对基线的变化，直接写在小字里：读者不必心算 1.8s 比 1.2s 慢了多少。延迟按比例（±5% 以内
  // 不写），命中率按百分点（±1pt 以内不写）。
  const ttftDelta = ttftNow && ttftBase && ttftBase.p50_ms > 0
    ? Math.round((ttftNow.p.p50_ms / ttftBase.p50_ms - 1) * 100)
    : null
  const ttftDeltaText = ttftDelta == null || Math.abs(ttftDelta) < 5
    ? ''
    : ` ${ttftDelta > 0 ? '↑' : '↓'}${Math.abs(ttftDelta)}%`
  const cacheDelta = cacheRate != null && cacheBase != null
    ? Math.round((cacheRate - cacheBase) * 100)
    : null
  const cacheDeltaText = cacheDelta == null || Math.abs(cacheDelta) < 1
    ? ''
    : ` ${cacheDelta > 0 ? '↑' : '↓'}${Math.abs(cacheDelta)}pt`
  // 近 1 小时 luban 自己拒掉的请求（设备满、会话满、RPM 满……）：这几道闸加了之后，被拒的只在
  // 流水里记成本地 429，概览得有个数才知道有号被打满。
  const rejectionsQuery = useQuery({
    queryKey: ['rejections', 1],
    queryFn: () => getRejections(1),
    refetchInterval: 60_000,
    enabled: seesWholePool,
  })
  const rejectedTotal = rejectionsQuery.data?.total ?? 0
  const rejectionKindLabel = (kind: string) => ({
    'device-limit': t('设备名额已满', 'device slots full'),
    'session-limit': t('会话名额已满', 'session slots full'),
    'account-rpm': t('账号 RPM 已达上限', 'account RPM full'),
    'device-rpm': t('设备 RPM 已达上限', 'device RPM full'),
    'session-rpm': t('会话 RPM 已达上限', 'session RPM full'),
    'session-concurrency': t('会话并发已达上限', 'session concurrency full'),
    'bare-rate-limit': t('无设备身份限流', 'no-device-identity rate limit'),
    'all-cooling-down': t('所有账号冷却中', 'all accounts cooling down'),
    'no-device-id': t('无设备身份', 'no device identity'),
    // 已撤掉的「按模型最低客户端版本」闸（v0.3.157 ~ v0.3.160）留下的历史行，只为它们仍显示成中文。
    'model-min-version': t('客户端版本不支持该模型', 'client too old for model'),
    'model-unsupported': t('套餐不含模型', 'model not in plan'),
    'refresh-failed': t('token 刷新失败', 'token refresh failed'),
    'unavailable': t('无可用账号', 'no account available'),
    'third-party': t('已知第三方特征', 'known third-party signature'),
    'enabled-thinking': t('模型不接受 enabled thinking', 'enabled thinking not accepted by model'),
    'anthropic-version': t('缺少版本头', 'missing version header'),
    'thinking-display': t('thinking.display 取值不被接受', 'thinking.display value not accepted'),
    'thinking-budget': t('思考预算不小于 max_tokens', 'thinking budget not below max_tokens'),
  } as Record<string, string>)[kind] ?? t('其他', 'other')
  // 颜色看「现在比平时慢了多少」：慢一半黄、慢一倍红；没有基线时按绝对值（3s / 8s）。
  const ttftTone = (() => {
    if (!ttftNow) return 'neutral' as const
    const now = ttftNow.p.p50_ms
    if (ttftBase) {
      const ratio = now / Math.max(1, ttftBase.p50_ms)
      if (ratio >= 2 || now > 8000) return 'bad' as const
      if (ratio >= 1.5 || now > 3000) return 'warn' as const
      return 'ok' as const
    }
    return now <= 3000 ? ('ok' as const) : now <= 8000 ? ('warn' as const) : ('bad' as const)
  })()
  const numberFormatter = useMemo(() => new Intl.NumberFormat(locale), [locale])
  const formatNumber = (value: number) => numberFormatter.format(value)
  // 设备按会话占名额时设备上限不生效：设备那组筛选、按设备数排序、概览里的设备数都不出现。
  const devicesBySession = useDevicesBySession()
  const filterItems = useMemo(
    () => FILTERS
      .filter((item) => !(devicesBySession && item.group === 'device'))
      .map((item) => ({ ...item, label: t(...item.label) })),
    [t, devicesBySession],
  )
  const filterGroups = useMemo(() => {
    const groups: (typeof filterItems)[] = []
    for (const item of filterItems) {
      const last = groups[groups.length - 1]
      if (last && last[0].group === item.group) last.push(item)
      else groups.push([item])
    }
    return groups
  }, [filterItems])
  const tierItems = useMemo(
    () => TIER_FILTERS.map((item) => ({ ...item, label: t(...item.label) })),
    [t],
  )
  const sortItems = useMemo(
    () => SORTS.map(({ key }) => ({ key, label: t(...SORT_LABELS[key]) })),
    [t],
  )
  const sortGroups = useMemo(
    () => SORT_GROUPS.map((keys) => keys
      .filter((key) => !(devicesBySession && key === 'devices'))
      .map((key) => ({ key, label: t(...SORT_LABELS[key]) }))),
    [t, devicesBySession],
  )
  const activeFilterLabel = filterItems.find((item) => item.key === filter)?.label
    ?? t(...FILTERS[0].label)
  const activeTierLabel = tier.startsWith(TEAM_TIER_PREFIX)
    ? tier.slice(TEAM_TIER_PREFIX.length)
    : tierItems.find((item) => item.key === tier)?.label ?? t(...TIER_FILTERS[0].label)
  const activeSortLabel = sortItems.find((item) => item.key === sort)?.label
    ?? t(...SORT_LABELS.priority)
  // 成员筛选：管理员按代理（连同下属）或具体某人缩小，代理按本人或某个下属。它是「看谁的号」，
  // 不是号的某种状态，所以概览各格与状态 / 套餐两个下拉的计数都跟着它走；名单只有管理员和代理
  // 拉得到，访客按号上的号主平铺。
  const canManageUsers = useCanManageUsers()
  const usersQuery = useQuery({
    queryKey: ['users'],
    queryFn: listUsers,
    enabled: canManageUsers && !!me.data && !me.isPlaceholderData,
    staleTime: 60_000,
  })
  const ownerGroups = useMemo(
    () => ownerFacets(pool, usersQuery.data, me.isPlaceholderData ? undefined : me.data, t),
    [pool, usersQuery.data, me.data, me.isPlaceholderData, t],
  )
  // 选中的那一项不在下拉里了（人删了、号挪走了、名单还没拉回来）就按「全部」算，不留一个
  // 看不见的条件把列表筛空。
  const activeOwner = ownerGroups.flat().find((item) => item.value === owner)
  const ownerValue = activeOwner?.value ?? 'all'
  const activeOwnerLabel = activeOwner?.label ?? t('全部成员', 'All members')
  const evaluatedPool = useMemo(
    () => pool
      .filter((credential) => !activeOwner || (credential.owner_id != null && activeOwner.owners.has(credential.owner_id)))
      .map((credential) => evaluateCredential(credential, now, language)),
    [pool, activeOwner, now, language],
  )
  // 代理筛选：按号绑的出站代理缩小（直连 / 自定义 / 代理池某一条），筛出来全选后用批量操作里的
  // 「出站代理」整批换掉。它是号的一项属性，与套餐同级：下拉计数跟着成员筛选走，不影响概览各格。
  const proxiesQuery = useQuery({ queryKey: ['proxies'], queryFn: listProxies, staleTime: 60_000 })
  const proxyGroups = useMemo(
    () => proxyFacets(evaluatedPool.map((evaluation) => evaluation.credential), proxiesQuery.data, t),
    [evaluatedPool, proxiesQuery.data, t],
  )
  // 选中的那条代理已没有号在用（刚整批换走）时按「全部」算，同成员筛选。
  const activeProxy = proxyGroups.flat().find((item) => item.value === proxy)
  const proxyValue = activeProxy?.value ?? 'all'
  const activeProxyLabel = activeProxy?.label ?? t('全部代理', 'All proxies')

  const sorted = useMemo(() => {
    const match = FILTERS.find((item) => item.key === filter)?.match ?? (() => true)
    return sortCreds(
      evaluatedPool
        .filter((evaluation) => (
          match(evaluation)
          && matchTier(evaluation.credential, tier)
          && (proxyValue === 'all' || proxyBucket(evaluation.credential) === proxyValue)
          && matchQuery(evaluation, debouncedQuery, language)
        ))
        .map((evaluation) => evaluation.credential),
      sort,
      dir,
      now,
      language,
    )
  }, [evaluatedPool, sort, dir, filter, tier, proxyValue, debouncedQuery, now, language])

  // 下拉与筛选弹层里的计数：每一维按「其余各维已生效的条件 + 搜索词」来数（常见的分面计数）。
  // 数字回答的是「切过去能看到几个」：本维自己的选择不影响本维的数（选一次数字不会跟着跳），
  // 但别的维度已筛「套餐 Pro」时，状态菜单里的「可调度」就是 Pro 里能调度的个数，点下去与数字一致。
  // 成员是「看谁的号」的范围，早在 evaluatedPool 就收窄了，不参与交叉。概览各格仍用 metrics 的全池口径。
  const facetCounts = useMemo(() => {
    const statusMatch = FILTERS.find((item) => item.key === filter)?.match ?? (() => true)
    const base = (skip: 'status' | 'tier' | 'proxy') => evaluatedPool.filter((evaluation) => (
      (skip === 'status' || statusMatch(evaluation))
      && (skip === 'tier' || matchTier(evaluation.credential, tier))
      && (skip === 'proxy' || proxyValue === 'all' || proxyBucket(evaluation.credential) === proxyValue)
      && matchQuery(evaluation, debouncedQuery, language)
    ))
    const forStatus = base('status')
    const status = Object.fromEntries(
      FILTERS.map((item) => [item.key, forStatus.filter(item.match).length]),
    ) as Record<CredentialFilterKey, number>
    const forTier = base('tier')
    const tierAll: Record<'all' | PlanKey, number> = { all: 0, max20x: 0, max5x: 0, max: 0, team: 0, pro: 0, free: 0, unknown: 0 }
    const team = new Map<string, number>()
    for (const { credential } of forTier) {
      tierAll.all += 1
      const plan = planKey(credential.tier)
      tierAll[plan] += 1
      if (plan === 'team') team.set(teamSubtier(credential), (team.get(teamSubtier(credential)) ?? 0) + 1)
    }
    const forProxy = base('proxy')
    const proxy = new Map<string, number>()
    for (const { credential } of forProxy) {
      const bucket = proxyBucket(credential)
      proxy.set(bucket, (proxy.get(bucket) ?? 0) + 1)
    }
    return { status, tier: tierAll, team, proxy, proxyAll: forProxy.length }
  }, [evaluatedPool, filter, tier, proxyValue, debouncedQuery, language])

  // 「今日费用」那格的小字：名下各号累计费用之和（账号列表里本来就有，不必另查）。代理的列表里
  // 混着下属的号，只加本人的，与「今日费用」同口径。
  const lifetimeCost = useMemo(
    () => (credentials ?? []).filter(owns).reduce((sum, c) => sum + (c.cost_total ?? 0), 0),
    [credentials],
  )
  const metrics = useMemo(() => {
    const filterCounts: Record<CredentialFilterKey, number> = {
      all: 0,
      schedulable: 0,
      attention: 0,
      enabled: 0,
      disabled: 0,
      banned: 0,
      tokenInvalid: 0,
      subscriptionInactive: 0,
      nearLimit: 0,
      cooldown: 0,
      hasDevice: 0,
      deviceFull: 0,
    }
    let rateLimitedPauseCount = 0
    // 「需处理」那一格的小字按**主状态**分项：每个号只落一档（与卡片上显示的那个状态同一个），
    // 各项加起来恰好等于大数。原先小字拼的是几个互相重叠的筛选计数——「冷却」连只是某个模型
    // 在冷却、账号照常可调度的号也数进去——于是「6」下面写着「1 Token 失效 · 11 冷却」。
    const attentionKinds: Record<string, number> = {}
    // 只有模型级冷却、账号本身没进「需处理」的号：另起一项，不算进大数。
    let modelOnlyCoolingCount = 0
    let nearLimitCount = 0
    let activeOverageCount = 0
    let unknownOverageCount = 0
    let deviceCount = 0
    let deviceCapacity = 0
    let unlimitedDeviceAccounts = 0

    for (const evaluation of evaluatedPool) {
      const credential = evaluation.credential
      filterCounts.all += 1
      if (evaluation.schedulable) filterCounts.schedulable += 1
      if (evaluation.needsAttention) {
        filterCounts.attention += 1
        const kind = evaluation.status.kind
        attentionKinds[kind] = (attentionKinds[kind] ?? 0) + 1
      } else if (!credential.disabled && evaluation.modelCooling) {
        modelOnlyCoolingCount += 1
      }
      if (!credential.disabled) filterCounts.enabled += 1
      else if (!credential.ban_reason && credential.resume_at == null) filterCounts.disabled += 1
      if (credential.resume_at != null) rateLimitedPauseCount += 1
      if (credential.ban_reason && credential.resume_at == null) {
        if (isSubscriptionPause(credential.ban_reason)) filterCounts.subscriptionInactive += 1
        else if (isAccountBan(credential.ban_reason)) filterCounts.banned += 1
        else filterCounts.tokenInvalid += 1
      }
      if (evaluation.quotaRisk) filterCounts.nearLimit += 1
      // 口径必须与上面 'cooldown' 那条筛选完全一致，否则芯片上的计数和点开后的条数对不上。
      if (
        credential.resume_at != null
        || (!credential.disabled
          && (credential.rate_limited_secs > 0 || evaluation.modelCooling))
      ) {
        filterCounts.cooldown += 1
      }
      if (credential.device_count > 0) filterCounts.hasDevice += 1
      if (
        credential.device_limit_effective > 0
        && credential.device_count >= credential.device_limit_effective
      ) {
        filterCounts.deviceFull += 1
      }
      // 额度概览按「Usage credits > 待确认 > 额度将满」互斥归类，避免一个账号重复出现在两项里。
      if (
        evaluation.nearLimit
        && evaluation.quota.overage !== 'active'
        && evaluation.quota.overage !== 'unknown'
      ) {
        nearLimitCount += 1
      }
      if (!credential.disabled && evaluation.quota.overage === 'active') {
        activeOverageCount += 1
      }
      if (!credential.disabled && evaluation.quota.overage === 'unknown') {
        unknownOverageCount += 1
      }
      deviceCount += credential.device_count
      if (credential.device_limit_effective > 0) {
        deviceCapacity += credential.device_limit_effective
      } else {
        unlimitedDeviceAccounts += 1
      }
    }

    return {
      filterCounts,
      attentionKinds,
      modelOnlyCoolingCount,
      rateLimitedPauseCount,
      nearLimitCount,
      activeOverageCount,
      unknownOverageCount,
      deviceCount,
      deviceCapacity,
      unlimitedDeviceAccounts,
    }
  }, [evaluatedPool])

  const count = pool.length
  const total = sorted.length
  // 能勾选、能批量改的：代理的列表里下属的号只读，不进勾选。
  const editable = useMemo(() => sorted.filter(owns), [sorted])
  const enabledCount = metrics.filterCounts.enabled
  const schedulableCount = metrics.filterCounts.schedulable
  const attentionCount = metrics.filterCounts.attention
  const quotaRiskCount = metrics.filterCounts.nearLimit
  const fullDeviceCount = metrics.filterCounts.deviceFull
  const filtering = filter !== 'all' || tier !== 'all' || ownerValue !== 'all' || proxyValue !== 'all' || debouncedQuery.trim() !== ''
  const pageCount = Math.max(1, Math.ceil(total / pageSize))
  const current = Math.min(page, pageCount)
  const pageItems = sorted.slice((current - 1) * pageSize, current * pageSize)
  const bannedCount = metrics.filterCounts.banned
  const rejectionsHint = rejectedTotal > 0 && rejectionsQuery.data
    ? t(
        `近 1 小时本地拒绝 ${formatNumber(rejectedTotal)} 条：${rejectionsQuery.data.rows.map((r) => `${rejectionKindLabel(r.kind)} ${formatNumber(r.count)}`).join('，')}。`,
        `${formatNumber(rejectedTotal)} rejected locally in the last hour: ${rejectionsQuery.data.rows.map((r) => `${rejectionKindLabel(r.kind)} ${formatNumber(r.count)}`).join(', ')}.`,
      )
    : undefined
  // 分项顺序与卡片状态的严重程度（`rank`）一致：封禁 → 订阅未生效 / 限流暂停 → Token 失效 → credits → 冷却 → 将满。
  const attentionKindLabels: [string, string, string][] = [
    ['banned', '封禁', 'banned'],
    ['subscription-inactive', '订阅未生效', 'subscription inactive'],
    ['rate-limited', '限流暂停', 'paused'],
    ['token-invalid', 'token 失效', 'token expired'],
    ['overage', '使用 credits', 'on credits'],
    ['overage-unknown', 'credits 待确认', 'credits unconfirmed'],
    ['cooldown', '冷却', 'cooling down'],
    ['near-limit', '将满', 'near limit'],
  ]
  const attentionItems = [
    ...attentionKindLabels.map(([kind, zh, en]) => {
      const n = metrics.attentionKinds[kind] ?? 0
      return n > 0 ? t(`${formatNumber(n)} ${zh}`, `${formatNumber(n)} ${en}`) : ''
    }),
    // 以下两项不是「需处理」的账号，不计入大数：模型级冷却只挡这个号的某几个模型；
    // 本地拒绝数的是请求条数。
    metrics.modelOnlyCoolingCount > 0
      ? t(
          `另有 ${formatNumber(metrics.modelOnlyCoolingCount)} 个账号存在模型冷却`,
          `+${formatNumber(metrics.modelOnlyCoolingCount)} with model cooldown`,
        )
      : '',
    rejectedTotal > 0
      ? t(`${formatNumber(rejectedTotal)} 条拒绝/1h`, `${formatNumber(rejectedTotal)} rejected/1h`)
      : '',
  ].filter(Boolean)
  const quotaRiskItems = [
    metrics.activeOverageCount > 0
      ? t(
          `${formatNumber(metrics.activeOverageCount)} 使用 credits`,
          `${formatNumber(metrics.activeOverageCount)} on credits`,
        )
      : '',
    metrics.unknownOverageCount > 0
      ? t(
          `${formatNumber(metrics.unknownOverageCount)} 待确认`,
          `${formatNumber(metrics.unknownOverageCount)} pending`,
        )
      : '',
    metrics.nearLimitCount > 0
      ? t(
          `${formatNumber(metrics.nearLimitCount)} 将满`,
          `${formatNumber(metrics.nearLimitCount)} near limit`,
        )
      : '',
  ].filter(Boolean)
  /**
   * 概览格的小字只露**最严重的一项 + 「+N」**，完整分项进悬浮提示、一项一行。
   *
   * 原来是把所有分项用「 · 」连成一串：一格的小字只有 120px 上下，「1 token 失效 · 2 使用 credits ·
   * 1 credits 待确认 · 1 冷却 · …」在任何宽度下都被截断，排在后面的几项平时永远看不见。分项本来
   * 就按严重程度排（封禁 → 限流暂停 → token 失效 → …），露出来的那一项正是最该先看的。
   * 用「+N」而不是「· 另 N 项」：桌面上一格小字只有 120px，后者还是会把「2 使用 credits」截掉；
   * 分项口径不变（按主状态、加起来等于大数），只是从挤在一行改成在提示里列全。
   */
  const summarize = (items: string[]) => items.length <= 1
    ? items[0]
    : `${items[0]} +${items.length - 1}`
  const itemList = (items: string[], footer?: string) => items.length > 1 || footer
    ? (
        <span className="block space-y-0.5">
          {items.map((item) => <span key={item} className="block">{item}</span>)}
          {footer && <span className="mt-1 block text-muted-foreground">{footer}</span>}
        </span>
      )
    : undefined
  const attentionStatus = summarize(attentionItems)
  const quotaRiskStatus = summarize(quotaRiskItems)
  const deviceStatus = fullDeviceCount > 0
    ? t(
        `${formatNumber(fullDeviceCount)} 个账号设备名额已满`,
        `${formatNumber(fullDeviceCount)} ${fullDeviceCount === 1 ? 'account' : 'accounts'} at limit`,
      )
    : metrics.unlimitedDeviceAccounts > 0
      ? t(
          `${formatNumber(metrics.unlimitedDeviceAccounts)} 个账号不限设备数`,
          `${formatNumber(metrics.unlimitedDeviceAccounts)} ${metrics.unlimitedDeviceAccounts === 1 ? 'account' : 'accounts'} with no device limit`,
        )
      : metrics.deviceCapacity > 0
        ? t(
            `共 ${formatNumber(metrics.deviceCapacity)} 个名额`,
            `${formatNumber(metrics.deviceCapacity)} ${metrics.deviceCapacity === 1 ? 'slot' : 'slots'} total`,
          )
        : undefined

  const clearSelection = () => actions.onSelectedChange(new Set())
  const changeQuery = (value: string) => {
    actions.onQueryChange(value)
    actions.onPageChange(1)
    clearSelection()
  }
  const changeFilter = (value: CredentialFilterKey) => {
    actions.onFilterChange(value)
    actions.onPageChange(1)
    clearSelection()
  }
  const changeTier = (value: CredentialTierFilterKey) => {
    actions.onTierChange(value)
    actions.onPageChange(1)
    clearSelection()
  }
  const changeOwner = (value: CredentialOwnerFilter) => {
    actions.onOwnerChange(value)
    actions.onPageChange(1)
    clearSelection()
  }
  const changeProxy = (value: CredentialProxyFilter) => {
    actions.onProxyChange(value)
    actions.onPageChange(1)
    clearSelection()
  }
  // 下拉里字段与方向分开选：换字段时用该字段的默认方向，重选当前字段不翻转方向
  //（翻转是列表表头点击的交互，放进单选菜单里会让「点了已勾选的项」反而改掉排序）。
  const changeSortKey = (key: SortKey) => {
    if (key === sort) return
    actions.onSortChange(key, SORT_DIR_DEFAULT[key])
    actions.onPageChange(1)
  }
  const changeSortDir = (next: SortDir) => {
    if (next === dir) return
    actions.onSortChange(sort, next)
    actions.onPageChange(1)
  }
  const changeSort = (key: SortKey) => {
    actions.onSortChange(
      key,
      key === sort ? (dir === 'asc' ? 'desc' : 'asc') : SORT_DIR_DEFAULT[key],
    )
    actions.onPageChange(1)
  }
  // 勾选回调必须在多次渲染之间保持同一个引用，否则 memo 过的卡片/行每次都要重渲染
  // （搜索框每敲一个字就是一轮）。改成收 id 的形式，就不用为每张卡片现做一个闭包；
  // 最新的 selected 与 setter 走 ref 读取，避免闭包读到上一轮的集合把别人的勾选覆盖掉。
  const selectedRef = useRef(selected)
  selectedRef.current = selected
  const onSelectedChangeRef = useRef(actions.onSelectedChange)
  onSelectedChangeRef.current = actions.onSelectedChange
  const toggleSelected = useCallback((id: number, checked: boolean) => {
    const next = new Set(selectedRef.current)
    if (checked) next.add(id)
    else next.delete(id)
    onSelectedChangeRef.current(next)
  }, [])
  // 勾选只保留当前列表里还在的账号：30 秒刷新后离开筛选结果的、或已单独删掉的号，
  // 不能还留在集合里被批量操作带上。列表没拉到之前不裁，免得加载中把勾选清空。
  useEffect(() => {
    if (!credentials || selected.size === 0) return
    const visible = new Set(editable.map((item) => item.id))
    if ([...selected].every((id) => visible.has(id))) return
    onSelectedChangeRef.current(new Set([...selected].filter((id) => visible.has(id))))
  }, [credentials, editable, selected])
  // 「筛选」弹层的三节。成员与代理只有可分的时候才出（成员名单只有一个人时不出）；套餐按
  // TIER_GROUP_STARTS 切组，组间画分隔线。各节第一项是「全部」。
  const facetSections = useMemo(() => {
    const sections: FacetFilterSection[] = []
    if (ownerGroups.length > 0) {
      sections.push({
        key: 'owner',
        title: t('成员', 'Member'),
        value: ownerValue,
        allValue: 'all',
        onChange: changeOwner,
        groups: [
          [{ value: 'all', label: t('全部成员', 'All members'), count: pool.length }],
          ...ownerGroups.map((items) => items.map((item) => ({
            value: item.value, label: item.label, count: item.count, nested: item.nested,
          }))),
        ],
      })
    }
    const tierGroups: FacetFilterOption[][] = []
    tierItems.forEach((item, index) => {
      const entry = { value: item.key, label: item.label, count: facetCounts.tier[item.key] }
      if (index === 0 || TIER_GROUP_STARTS.has(item.key)) tierGroups.push([entry])
      else tierGroups[tierGroups.length - 1].push(entry)
      // 团队 / 企业下面按套餐全称缩进列出各细分（Standard / Premium / Enterprise…），按名称排。
      if (item.key === 'team') {
        const subs = [...facetCounts.team]
          .sort(([a], [b]) => a.localeCompare(b))
          .map(([name, count]) => ({ value: `${TEAM_TIER_PREFIX}${name}`, label: name, count, nested: true }))
        tierGroups[tierGroups.length - 1].push(...subs)
      }
    })
    sections.push({
      key: 'tier',
      title: t('套餐', 'Plan'),
      value: tier,
      allValue: 'all',
      onChange: (value) => changeTier(value as CredentialTierFilterKey),
      groups: tierGroups,
    })
    if (proxyGroups.length > 0) {
      sections.push({
        key: 'proxy',
        title: t('代理', 'Proxy'),
        value: proxyValue,
        allValue: 'all',
        onChange: changeProxy,
        searchPlaceholder: t('搜索 #id 或名称', 'Search #id or name'),
        groups: [
          [{ value: 'all', label: t('全部代理', 'All proxies'), count: facetCounts.proxyAll }],
          // 名单（有哪些代理）按成员范围内的全池定，计数按交叉口径：选中的那条不会因为别的
          // 条件把它数成 0 就从名单里消失、悄悄退回「全部」。
          ...proxyGroups.map((items) => items.map((item) => ({ ...item, count: facetCounts.proxy.get(item.value) ?? 0 }))),
        ],
      })
    }
    return sections
    // change* 每轮都是新函数但行为只依赖 actions，不必进依赖。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [ownerGroups, ownerValue, pool.length, tierItems, facetCounts, tier, proxyGroups, proxyValue, t])
  // 工具条下的生效条件：只列「筛选」弹层里那几维。状态已写在自己的按钮上、概览对应那格也高亮，
  // 再列一枚是三处重复，只筛状态时还会把页头凭空撑高一行；搜索词同理，搜索框里看得见。
  const filterChips: ActiveFilterChip[] = [
    ...(ownerValue !== 'all' ? [{ key: 'owner', dimension: t('成员', 'Member'), value: activeOwnerLabel, onRemove: () => changeOwner('all') }] : []),
    ...(tier !== 'all' ? [{ key: 'tier', dimension: t('套餐', 'Plan'), value: activeTierLabel, onRemove: () => changeTier('all') }] : []),
    ...(proxyValue !== 'all' ? [{ key: 'proxy', dimension: t('代理', 'Proxy'), value: activeProxyLabel, onRemove: () => changeProxy('all') }] : []),
  ]
  // 筛空时空状态里列出的「其余条件」：弹层那几维 + 搜索词（状态单独写在标题里）。
  const emptyConditions = [
    ...filterChips.map((chip) => `${chip.dimension} ${chip.value}`),
    ...(debouncedQuery.trim() ? [t(`搜索「${debouncedQuery.trim()}」`, `search “${debouncedQuery.trim()}”`)] : []),
  ]
  const selectMetric = (key: CredentialFilterKey) => changeFilter(filter === key ? 'all' : key)

  return (
    <div className="space-y-3 sm:space-y-4" data-slot="credential-workspace">
      {/* 页面头 + 工具条 + 指标条同在一张卡片里，**标题与工具条在宽屏上合成一行**
          （用户指定回到 v0.3.142 的这个版式）：
              xl 起  [账号池 13 个账号 7/65 台设备 ● 30 秒刷新][搜索 状态 筛选 | 显示]
              xl 以下 两行，标题行内「刷新」靠 justify-between 推到右端
          生效的筛选条件在工具条下面另起一行（ActiveFilterChips），xl 起落在右列、靠右对齐。
          机制就是外层那个 `xl:grid-cols-[auto_minmax(0,1fr)]`：左列按内容宽、右列吃掉剩下的，
          工具条自己 `xl:justify-end` 靠右；搜索框要 `xl:max-w-64` 封顶，否则它会把筛选器挤到屏幕外。
          「添加账号」不在这里——它是顶栏上那枚橙色 `+`（见 App.tsx 的 AppHeader actions）。 */}
      <section
        className="overflow-hidden rounded-2xl border bg-card shadow-xs/5"
        aria-labelledby="page-title"
      >
        <div
          className={cn(
            'grid gap-3 px-4 py-4 sm:px-5',
            (isLoading || count > 0) && 'xl:grid-cols-[auto_minmax(0,1fr)] xl:items-center',
          )}
        >
          <div className="flex min-w-0 flex-wrap items-center justify-between gap-x-3 gap-y-2 xl:justify-start">
            <div className="flex min-w-0 flex-wrap items-center gap-2.5">
              <h1 id="page-title" className="min-w-0 text-lg font-semibold tracking-tight">
                {t('账号池', 'Account pool')}
              </h1>
              {!isLoading && (
                <Badge variant="secondary" size="lg">
                  {t(
                    `${formatNumber(count)} 个账号`,
                    `${formatNumber(count)} ${count === 1 ? 'account' : 'accounts'}`,
                  )}
                </Badge>
              )}
              {/* 绑定设备数从概览格挪到这里：概览那一行留给「号的状态」与「流量质量」，设备数
                  是池子的容量属性，与账号数并排读更顺。点击仍是筛选（已满 > 已绑定）。 */}
              {!isLoading && count > 0 && !devicesBySession && (
                <Tooltip>
                  <TooltipTrigger
                    // 这枚是**控件**（点下去筛列表），所以用小号 outline 的 Toggle：白底 + 边框 +
                    // hover 变底色。旁边那枚「N 个账号」是纯展示的 secondary 徽标，没有边框。
                    // 原来两枚逐字同样式，谁点得动全靠猜——选中态那条竖条只在点过之后才出现，
                    // 没法在点之前给提示。
                    // 按下态与概览格同一套记号：淡底 + 一条左侧竖条。Toggle 自带的按下底色与
                    // outline 的「按下去掉阴影」与这两条同权重，所以加 `!` 盖过去。
                    // 点击不看 Toggle 给的新状态：已筛「有设备」而池里又有满额号时，点下去是改筛
                    // 「已满」而不是取消，沿用 selectMetric 的切换规则。
                    render={(
                      <Toggle
                        variant="outline"
                        size="sm"
                        pressed={filter === 'deviceFull' || filter === 'hasDevice'}
                        onPressedChange={() => selectMetric(fullDeviceCount > 0 ? 'deviceFull' : 'hasDevice')}
                      />
                    )}
                    className={cn(
                      'gap-1 px-2 text-xs text-muted-foreground hover:text-foreground',
                      'data-pressed:bg-marine/10! data-pressed:text-foreground data-pressed:shadow-[inset_2px_0_0_0_var(--marine)]!',
                      fullDeviceCount > 0 && 'text-warning-foreground',
                    )}
                  >
                    <SmartphoneIcon className="size-3" aria-hidden />
                    <span className="tnum">
                      {metrics.deviceCapacity > 0 && metrics.unlimitedDeviceAccounts === 0
                        ? `${formatNumber(metrics.deviceCount)}/${formatNumber(metrics.deviceCapacity)}`
                        : formatNumber(metrics.deviceCount)}
                    </span>
                    <span>{t('台设备', metrics.deviceCount === 1 ? 'device' : 'devices')}</span>
                  </TooltipTrigger>
                  {/* 原来挂在原生 `title` 上，手机上完全出不来，而这句写的正是「这个数是怎么算的、
                      点下去会筛出什么」。换成 Tooltip 后桌面悬停可见；这枚是按钮，触屏上长按弹出。 */}
                  <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
                    {deviceStatus}
                  </TooltipPopup>
                </Tooltip>
              )}
            </div>
            <div
              className="flex shrink-0 items-center gap-1.5 text-xs text-muted-foreground"
              aria-live="polite"
              aria-atomic="true"
            >
              {isRefetchError ? (
                <>
                  <TriangleAlertIcon className="size-3.5 text-destructive-foreground" aria-hidden />
                  <Button
                    variant="link"
                    size="xs"
                    className="px-1 text-destructive-foreground"
                    onClick={actions.onRetry}
                  >
                    {t('刷新失败，点击重试', 'Refresh failed. Click to retry')}
                  </Button>
                </>
              ) : (
                // 自动刷新指示器同时是手动刷新入口：等下一轮 30 秒才能确认操作结果，
                // 是这类常驻列表最常见的抱怨，而这块本来就在讲「数据有多新」。
                <Tooltip>
                  <TooltipTrigger
                    // disabled 必须写在 render 的按钮上：写在 TooltipTrigger 上只会禁用提示，按钮照样能点。
                    // `disabled:opacity-100`：后台每 30 秒取一次数，按钮跟着禁用；Button 默认的禁用变淡
                    // 会让这行字每半分钟闪一次，转圈图标已经说明「正在刷新」。
                    render={<Button variant="link" size="xs" disabled={isLoading || isFetching} />}
                    className="gap-1.5 px-1 font-normal text-muted-foreground hover:text-foreground disabled:opacity-100"
                    onClick={actions.onRetry}
                    aria-label={t('立即刷新账号数据', 'Refresh account data now')}
                  >
                    <span className="flex size-3.5 shrink-0 items-center justify-center" aria-hidden>
                      {isLoading || isFetching ? (
                        <RefreshCwIcon className="size-3.5 animate-spin" />
                      ) : (
                        <span className="size-1.5 rounded-full bg-success" />
                      )}
                    </span>
                    {/* ≤384px（360 那档手机）标题行放不下「账号池 · N 个账号 · N 台设备 · 30 秒刷新」四样，
                        这一枚会被挤成单独一行、只有四个字。窄屏上只留绿点（它本身就是「在自动刷新」），
                        文字留给读屏；刷新失败那句照旧整句显示，那是要人处理的。 */}
                    {/* `min-w-0` 不能省：sr-only 把它变成绝对定位的 1px 盒子，但 `min-w-14` 仍会把它撑回 56px，
                        落在页头右缘外面，360 的屏上整页因此多出 4px 横向滚动。 */}
                    <span className="min-w-14 text-left max-[24rem]:sr-only max-[24rem]:min-w-0">
                      {isLoading ? t('正在加载', 'Loading') : t('30 秒刷新', '30s refresh')}
                    </span>
                  </TooltipTrigger>
                  <TooltipPopup>
                    {isLoading
                      ? t('正在加载账号数据', 'Loading account data')
                      : t('每 30 秒自动刷新，点击立即刷新', 'Refreshes automatically every 30 seconds. Click to refresh now')}
                  </TooltipPopup>
                </Tooltip>
              )}
            </div>
          </div>
        {isLoading ? (
          <WorkspaceToolbarSkeleton />
        ) : count > 0 && (
          <Toolbar className="grid w-full grid-cols-3 items-stretch gap-2 border-0 bg-transparent p-0 sm:flex sm:flex-row sm:flex-wrap sm:items-center xl:justify-end">
            {/* sm–lg（工具条独占一行）时搜索框吃掉剩余宽度，整行没有死区；xl 起与标题同行，
                必须用 `xl:max-w-64` 封顶——不封顶它会一路撑开，把筛选与视图切换挤出可视区。 */}
            <ToolbarSearch
              ariaLabel={t('搜索账号', 'Search accounts')}
              className="max-sm:col-span-3 sm:min-w-56 sm:flex-1 xl:max-w-64"
              placeholder={t('搜索名称、#id、套餐或状态', 'Search name, #id, plan or status')}
              value={query}
              onChange={changeQuery}
            />

            {/* 状态是最常用的一维，概览各格点下去切的也是它，单独一个按钮；成员、套餐、代理这些
                「按属性缩小范围」的收进「筛选」弹层，工具条上只占一格，再加维度也不变长。手机上
                固定三格（状态 / 筛选 / 排序），不随维度多少变列。排序在分隔线之后，属于「怎么展示」。 */}
            <ToolbarGroup className="grid min-w-0 grid-cols-3 max-sm:col-span-3 sm:flex sm:flex-wrap">
              <ToolbarMenuSelect
                active={filter !== 'all'}
                ariaLabel={t(`状态：${activeFilterLabel}`, `Status: ${activeFilterLabel}`)}
                groups={filterGroups.map((items) => items.map((item) => ({
                  value: item.key,
                  label: (
                    <FacetOption
                      label={item.label}
                      count={formatNumber(facetCounts.status[item.key])}
                      empty={item.key !== 'all' && facetCounts.status[item.key] === 0}
                    />
                  ),
                })))}
                groupLabels={filterGroups.map((items) => {
                  const label = FILTER_GROUP_LABELS[items[0].group]
                  return label ? t(...label) : undefined
                })}
                icon={ListFilterIcon}
                label={(
                  <span className="min-w-0 truncate">
                    <span className="max-sm:hidden">{t('状态：', 'Status: ')}</span>
                    {activeFilterLabel}
                  </span>
                )}
                value={filter}
                onChange={changeFilter}
              />

              <ToolbarFacetFilter sections={facetSections} />

              <ToolbarSeparator orientation="vertical" className="mx-1 hidden sm:block" />

              <DisplayOptions
                view={view}
                onViewChange={actions.onViewChange}
                sort={sort}
                dir={dir}
                sortGroups={sortGroups}
                sortLabel={activeSortLabel}
                onSortKeyChange={changeSortKey}
                onSortDirChange={changeSortDir}
              />
            </ToolbarGroup>
          </Toolbar>
        )}
        {/* 生效条件紧跟工具条下面一行，与页头同在一块里，xl 起横跨标题与工具条两列、靠左排。 */}
        {!isLoading && count > 0 && (
          <ActiveFilterChips
            className="xl:col-span-2"
            summary={t(
              `${formatNumber(total)} / ${formatNumber(count)} 个账号`,
              `${formatNumber(total)} of ${formatNumber(count)} accounts`,
            )}
            chips={filterChips}
            onClearAll={() => {
              changeOwner('all')
              changeTier('all')
              changeProxy('all')
            }}
          />
        )}
        </div>
      {isLoading ? (
        <section
          aria-label={t('正在加载账号池概览', 'Loading account pool overview')}
          className={seesWholePool ? 'grid grid-cols-2 border-t lg:grid-cols-6' : 'grid grid-cols-2 border-t lg:grid-cols-5'}
        >
          {/* 与加载完的格数、边框一一对应（管理员六格、代理和用户五格），数据到了不跳。 */}
          <OverviewMetricSkeleton className="border-r border-b lg:border-b-0" />
          <OverviewMetricSkeleton className="border-b lg:border-r lg:border-b-0" />
          <OverviewMetricSkeleton className="border-r border-b lg:border-b-0" />
          <OverviewMetricSkeleton className="border-b lg:border-r lg:border-b-0" />
          {seesWholePool ? (
            <>
              <OverviewMetricSkeleton className="border-r" />
              <OverviewMetricSkeleton />
            </>
          ) : (
            <OverviewMetricSkeleton className="col-span-2 lg:col-span-1" />
          )}
        </section>
      ) : count > 0 && (
        <section
          aria-label={t('账号池概览', 'Account pool overview')}
          className={seesWholePool ? 'grid grid-cols-2 border-t lg:grid-cols-6' : 'grid grid-cols-2 border-t lg:grid-cols-5'}
        >
          {/* 手机上齐整的 2 列 × 3 行，lg 起一字排开六格。
              原来末两格各带 `col-span-2` 独占一整行——「首字时延 — 暂无数据」右半边整片空着，
              六格铺成五行，把账号列表一路顶下去。半格宽度对这两格是够的：手机上去掉 32px 图标方块后
              正文有 155px，「0 RPM · 0 在途」与「— 暂无数据」都装得下，见 overview-metric 里的注。 */}
          <OverviewMetric
            className="border-r border-b lg:border-b-0"
            label={t('可调度账号', 'Schedulable accounts')}
            value={`${formatNumber(schedulableCount)}/${formatNumber(count)}`}
            status={schedulableCount < count
              ? t(
                  `${formatNumber(count - schedulableCount)} 暂不可用`,
                  `${formatNumber(count - schedulableCount)} unavailable`,
                )
              : t(
                  `${formatNumber(enabledCount)} 已启用`,
                  `${formatNumber(enabledCount)} enabled`,
                )}
            icon={ShieldCheckIcon}
            tone={schedulableCount > 0 ? 'ok' : 'bad'}
            active={filter === 'schedulable'}
            onClick={() => selectMetric('schedulable')}
          />
          <OverviewMetric
            className="border-b lg:border-r lg:border-b-0"
            label={t('需处理', 'Needs attention')}
            value={formatNumber(attentionCount)}
            status={attentionStatus}
            statusDetail={itemList(attentionItems, rejectionsHint)}
            statusHint={rejectionsHint}
            icon={TriangleAlertIcon}
            tone={bannedCount > 0 || metrics.activeOverageCount > 0
              ? 'bad'
              : attentionCount > 0 || rejectedTotal > 0
                ? 'warn'
                : 'neutral'}
            active={filter === 'attention'}
            onClick={() => selectMetric('attention')}
          />
          <OverviewMetric
            className="border-r border-b lg:border-b-0"
            label={t('用量风险', 'Usage risk')}
            value={formatNumber(quotaRiskCount)}
            status={quotaRiskStatus}
            statusDetail={itemList(quotaRiskItems)}
            icon={RadioIcon}
            tone={metrics.activeOverageCount > 0 ? 'bad' : quotaRiskCount > 0 ? 'warn' : 'neutral'}
            active={filter === 'nearLimit'}
            onClick={() => selectMetric('nearLimit')}
          />
          {/* 缓存命中率与首字时延不来自账号列表，点开是趋势而不是筛选——它们讲的是「转发出去的
              请求质量如何」。摆在实时流量左边：三格都是流量的属性，凑在一起读。 */}
          {seesWholePool && (<>
          <OverviewMetric
            className="border-b lg:border-r lg:border-b-0"
            label={cacheNow
              ? t(`缓存命中率 · ${cacheNow.label[0]}`, `Cache hit rate · ${cacheNow.label[1]}`)
              : t('缓存命中率', 'Cache hit rate')}
            value={formatPercent(cacheRate)}
            // 这一行只放数值与迷你线：一格的内容区约 206px，「数值 52 + 7d 小字 85 + 迷你线 80」
            // 要 220px 以上，六格并排得到约 1620px 视口才装得下——小字一直被截成「7d 95.6% ↓…」、
            // 迷你线被压成一条。7 天基线与相对变化搬进悬浮提示（以及点开的趋势图），小字只在
            // 没用量时留一句说明。
            trend={cacheRate == null ? undefined : <CacheHitSparkline slots={cacheSeries.slots} />}
            status={cacheRate == null ? t('暂无用量', 'No usage yet') : undefined}
            statusHint={cacheNow
              ? t(
                  `${cacheNow.label[0]}：${cacheSplitText(cacheNow.p, t)}${cacheBase == null ? '' : `；近 7 天基线 ${formatPercent(cacheBase)}${cacheDeltaText}`}。按 token 加权；迷你线为近 24 小时的逐小时数据。点击查看趋势及按模型 / 账号的拆分。`,
                  `${cacheNow.label[1]}: ${cacheSplitText(cacheNow.p, t)}${cacheBase == null ? '' : `; 7-day baseline ${formatPercent(cacheBase)}${cacheDeltaText}`}. Token-weighted; the sparkline is the last 24 hours by hour. Click for the trend and the per-model / per-account breakdown.`,
                )
              : undefined}
            icon={DatabaseZapIcon}
            tone={cacheRate == null ? 'neutral' : cacheRate >= 0.5 ? 'ok' : 'warn'}
            opensDetail
            onClick={() => setCacheTrendOpen(true)}
          />
          {/* 与缓存那格同一写法：这一行只放数值与迷你线，7 天基线、p95、吞吐都在悬浮提示里。 */}
          <OverviewMetric
            className="border-r"
            label={ttftNow
              ? t(`首字时延 p50 · ${ttftNow.label[0]}`, `TTFT p50 · ${ttftNow.label[1]}`)
              : t('首字时延', 'TTFT')}
            value={formatMs(ttftNow ? ttftNow.p.p50_ms : null)}
            trend={ttftNow == null ? undefined : <TtftSparkline slots={ttftSeries.slots} />}
            status={ttftNow == null ? t('暂无数据', 'No data yet') : undefined}
            statusHint={ttftNow
              ? t(
                  `${ttftNow.label[0]}：p50 ${formatMs(ttftNow.p.p50_ms)} · p95 ${formatMs(ttftNow.p.p95_ms)} · 平均 ${formatMs(ttftNow.p.avg_ms)} · ${formatNumber(ttftNow.p.count)} 次成功请求 · 吞吐 ${formatTokensPerSec(ttftNow.p.tokens_per_sec)}${ttftBase ? `；近 7 天基线 p50 ${formatMs(ttftBase.p50_ms)}${ttftDeltaText} · p95 ${formatMs(ttftBase.p95_ms)}` : ''}。迷你线为近 24 小时逐小时的 p50。点击查看趋势及按模型 / 账号的拆分。`,
                  `${ttftNow.label[1]}: p50 ${formatMs(ttftNow.p.p50_ms)} · p95 ${formatMs(ttftNow.p.p95_ms)} · avg ${formatMs(ttftNow.p.avg_ms)} · ${formatNumber(ttftNow.p.count)} successful requests · throughput ${formatTokensPerSec(ttftNow.p.tokens_per_sec)}${ttftBase ? `; 7-day baseline p50 ${formatMs(ttftBase.p50_ms)}${ttftDeltaText} · p95 ${formatMs(ttftBase.p95_ms)}` : ''}. The sparkline is hourly p50 over the last 24 hours. Click for the trend and the per-model / per-account breakdown.`,
                )
              : undefined}
            icon={TimerIcon}
            tone={ttftTone}
            opensDetail
            onClick={() => setTtftTrendOpen(true)}
          />
          </>)}
          {/* 代理和用户没有缓存与时延那两格（全池口径），换成自己名下的「今日费用」：只计费不扣费，
              看的是这些号今天跑出了多少等价费用。点开去费用页看按号 / 模型的明细。 */}
          {!seesWholePool && (
            <OverviewMetric
              className="border-b lg:border-r lg:border-b-0"
              label={t('今日费用', 'Cost today')}
              value={todayCostQuery.data ? formatUsd(todayCostQuery.data.total.cost_usd) : '—'}
              status={t(`累计 ${formatUsd(lifetimeCost)}`, `${formatUsd(lifetimeCost)} total`)}
              statusHint={t(
                `今日零点至今，名下账号按官方价格折算的费用${todayCostQuery.data ? `，共 ${formatNumber(todayCostQuery.data.total.requests)} 次请求` : ''}。累计为各账号接入以来的总和。仅作计量，不实际扣费；每分钟更新，点击查看明细。`,
                `Cost of your accounts since midnight at official API prices${todayCostQuery.data ? ` across ${formatNumber(todayCostQuery.data.total.requests)} requests` : ''}. The total covers each account since it was added. Metered only, never charged; updated every minute. Click for details.`,
              )}
              icon={ReceiptIcon}
              tone="neutral"
              opensDetail={!!actions.onOpenBilling}
              onClick={actions.onOpenBilling}
            />
          )}
          <LiveTrafficMetric
            className={seesWholePool ? undefined : 'col-span-2 lg:col-span-1'}
            label={t('实时流量', 'Live traffic')}
            value={metricsQuery.data ? formatNumber(metricsQuery.data.rpm) : '—'}
            unit="RPM"
            detail={metricsQuery.data
              ? t(
                  `${formatNumber(metricsQuery.data.in_flight)} 在途`,
                  `${formatNumber(metricsQuery.data.in_flight)} in flight`,
                )
              : t('读取中', 'Loading')}
            live={(metricsQuery.data?.in_flight ?? 0) > 0}
            hint={seesWholePool
              ? t(
                  `全池实时流量：最近 ${metricsQuery.data?.window_secs ?? 60} 秒转发的请求总数（各账号 RPM 之和），以及当前已进入转发、响应尚未结束的在途请求数。每 10 秒刷新一次。`,
                  `Live traffic across the pool: requests forwarded in the last ${metricsQuery.data?.window_secs ?? 60} seconds (the sum of every account's RPM), plus the requests in flight right now — accepted for forwarding but not finished responding. Refreshed every 10 seconds.`,
                )
              : t(
                  `名下账号近 ${metricsQuery.data?.window_secs ?? 60} 秒转发的请求数，以及已发往上游、尚未完成的在途请求数。每 10 秒更新。`,
                  `Requests forwarded through your accounts in the last ${metricsQuery.data?.window_secs ?? 60} seconds, and those sent upstream that are still in progress. Updated every 10 seconds.`,
                )}
            icon={ActivityIcon}
          />
        </section>
      )}
      </section>

      <section className="min-w-0" aria-labelledby="account-list-title">
        <h2 id="account-list-title" className="sr-only">{t('账号列表', 'Account list')}</h2>
        <p className="sr-only" aria-live="polite">
          {isLoading
            ? t('正在加载账号', 'Loading accounts')
            : filtering
            ? t(
                `筛选出 ${formatNumber(total)} 个，共 ${formatNumber(count)} 个账号`,
                `${formatNumber(total)} ${total === 1 ? 'match' : 'matches'} out of ${formatNumber(count)} ${count === 1 ? 'account' : 'accounts'}`,
              )
            : t(
                `共 ${formatNumber(count)} 个账号`,
                `${formatNumber(count)} ${count === 1 ? 'account' : 'accounts'} total`,
              )}
        </p>
        <div className="min-w-0 space-y-3 sm:space-y-4">

          {!readOnly && count > 0 && selected.size > 0 && (
            <div className="relative">
              <BatchActionsBar
                all={editable}
                selected={selected}
                onSelectedChange={actions.onSelectedChange}
                onClear={clearSelection}
              />
            </div>
          )}

          {isLoading ? (
            <div className="relative">
              <CredentialLoadingState view={view} selectable={!readOnly} count={pageSize} />
            </div>
          ) : isError && !credentials ? (
            <Card><ErrorState error={error} title={t('暂时无法读取账号', 'Unable to load accounts')} onRetry={actions.onRetry} /></Card>
          ) : count === 0 ? (
            <Card><EmptyState onAdd={actions.onAdd} /></Card>
          ) : total === 0 ? (
            // 筛空时说清是哪个条件筛空的：状态筛空最常见（点了个 0 的状态），标题直接点名该状态，
            // 描述列出其余生效条件，按钮先给「只撤状态」这条最小退路，再给「全部清除」。
            <Card>
              <Empty>
                <EmptyHeader>
                  <EmptyMedia variant="icon"><SearchIcon /></EmptyMedia>
                  <EmptyTitle>
                    {filter !== 'all'
                      ? t(`没有「${activeFilterLabel}」的账号`, `No accounts in “${activeFilterLabel}”`)
                      : t('没有符合条件的账号', 'No matching accounts')}
                  </EmptyTitle>
                  <EmptyDescription>
                    {emptyConditions.length > 0
                      ? t(
                        `当前条件：${emptyConditions.join('、')}。`,
                        `Current conditions: ${emptyConditions.join(', ')}.`,
                      )
                      : filter !== 'all'
                        ? t('账号池里目前没有处于这个状态的账号。', 'No account in the pool is in this state right now.')
                        : t('尝试清除当前筛选条件或搜索关键字。', 'Try clearing the current filters or search terms.')}
                  </EmptyDescription>
                </EmptyHeader>
                <EmptyContent className="flex-row flex-wrap justify-center gap-2">
                  {filter !== 'all' && (
                    <Button variant={emptyConditions.length > 0 ? 'default' : 'outline'} onClick={() => changeFilter('all')}>
                      {t('查看全部状态', 'Show all statuses')}
                    </Button>
                  )}
                  {(filter === 'all' || emptyConditions.length > 0) && (
                    <Button
                      variant="outline"
                      onClick={() => {
                        actions.onQueryChange('')
                        changeFilter('all')
                        changeTier('all')
                        changeOwner('all')
                        changeProxy('all')
                      }}
                    >
                      {t('清除筛选与搜索', 'Clear filters and search')}
                    </Button>
                  )}
                </EmptyContent>
              </Empty>
            </Card>
          ) : view === 'list' ? (
            // 一律 table-fixed：列宽由 CredentialRow 的 COL 预算决定、总宽等于容器宽，表格永远
            // 不横向滚动。之前 xl 起改成 table-auto + 最小宽 72rem，不换行的单元格会把表撑出
            // 容器，1280–1536 宽的屏上底部多出一条横向滚动条。
            <Table variant="card" className="table-fixed">
              <TableCaption className="sr-only">{t('账号列表', 'Account list')}</TableCaption>
              <CredentialListHeader
                selectable={!readOnly && editable.length > 0}
                sort={sort}
                dir={dir}
                onSortChange={changeSort}
                allSelected={editable.length > 0 && editable.every((item) => selected.has(item.id))}
                onSelectAll={(checked) => actions.onSelectedChange(
                  checked ? new Set(editable.map((item) => item.id)) : new Set(),
                )}
              />
              <TableBody>
                {pageItems.map((item) => (
                  <ReadOnlyScope key={item.id} readOnly={!owns(item)}>
                    <CredentialRow
                      cred={item}
                      now={now}
                      selectable={!readOnly && owns(item)}
                      selected={selected.has(item.id)}
                      onSelectedChange={toggleSelected}
                    />
                  </ReadOnlyScope>
                ))}
              </TableBody>
            </Table>
          ) : (
            // 网格规则与加载骨架共用，见 CREDENTIAL_CARD_GRID_CLASS。
            <ul className={CREDENTIAL_CARD_GRID_CLASS}>
              {pageItems.map((item) => (
                <ReadOnlyScope key={item.id} readOnly={!owns(item)}>
                  <CredentialCard
                    cred={item}
                    now={now}
                    selectable={!readOnly && owns(item)}
                    selected={selected.has(item.id)}
                    onSelectedChange={toggleSelected}
                  />
                </ReadOnlyScope>
              ))}
            </ul>
          )}

          {/* 门槛是「总数超过最小的每页档位」而不是「不止一页」：选成 50 个/页后 21 个账号只剩一页，
            整条分页条跟着消失的话，那个「每页」选择器也没了，再想改回 10 个/页就无从下手。 */}
          {!isLoading && (pageCount > 1 || total > CREDENTIAL_PAGE_SIZES[0]) && (
            <div className="relative py-2">
              <PaginationBar
                total={total}
                page={current}
                pageCount={pageCount}
                pageSize={pageSize}
                pageSizes={CREDENTIAL_PAGE_SIZES}
                unit="account"
                pageSizeLabel={t('每页账号数', 'Accounts per page')}
                onPageChange={actions.onPageChange}
                onPageSizeChange={(size) => {
                  actions.onPageSizeChange(size)
                  actions.onPageChange(1)
                }}
              />
            </div>
          )}
        </div>
      </section>

      <CacheHitTrendDialog open={cacheTrendOpen} onOpenChange={setCacheTrendOpen} />
      <TtftTrendDialog open={ttftTrendOpen} onOpenChange={setTtftTrendOpen} />
    </div>
  )
}

function EmptyState({ onAdd }: { onAdd: () => void }) {
  const { t } = useI18n()
  const readOnly = useReadOnly()
  return (
    <Empty>
      <EmptyHeader>
        <EmptyMedia variant="icon"><PlusIcon /></EmptyMedia>
        <EmptyTitle>{t('建立第一个调度账号', 'Add your first schedulable account')}</EmptyTitle>
        <EmptyDescription>
          {t(
            '账号池中尚无账号。完成 Claude OAuth 授权后，账号将加入当前网关的调度池。',
            'There are no accounts in the pool yet. After Claude OAuth authorization, the account joins this gateway’s scheduling pool.',
          )}
        </EmptyDescription>
      </EmptyHeader>
      {!readOnly && (
        <EmptyContent>
          <Button onClick={onAdd}>
            <PlusIcon />
            {t('添加第一个账号', 'Add first account')}
          </Button>
        </EmptyContent>
      )}
    </Empty>
  )
}
