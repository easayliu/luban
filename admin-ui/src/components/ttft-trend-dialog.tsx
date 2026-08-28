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

const TTFT_RANGES = {
  '24h': { hours: 24, slots: 24, granularity: 'hour' as CacheGranularity },
  '7d': { hours: 7 * 24, slots: 7, granularity: 'day' as CacheGranularity },
  '30d': { hours: 30 * 24, slots: 30, granularity: 'day' as CacheGranularity },
} as const

type TtftRangeKey = keyof typeof TTFT_RANGES

export const DEFAULT_TTFT_RANGE: TtftRangeKey = '7d'

export function useTtftSeries(range: TtftRangeKey, enabled = true) {
  const preset = TTFT_RANGES[range]
  const query = useQuery({
    queryKey: ['ttft-series', preset.hours],
    queryFn: () => getTtftSeries(preset.hours),
    enabled,
    refetchInterval: 60_000,
    placeholderData: keepPreviousData,
  })
  const slots = bucketTtftSeries(query.data?.points ?? [], preset.granularity, preset.slots)
  return { query, slots, granularity: preset.granularity }
}

export function aggregateTtft(slots: TtftSlot[]): { avgMs: number | null; totalCount: number } {
  const totalCount = slots.reduce((sum, s) => sum + s.count, 0)
  if (totalCount === 0) return { avgMs: null, totalCount: 0 }
  const weightedSum = slots.reduce((sum, s) => sum + s.avgMs * s.count, 0)
  return { avgMs: Math.round(weightedSum / totalCount), totalCount }
}

/** 毫秒 → `842ms` / `4.0s`。 */
export function formatMs(ms: number | null): string {
  if (ms == null) return '—'
  if (ms < 1000) return `${ms}ms`
  return `${(ms / 1000).toFixed(1)}s`
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
    value: formatMs(slot.avgMs),
    detail: t(`${slot.count.toLocaleString(locale)} 次请求`, `${slot.count.toLocaleString(locale)} requests`),
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
  const maxAvg = Math.max(0, ...slots.filter((s) => s.hasTraffic).map((s) => s.avgMs))
  const yMax = maxAvg > 0 ? Math.ceil(maxAvg / 1000) * 1000 : 5000

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
                const heightPct = slot.hasTraffic && yMax > 0
                  ? Math.min(100, (slot.avgMs / yMax) * 100)
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
    <div className="max-h-64 overflow-y-auto rounded-xl border">
      <table className="w-full text-xs">
        <thead className="sticky top-0 bg-muted/96 backdrop-blur-sm">
          <tr className="[&>th]:h-7 [&>th]:border-b [&>th]:px-3 [&>th]:text-2xs [&>th]:font-medium [&>th]:text-muted-foreground">
            <th scope="col" className="text-start">
              {granularity === 'hour' ? t('时段', 'Hour') : t('日期', 'Day')}
            </th>
            <th scope="col" className="text-end">{t('平均首字', 'Avg TTFT')}</th>
            <th scope="col" className="text-end">{t('请求数', 'Requests')}</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((slot) => {
            const r = slotReadout(slot, granularity, t, locale)
            return (
              <tr key={slot.ts} className="[&>td]:border-b [&>td]:px-3 [&>td]:py-1.5 last:[&>td]:border-b-0">
                <td className="whitespace-nowrap tabular-nums">{r.when}</td>
                <td className="whitespace-nowrap text-end font-medium tabular-nums">{r.value}</td>
                <td className="whitespace-nowrap text-end tabular-nums">
                  {slot.count.toLocaleString(locale)}
                </td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </div>
  )
}

/** 概览那一格里的迷你趋势。 */
export function TtftSparkline({ slots, className }: { slots: TtftSlot[]; className?: string }) {
  const maxAvg = Math.max(0, ...slots.filter((s) => s.hasTraffic).map((s) => s.avgMs))
  return (
    <span aria-hidden className={cn('flex h-5 items-end gap-px', className)}>
      {slots.map((slot, i) => {
        const heightPct = slot.hasTraffic && maxAvg > 0
          ? Math.min(100, (slot.avgMs / maxAvg) * 100)
          : null
        const last = i === slots.length - 1
        return (
          <span
            key={slot.ts}
            className={cn('w-1.5 rounded-t', heightPct == null ? 'bg-muted-foreground/24' : 'bg-chart-2')}
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
  const { query, slots, granularity } = useTtftSeries(range, open)
  const agg = aggregateTtft(slots)
  const hasTraffic = slots.some((s) => s.hasTraffic)

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
                  '上游首个 token 到达的平均耗时（仅统计成功请求）。',
                  'Average time to first token from upstream (successful requests only).',
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

          <section className="flex flex-wrap items-baseline gap-x-3 gap-y-1 rounded-xl border bg-muted/32 px-3 py-2.5 sm:px-4">
            <p className="text-2xs font-medium text-muted-foreground">{rangeLabel[range]}</p>
            <p className="text-2xl font-semibold leading-none">{formatMs(agg.avgMs)}</p>
            <p className="text-2xs text-muted-foreground tabular-nums">
              {t(
                `共 ${agg.totalCount.toLocaleString(locale)} 次请求`,
                `${agg.totalCount.toLocaleString(locale)} requests total`,
              )}
            </p>
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
              '首字时延（TTFT）= 上游返回第一个 token 的耗时。空着的格子是那个时段没有成功请求。请求明细只保留 30 天。',
              'TTFT = time to first token from upstream. A gap means no successful requests in that period. Request logs are kept for 30 days.',
            )}
          </p>
        </DialogPanel>
      </DialogPopup>
    </Dialog>
  )
}
