import { Fragment, useId, useState, type ReactNode } from 'react'
import { keepPreviousData, useQuery } from '@tanstack/react-query'
import { BarChart3Icon, ChartColumnIcon, TableIcon } from 'lucide-react'
import {
  getCredentialStats,
  type Credential,
  type CredentialStatsBucket,
  type CredentialStatsGroup,
} from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import {
  cn,
  extractError,
  formatCompactNumber,
  formatTokens,
  formatUsd,
  relativeTime,
  type CacheGranularity,
} from '@/lib/utils'
import { ChartLegend, type ChartLegendItem } from '@/components/chart-legend'
import { DetailSection } from '@/components/detail-section'
import { RequestLookupDialog, type UsageDrillFilter } from '@/components/request-lookup-dialog'
import { statusVariant } from '@/components/usage-shared'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Badge } from '@/components/ui/badge'
import { Skeleton } from '@/components/ui/skeleton'
import { Spinner } from '@/components/ui/spinner'
import { ToggleGroup, ToggleGroupItem, ToggleGroupSeparator } from '@/components/ui/toggle-group'

/** 时间范围与粒度：与缓存 / 延迟趋势对话框同一套三档（24h 逐小时，7d / 30d 逐天）。 */
const RANGES = {
  '24h': { hours: 24, slots: 24, granularity: 'hour' as CacheGranularity, bucketSecs: 3600 },
  '7d': { hours: 7 * 24, slots: 7, granularity: 'day' as CacheGranularity, bucketSecs: 86400 },
  '30d': { hours: 30 * 24, slots: 30, granularity: 'day' as CacheGranularity, bucketSecs: 86400 },
}
type RangeKey = keyof typeof RANGES

type Metric = 'requests' | 'tokens' | 'cost'
type Dimension = 'model' | 'device' | 'client' | 'status'

type Translate = (zh: string, en: string) => string

/** 图里的一格：后端只回有请求的桶，这里补齐成连续的若干格。 */
interface StatsSlot extends CredentialStatsBucket {
  hasTraffic: boolean
}

function emptyBucket(ts: number): CredentialStatsBucket {
  return {
    ts,
    requests: 0,
    errors: 0,
    rejected: 0,
    input_tokens: 0,
    output_tokens: 0,
    cache_write_tokens: 0,
    cache_read_tokens: 0,
    cost_usd: 0,
  }
}

/**
 * 补齐连续的格子，口径同 `bucketCacheSeries`：逐小时按整点、逐天按本地零点。后端已按本地时区
 * 切好了天，这里只是按同一把尺子找到每个点落在哪一格。
 */
function fillSlots(
  points: readonly CredentialStatsBucket[],
  granularity: CacheGranularity,
  slots: number,
  nowMs = Date.now(),
): StatsSlot[] {
  const localDayStart = (ms: number) => {
    const d = new Date(ms)
    d.setHours(0, 0, 0, 0)
    return d
  }
  const starts: number[] = []
  if (granularity === 'hour') {
    const last = Math.floor(nowMs / 1000 / 3600) * 3600
    for (let i = slots - 1; i >= 0; i--) starts.push(last - i * 3600)
  } else {
    const cursor = localDayStart(nowMs)
    cursor.setDate(cursor.getDate() - (slots - 1))
    for (let i = 0; i < slots; i++) {
      starts.push(Math.floor(cursor.getTime() / 1000))
      cursor.setDate(cursor.getDate() + 1)
    }
  }
  const out: StatsSlot[] = starts.map((ts) => ({ ...emptyBucket(ts), hasTraffic: false }))
  const indexOf = new Map(starts.map((ts, i) => [ts, i]))
  for (const p of points) {
    const key = granularity === 'hour'
      ? Math.floor(p.ts / 3600) * 3600
      : Math.floor(localDayStart(p.ts * 1000).getTime() / 1000)
    const i = indexOf.get(key)
    if (i == null) continue
    const slot = out[i]
    slot.requests += p.requests
    slot.errors += p.errors
    slot.rejected += p.rejected
    slot.input_tokens += p.input_tokens
    slot.output_tokens += p.output_tokens
    slot.cache_write_tokens += p.cache_write_tokens
    slot.cache_read_tokens += p.cache_read_tokens
    slot.cost_usd += p.cost_usd
    slot.hasTraffic = slot.hasTraffic || p.requests > 0
  }
  return out
}

