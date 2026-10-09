import { useMemo, useState } from 'react'
import { keepPreviousData, useQuery } from '@tanstack/react-query'
import {
  ActivityIcon,
  CalendarIcon,
  ChartBarIcon,
  ChevronDownIcon,
  ChevronUpIcon,
  DatabaseZapIcon,
  ReceiptIcon,
  RefreshCwIcon,
  SearchIcon,
  SparklesIcon,
  XIcon,
} from 'lucide-react'
import { getBilling, type BillingDim, type BillingRow } from '@/api/billing'
import { useI18n } from '@/lib/i18n'
import { useMe } from '@/lib/role'
import { useDocumentTitle } from '@/lib/use-document-title'
import { cacheHitRate, cn, formatPercent, formatTokens, formatUsd } from '@/lib/utils'
import { AppFooter } from '@/components/app-footer'
import { AccountMenu, AppHeader, MainNav, type MainSection } from '@/components/app-header'
import { OverviewMetric, OverviewMetricSkeleton } from '@/components/overview-metric'
import { ErrorState } from '@/components/state-placeholders'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
import { Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from '@/components/ui/empty'
import { Meter, MeterIndicator, MeterTrack } from '@/components/ui/meter'
import { Skeleton } from '@/components/ui/skeleton'
import { Table, TableBody, TableCaption, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { Toolbar } from '@/components/ui/toolbar'
import { ToolbarMenuSelect, ToolbarSearch } from '@/components/toolbar-controls'
import { Hint } from '@/components/ui/tooltip'

/**
 * 列宽预算（表格 `table-fixed`，名称列吃掉剩余宽度），口径同账号池列表的 COL：每格内边距
 * p-2.5，可用内容宽 = 列宽 − 20px。占比单独一列、条与百分比都定宽，各行的条才对得齐。
 * token 四列只在 lg 起显示，窄屏留名称、请求、占比与费用。
 */
const COL = {
  name: 'w-auto',
  requests: 'w-24',
  tokens: 'hidden w-24 lg:table-cell',
  share: 'hidden w-40 sm:table-cell',
  cost: 'w-28',
} as const

type SortKey = 'cost' | 'requests' | 'input' | 'output' | 'cache_write' | 'cache_read' | 'name'
type SortDir = 'asc' | 'desc'

type RangeKey = 'today' | '7d' | '30d' | 'month' | 'last-month'
const RANGE_KEYS: readonly RangeKey[] = ['today', '7d', '30d', 'month', 'last-month']

/** 时间范围（本地时区）：`[from, to)` 的 Unix 秒，以及其中已经过去的天数（算日均用）。 */
function rangeOf(key: RangeKey): { from: number; to: number; days: number } {
  const now = new Date()
  const secs = (d: Date) => Math.floor(d.getTime() / 1000)
  const today = new Date(now.getFullYear(), now.getMonth(), now.getDate())
  const tomorrow = secs(today) + 86400
  const span = (from: number, to: number) => ({ from, to, days: Math.max(1, Math.round((Math.min(to, tomorrow) - from) / 86400)) })
  switch (key) {
    case 'today':
      return span(secs(today), tomorrow)
    case '7d':
      return span(tomorrow - 7 * 86400, tomorrow)
    case '30d':
      return span(tomorrow - 30 * 86400, tomorrow)
    case 'month':
      return span(secs(new Date(now.getFullYear(), now.getMonth(), 1)), tomorrow)
    case 'last-month':
      return span(
        secs(new Date(now.getFullYear(), now.getMonth() - 1, 1)),
        secs(new Date(now.getFullYear(), now.getMonth(), 1)),
      )
  }
}

/** 正在查看的那个人（从「按人」下钻）。 */
interface Focus {
  id: number
  name: string
}

/**
 * 费用：只计费、不扣费，看号跑出了多少等价 API 费用。版式与账号池一致：页头卡片里是标题、
 * 工具条与概览指标条（复用账号池的 [OverviewMetric]），下面是同款卡片表格。
 *
 * 可看的范围由后端按身份收窄，这里只决定给哪些拆分方式：
 * - 管理员 / 访客：按人、号、模型、接入 Key、分组、日；点某个人下钻看他一个人的；
 * - 代理：默认按人看自己与名下用户；点自己能拆到号、模型、分组；点下属只看按日走势
 *   （代理看不到下属的号）；
 * - 用户：只看自己，按号、模型、分组、日。
 */
export function BillingPage({
  onNavigate,
  onSignOut,
}: {
  onNavigate: (section: MainSection) => void
  onSignOut: () => void
}) {
  const { t, locale } = useI18n()
  const me = useMe().data
  const role = me?.role
  const [range, setRange] = useState<RangeKey>('month')
  const [focus, setFocus] = useState<Focus | null>(null)
  const [dim, setDim] = useState<BillingDim | null>(null)
  const [search, setSearch] = useState('')
  const [sort, setSort] = useState<{ key: SortKey; dir: SortDir } | null>(null)

  useDocumentTitle(`${t('费用', 'Billing')} · Luban`)

  // 当前身份、当前下钻下能用哪些拆分方式。
  const dims: BillingDim[] = useMemo(() => {
    if (role === 'admin' || role === 'viewer') {
      return focus ? ['cred', 'model', 'group', 'key', 'day'] : ['owner', 'cred', 'model', 'key', 'group', 'day']
    }
    if (role === 'agent') {
      if (!focus) return ['owner', 'day']
      return focus.id === me?.id ? ['cred', 'model', 'group', 'day'] : ['day']
    }
    return ['cred', 'model', 'group', 'day']
  }, [role, focus, me?.id])
  const activeDim = dim && dims.includes(dim) ? dim : dims[0]
  const { from, to, days } = rangeOf(range)

  const query = useQuery({
    queryKey: ['billing', from, to, activeDim, focus?.id ?? null],
    queryFn: () => getBilling({ from, to, by: activeDim, owner_id: focus?.id }),
    enabled: !!role,
    // 只在同一拆分方式、同一下钻对象下沿用上一份数据当占位（换时间范围时不闪空）：换了维度
    // 还拿旧数据的话，模型名会被当成日期去格式化。
    placeholderData: (previous, previousQuery) => {
      const key = previousQuery?.queryKey
      return key && key[3] === activeDim && key[4] === (focus?.id ?? null) ? keepPreviousData(previous) : undefined
    },
    refetchInterval: 60_000,
  })
  const allRows = query.data?.rows ?? []
  const total = query.data?.total
  const number = (n: number) => n.toLocaleString(locale)

  const dimLabel: Record<BillingDim, string> = {
    owner: t('按人', 'By person'),
    cred: t('按号', 'By account'),
    model: t('按模型', 'By model'),
    key: t('按接入 Key', 'By access key'),
    group: t('按分组', 'By group'),
    day: t('按日', 'By day'),
  }
  const columnLabel: Record<BillingDim, string> = {
    owner: t('人', 'Person'),
    cred: t('号', 'Account'),
    model: t('模型', 'Model'),
    key: t('接入 Key', 'Access key'),
    group: t('分组', 'Group'),
    day: t('日期', 'Date'),
  }
  const rangeLabel: Record<RangeKey, string> = {
    today: t('今天', 'Today'),
    '7d': t('近 7 天', 'Last 7 days'),
    '30d': t('近 30 天', 'Last 30 days'),
    month: t('本月', 'This month'),
    'last-month': t('上月', 'Last month'),
  }
  const rangeItems = RANGE_KEYS.map((key) => ({ value: key, label: rangeLabel[key] }))
  const dimItems = dims.map((d) => ({ value: d, label: dimLabel[d] }))
  const dayFormatter = useMemo(
    () => new Intl.DateTimeFormat(locale, { month: '2-digit', day: '2-digit', weekday: 'short' }),
    [locale],
  )
  const nameOf = (row: BillingRow): string => {
    switch (activeDim) {
      case 'owner':
        return row.key === '0' ? t('未归属', 'Unassigned') : (row.label ?? t(`已删除的账号 #${row.key}`, `Deleted account #${row.key}`))
      case 'cred':
        return row.label ?? t(`已删除的号 #${row.key}`, `Deleted account #${row.key}`)
      case 'key':
        return row.key === '0'
          ? t('环境变量 / 未配置 Key', 'Environment / no key')
          : (row.label || t(`已删除的 Key #${row.key}`, `Deleted key #${row.key}`))
      case 'group':
        return row.key === '0' ? t('无分组', 'No group') : (row.label ?? t(`已删除的分组 #${row.key}`, `Deleted group #${row.key}`))
      case 'model':
        return row.key || t('未知模型', 'Unknown model')
      case 'day': {
        const date = new Date(Number(row.key) * 1000)
        return Number.isNaN(date.getTime()) ? row.key : dayFormatter.format(date)
      }
    }
  }
  // 排序：缺省按费用从高到低；按日拆时缺省按日期先后（「名称」那一列就是日期）。
  const effectiveSort = sort ?? (activeDim === 'day' ? { key: 'name' as const, dir: 'asc' as const } : { key: 'cost' as const, dir: 'desc' as const })
  const sortValue = (row: BillingRow, key: SortKey): number | string => {
    switch (key) {
      case 'cost': return row.cost_usd
      case 'requests': return row.requests
      case 'input': return row.input_tokens
      case 'output': return row.output_tokens
      case 'cache_write': return row.cache_write_tokens
      case 'cache_read': return row.cache_read_tokens
      case 'name': return activeDim === 'day' ? Number(row.key) : nameOf(row).toLowerCase()
    }
  }
  const needle = search.trim().toLowerCase()
  const rows = allRows
    .filter((row) => !needle || nameOf(row).toLowerCase().includes(needle) || row.key.toLowerCase().includes(needle))
    .sort((a, b) => {
      const [x, y] = [sortValue(a, effectiveSort.key), sortValue(b, effectiveSort.key)]
      const cmp = x < y ? -1 : x > y ? 1 : 0
      return effectiveSort.dir === 'asc' ? cmp : -cmp
    })
  const changeSort = (key: SortKey) => setSort((current) => {
    const active = current ?? effectiveSort
    if (active.key === key) return { key, dir: active.dir === 'asc' ? 'desc' : 'asc' }
    return { key, dir: key === 'name' ? 'asc' : 'desc' }
  })
  // 表头：与账号池列表同款的小号大写标签，点一下按这一列排序，再点反向。数值列右对齐。
  const sortHead = (label: string, key: SortKey, className: string, numeric = true) => {
    const active = effectiveSort.key === key
    const Arrow = active && effectiveSort.dir === 'asc' ? ChevronUpIcon : ChevronDownIcon
    return (
      <TableHead
        aria-sort={active ? (effectiveSort.dir === 'asc' ? 'ascending' : 'descending') : undefined}
        className={className}
      >
        <Hint label={t(`按${label}排序`, `Sort by ${label}`)}>
          <Button
            className={cn(
              'w-full px-0 text-2xs font-semibold uppercase tracking-[0.06em] sm:text-2xs',
              numeric ? 'justify-end text-right' : 'justify-start text-left',
            )}
            size="xs"
            type="button"
            variant="ghost"
            onClick={() => changeSort(key)}
          >
            {/* 右对齐的数值列把箭头放在标签左边：没激活时箭头不可见但仍占位，放右边会把标签
                顶离右边缘，与下面右对齐的数字错开一截。 */}
            {numeric && <Arrow className={cn(!active && 'opacity-0')} />}
            {label}
            {!numeric && <Arrow className={cn(!active && 'opacity-0')} />}
          </Button>
        </Hint>
      </TableHead>
    )
  }
  // 名称下面那行小字：有 id 的维度标出 #id，再带请求数与单次均价。
  const subtitle = (row: BillingRow) => {
    const parts: string[] = []
    if (activeDim !== 'model' && activeDim !== 'day' && row.key !== '0') parts.push(`#${row.key}`)
    parts.push(t(`${number(row.requests)} 次请求`, `${number(row.requests)} requests`))
    if (row.requests > 0) parts.push(t(`均价 ${formatUsd(row.cost_usd / row.requests)}`, `${formatUsd(row.cost_usd / row.requests)} each`))
    return parts.join(' · ')
  }

  // 「按人」那一表点一行下钻：管理员 / 访客谁都能点，代理能点自己与下属。
  const canDrill = activeDim === 'owner' && !focus
  const drill = (row: BillingRow) => {
    if (!canDrill || row.key === '0') return
    setFocus({ id: Number(row.key), name: nameOf(row) })
    setDim(null)
    setSort(null)
    setSearch('')
  }

  // 概览：等价费用（日均）、请求数（单次均价）、输出 token（输入）、缓存命中率。
  const totalInput = total ? total.input_tokens + total.cache_write_tokens + total.cache_read_tokens : 0
  const hitRate = total ? cacheHitRate(totalInput, total.cache_read_tokens) : null
  const description = role === 'agent'
    ? t('按官方 API 价格折算的等价费用。你和名下用户的费用汇总在这里，下属用户只显示到人。', 'Equivalent cost at official API prices for you and your users; your users are shown per person only.')
    : role === 'user'
      ? t('按官方 API 价格折算的等价费用，统计你名下的号。', 'Equivalent cost at official API prices for the accounts you added.')
      : t('按官方 API 价格折算的等价费用。只计费，不扣费。', 'Equivalent cost at official API prices. Metered only; nothing is charged.')

  return (
    <div className="app-shell flex min-h-dvh flex-col text-foreground">
      <AppHeader
        actions={<AccountMenu onSignOut={onSignOut} />}
        nav={<MainNav current="billing" onNavigate={onNavigate} />}
        onNavigateHome={() => onNavigate('pool')}
      />

      <main className="page-frame relative flex-1 py-4 pb-8 sm:py-5 sm:pb-10">
        <div className="space-y-3 sm:space-y-4">
          {/* 页头卡片：与账号池同构——标题与工具条在宽屏上合成一行，概览指标条贴在卡片底部。 */}
          <section aria-labelledby="billing-title" className="overflow-hidden rounded-2xl border bg-card shadow-xs/5">
            <div className="grid gap-3 px-4 py-4 sm:px-5 xl:grid-cols-[auto_minmax(0,1fr)] xl:items-center">
              <div className="flex min-w-0 flex-wrap items-center gap-2.5">
                <h1 className="min-w-0 text-lg font-semibold tracking-tight" id="billing-title">{t('费用', 'Billing')}</h1>
                <Hint label={description}>
                  <Badge size="lg" variant="secondary">
                    {t('等价 API 费用', 'Equivalent API cost')}
                  </Badge>
                </Hint>
                {focus && (
                  // 关闭按钮挨着徽标右缘：徽标右内边距收掉，让 × 的点击区贴边而不是悬在半空。
                  <Badge className="gap-0.5 pe-0" size="lg" variant="outline">
                    {t(`正在查看：${focus.name}`, `Viewing: ${focus.name}`)}
                    <Button
                      aria-label={t('返回汇总', 'Back to summary')}
                      className="size-5 sm:size-5"
                      size="icon-xs"
                      variant="ghost"
                      onClick={() => { setFocus(null); setDim(null); setSort(null) }}
                    >
                      <XIcon />
                    </Button>
                  </Badge>
                )}
              </div>
              {/* 搜索框与下拉用列表页公共的工具条控件（与账号池同高、弹层不盖住触发器）。 */}
              <Toolbar className="flex flex-wrap items-center gap-2 border-0 bg-transparent p-0 xl:justify-end">
                <ToolbarSearch
                  ariaLabel={t('搜索名称', 'Search names')}
                  className="max-sm:basis-full sm:min-w-56 sm:flex-1 xl:max-w-64"
                  placeholder={t(`搜索${columnLabel[activeDim]}`, `Search ${columnLabel[activeDim].toLowerCase()}`)}
                  value={search}
                  onChange={setSearch}
                />
                <ToolbarMenuSelect
                  ariaLabel={t(`时间范围：${rangeLabel[range]}`, `Time range: ${rangeLabel[range]}`)}
                  className="max-sm:flex-1"
                  groups={[rangeItems]}
                  icon={CalendarIcon}
                  label={rangeLabel[range]}
                  value={range}
                  onChange={setRange}
                />
                {dims.length > 1 && (
                  <ToolbarMenuSelect
                    ariaLabel={t(`拆分方式：${dimLabel[activeDim]}`, `Breakdown: ${dimLabel[activeDim]}`)}
                    className="max-sm:flex-1"
                    groups={[dimItems]}
                    icon={ChartBarIcon}
                    label={dimLabel[activeDim]}
                    value={activeDim}
                    onChange={(next) => { setDim(next); setSort(null) }}
                  />
                )}
                <Hint label={t('刷新', 'Refresh')}>
                  <Button
                    aria-label={t('刷新', 'Refresh')}
                    disabled={query.isFetching}
                    size="icon"
                    variant="outline"
                    onClick={() => void query.refetch()}
                  >
                    <RefreshCwIcon className={cn(query.isFetching && 'animate-spin')} />
                  </Button>
                </Hint>
              </Toolbar>
            </div>

            {query.isPending ? (
              <section aria-label={t('正在加载费用概览', 'Loading billing overview')} className="grid grid-cols-2 border-t lg:grid-cols-4">
                <OverviewMetricSkeleton className="border-r border-b lg:border-b-0" />
                <OverviewMetricSkeleton className="border-b lg:border-r lg:border-b-0" />
                <OverviewMetricSkeleton className="border-r" />
                <OverviewMetricSkeleton />
              </section>
            ) : (
              <section aria-label={t('费用概览', 'Billing overview')} className="grid grid-cols-2 border-t lg:grid-cols-4">
                <OverviewMetric
                  className="border-r border-b lg:border-b-0"
                  icon={ReceiptIcon}
                  label={t(`等价费用 · ${rangeLabel[range]}`, `Equivalent cost · ${rangeLabel[range]}`)}
                  status={total ? t(`日均 ${formatUsd(total.cost_usd / days)}`, `${formatUsd(total.cost_usd / days)}/day`) : undefined}
                  tone="neutral"
                  value={total ? formatUsd(total.cost_usd) : '—'}
                />
                <OverviewMetric
                  className="border-b lg:border-r lg:border-b-0"
                  icon={ActivityIcon}
                  label={t('请求数', 'Requests')}
                  status={total && total.requests > 0
                    ? t(`均价 ${formatUsd(total.cost_usd / total.requests)}`, `${formatUsd(total.cost_usd / total.requests)} each`)
                    : undefined}
                  tone="neutral"
                  value={total ? number(total.requests) : '—'}
                />
                <OverviewMetric
                  className="border-r"
                  icon={SparklesIcon}
                  label={t('输出 token', 'Output tokens')}
                  status={total ? t(`输入 ${formatTokens(totalInput)}`, `${formatTokens(totalInput)} in`) : undefined}
                  tone="neutral"
                  value={total ? formatTokens(total.output_tokens) : '—'}
                />
                <OverviewMetric
                  icon={DatabaseZapIcon}
                  label={t('缓存命中率', 'Cache hit rate')}
                  status={total && hitRate != null ? t(`读 ${formatTokens(total.cache_read_tokens)}`, `${formatTokens(total.cache_read_tokens)} read`) : undefined}
                  statusHint={t(
                    '缓存读占全部输入 token（含缓存写与缓存读）的比例。',
                    'Cache reads as a share of all input tokens (including cache writes and reads).',
                  )}
                  tone={hitRate == null ? 'neutral' : hitRate >= 0.5 ? 'ok' : 'warn'}
                  value={formatPercent(hitRate)}
                />
              </section>
            )}
          </section>

          <section aria-labelledby="billing-list-title" className="min-w-0">
            <h2 className="sr-only" id="billing-list-title">{t('费用明细', 'Billing breakdown')}</h2>
            {query.isPending ? (
              <Card className="space-y-2 p-4">
                {Array.from({ length: 6 }, (_, i) => <Skeleton className="h-9 w-full" key={i} />)}
              </Card>
            ) : query.isError && !query.data ? (
              <Card>
                <ErrorState
                  error={query.error}
                  retrying={query.isFetching}
                  title={t('暂时无法读取费用', 'Unable to load billing')}
                  onRetry={() => void query.refetch()}
                />
              </Card>
            ) : rows.length === 0 ? (
              <Card>
                <Empty>
                  <EmptyHeader>
                    <EmptyMedia variant="icon">{needle ? <SearchIcon /> : <ReceiptIcon />}</EmptyMedia>
                    <EmptyTitle>
                      {needle ? t('没有符合条件的记录', 'No matching rows') : t('这段时间没有费用', 'No usage in this period')}
                    </EmptyTitle>
                    <EmptyDescription>
                      {needle ? t('尝试清除搜索关键字。', 'Try clearing the search.') : t('换个时间范围看看。', 'Try another time range.')}
                    </EmptyDescription>
                  </EmptyHeader>
                  {needle && (
                    <EmptyContent>
                      <Button variant="outline" onClick={() => setSearch('')}>{t('清除搜索', 'Clear search')}</Button>
                    </EmptyContent>
                  )}
                </Empty>
              </Card>
            ) : (
              <Table className="table-fixed" variant="card">
                <TableCaption className="sr-only">{t('费用明细', 'Billing breakdown')}</TableCaption>
                <TableHeader>
                  <TableRow>
                    {sortHead(columnLabel[activeDim], 'name', COL.name, false)}
                    {sortHead(t('请求数', 'Requests'), 'requests', COL.requests)}
                    {sortHead(t('输入', 'Input'), 'input', COL.tokens)}
                    {sortHead(t('输出', 'Output'), 'output', COL.tokens)}
                    {sortHead(t('缓存写', 'Cache write'), 'cache_write', COL.tokens)}
                    {sortHead(t('缓存读', 'Cache read'), 'cache_read', COL.tokens)}
                    <TableHead className={cn(COL.share, 'text-2xs font-semibold uppercase tracking-[0.06em]')}>
                      {t('占比', 'Share')}
                    </TableHead>
                    {sortHead(t('等价费用', 'Cost'), 'cost', COL.cost)}
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {rows.map((row) => {
                    const drillable = canDrill && row.key !== '0'
                    const share = total && total.cost_usd > 0 ? row.cost_usd / total.cost_usd : 0
                    const name = nameOf(row)
                    return (
                      <TableRow className={cn(drillable && 'cursor-pointer')} key={row.key} onClick={() => drill(row)}>
                        <TableCell className={cn(COL.name, 'overflow-hidden')}>
                          <Hint label={drillable ? t(`查看 ${name} 的明细`, `View details for ${name}`) : name}>
                            {drillable ? (
                              // 点击由整行接住（见 TableRow 的 onClick），这枚按钮只给键盘一个落点。
                              // `h-auto p-0`：链接样式的按钮不要按钮的高度与内边距，与不可下钻时的纯文本对齐。
                              <Button
                                className="block h-auto max-w-full truncate rounded-sm border-0 p-0 text-left font-semibold leading-snug sm:h-auto"
                                variant="link"
                              >
                                {name}
                              </Button>
                            ) : (
                              <span className="block truncate text-sm font-semibold leading-snug">{name}</span>
                            )}
                          </Hint>
                          <span className="mt-1 block truncate text-xs text-muted-foreground tabular-nums">{subtitle(row)}</span>
                        </TableCell>
                        <TableCell className={cn(COL.requests, 'text-right tabular-nums')}>{number(row.requests)}</TableCell>
                        <TableCell className={cn(COL.tokens, 'text-right tabular-nums')}>{formatTokens(row.input_tokens)}</TableCell>
                        <TableCell className={cn(COL.tokens, 'text-right tabular-nums')}>{formatTokens(row.output_tokens)}</TableCell>
                        <TableCell className={cn(COL.tokens, 'text-right tabular-nums')}>{formatTokens(row.cache_write_tokens)}</TableCell>
                        <TableCell className={cn(COL.tokens, 'text-right tabular-nums')}>{formatTokens(row.cache_read_tokens)}</TableCell>
                        <TableCell className={COL.share}>
                          {/* 条吃掉剩余宽度、百分比定宽右对齐：每一行的条起止都在同一条竖线上。
                              条长按占全部费用的比例，不按本页最大值拉满——拉满会让「61%」与「100%」看着一样长。 */}
                          <div className="flex items-center gap-2">
                            {/* 条只给眼睛看，读屏念右边那格百分比。 */}
                            <Meter aria-hidden className="min-w-0 flex-1" max={100} value={Math.min(100, share * 100)}>
                              <MeterTrack className="h-1.5 rounded-full bg-muted">
                                <MeterIndicator className="rounded-full bg-primary/72" />
                              </MeterTrack>
                            </Meter>
                            <span className="w-12 shrink-0 text-right text-xs text-muted-foreground tabular-nums">{formatPercent(share)}</span>
                          </div>
                        </TableCell>
                        <TableCell className={cn(COL.cost, 'text-right font-semibold tabular-nums')}>{formatUsd(row.cost_usd)}</TableCell>
                      </TableRow>
                    )
                  })}
                </TableBody>
              </Table>
            )}
            {canDrill && rows.length > 0 && (
              <p className="mt-2 text-xs text-muted-foreground">{t('点击某个人查看他的明细。', 'Click a person to see their details.')}</p>
            )}
          </section>
        </div>
      </main>
      <AppFooter />
    </div>
  )
}
