import { useCallback, useEffect, useMemo, useRef, useState, type RefObject } from 'react'
import {
  ArrowUpDownIcon,
  ChevronLeftIcon,
  ChevronRightIcon,
  DatabaseZapIcon,
  LayersIcon,
  LayoutGridIcon,
  ListFilterIcon,
  ListIcon,
  PlusIcon,
  ActivityIcon,
  RadioIcon,
  RefreshCwIcon,
  SearchIcon,
  ShieldCheckIcon,
  SmartphoneIcon,
  TimerIcon,
  TriangleAlertIcon,
  XIcon,
} from 'lucide-react'
import { useQuery } from '@tanstack/react-query'
import type { Credential } from '@/api/credentials'
import { getMetrics } from '@/api/metrics'
import { BatchActionsBar } from '@/components/batch-actions-bar'
import {
  CacheHitSparkline,
  aggregateCacheHitRate,
  cacheTotalsText,
} from '@/components/cache-hit-chart'
import {
  CacheHitTrendDialog,
  DEFAULT_CACHE_RANGE,
  useCacheSeries,
} from '@/components/cache-hit-trend-dialog'
import { CredentialCard } from '@/components/credential-card'
import { CredentialLoadingState } from '@/components/credential-loading'
import {
  SORTS,
  SORT_DIR_DEFAULT,
  evaluateCredential,
  isAccountBan,
  planKey,
  sortCreds,
  type CredentialEvaluation,
  type PlanKey,
  type SortDir,
  type SortKey,
} from '@/components/credential-shared'
import { CredentialListHeader, CredentialRow } from '@/components/credential-row'
import { LiveTrafficMetric, OverviewMetric, OverviewMetricSkeleton } from '@/components/overview-metric'
import {
  DEFAULT_TTFT_RANGE,
  TtftSparkline,
  TtftTrendDialog,
  aggregateTtft,
  formatMs,
  useTtftSeries,
} from '@/components/ttft-trend-dialog'
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
import { InputGroup, InputGroupAddon, InputGroupInput } from '@/components/ui/input-group'
import {
  Menu,
  MenuPopup,
  MenuRadioGroup,
  MenuRadioItem,
  MenuTrigger,
} from '@/components/ui/menu'
import {
  Pagination as CossPagination,
  PaginationContent,
  PaginationItem,
  PaginationLink,
} from '@/components/ui/pagination'
import { Select, SelectItem, SelectPopup, SelectTrigger, SelectValue } from '@/components/ui/select'
import { Skeleton } from '@/components/ui/skeleton'
import { Table, TableBody, TableCaption } from '@/components/ui/table'
import { ToggleGroup, ToggleGroupItem, ToggleGroupSeparator } from '@/components/ui/toggle-group'
import { Toolbar, ToolbarGroup, ToolbarSeparator } from '@/components/ui/toolbar'
import { useI18n, type Language } from '@/lib/i18n'
import { useDebounced } from '@/lib/use-debounced'
import { cn, displayCredentialLabel, extractError, formatPercent } from '@/lib/utils'

export type CredentialFilterKey =
  | 'all'
  | 'schedulable'
  | 'attention'
  | 'enabled'
  | 'disabled'
  | 'banned'
  | 'tokenInvalid'
  | 'nearLimit'
  | 'cooldown'
  | 'hasDevice'
  | 'deviceFull'

/**
 * 套餐筛选与上面那组状态筛选**是两个维度，各自独立**：一个说「这个号现在怎么样」，另一个说
 * 「这个号是什么档位」。合进同一个单选列表的话，「Pro 里有哪些需要处理」这种最常问的问题就
 * 提不出来——选了 Pro 就丢掉了状态条件。故单开一列，两个菜单同时生效（取交集）。
 */
export type CredentialTierFilterKey = 'all' | PlanKey

export type CredentialViewMode = 'card' | 'list'

export const CREDENTIAL_PAGE_SIZES = [10, 20, 50] as const
export type CredentialPageSize = (typeof CREDENTIAL_PAGE_SIZES)[number]

export const CREDENTIAL_VIEW_MODES = ['card', 'list'] as const

const PAGE_SIZE_ITEMS = CREDENTIAL_PAGE_SIZES.map((size) => ({
  size,
  value: String(size),
}))

type LocalizedLabel = readonly [chinese: string, english: string]