function totalTokens(b: CredentialStatsBucket): number {
  return b.input_tokens + b.output_tokens + b.cache_write_tokens + b.cache_read_tokens
}

function slotLabel(ts: number, granularity: CacheGranularity): { when: string; axis: string } {
  const d = new Date(ts * 1000)
  const p = (n: number) => String(n).padStart(2, '0')
  const day = `${d.getMonth() + 1}/${d.getDate()}`
  return granularity === 'hour'
    ? { when: `${day} ${p(d.getHours())}:00`, axis: `${p(d.getHours())}:00` }
    : { when: day, axis: day }
}

/**
 * 一根柱子分几段、每段什么颜色。段的顺序是**自下而上**。
 *
 * 请求数：成功垫底、失败叠在上面，失败走 destructive——这一格里坏了多少一眼可见。
 * token：四项同一色系由深到浅，缓存读放最上面用灰：它通常占大头、却按 0.1 倍计价，
 * 染成主色会让「token 很多」看起来像「很贵」。费用只有一段。
 */
function segmentsOf(slot: CredentialStatsBucket, metric: Metric): { value: number; className: string }[] {
  if (metric === 'requests') {
    return [
      { value: slot.requests - slot.errors, className: 'bg-chart-1' },
      { value: slot.errors, className: 'bg-destructive/72' },
    ]
  }
  if (metric === 'tokens') {
    return [
      { value: slot.output_tokens, className: 'bg-chart-1' },
      { value: slot.input_tokens, className: 'bg-chart-1/60' },
      { value: slot.cache_write_tokens, className: 'bg-chart-1/32' },
      { value: slot.cache_read_tokens, className: 'bg-muted-foreground/24' },
    ]
  }
  return [{ value: slot.cost_usd, className: 'bg-chart-1' }]
}

function metricValue(slot: CredentialStatsBucket, metric: Metric): number {
  if (metric === 'requests') return slot.requests
  if (metric === 'tokens') return totalTokens(slot)
  return slot.cost_usd
}

function formatMetric(value: number, metric: Metric): string {
  if (metric === 'cost') return formatUsd(value)
  return formatCompactNumber(value)
}

function legendOf(metric: Metric, t: Translate): ChartLegendItem[] {
  if (metric === 'requests') {
    return [
      { swatch: 'bg-chart-1', label: t('成功', 'Succeeded') },
      { swatch: 'bg-destructive/72', label: t('失败', 'Failed') },
    ]
  }
  if (metric === 'tokens') {
    return [
      { swatch: 'bg-chart-1', label: t('输出', 'Output') },
      { swatch: 'bg-chart-1/60', label: t('输入', 'Input') },
      { swatch: 'bg-chart-1/32', label: t('缓存写', 'Cache write') },
      { swatch: 'bg-muted-foreground/24', label: t('缓存读', 'Cache read') },
    ]
  }
  return [{ swatch: 'bg-chart-1', label: t('等价 API 费用', 'Equivalent API cost') }]
}

/**
 * 单账号的用量统计：时间维度（逐小时 / 逐天）× 指标（请求数 / token / 费用），外加按模型、
 * 设备、客户端、状态码四个维度的拆分。数据来自 `GET /credentials/{id}/stats`，一次扫描全拿齐。
 */
