import { Fragment, useRef, useState } from 'react'
import { keepPreviousData, useQuery } from '@tanstack/react-query'
import { BarChart3Icon, TableIcon, TimerIcon } from 'lucide-react'
import { getTtftSeries } from '@/api/metrics'
import { useI18n } from '@/lib/i18n'
import { bucketTtftSeries, cn, extractError, type CacheGranularity, type TtftSlot } from '@/lib/utils'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Avatar, AvatarFallback } from '@/components/ui/avatar'
import { Badge } from '@/components/ui/badge'
import {
  Dialog,
  DialogDescription,
  DialogHeader,
  DialogPanel,
  DialogPopup,
  DialogTitle,
} from '@/components/ui/dialog'
import { Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from '@/components/ui/empty'
import { Skeleton } from '@/components/ui/skeleton'
import { Spinner } from '@/components/ui/spinner'
import { ToggleGroup, ToggleGroupItem, ToggleGroupSeparator } from '@/components/ui/toggle-group'
import { UsageBreakdown } from '@/components/usage-breakdown'
import type { TtftSeriesPoint } from '@/api/metrics'

/** 汇总条里的一格：p50 大字，p95 / 平均 / 请求数 / 吞吐小字。没有请求时只写「无请求」。 */
function SummaryCard({
  label,
  stats,
  t,
  locale,
}: {
  label: string
  stats: TtftSeriesPoint | null
  t: (zh: string, en: string) => string
  locale: string
}) {
  const empty = !stats || stats.count === 0
  return (
    <div className="rounded-xl border bg-muted/32 px-3 py-2.5 sm:px-4">
      <p className="text-2xs font-medium text-muted-foreground">{label}</p>
      <div className="mt-1 flex flex-wrap items-baseline gap-x-3 gap-y-1">
        <p className="text-2xl font-semibold leading-none tabular-nums">{empty ? '—' : formatMs(stats.p50_ms)}</p>
        <p className="text-2xs text-muted-foreground tabular-nums">
          {empty
            ? t('无请求', 'No requests')
            : t(
                `p95 ${formatMs(stats.p95_ms)} · 平均 ${formatMs(stats.avg_ms)} · ${stats.count.toLocaleString(locale)} 次 · ${formatTokensPerSec(stats.tokens_per_sec)}`,
                `p95 ${formatMs(stats.p95_ms)} · avg ${formatMs(stats.avg_ms)} · ${stats.count.toLocaleString(locale)} requests · ${formatTokensPerSec(stats.tokens_per_sec)}`,
              )}
        </p>
      </div>
    </div>
  )
}

/** 桶宽随格子走：逐小时一格 3600 秒，逐天一格 86400 秒——分位数没法从更细的桶合并。 */
export const TTFT_RANGES = {
  '24h': { hours: 24, slots: 24, granularity: 'hour' as CacheGranularity, bucketSecs: 3600 },
  '7d': { hours: 7 * 24, slots: 7, granularity: 'day' as CacheGranularity, bucketSecs: 86400 },
  '30d': { hours: 30 * 24, slots: 30, granularity: 'day' as CacheGranularity, bucketSecs: 86400 },
} as const

export type TtftRangeKey = keyof typeof TTFT_RANGES

/** 默认看近 24 小时：先看今天怎么样，7 天与 30 天是往回翻。 */
export const DEFAULT_TTFT_RANGE: TtftRangeKey = '24h'

export function useTtftSeries(range: TtftRangeKey, enabled = true) {
  const preset = TTFT_RANGES[range]
  const query = useQuery({
    queryKey: ['ttft-series', preset.hours, preset.bucketSecs],
    queryFn: () => getTtftSeries({ hours: preset.hours, bucketSecs: preset.bucketSecs }),
    enabled,
    refetchInterval: 60_000,
    placeholderData: keepPreviousData,
  })
  const slots = bucketTtftSeries(query.data?.points ?? [], preset.granularity, preset.slots)
  return {
    query,
    slots,
    granularity: preset.granularity,
    /** 整个窗口的分位与吞吐（后端对整窗口原始值算的，不是各格平均）。 */
    summary: query.data?.summary ?? null,
    /** 近 60 分钟。 */
    recent: query.data?.recent ?? null,
  }
}

/** 毫秒 → `842ms` / `4.0s`。 */
export function formatMs(ms: number | null): string {
  if (ms == null) return '—'
  if (ms < 1000) return `${ms}ms`
  return `${(ms / 1000).toFixed(1)}s`
}

/** 吞吐 → `42 tok/s`；没有可算的请求时 `—`。 */
export function formatTokensPerSec(tps: number | null | undefined): string {
  if (tps == null) return '—'
  return `${tps >= 100 ? Math.round(tps) : tps.toFixed(1)} tok/s`
}

function slotReadout(
  slot: TtftSlot,
  granularity: CacheGranularity,
  t: (zh: string, en: string) => string,
  locale: string,
): { when: string; axis: string; value: string; detail: string } {
  const d = new Date(slot.ts * 1000)
  const p = (n: number) => String(n).padStart(2, '0')
  const day = `${d.getMonth() + 1}/${d.getDate()}`
  const when = granularity === 'hour' ? `${day} ${p(d.getHours())}:00` : day
  const axis = granularity === 'hour' ? `${p(d.getHours())}:00` : day
  if (!slot.hasTraffic) {
    return { when, axis, value: '—', detail: t('这个时段没有请求', 'No requests in this period') }
  }
  return {
    when,
    axis,
    value: formatMs(slot.p50Ms),
    detail: t(
      `p95 ${formatMs(slot.p95Ms)} · 平均 ${formatMs(slot.avgMs)} · ${slot.count.toLocaleString(locale)} 次 · ${formatTokensPerSec(slot.tokensPerSec)}`,
      `p95 ${formatMs(slot.p95Ms)} · avg ${formatMs(slot.avgMs)} · ${slot.count.toLocaleString(locale)} requests · ${formatTokensPerSec(slot.tokensPerSec)}`,
    ),
  }
}

function tickStep(slots: number): number {
  return Math.max(1, Math.ceil(slots / 7))
}

function TtftColumns({
  slots,
  granularity,
  refetching = false,
  className,
}: {
  slots: TtftSlot[]
  granularity: CacheGranularity
  refetching?: boolean
  className?: string
}) {
  const { t, locale } = useI18n()
  const [active, setActive] = useState<number | null>(null)
  const step = tickStep(slots.length)
  const readouts = slots.map((s) => slotReadout(s, granularity, t, locale))
  // 纵轴按 p95 定顶，p50 的柱子才不会被 p95 的刻度线顶出图外。
  const maxP95 = Math.max(0, ...slots.filter((s) => s.hasTraffic).map((s) => s.p95Ms))
  const yMax = maxP95 > 0 ? Math.ceil(maxP95 / 1000) * 1000 : 5000

  return (
    <div className={cn('transition-opacity', refetching && 'opacity-60', className)}>
      <div className="flex gap-2">
        <div className="flex h-40 w-8 shrink-0 flex-col justify-between py-0 text-end text-2xs text-muted-foreground tabular-nums">
          <span className="-translate-y-1/2">{formatMs(yMax)}</span>
          <span>{formatMs(yMax / 2)}</span>
          <span className="translate-y-1/2">0</span>
        </div>

        <div className="min-w-0 flex-1">
          <div className="relative h-40">
            {[0, 50, 100].map((pct) => (
              <div
                key={pct}
                aria-hidden
                className="absolute inset-x-0 border-t border-border"
                style={{ bottom: `${pct}%` }}
              />
            ))}
            <div className="absolute inset-0 flex items-end">
              {slots.map((slot, i) => {
                // 柱子是 p50，柱顶上方一道短横线是 p95：两者的距离就是长尾有多长。
                const heightPct = slot.hasTraffic && yMax > 0
                  ? Math.min(100, (slot.p50Ms / yMax) * 100)
                  : null
                const p95Pct = slot.hasTraffic && yMax > 0
                  ? Math.min(100, (slot.p95Ms / yMax) * 100)
                  : null
                return (
                  <div
                    key={slot.ts}
                    role="img"
                    tabIndex={0}
                    aria-label={`${readouts[i].when} · ${readouts[i].value} · ${readouts[i].detail}`}
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
                    {heightPct == null ? (
                      <span
                        aria-hidden
                        className="relative h-0.5 w-full max-w-6 rounded-full bg-muted-foreground/24"
                      />
                    ) : (
                      <span
                        aria-hidden
                        className="relative w-full max-w-6 rounded-t bg-chart-2"
                        style={{ height: `max(0.125rem, ${heightPct}%)` }}
                      />
                    )}
                    {p95Pct != null && (
                      <span
                        aria-hidden
                        className="absolute left-1/2 h-0.5 w-full max-w-6 -translate-x-1/2 rounded-full bg-chart-2/50"
                        style={{ bottom: `${p95Pct}%` }}
                      />
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
                    'w-max max-w-[min(16rem,100%)]',
                    anchor === 'center' && '-translate-x-1/2',
                  )}
                  style={
                    anchor === 'start'
                      ? { left: 0 }
                      : anchor === 'end'
                        ? { right: 0 }
                        : { left: `${pos * 100}%` }
                  }
                >
                  <p className="flex items-baseline gap-1.5 tabular-nums">
                    <span className="font-semibold">{readouts[active].value}</span>
                    <span className="text-muted-foreground">{readouts[active].when}</span>
                  </p>
                  <p className="text-muted-foreground tabular-nums">{readouts[active].detail}</p>
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
                  {readouts[i].axis}
                </span>
              ) : null,
            )}
          </div>
        </div>
      </div>
    </div>
  )
}

function TtftTable({
  slots,
  granularity,
}: {
  slots: TtftSlot[]
  granularity: CacheGranularity
}) {
  const { t, locale } = useI18n()
  const rows = slots.filter((s) => s.hasTraffic)

  return (
    <div className="max-h-64 overflow-auto rounded-xl border">
      <table className="w-full text-xs">
        <thead className="sticky top-0 bg-surface-subtle">
          <tr className="[&>th]:h-7 [&>th]:border-b [&>th]:px-3 [&>th]:text-2xs [&>th]:font-medium [&>th]:text-muted-foreground">
            <th scope="col" className="text-start">
              {granularity === 'hour' ? t('时段', 'Hour') : t('日期', 'Day')}
            </th>
            <th scope="col" className="text-end">p50</th>
            <th scope="col" className="text-end">p95</th>
            <th scope="col" className="text-end">{t('平均', 'Avg')}</th>
            <th scope="col" className="text-end">{t('请求数', 'Requests')}</th>
            <th scope="col" className="text-end">{t('吞吐', 'Throughput')}</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((slot) => {
            const r = slotReadout(slot, granularity, t, locale)
            return (
              <tr key={slot.ts} className="[&>td]:border-b [&>td]:px-3 [&>td]:py-1.5 last:[&>td]:border-b-0">
                <td className="whitespace-nowrap tabular-nums">{r.when}</td>
                <td className="whitespace-nowrap text-end font-medium tabular-nums">{formatMs(slot.p50Ms)}</td>
                <td className="whitespace-nowrap text-end tabular-nums">{formatMs(slot.p95Ms)}</td>
                <td className="whitespace-nowrap text-end tabular-nums text-muted-foreground">{formatMs(slot.avgMs)}</td>
                <td className="whitespace-nowrap text-end tabular-nums">
                  {slot.count.toLocaleString(locale)}
                </td>
                <td className="whitespace-nowrap text-end tabular-nums">{formatTokensPerSec(slot.tokensPerSec)}</td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </div>
  )
}

/** 概览那一格里的迷你趋势（p50）。总宽固定、柱子按格数均分，理由见 CacheHitSparkline。 */
export function TtftSparkline({ slots, className }: { slots: TtftSlot[]; className?: string }) {
  const maxP50 = Math.max(0, ...slots.filter((s) => s.hasTraffic).map((s) => s.p50Ms))
  return (
    <span aria-hidden className={cn('flex h-5 w-20 max-w-full shrink-0 items-end gap-px', className)}>
      {slots.map((slot, i) => {
        const heightPct = slot.hasTraffic && maxP50 > 0
          ? Math.min(100, (slot.p50Ms / maxP50) * 100)
          : null
        const last = i === slots.length - 1
        return (
          <span
            key={slot.ts}
            className={cn('min-w-0 flex-1 rounded-t', heightPct == null ? 'bg-muted-foreground/24' : 'bg-chart-2')}
            style={{
              height: heightPct == null ? '0.125rem' : `max(0.125rem, ${heightPct}%)`,
              opacity: heightPct == null ? undefined : last ? 1 : 0.4,
            }}
          />
        )
      })}
    </span>
  )
}

export function TtftTrendDialog({
  open,
  onOpenChange,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const { t, locale } = useI18n()
  const titleRef = useRef<HTMLHeadingElement>(null)
  const [range, setRange] = useState<TtftRangeKey>(DEFAULT_TTFT_RANGE)
  const [view, setView] = useState<'chart' | 'table'>('chart')
  const { query, slots, granularity, summary, recent } = useTtftSeries(range, open)
  const hasTraffic = slots.some((s) => s.hasTraffic)
  const preset = TTFT_RANGES[range]

  const rangeLabel: Record<TtftRangeKey, string> = {
    '24h': t('近 24 小时', 'Last 24 hours'),
    '7d': t('近 7 天', 'Last 7 days'),
    '30d': t('近 30 天', 'Last 30 days'),
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogPopup className="max-w-3xl" initialFocus={titleRef}>
        <DialogHeader className="border-b bg-muted/32 p-4 sm:p-5">
          <div className="flex items-center gap-3 pr-8">
            <Avatar>
              <AvatarFallback><TimerIcon /></AvatarFallback>
            </Avatar>
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-center gap-2">
                <DialogTitle ref={titleRef} tabIndex={-1}>
                  {t('首字时延趋势', 'TTFT trend')}
                </DialogTitle>
                <Badge variant="info" aria-live="polite">{rangeLabel[range]}</Badge>
                {query.isFetching && !query.isPending && <Spinner />}
              </div>
              <DialogDescription className="mt-1">
                {t(
                  '上游首个 token 到达的耗时，按 p50 / p95 看（仅统计成功请求）；吞吐是首字之后的输出速度。',
                  'Time to first token from upstream as p50 / p95 (successful requests only); throughput is the output speed after the first token.',
                )}
              </DialogDescription>
            </div>
          </div>
        </DialogHeader>

        <DialogPanel className="space-y-3 p-4 pt-3 sm:p-5 sm:pt-3">
          <div className="flex flex-wrap items-center justify-between gap-2">
            <ToggleGroup
              value={[range]}
              onValueChange={(values) => {
                const next = values[values.length - 1]
                if (next && next in TTFT_RANGES) setRange(next as TtftRangeKey)
              }}
              variant="outline"
              aria-label={t('回看跨度', 'Time range')}
            >
              {(Object.keys(TTFT_RANGES) as TtftRangeKey[]).map((key, i) => (
                <Fragment key={key}>
                  {i > 0 && <ToggleGroupSeparator />}
                  <ToggleGroupItem value={key} aria-label={rangeLabel[key]}>
                    {key}
                  </ToggleGroupItem>
                </Fragment>
              ))}
            </ToggleGroup>

            <ToggleGroup
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

          {/* 汇总条：左边整个窗口，右边近 1 小时——「现在」和「基线」并排，一眼看出今天是不是变慢了。 */}
          <section className="grid gap-2 sm:grid-cols-2">
            <SummaryCard
              label={rangeLabel[range]}
              stats={summary}
              t={t}
              locale={locale}
            />
            <SummaryCard
              label={t('近 1 小时', 'Last hour')}
              stats={recent}
              t={t}
              locale={locale}
            />
          </section>

          {query.error ? (
            <Alert variant="error">
              <AlertTitle>{t('读取失败', 'Failed to load')}</AlertTitle>
              <AlertDescription>{extractError(query.error)}</AlertDescription>
            </Alert>
          ) : query.isPending ? (
            <Skeleton className="h-48 w-full rounded-xl" />
          ) : !hasTraffic ? (
            <Empty>
              <EmptyHeader>
                <EmptyMedia variant="icon"><TimerIcon /></EmptyMedia>
                <EmptyTitle>{t('这段时间没有请求', 'No requests in this period')}</EmptyTitle>
                <EmptyDescription>
                  {t(
                    '换个更长的跨度，或先跑几条请求。',
                    'Try a longer range, or send some requests first.',
                  )}
                </EmptyDescription>
              </EmptyHeader>
            </Empty>
          ) : view === 'chart' ? (
            <TtftColumns
              slots={slots}
              granularity={granularity}
              refetching={query.isFetching && !query.isPending}
            />
          ) : (
            <TtftTable slots={slots} granularity={granularity} />
          )}

          <p className="text-2xs leading-4 text-muted-foreground">
            {t(
              '柱子是 p50，柱顶上方的短横线是 p95，两者的距离就是长尾。空着的格子是那个时段没有成功请求。请求明细只保留 30 天。',
              'Bars are p50; the short line above each bar is p95, and the gap between them is the tail. A gap means no successful requests in that period. Request logs are kept for 30 days.',
            )}
          </p>

          <UsageBreakdown hours={preset.hours} kind="latency" />
        </DialogPanel>
      </DialogPopup>
    </Dialog>
  )
}