const FILTERS: {
  key: CredentialFilterKey
  label: LocalizedLabel
  match: (evaluation: CredentialEvaluation) => boolean
}[] = [
  { key: 'all', label: ['全部', 'All'], match: () => true },
  {
    key: 'schedulable',
    label: ['可调度', 'Schedulable'],
    match: (evaluation) => evaluation.schedulable,
  },
  {
    key: 'attention',
    label: ['需处理', 'Needs attention'],
    match: (evaluation) => evaluation.needsAttention,
  },
  { key: 'enabled', label: ['启用', 'Enabled'], match: ({ credential }) => !credential.disabled },
  {
    key: 'disabled',
    label: ['手动停用', 'Manually disabled'],
    // 只匹配人工手动关掉的号：排除封禁、Token 失效（有 ban_reason）和限流暂停（有 resume_at），
    // 那几种各有各的 tab，混在一起会让计数虚高、点开找不到预期的号。
    match: ({ credential }) =>
      credential.disabled && !credential.ban_reason && credential.resume_at == null,
  },
  {
    key: 'banned',
    label: ['已封禁', 'Banned'],
    match: ({ credential }) =>
      !!credential.ban_reason && credential.resume_at == null && isAccountBan(credential.ban_reason),
  },
  {
    key: 'tokenInvalid',
    label: ['Token 失效', 'Token expired'],
    match: ({ credential }) =>
      !!credential.ban_reason && credential.resume_at == null && !isAccountBan(credential.ban_reason),
  },
  { key: 'nearLimit', label: ['用量风险', 'Usage risk'], match: (evaluation) => evaluation.quotaRisk },
  {
    key: 'cooldown',
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
    label: ['已绑定设备', 'Devices linked'],
    match: ({ credential }) => credential.device_count > 0,
  },
  {
    key: 'deviceFull',
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
const TIER_FILTERS: { key: CredentialTierFilterKey; label: LocalizedLabel }[] = [
  { key: 'all', label: ['全部套餐', 'All plans'] },
  { key: 'max20x', label: ['Max 20x', 'Max 20x'] },
  { key: 'max5x', label: ['Max 5x', 'Max 5x'] },
  // 上游偶尔只给 `claude_max` 这种不带倍率的写法，单列一档收着，否则它会掉进「未知」里。
  { key: 'max', label: ['Max（未标倍率）', 'Max (no multiplier)'] },
  { key: 'pro', label: ['Pro', 'Pro'] },
  { key: 'free', label: ['Free', 'Free'] },
  { key: 'unknown', label: ['未知', 'Unknown'] },
]

const SORT_LABELS: Record<SortKey, LocalizedLabel> = {
  priority: ['优先级', 'Priority'],
  status: ['状态', 'Status'],
  name: ['名称', 'Name'],
  tier: ['套餐', 'Plan'],
  usage5h: ['5h 使用率', '5h usage'],
  usage7d: ['7d 使用率', '7d usage'],
  devices: ['设备数', 'Devices'],
  rpm: ['当前 RPM', 'Current RPM'],
  cost: ['累计花费', 'Total cost'],
  recent: ['最近使用', 'Last used'],
  created: ['添加时间', 'Date added'],
}

/** 筛选/套餐触发器在「已生效」时的染色，见 FILTERS 上方的说明。 */
const ACTIVE_FILTER_CLASS =
  'border-marine/40 bg-marine/10 text-marine-foreground hover:border-marine/40 hover:bg-marine/16 data-pressed:bg-marine/16'

export const CREDENTIAL_FILTER_KEYS = FILTERS.map((filter) => filter.key)
export const CREDENTIAL_TIER_FILTER_KEYS = TIER_FILTERS.map((item) => item.key)

export function preferredInitialCredentialView(): CredentialViewMode {
  return typeof window !== 'undefined' && window.matchMedia('(min-width: 80rem)').matches
    ? 'list'
    : 'card'
}

/** 额度 reset 与相对时间都依赖当前时刻；30 秒 tick 与接口刷新节奏一致。 */
function useNowSeconds(): number {
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
 * `/` 与 ⌘K / Ctrl+K 聚焦搜索框——列表型控制台的通用约定。
 *
 * 已经在输入的时候不抢键（否则打不出 `/`）；弹层/对话框打开时也不抢，
 * 否则焦点会跳到被遮住的输入框上，模态里反而按不动。
 */
function useSearchHotkey(ref: RefObject<HTMLInputElement | null>): void {
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      const slash = event.key === '/' && !event.metaKey && !event.ctrlKey && !event.altKey
      const commandK = (event.key === 'k' || event.key === 'K') && (event.metaKey || event.ctrlKey)
      if (!slash && !commandK) return
      const target = event.target as HTMLElement | null
      if (target?.isContentEditable) return
      if (target && /^(input|textarea|select)$/i.test(target.tagName)) return
      if (target?.closest('[role="dialog"], [role="alertdialog"], [role="menu"], [role="listbox"]')) return
      const input = ref.current
      if (!input) return
      event.preventDefault()
      input.focus()
      input.select()
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [ref])
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
  onSortChange: (key: SortKey, dir: SortDir) => void
  onViewChange: (value: CredentialViewMode) => void
  onSelectedChange: (value: Set<number>) => void
  onPageChange: (value: number) => void
  onPageSizeChange: (value: CredentialPageSize) => void
  onRetry: () => void
  onAdd: () => void
}

export interface CredentialWorkspaceProps {
  data: CredentialWorkspaceData
  state: CredentialWorkspaceState
  actions: CredentialWorkspaceActions
}

function WorkspaceToolbarSkeleton() {
  return (
    <div
      className="grid w-full grid-cols-[minmax(0,1fr)_auto] items-stretch gap-2 sm:flex sm:flex-row sm:flex-wrap sm:items-center xl:justify-end"
      aria-hidden="true"
    >
      <Skeleton className="col-span-2 h-9 sm:h-8 sm:min-w-56 sm:flex-1 xl:max-w-64" />
      <div className="grid min-w-0 grid-cols-2 gap-1 sm:flex">
        <Skeleton className="h-9 min-w-0 sm:h-8 sm:w-24" />
        <Skeleton className="h-9 min-w-0 sm:h-8 sm:w-28" />
      </div>
      <Skeleton className="h-9 w-[4.5rem] justify-self-end sm:ml-auto sm:h-8 sm:w-16 xl:ml-0" />
    </div>
  )
}

/**
 * 账号页唯一的工作区组件。真实页面和离线预览共同使用这棵组件树，避免概览、工具栏、
 * 列表与分页在两处独立演进后产生视觉和交互差异。
 */
export function CredentialWorkspace({ data, state, actions }: CredentialWorkspaceProps) {
  const { language, locale, t } = useI18n()
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
    sort,
    dir,
    view,
    selected,
    page,
    pageSize,
  } = state
  const pool = credentials ?? []
  const debouncedQuery = useDebounced(query)
  const searchRef = useRef<HTMLInputElement>(null)
  useSearchHotkey(searchRef)
  const now = useNowSeconds()
  // 实时指标单独轮询，10 秒一次：全局 RPM 与在途并发都是秒级变化的量，跟着账号列表那份
  // 30 秒的节奏走就成了「一直在看十几秒前的现场」。这个接口只有两条查询，拉得起。
  const metricsQuery = useQuery({ queryKey: ['metrics'], queryFn: getMetrics, refetchInterval: 10_000 })
  const cacheSeries = useCacheSeries(DEFAULT_CACHE_RANGE)
  const poolCache = aggregateCacheHitRate(cacheSeries.slots)
  const [cacheTrendOpen, setCacheTrendOpen] = useState(false)
  const ttftSeries = useTtftSeries(DEFAULT_TTFT_RANGE)
  const poolTtft = aggregateTtft(ttftSeries.slots)
  const [ttftTrendOpen, setTtftTrendOpen] = useState(false)
  const numberFormatter = useMemo(() => new Intl.NumberFormat(locale), [locale])
  const formatNumber = (value: number) => numberFormatter.format(value)
  const filterItems = useMemo(
    () => FILTERS.map((item) => ({ ...item, label: t(...item.label) })),
    [t],
  )
  const tierItems = useMemo(
    () => TIER_FILTERS.map((item) => ({ ...item, label: t(...item.label) })),
    [t],
  )
  const sortItems = useMemo(
    () => SORTS.map(({ key }) => ({ key, label: t(...SORT_LABELS[key]) })),
    [t],
  )
  const activeFilterLabel = filterItems.find((item) => item.key === filter)?.label
    ?? t(...FILTERS[0].label)
  const activeTierLabel = tierItems.find((item) => item.key === tier)?.label
    ?? t(...TIER_FILTERS[0].label)
  const activeSortLabel = sortItems.find((item) => item.key === sort)?.label
    ?? t(...SORT_LABELS.priority)
  const evaluatedPool = useMemo(
    () => pool.map((credential) => evaluateCredential(credential, now, language)),
    [pool, now, language],
  )

  const sorted = useMemo(() => {
    const match = FILTERS.find((item) => item.key === filter)?.match ?? (() => true)
    return sortCreds(
      evaluatedPool
        .filter((evaluation) => (
          match(evaluation)
          && (tier === 'all' || planKey(evaluation.credential.tier) === tier)
          && matchQuery(evaluation, debouncedQuery, language)
        ))
        .map((evaluation) => evaluation.credential),
      sort,
      dir,
      now,
      language,
    )
  }, [evaluatedPool, sort, dir, filter, tier, debouncedQuery, now, language])

  const metrics = useMemo(() => {
    const filterCounts: Record<CredentialFilterKey, number> = {
      all: 0,
      schedulable: 0,
      attention: 0,
      enabled: 0,
      disabled: 0,
      banned: 0,
      tokenInvalid: 0,
      nearLimit: 0,
      cooldown: 0,
      hasDevice: 0,
      deviceFull: 0,
    }
    // 与状态筛选那份一样，按**整池**统计而不是按当前可见的那一屏：下拉里的数字要回答
    // 「切过去能看到几个」，跟着当前筛选走的话每选一次数字就变一次，等于没有参考价值。
    const tierCounts: Record<CredentialTierFilterKey, number> = {
      all: 0,
      max20x: 0,
      max5x: 0,
      max: 0,
      pro: 0,
      free: 0,
      unknown: 0,
    }
    let rateLimitedPauseCount = 0
    let nearLimitCount = 0
    let activeOverageCount = 0
    let unknownOverageCount = 0
    let deviceCount = 0
    let deviceCapacity = 0
    let unlimitedDeviceAccounts = 0

    for (const evaluation of evaluatedPool) {
      const credential = evaluation.credential
      filterCounts.all += 1
      tierCounts.all += 1
      tierCounts[planKey(credential.tier)] += 1
      if (evaluation.schedulable) filterCounts.schedulable += 1
      if (evaluation.needsAttention) filterCounts.attention += 1
      if (!credential.disabled) filterCounts.enabled += 1
      else if (!credential.ban_reason && credential.resume_at == null) filterCounts.disabled += 1
      if (credential.resume_at != null) rateLimitedPauseCount += 1
      if (credential.ban_reason && credential.resume_at == null) {
        if (isAccountBan(credential.ban_reason)) filterCounts.banned += 1
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
      tierCounts,
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
  const enabledCount = metrics.filterCounts.enabled
  const schedulableCount = metrics.filterCounts.schedulable
  const cooldownCount = metrics.filterCounts.cooldown
  const attentionCount = metrics.filterCounts.attention
  const quotaRiskCount = metrics.filterCounts.nearLimit
  const fullDeviceCount = metrics.filterCounts.deviceFull
  const filtering = filter !== 'all' || tier !== 'all' || debouncedQuery.trim() !== ''
  const pageCount = Math.max(1, Math.ceil(total / pageSize))
  const current = Math.min(page, pageCount)
  const pageItems = sorted.slice((current - 1) * pageSize, current * pageSize)
  const bannedCount = metrics.filterCounts.banned
  const tokenInvalidCount = metrics.filterCounts.tokenInvalid
  const attentionStatus = [
    bannedCount > 0
      ? t(`${formatNumber(bannedCount)} 封禁`, `${formatNumber(bannedCount)} banned`)
      : '',
    tokenInvalidCount > 0
      ? t(`${formatNumber(tokenInvalidCount)} Token 失效`, `${formatNumber(tokenInvalidCount)} token expired`)
      : '',
    metrics.activeOverageCount > 0
      ? t(
          `${formatNumber(metrics.activeOverageCount)} 用 credits`,
          `${formatNumber(metrics.activeOverageCount)} on credits`,
        )
      : '',
    metrics.unknownOverageCount > 0
      ? t(
          `${formatNumber(metrics.unknownOverageCount)} credits 待确认`,
          `${formatNumber(metrics.unknownOverageCount)} credits unconfirmed`,
        )
      : '',
    cooldownCount > 0
      ? t(`${formatNumber(cooldownCount)} 冷却`, `${formatNumber(cooldownCount)} cooling down`)
      : '',
    metrics.nearLimitCount > 0
      ? t(`${formatNumber(metrics.nearLimitCount)} 将满`, `${formatNumber(metrics.nearLimitCount)} near limit`)
      : '',
  ].filter(Boolean).join(' · ') || undefined
  const quotaRiskStatus = [
    metrics.activeOverageCount > 0
      ? t(
          `${formatNumber(metrics.activeOverageCount)} 用 credits`,
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
  ].filter(Boolean).join(' · ') || undefined
  const deviceStatus = fullDeviceCount > 0
    ? t(
        `${formatNumber(fullDeviceCount)} 个账号已满`,
        `${formatNumber(fullDeviceCount)} ${fullDeviceCount === 1 ? 'account' : 'accounts'} at limit`,
      )
    : metrics.unlimitedDeviceAccounts > 0
      ? t(
          `${formatNumber(metrics.unlimitedDeviceAccounts)} 个不限额账号`,
          `${formatNumber(metrics.unlimitedDeviceAccounts)} unlimited ${metrics.unlimitedDeviceAccounts === 1 ? 'account' : 'accounts'}`,
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
  const selectMetric = (key: CredentialFilterKey) => changeFilter(filter === key ? 'all' : key)

  return (
    <div className="space-y-3 sm:space-y-4" data-slot="credential-workspace">
      <section
        className="overflow-hidden rounded-xl border bg-card shadow-xs/5"
        aria-labelledby="page-title"
      >
        <div
          className={cn(
            'grid gap-3 p-3 sm:p-4',
            (isLoading || count > 0) && 'xl:grid-cols-[auto_minmax(0,1fr)] xl:items-center',
          )}
        >
          <div className="flex min-w-0 items-center justify-between gap-3 xl:justify-start">
            <div className="flex min-w-0 items-center gap-2.5">
              <h1 id="page-title" className="min-w-0 text-lg font-semibold tracking-tight">
                {t('账号池', 'Account pool')}
              </h1>
              {!isLoading && (
                <span className="shrink-0 rounded-md bg-muted px-2 py-1 text-2xs font-medium text-muted-foreground">
                  {t(
                    `${formatNumber(count)} 个账号`,
                    `${formatNumber(count)} ${count === 1 ? 'account' : 'accounts'}`,
                  )}
                </span>
              )}
              {/* 绑定设备数从概览格挪到这里：概览那一行留给「号的状态」与「流量质量」，设备数
                  是池子的容量属性，与账号数并排读更顺。点击仍是筛选（已满 > 已绑定）。 */}
              {!isLoading && count > 0 && (
                <button
                  type="button"
                  className={cn(
                    'inline-flex shrink-0 items-center gap-1 rounded-md bg-muted px-2 py-1 text-2xs font-medium text-muted-foreground transition-colors',
                    'hover:bg-muted/72 hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/60',
                    (filter === 'deviceFull' || filter === 'hasDevice') && 'bg-foreground/8 text-foreground',
                    fullDeviceCount > 0 && 'text-warning-foreground',
                  )}
                  title={deviceStatus}
                  aria-pressed={filter === 'deviceFull' || filter === 'hasDevice'}
                  onClick={() => selectMetric(fullDeviceCount > 0 ? 'deviceFull' : 'hasDevice')}
                >
                  <SmartphoneIcon className="size-3" aria-hidden />
                  <span className="tnum">
                    {metrics.deviceCapacity > 0 && metrics.unlimitedDeviceAccounts === 0
                      ? `${formatNumber(metrics.deviceCount)}/${formatNumber(metrics.deviceCapacity)}`
                      : formatNumber(metrics.deviceCount)}
                  </span>
                  <span>{t('台设备', metrics.deviceCount === 1 ? 'device' : 'devices')}</span>
                </button>
              )}
            </div>
            <div
              className="flex shrink-0 items-center gap-1.5 text-2xs text-muted-foreground"
              aria-live="polite"
              aria-atomic="true"
            >
              {isRefetchError ? (
                <>
                  <TriangleAlertIcon className="size-3.5 text-destructive-foreground" aria-hidden />
                  <button
                    type="button"
                    className="rounded-sm font-medium text-destructive-foreground underline-offset-2 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/60"
                    onClick={actions.onRetry}
                  >
                    {t('刷新失败，重试', 'Refresh failed. Retry')}
                  </button>
                </>
              ) : (
                // 自动刷新指示器同时是手动刷新入口：等下一轮 30 秒才能确认操作结果，
                // 是这类常驻列表最常见的抱怨，而这块本来就在讲「数据有多新」。
                <button
                  type="button"
                  className="inline-flex items-center gap-1.5 rounded-sm px-1 py-0.5 transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/60 disabled:hover:text-muted-foreground"
                  onClick={actions.onRetry}
                  disabled={isLoading || isFetching}
                  title={isLoading
                    ? t('正在加载账号数据', 'Loading account data')
                    : t('每 30 秒自动刷新，点击立即刷新', 'Refreshes automatically every 30 seconds. Click to refresh now')}
                  aria-label={t('立即刷新账号数据', 'Refresh account data now')}
                >
                  <span className="flex size-3.5 shrink-0 items-center justify-center" aria-hidden>
                    {isLoading || isFetching ? (
                      <RefreshCwIcon className="size-3.5 animate-spin" />
                    ) : (
                      <span className="size-1.5 rounded-full bg-success" />
                    )}
                  </span>
                  <span className="min-w-14 text-left">
                    {isLoading ? t('正在加载', 'Loading') : t('30 秒刷新', '30s refresh')}
                  </span>
                </button>
              )}
            </div>
          </div>

          {isLoading ? (
            <WorkspaceToolbarSkeleton />
          ) : count > 0 && (
            <Toolbar className="grid w-full grid-cols-[minmax(0,1fr)_auto] items-stretch gap-2 border-0 bg-transparent p-0 sm:flex sm:flex-row sm:flex-wrap sm:items-center xl:justify-end">
              <InputGroup className="col-span-2 sm:min-w-56 sm:flex-1 xl:max-w-64">
                <InputGroupAddon><SearchIcon /></InputGroupAddon>
                <InputGroupInput
                  ref={searchRef}
                  value={query}
                  onChange={(event) => changeQuery(event.target.value)}
                  onKeyDown={(event) => {
                    // Esc 先清空、再退出输入框：清空和失焦是两个不同的意图，一次按键只做一件。
                    if (event.key !== 'Escape') return
                    event.preventDefault()
                    if (query) changeQuery('')
                    else event.currentTarget.blur()
                  }}
                  placeholder={t('搜索名称、#id、套餐或状态', 'Search name, #id, plan or status')}
                  aria-label={t('搜索账号', 'Search accounts')}
                />
                <InputGroupAddon align="inline-end">
                  {query ? (
                    <Button
                      size="icon-xs"
                      variant="ghost"
                      onClick={() => changeQuery('')}
                      aria-label={t('清除搜索', 'Clear search')}
                    >
                      <XIcon />
                    </Button>
                  ) : (
                    // 只在指针设备上提示：触屏没有物理按键，画个 kbd 只是噪声。
                    <kbd
                      className="pointer-events-none hidden rounded border bg-muted px-1 font-sans text-2xs text-muted-foreground pointer-fine:inline-block"
                      aria-hidden
                    >
                      /
                    </kbd>
                  )}
                </InputGroupAddon>
              </InputGroup>

              <ToolbarSeparator orientation="vertical" className="hidden sm:block" />
              <ToolbarGroup className="grid min-w-0 grid-cols-2 sm:flex sm:flex-wrap">
                <Menu>
                  <MenuTrigger
                    aria-label={t(`筛选：${activeFilterLabel}`, `Filter: ${activeFilterLabel}`)}
                    className={cn(
                      buttonVariants({ variant: 'outline' }),
                      'w-full min-w-0 justify-between max-sm:[&_svg]:hidden sm:w-auto',
                      filter !== 'all' && ACTIVE_FILTER_CLASS,
                    )}
                  >
                    <ListFilterIcon />
                    <span className="min-w-0 truncate">
                      {activeFilterLabel}
                    </span>
                  </MenuTrigger>
                  <MenuPopup align="end" className="w-52">
                    <MenuRadioGroup value={filter}>
                      {filterItems.map((item) => (
                        <MenuRadioItem key={item.key} value={item.key} onClick={() => changeFilter(item.key)}>
                          <span className="flex min-w-0 flex-1 items-center justify-between gap-4">
                            <span>{item.label}</span>
                            <span className="tnum text-xs text-muted-foreground">
                              {formatNumber(metrics.filterCounts[item.key])}
                            </span>
                          </span>
                        </MenuRadioItem>
                      ))}
                    </MenuRadioGroup>
                  </MenuPopup>
                </Menu>

                <Menu>
                  <MenuTrigger
                    aria-label={t(`套餐：${activeTierLabel}`, `Plan: ${activeTierLabel}`)}
                    className={cn(
                      buttonVariants({ variant: 'outline' }),
                      'w-full min-w-0 justify-between max-sm:[&_svg]:hidden sm:w-auto',
                      tier !== 'all' && ACTIVE_FILTER_CLASS,
                    )}
                  >
                    <LayersIcon />
                    <span className="min-w-0 truncate">
                      {activeTierLabel}
                    </span>
                  </MenuTrigger>
                  <MenuPopup align="end" className="w-52">
                    <MenuRadioGroup value={tier}>
                      {tierItems.map((item) => (
                        <MenuRadioItem key={item.key} value={item.key} onClick={() => changeTier(item.key)}>
                          <span className="flex min-w-0 flex-1 items-center justify-between gap-4">
                            <span className="min-w-0 truncate">{item.label}</span>
                            <span className="tnum text-xs text-muted-foreground">
                              {formatNumber(metrics.tierCounts[item.key])}
                            </span>
                          </span>
                        </MenuRadioItem>
                      ))}
                    </MenuRadioGroup>
                  </MenuPopup>
                </Menu>

                <Menu>
                  <MenuTrigger
                    aria-label={t(
                      `排序：${activeSortLabel}，${dir === 'asc' ? '升序' : '降序'}`,
                      `Sort by ${activeSortLabel}, ${dir === 'asc' ? 'ascending' : 'descending'}`,
                    )}
                    className={cn(
                      buttonVariants({ variant: 'outline' }),
                      'w-full min-w-0 justify-between max-sm:col-span-2 max-sm:[&_svg]:hidden sm:w-auto',
                    )}
                  >
                    <ArrowUpDownIcon />
                    <span className="min-w-0 truncate max-[22rem]:hidden">
                      {activeSortLabel} {dir === 'asc' ? '↑' : '↓'}
                    </span>
                    <span className="hidden shrink-0 max-[22rem]:inline">
                      {t('排序', 'Sort')} {dir === 'asc' ? '↑' : '↓'}
                    </span>
                  </MenuTrigger>
                  <MenuPopup align="end" className="w-48">
                    <MenuRadioGroup value={sort}>
                      {sortItems.map((item) => (
                        <MenuRadioItem key={item.key} value={item.key} onClick={() => changeSort(item.key)}>
                          <span className="flex min-w-0 flex-1 items-center justify-between gap-4">
                            <span>{item.label}</span>
                            {sort === item.key && (
                              <span className="text-xs text-muted-foreground">
                                {dir === 'asc'
                                  ? t('升序', 'Ascending')
                                  : t('降序', 'Descending')}
                              </span>
                            )}
                          </span>
                        </MenuRadioItem>
                      ))}
                    </MenuRadioGroup>
                  </MenuPopup>
                </Menu>
              </ToolbarGroup>

              <ToolbarSeparator orientation="vertical" className="hidden sm:ml-auto sm:block xl:ml-0" />
              <ToolbarGroup className="self-center justify-end">
                <ToggleGroup
                  value={[view]}
                  onValueChange={(values) => {
                    const next = values[values.length - 1]
                    if (next === 'card' || next === 'list') actions.onViewChange(next)
                  }}
                  variant="outline"
                  aria-label={t('账号视图', 'Account view')}
                >
                  <ToggleGroupItem
                    value="card"
                    aria-label={t('卡片视图', 'Card view')}
                    title={t('卡片视图', 'Card view')}
                  >
                    <LayoutGridIcon />
                  </ToggleGroupItem>
                  <ToggleGroupSeparator />
                  <ToggleGroupItem
                    value="list"
                    aria-label={t('列表视图', 'List view')}
                    title={t('列表视图', 'List view')}
                  >
                    <ListIcon />
                  </ToggleGroupItem>
                </ToggleGroup>
              </ToolbarGroup>
            </Toolbar>
          )}
        </div>

        {isLoading ? (
          <section
            aria-label={t('正在加载账号池概览', 'Loading account pool overview')}
            className="grid grid-cols-2 border-t lg:grid-cols-6"
          >
            <OverviewMetricSkeleton className="border-r border-b lg:border-b-0" />
            <OverviewMetricSkeleton className="border-b lg:border-r lg:border-b-0" />
            <OverviewMetricSkeleton className="border-r border-b lg:border-b-0" />
            <OverviewMetricSkeleton className="border-b lg:border-r lg:border-b-0" />
            <OverviewMetricSkeleton className="col-span-2 border-b lg:col-span-1 lg:border-r lg:border-b-0" />
            <OverviewMetricSkeleton className="col-span-2 lg:col-span-1" />
          </section>
        ) : count > 0 && (
          <section
            aria-label={t('账号池概览', 'Account pool overview')}
            className="grid grid-cols-2 border-t lg:grid-cols-6"
          >
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
              icon={TriangleAlertIcon}
              tone={bannedCount > 0 || metrics.activeOverageCount > 0
                ? 'bad'
                : attentionCount > 0
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
              icon={RadioIcon}
              tone={metrics.activeOverageCount > 0 ? 'bad' : quotaRiskCount > 0 ? 'warn' : 'neutral'}
              active={filter === 'nearLimit'}
              onClick={() => selectMetric('nearLimit')}
            />
            {/* 缓存命中率与首字时延不来自账号列表，点开是趋势而不是筛选——它们讲的是「转发出去的
                请求质量如何」。摆在实时流量左边：三格都是流量的属性，凑在一起读。 */}
            <OverviewMetric
              className="border-b lg:border-r lg:border-b-0"
              label={t('缓存命中率 · 7d', 'Cache hit rate · 7d')}
              value={formatPercent(poolCache.rate)}
              trend={
                poolCache.rate == null ? undefined : (
                  <CacheHitSparkline slots={cacheSeries.slots} className="shrink-0" />
                )
              }
              status={poolCache.rate == null ? t('暂无用量', 'No usage yet') : undefined}
              statusHint={poolCache.rate == null
                ? undefined
                : t(
                    `${cacheTotalsText(poolCache.cachedTokens, poolCache.inputTokens, t)}（按 token 加权，不是各账号命中率的平均）。点开看趋势。`,
                    `${cacheTotalsText(poolCache.cachedTokens, poolCache.inputTokens, t)} (token-weighted, not an average of per-account rates). Click for the trend.`,
                  )}
              icon={DatabaseZapIcon}
              tone={poolCache.rate == null ? 'neutral' : poolCache.rate >= 0.5 ? 'ok' : 'warn'}
              onClick={() => setCacheTrendOpen(true)}
            />
            <OverviewMetric
              className="col-span-2 border-b lg:col-span-1 lg:border-r lg:border-b-0"
              label={t('首字时延 · 7d', 'TTFT · 7d')}
              value={formatMs(poolTtft.avgMs)}
              trend={
                poolTtft.avgMs == null ? undefined : (
                  <TtftSparkline slots={ttftSeries.slots} className="shrink-0" />
                )
              }
              status={poolTtft.avgMs == null ? t('暂无数据', 'No data yet') : undefined}
              statusHint={poolTtft.avgMs == null
                ? undefined
                : t(
                    `近 7 天成功请求的平均首字时延，共 ${formatNumber(poolTtft.totalCount)} 次请求。点开看趋势。`,
                    `Average time to first token over the last 7 days, across ${formatNumber(poolTtft.totalCount)} successful requests. Click for the trend.`,
                  )}
              icon={TimerIcon}
              tone={poolTtft.avgMs == null ? 'neutral' : poolTtft.avgMs <= 3000 ? 'ok' : poolTtft.avgMs <= 8000 ? 'warn' : 'bad'}
              onClick={() => setTtftTrendOpen(true)}
            />
            <LiveTrafficMetric
              className="col-span-2 lg:col-span-1"
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
              hint={t(
                `全池实时流量：最近 ${metricsQuery.data?.window_secs ?? 60} 秒转发的请求总数（各账号 RPM 之和），以及此刻已进入转发、响应还没走完的在途请求数。每 10 秒刷新。`,
                `Live traffic across the pool: requests forwarded in the last ${metricsQuery.data?.window_secs ?? 60} seconds (the sum of every account's RPM), plus the requests in flight right now — accepted for forwarding but not finished responding. Refreshed every 10 seconds.`,
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

          {count > 0 && selected.size > 0 && (
            <div className="relative">
              <BatchActionsBar
                all={sorted}
                selected={selected}
                onSelectedChange={actions.onSelectedChange}
                onClear={clearSelection}
              />
            </div>
          )}

          {isLoading ? (
            <div className="relative">
              <CredentialLoadingState view={view} selectable count={pageSize} />
            </div>
          ) : isError && !credentials ? (
            <Card><ErrorState error={error} onRetry={actions.onRetry} /></Card>
          ) : count === 0 ? (
            <Card><EmptyState onAdd={actions.onAdd} /></Card>
          ) : total === 0 ? (
            <Card>
              <Empty>
                <EmptyHeader>
                  <EmptyMedia variant="icon"><SearchIcon /></EmptyMedia>
                  <EmptyTitle>{t('没有符合条件的账号', 'No matching accounts')}</EmptyTitle>
                  <EmptyDescription>
                    {t(
                      '尝试清除当前筛选条件或搜索关键字。',
                      'Try clearing the current filters or search terms.',
                    )}
                  </EmptyDescription>
                </EmptyHeader>
                <EmptyContent>
                  <Button
                    variant="outline"
                    onClick={() => {
                      actions.onQueryChange('')
                      changeFilter('all')
                      changeTier('all')
                    }}
                  >
                    {t('清除筛选与搜索', 'Clear filters and search')}
                  </Button>
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
                selectable
                sort={sort}
                dir={dir}
                onSortChange={changeSort}
                allSelected={sorted.length > 0 && sorted.every((item) => selected.has(item.id))}
                onSelectAll={(checked) => actions.onSelectedChange(
                  checked ? new Set(sorted.map((item) => item.id)) : new Set(),
                )}
              />
              <TableBody>
                {pageItems.map((item) => (
                  <CredentialRow
                    key={item.id}
                    cred={item}
                    now={now}
                    selectable
                    selected={selected.has(item.id)}
                    onSelectedChange={toggleSelected}
                  />
                ))}
              </TableBody>
            </Table>
          ) : (
            <ul className="relative grid list-none items-stretch gap-3 p-0 [grid-template-columns:repeat(auto-fill,minmax(min(100%,27rem),1fr))] sm:gap-4">
              {pageItems.map((item) => (
                <CredentialCard
                  key={item.id}
                  cred={item}
                  now={now}
                  selectable
                  selected={selected.has(item.id)}
                  onSelectedChange={toggleSelected}
                />
              ))}
            </ul>
          )}

          {!isLoading && pageCount > 1 && (
            <div className="relative py-2">
              <AccountPagination
                total={total}
                page={current}
                pageCount={pageCount}
                pageSize={pageSize}
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

function AccountPagination({
  total,
  page,
  pageCount,
  pageSize,
  onPageChange,
  onPageSizeChange,
}: {
  total: number
  page: number
  pageCount: number
  pageSize: CredentialPageSize
  onPageChange: (page: number) => void
  onPageSizeChange: (pageSize: CredentialPageSize) => void
}) {
  const { locale, t } = useI18n()
  const numberFormatter = useMemo(() => new Intl.NumberFormat(locale), [locale])
  const formatNumber = (value: number) => numberFormatter.format(value)
  const pageSizeItems = useMemo(
    () => PAGE_SIZE_ITEMS.map(({ size, value }) => ({
      value,
      label: t(`${numberFormatter.format(size)} 个`, `${numberFormatter.format(size)} items`),
    })),
    [numberFormatter, t],
  )
  const from = (page - 1) * pageSize + 1
  const to = Math.min(page * pageSize, total)
  const start = Math.max(1, Math.min(page - 2, pageCount - 4))
  const pages = Array.from({ length: Math.min(5, pageCount) }, (_, index) => start + index)
  const navigate = (event: React.MouseEvent<HTMLAnchorElement>, next: number) => {
    event.preventDefault()
    if (next >= 1 && next <= pageCount) onPageChange(next)
  }

  return (
    <div className="grid grid-cols-[1fr_auto] items-center gap-3 text-xs text-muted-foreground md:grid-cols-[1fr_auto_1fr]">
      <span className="min-w-0">
        <span className="sm:hidden">
          <span className="tnum text-foreground">{formatNumber(from)}–{formatNumber(to)}</span>
          {' / '}
          <span className="tnum text-foreground">{formatNumber(total)}</span>
        </span>
        <span className="hidden sm:inline">
          {t('第 ', 'Showing ')}
          <span className="tnum text-foreground">{formatNumber(from)}–{formatNumber(to)}</span>
          {t(' 个，共 ', ' of ')}
          <span className="tnum text-foreground">{formatNumber(total)}</span>
          {t(' 个账号', ` ${total === 1 ? 'account' : 'accounts'}`)}
        </span>
      </span>
      <CossPagination className="col-span-2 row-start-2 justify-center md:col-span-1 md:col-start-2 md:row-start-1">
        <PaginationContent>
          <PaginationItem>
            <PaginationLink
              href="#"
              size="icon-sm"
              className={cn(page <= 1 && 'pointer-events-none opacity-50')}
              aria-disabled={page <= 1}
              aria-label={t('上一页', 'Previous page')}
              onClick={(event) => navigate(event, page - 1)}
            >
              <ChevronLeftIcon />
            </PaginationLink>
          </PaginationItem>
          {pages.map((item) => (
            <PaginationItem key={item} className="max-sm:hidden">
              <PaginationLink
                href="#"
                size="icon-sm"
                isActive={item === page}
                aria-label={t(
                  `第 ${formatNumber(item)} 页`,
                  `Page ${formatNumber(item)}`,
                )}
                onClick={(event) => navigate(event, item)}
              >
                <span className="tnum">{formatNumber(item)}</span>
              </PaginationLink>
            </PaginationItem>
          ))}
          <PaginationItem className="sm:hidden">
            <span className="tnum px-2 text-foreground">
              {formatNumber(page)} / {formatNumber(pageCount)}
            </span>
          </PaginationItem>
          <PaginationItem>
            <PaginationLink
              href="#"
              size="icon-sm"
              className={cn(page >= pageCount && 'pointer-events-none opacity-50')}
              aria-disabled={page >= pageCount}
              aria-label={t('下一页', 'Next page')}
              onClick={(event) => navigate(event, page + 1)}
            >
              <ChevronRightIcon />
            </PaginationLink>
          </PaginationItem>
        </PaginationContent>
      </CossPagination>
      <div className="row-start-1 flex items-center gap-2 justify-self-end md:col-start-3">
        <span className="max-sm:sr-only">{t('每页', 'Per page')}</span>
        <Select
          items={pageSizeItems}
          value={String(pageSize)}
          onValueChange={(value) => {
            const next = Number(value)
            if (CREDENTIAL_PAGE_SIZES.includes(next as CredentialPageSize)) {
              onPageSizeChange(next as CredentialPageSize)
            }
          }}
        >
          <SelectTrigger
            aria-label={t('每页账号数', 'Accounts per page')}
            size="sm"
            className="min-w-20"
          >
            <SelectValue />
          </SelectTrigger>
          <SelectPopup align="end">
            {pageSizeItems.map((item) => (
              <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>
            ))}
          </SelectPopup>
        </Select>
      </div>
    </div>
  )
}

function EmptyState({ onAdd }: { onAdd: () => void }) {
  const { t } = useI18n()
  return (
    <Empty>
      <EmptyHeader>
        <EmptyMedia variant="icon"><PlusIcon /></EmptyMedia>
        <EmptyTitle>{t('建立第一个调度账号', 'Add your first schedulable account')}</EmptyTitle>
        <EmptyDescription>
          {t(
            '完成 Claude OAuth 授权后，账号会加入当前网关的调度池。',
            'After Claude OAuth authorization, the account joins this gateway’s scheduling pool.',
          )}
        </EmptyDescription>
      </EmptyHeader>
      <EmptyContent>
        <Button onClick={onAdd}>
          <PlusIcon />
          {t('添加第一个账号', 'Add first account')}
        </Button>
      </EmptyContent>
    </Empty>
  )
}

function ErrorState({ error, onRetry }: { error: unknown; onRetry: () => void }) {
  const { language, t } = useI18n()
  return (
    <Empty role="alert">
      <EmptyHeader>
        <EmptyMedia variant="icon"><TriangleAlertIcon /></EmptyMedia>
        <EmptyTitle>{t('暂时无法读取账号', 'Unable to load accounts')}</EmptyTitle>
        <EmptyDescription className="break-words">{extractError(error, language)}</EmptyDescription>
      </EmptyHeader>
      <EmptyContent>
        <Button variant="outline" onClick={onRetry}>
          <RefreshCwIcon />
          {t('重新加载', 'Reload')}
        </Button>
      </EmptyContent>
    </Empty>
  )
}