export function CredentialStatsSection({ cred }: { cred: Credential }) {
  const { t, locale } = useI18n()
  const [range, setRange] = useState<RangeKey>('7d')
  const [metric, setMetric] = useState<Metric>('requests')
  const [view, setView] = useState<'chart' | 'table'>('chart')
  const preset = RANGES[range]
  const query = useQuery({
    queryKey: ['credential-stats', cred.id, preset.hours, preset.bucketSecs],
    queryFn: () => getCredentialStats(cred.id, preset),
    refetchInterval: 60_000,
    placeholderData: keepPreviousData,
  })
  const slots = fillSlots(query.data?.points ?? [], preset.granularity, preset.slots)
  const summary = query.data?.summary
  const hasTraffic = (summary?.requests ?? 0) > 0
  const refetching = query.isFetching && !query.isPending

  const rangeLabel: Record<RangeKey, string> = {
    '24h': t('近 24 小时', 'Last 24 hours'),
    '7d': t('近 7 天', 'Last 7 days'),
    '30d': t('近 30 天', 'Last 30 days'),
  }
  const metricLabel: Record<Metric, string> = {
    requests: t('请求数', 'Requests'),
    tokens: 'Token',
    cost: t('费用', 'Cost'),
  }

  return (
    <DetailSection
      icon={ChartColumnIcon}
      title={t('用量统计', 'Usage statistics')}
      description={t(
        `${rangeLabel[range]}${preset.granularity === 'hour' ? '逐小时' : '逐天'}的请求、token 与费用，以及按模型、设备、客户端、状态码的拆分。流水只保留 30 天。`,
        `${rangeLabel[range]} of requests, tokens and cost ${preset.granularity === 'hour' ? 'per hour' : 'per day'}, broken down by model, device, client and status. Logs are kept for 30 days.`,
      )}
      action={(
        <>
          {refetching && <Spinner />}
          <ToggleGroup
            value={[range]}
            onValueChange={(values) => {
              const next = values[values.length - 1]
              if (next && next in RANGES) setRange(next as RangeKey)
            }}
            variant="outline"
            aria-label={t('时间范围', 'Time range')}
          >
            {(Object.keys(RANGES) as RangeKey[]).map((key, i) => (
              <Fragment key={key}>
                {i > 0 && <ToggleGroupSeparator />}
                <ToggleGroupItem value={key} aria-label={rangeLabel[key]}>{key}</ToggleGroupItem>
              </Fragment>
            ))}
          </ToggleGroup>
        </>
      )}
      panelClassName="space-y-4 p-4 sm:p-5"
    >
      {query.error ? (
        <Alert variant="error">
          <AlertTitle>{t('读取失败', 'Failed to load')}</AlertTitle>
          <AlertDescription className="break-words">{extractError(query.error)}</AlertDescription>
        </Alert>
      ) : query.isPending || !summary ? (
        <div className="space-y-3">
          <div className="grid grid-cols-2 gap-2 lg:grid-cols-4">
            {Array.from({ length: 4 }, (_, i) => <Skeleton key={i} className="h-20 rounded-xl" />)}
          </div>
          <Skeleton className="h-48 w-full rounded-xl" />
        </div>
      ) : (
        <>
          <SummaryCards summary={summary} />

          {/* 手机上图例单独一行：Token 的四项图例与两组切换挤在一行放不下，原来会折成三行、
              图表 / 表格切换被挤到最底下。现在第一行固定「指标 ｜ 视图」一左一右，图例 `order-last`
              落到第二行铺满；sm 起三样回到同一行，图例吃掉中间的空。 */}
          <div className="flex flex-wrap items-center gap-x-3 gap-y-2">
              <ToggleGroup
                value={[metric]}
                onValueChange={(values) => {
                  const next = values[values.length - 1]
                  if (next === 'requests' || next === 'tokens' || next === 'cost') setMetric(next)
                }}
                variant="outline"
                aria-label={t('统计指标', 'Metric')}
              >
                {(['requests', 'tokens', 'cost'] as Metric[]).map((key, i) => (
                  <Fragment key={key}>
                    {i > 0 && <ToggleGroupSeparator />}
                    <ToggleGroupItem value={key} aria-label={metricLabel[key]}>{metricLabel[key]}</ToggleGroupItem>
                  </Fragment>
                ))}
              </ToggleGroup>
              {view === 'chart' && hasTraffic && (
                <ChartLegend items={legendOf(metric, t)} className="order-last w-full sm:order-none sm:w-auto sm:flex-1" />
              )}
            <ToggleGroup
              className="ml-auto shrink-0"
              value={[view]}
              onValueChange={(values) => {
                const next = values[values.length - 1]
                if (next === 'chart' || next === 'table') setView(next)
              }}
              variant="outline"
              aria-label={t('图表 / 表格', 'Chart or table')}
            >
              <ToggleGroupItem value="chart" aria-label={t('图表', 'Chart')} title={t('图表', 'Chart')}>
                <BarChart3Icon />
              </ToggleGroupItem>
              <ToggleGroupSeparator />
              <ToggleGroupItem value="table" aria-label={t('表格', 'Table')} title={t('表格', 'Table')}>
                <TableIcon />
              </ToggleGroupItem>
            </ToggleGroup>
          </div>

          {!hasTraffic ? (
            <p className="rounded-xl border px-4 py-8 text-center text-xs text-muted-foreground">
              {t('这段时间没有请求，换一个更长的时间范围看看。', 'No requests in this period; try a longer range.')}
            </p>
          ) : view === 'chart' ? (
            <StatsColumns slots={slots} metric={metric} granularity={preset.granularity} refetching={refetching} />
          ) : (
            <StatsTable slots={slots} granularity={preset.granularity} />
          )}

          <StatsBreakdown
            credId={cred.id}
            hours={preset.hours}
            data={query.data}
            locale={locale}
            refetching={refetching}
          />
        </>
      )}
    </DetailSection>
  )
}

/** 窗口合计：四枚与趋势对话框同款的汇总块（标签 / 大数 / 说明三行堆叠）。 */
function SummaryCards({ summary }: { summary: CredentialStatsBucket }) {
  const { t, locale } = useI18n()
  const ok = summary.requests - summary.errors
  const successRate = summary.requests > 0 ? ok / summary.requests : null
  const tokens = totalTokens(summary)
  const card = (label: string, value: string, detail: ReactNode, valueClassName?: string) => (
    <div className="rounded-xl border bg-muted/32 px-4 py-3">
      <p className="text-2xs font-medium text-muted-foreground">{label}</p>
      <p className={cn('mt-1 text-2xl font-semibold leading-none', valueClassName)}>{value}</p>
      <p className="mt-1.5 text-2xs text-muted-foreground tabular-nums">{detail}</p>
    </div>
  )
  return (
    <section className="grid grid-cols-2 gap-2 lg:grid-cols-4" aria-label={t('合计', 'Totals')}>
      {card(
        t('请求数', 'Requests'),
        summary.requests.toLocaleString(locale),
        t(
          `失败 ${summary.errors.toLocaleString(locale)} · 本地拒绝 ${summary.rejected.toLocaleString(locale)}`,
          `${summary.errors.toLocaleString(locale)} failed · ${summary.rejected.toLocaleString(locale)} rejected locally`,
        ),
      )}
      {card(
        t('成功率', 'Success rate'),
        successRate == null ? '—' : `${Number((successRate * 100).toFixed(1))}%`,
        t(`2xx ${ok.toLocaleString(locale)} 条`, `${ok.toLocaleString(locale)} × 2xx`),
        successRate != null && successRate < 0.9 ? 'text-warning-foreground' : undefined,
      )}
      {card(
        t('总 token', 'Total tokens'),
        formatTokens(tokens),
        // 四项一行在手机半格里放不下，会在随便哪个「·」后面折断；固定拆成「入 · 出」「写 · 读」两行，sm 起连成一行。
        <>
          <span className="block sm:inline">
            {t(
              `入 ${formatTokens(summary.input_tokens)} · 出 ${formatTokens(summary.output_tokens)}`,
              `in ${formatTokens(summary.input_tokens)} · out ${formatTokens(summary.output_tokens)}`,
            )}
          </span>
          <span className="max-sm:hidden"> · </span>
          <span className="block sm:inline">
            {t(
              `写 ${formatTokens(summary.cache_write_tokens)} · 读 ${formatTokens(summary.cache_read_tokens)}`,
              `write ${formatTokens(summary.cache_write_tokens)} · read ${formatTokens(summary.cache_read_tokens)}`,
            )}
          </span>
        </>,
      )}
      {card(
        t('等价 API 费用', 'Equivalent API cost'),
        formatUsd(summary.cost_usd),
        summary.requests > 0
          ? t(`平均每条 ${formatUsd(summary.cost_usd / summary.requests)}`, `${formatUsd(summary.cost_usd / summary.requests)} per request`)
          : t('无请求', 'No requests'),
      )}
    </section>
  )
}

/** 叠柱里段与段之间的 2px 留白，做法同 cache-hit-chart 的 SEGMENT_GAP：透明下边框 + bg-clip-padding。 */
const SEGMENT_GAP = 'border-b-2 border-transparent bg-clip-padding'
const GAP_MIN_PCT = 4

/**
 * 柱状趋势：骨架与 CacheHitColumns 一致（左侧刻度、三条网格线、悬浮读数、稀疏的横轴刻度），
 * 区别是纵轴按这段时间的最大值定，而不是固定 0–100%。
 */
function StatsColumns({
  slots,
  metric,
  granularity,
  refetching,
}: {
  slots: StatsSlot[]
  metric: Metric
  granularity: CacheGranularity
  refetching: boolean
}) {
  const { t, locale } = useI18n()
  const [active, setActive] = useState<number | null>(null)
  const max = Math.max(0, ...slots.map((s) => metricValue(s, metric)))
  const step = Math.max(1, Math.ceil(slots.length / 7))
  const labels = slots.map((s) => slotLabel(s.ts, granularity))
  const detailOf = (s: StatsSlot) => {
    if (!s.hasTraffic) return t('这个时段没有请求', 'No requests in this period')
    return t(
      `${s.requests.toLocaleString(locale)} 条（失败 ${s.errors.toLocaleString(locale)}）· ${formatTokens(totalTokens(s))} token · ${formatUsd(s.cost_usd)}`,
      `${s.requests.toLocaleString(locale)} req (${s.errors.toLocaleString(locale)} failed) · ${formatTokens(totalTokens(s))} tokens · ${formatUsd(s.cost_usd)}`,
    )
  }

  return (
    <div className={cn('transition-opacity', refetching && 'opacity-60')}>
      <div className="flex gap-2">
        <div className="flex h-40 w-10 shrink-0 flex-col justify-between text-end text-2xs text-muted-foreground tabular-nums">
          <span className="-translate-y-1/2">{formatMetric(max, metric)}</span>
          <span>{formatMetric(max / 2, metric)}</span>
          <span className="translate-y-1/2">0</span>
        </div>
        <div className="min-w-0 flex-1">
          <div className="relative h-40">
            {[0, 50, 100].map((pct) => (
              <div key={pct} aria-hidden className="absolute inset-x-0 border-t border-border" style={{ bottom: `${pct}%` }} />
            ))}
            <div className="absolute inset-0 flex items-end">
              {slots.map((slot, i) => {
                const value = metricValue(slot, metric)
                const heightPct = max > 0 ? (value / max) * 100 : 0
                const segments = segmentsOf(slot, metric)
                return (
                  <div
                    key={slot.ts}
                    role="img"
                    tabIndex={0}
                    aria-label={`${labels[i].when} · ${detailOf(slot)}`}
                    onPointerEnter={() => setActive(i)}
                    onPointerLeave={() => setActive((cur) => (cur === i ? null : cur))}
                    onFocus={() => setActive(i)}
                    onBlur={() => setActive((cur) => (cur === i ? null : cur))}
                    className="group relative flex h-full flex-1 items-end justify-center px-px outline-none"
                  >
                    <span
                      aria-hidden
                      className={cn(
                        'absolute inset-0 transition-colors',
                        active === i && 'bg-muted/56',
                        'group-focus-visible:ring-2 group-focus-visible:ring-ring group-focus-visible:ring-inset',
                      )}
                    />
                    {value <= 0 ? (
                      <span aria-hidden className="relative h-0.5 w-full max-w-6 rounded-full bg-muted-foreground/24" />
                    ) : (
                      <span
                        aria-hidden
                        className="relative flex w-full max-w-6 flex-col-reverse overflow-hidden rounded-t"
                        style={{ height: `max(0.125rem, ${heightPct}%)` }}
                      >
                        {segments.map((seg, j) => {
                          const pct = value > 0 ? (seg.value / value) * 100 : 0
                          // 最底下那段坐在基线上，不开缝；其余段够厚才开缝。
                          const gap = j > 0 && pct >= GAP_MIN_PCT ? SEGMENT_GAP : undefined
                          return (
                            <span
                              key={j}
                              className={cn('w-full shrink-0', seg.className, gap)}
                              style={{ height: `${pct}%` }}
                            />
                          )
                        })}
                      </span>
                    )}
                  </div>
                )
              })}
            </div>

            {active != null && (() => {
              const pos = (active + 0.5) / slots.length
              const anchor = pos < 0.2 ? 'start' : pos > 0.8 ? 'end' : 'center'
              return (
                <div
                  role="status"
                  aria-live="off"
                  className={cn(
                    'pointer-events-none absolute top-1 z-10 rounded-lg border bg-popover px-2 py-1',
                    'text-2xs leading-4 text-popover-foreground shadow-md',
                    'w-max max-w-[min(18rem,100%)]',
                    anchor === 'center' && '-translate-x-1/2',
                  )}
                  style={anchor === 'start' ? { left: 0 } : anchor === 'end' ? { right: 0 } : { left: `${pos * 100}%` }}
                >
                  <p className="flex items-baseline gap-1.5 tabular-nums">
                    <span className="font-semibold">{formatMetric(metricValue(slots[active], metric), metric)}</span>
                    <span className="text-muted-foreground">{labels[active].when}</span>
                  </p>
                  <p className="text-muted-foreground tabular-nums">{detailOf(slots[active])}</p>
                </div>
              )
            })()}
          </div>
          <div className="relative mt-1.5 h-4" aria-hidden>
            {slots.map((slot, i) =>
              i % step === 0 ? (
                <span
                  key={slot.ts}
                  className="absolute -translate-x-1/2 whitespace-nowrap text-2xs text-muted-foreground tabular-nums"
                  style={{ left: `${((i + 0.5) / slots.length) * 100}%` }}
                >
                  {labels[i].axis}
                </span>
              ) : null,
            )}
          </div>
        </div>
      </div>
    </div>
  )
}

/** 同一份数据的表格视图：只列有请求的时段，新的在上。 */
function StatsTable({ slots, granularity }: { slots: StatsSlot[]; granularity: CacheGranularity }) {
  const { t, locale } = useI18n()
  const captionId = useId()
  const rows = slots.filter((s) => s.hasTraffic).reverse()
  const n = (v: number) => v.toLocaleString(locale)
  return (
    <div className="max-h-72 overflow-auto rounded-xl border">
      <table className="w-full text-xs" aria-describedby={captionId}>
        <caption id={captionId} className="sr-only">{t('按时段的用量明细', 'Usage by period')}</caption>
        <thead className="sticky top-0 bg-surface-subtle">
          <tr className="[&>th]:h-7 [&>th]:border-b [&>th]:px-3 [&>th]:text-2xs [&>th]:font-medium [&>th]:text-muted-foreground">
            <th scope="col" className="text-start">{granularity === 'hour' ? t('时段', 'Hour') : t('日期', 'Day')}</th>
            <th scope="col" className="text-end">{t('请求', 'Requests')}</th>
            <th scope="col" className="text-end">{t('失败', 'Failed')}</th>
            <th scope="col" className="text-end">{t('输入', 'Input')}</th>
            <th scope="col" className="text-end">{t('输出', 'Output')}</th>
            <th scope="col" className="text-end">{t('缓存写', 'Cache write')}</th>
            <th scope="col" className="text-end">{t('缓存读', 'Cache read')}</th>
            <th scope="col" className="text-end">{t('费用', 'Cost')}</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((s) => (
            <tr key={s.ts} className="[&>td]:border-b [&>td]:px-3 [&>td]:py-1.5 [&>td]:whitespace-nowrap [&>td]:tabular-nums last:[&>td]:border-b-0">
              <td>{slotLabel(s.ts, granularity).when}</td>
              <td className="text-end font-medium">{n(s.requests)}</td>
              <td className={cn('text-end', s.errors > 0 ? 'text-destructive-foreground' : 'text-muted-foreground')}>{n(s.errors)}</td>
              <td className="text-end">{n(s.input_tokens)}</td>
              <td className="text-end">{n(s.output_tokens)}</td>
              <td className="text-end">{n(s.cache_write_tokens)}</td>
              <td className="text-end text-muted-foreground">{n(s.cache_read_tokens)}</td>
              <td className="text-end">{formatUsd(s.cost_usd)}</td>
            </tr>
          ))}
        </tbody>
      </table>
    </div>
  )
}

/**
 * 按维度拆开的表：模型 / 设备 / 客户端 / 状态码四选一，行数据后端已按请求数降序。
 * 「占比」那一格按请求数画一条细条，一眼看出谁是大头；按模型的行可以点开看这个号在该模型下的请求。
 */
function StatsBreakdown({
  credId,
  hours,
  data,
  locale,
  refetching,
}: {
  credId: number
  hours: number
  data: { summary: CredentialStatsBucket } & Record<`by_${Dimension}`, CredentialStatsGroup[]> | undefined
  locale: string
  refetching: boolean
}) {
  const { t, language } = useI18n()
  const [by, setBy] = useState<Dimension>('model')
  const [drill, setDrill] = useState<UsageDrillFilter | null>(null)
  const rows = data?.[`by_${by}`] ?? []
  const total = data?.summary.requests ?? 0
  const now = Math.floor(Date.now() / 1000)
  const byLabel: Record<Dimension, string> = {
    model: t('按模型', 'By model'),
    device: t('按设备', 'By device'),
    client: t('按客户端', 'By client'),
    status: t('按状态码', 'By status'),
  }
  const keyHeader: Record<Dimension, string> = {
    model: t('模型', 'Model'),
    device: t('设备', 'Device'),
    client: 'User-Agent',
    status: t('状态码', 'Status'),
  }
  const emptyKey: Record<Dimension, string> = {
    model: t('（未知模型）', '(unknown model)'),
    device: t('（无设备身份）', '(no device identity)'),
    client: t('（未带 UA）', '(no User-Agent)'),
    status: '—',
  }

  return (
    <section className="space-y-2" aria-label={t('分维度统计', 'Breakdown')}>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <h3 className="text-sm font-semibold tracking-tight">{t('分维度统计', 'Breakdown')}</h3>
        <ToggleGroup
          value={[by]}
          onValueChange={(values) => {
            const next = values[values.length - 1]
            if (next === 'model' || next === 'device' || next === 'client' || next === 'status') setBy(next)
          }}
          variant="outline"
          aria-label={t('拆分维度', 'Breakdown dimension')}
        >
          {(['model', 'device', 'client', 'status'] as Dimension[]).map((key, i) => (
            <Fragment key={key}>
              {i > 0 && <ToggleGroupSeparator />}
              <ToggleGroupItem value={key} aria-label={byLabel[key]}>{byLabel[key]}</ToggleGroupItem>
            </Fragment>
          ))}
        </ToggleGroup>
      </div>
      {rows.length === 0 ? (
        <p className="rounded-xl border px-4 py-6 text-center text-xs text-muted-foreground">
          {t('这段时间没有请求', 'No requests in this period')}
        </p>
      ) : (
        <div className={cn('max-h-80 overflow-auto rounded-xl border transition-opacity', refetching && 'opacity-60')}>
          <table className="w-full text-xs">
            <thead className="sticky top-0 bg-surface-subtle">
              <tr className="[&>th]:h-7 [&>th]:whitespace-nowrap [&>th]:border-b [&>th]:px-3 [&>th]:text-2xs [&>th]:font-medium [&>th]:text-muted-foreground">
                <th scope="col" className="text-start">{keyHeader[by]}</th>
                {/* 手机上七列放不下：占比条、Token（上面的图切到 Token 能看）与「最近」藏掉，留名称、占比、请求、失败、费用五列。 */}
                <th scope="col" className="w-40 text-start max-sm:w-auto max-sm:text-end">{t('占比', 'Share')}</th>
                <th scope="col" className="text-end">{t('请求', 'Requests')}</th>
                <th scope="col" className="text-end">{t('失败', 'Failed')}</th>
                <th scope="col" className="text-end max-sm:hidden">Token</th>
                <th scope="col" className="text-end">{t('费用', 'Cost')}</th>
                <th scope="col" className="text-end max-sm:hidden">{t('最近', 'Last')}</th>
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => {
                const share = total > 0 ? row.requests / total : 0
                const drillable = by === 'model' && row.key !== ''
                return (
                  <tr
                    key={row.key}
                    className={cn(
                      '[&>td]:border-b [&>td]:px-3 [&>td]:py-1.5 last:[&>td]:border-b-0',
                      drillable && 'cursor-pointer hover:bg-muted/40',
                    )}
                    onClick={drillable ? () => setDrill({ credId, model: row.key, label: row.key, hours }) : undefined}
                    title={drillable ? t('查看该模型的请求', 'View requests for this model') : undefined}
                  >
                    <td className="max-w-80 max-sm:max-w-36">
                      {by === 'status' ? (
                        <Badge size="sm" variant={statusVariant(Number(row.key))}>{row.key}</Badge>
                      ) : (
                        <span
                          className={cn('block truncate', by !== 'client' && 'font-mono', !row.key && 'font-sans text-muted-foreground')}
                          title={row.key || undefined}
                        >
                          {row.key || emptyKey[by]}
                        </span>
                      )}
                    </td>
                    <td>
                      <span className="flex items-center gap-2">
                        <span className="h-1.5 min-w-10 flex-1 overflow-hidden rounded-full bg-muted max-sm:hidden">
                          <span className="block h-full rounded-full bg-chart-1" style={{ width: `${share * 100}%` }} />
                        </span>
                        <span className="w-10 shrink-0 text-end text-muted-foreground tabular-nums max-sm:ml-auto">
                          {`${Number((share * 100).toFixed(1))}%`}
                        </span>
                      </span>
                    </td>
                    <td className="whitespace-nowrap text-end font-medium tabular-nums">{row.requests.toLocaleString(locale)}</td>
                    <td className={cn('whitespace-nowrap text-end tabular-nums', row.errors > 0 ? 'text-destructive-foreground' : 'text-muted-foreground')}>
                      {row.errors.toLocaleString(locale)}
                    </td>
                    <td className="whitespace-nowrap text-end tabular-nums max-sm:hidden" title={row.tokens.toLocaleString(locale)}>{formatTokens(row.tokens)}</td>
                    <td className="whitespace-nowrap text-end tabular-nums">{formatUsd(row.cost_usd)}</td>
                    <td className="whitespace-nowrap text-end text-muted-foreground max-sm:hidden">{relativeTime(row.last_ts, now, language)}</td>
                  </tr>
                )
              })}
            </tbody>
          </table>
        </div>
      )}
      {drill && (
        <RequestLookupDialog open onOpenChange={(open) => { if (!open) setDrill(null) }} filter={drill} />
      )}
    </section>
  )
}
