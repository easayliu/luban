import { Fragment, useRef, useState } from 'react'
import { keepPreviousData, useQuery } from '@tanstack/react-query'
import { BarChart3Icon, DatabaseZapIcon, TableIcon } from 'lucide-react'
import { getCacheSeries } from '@/api/metrics'
import { useI18n } from '@/lib/i18n'
import {
  bucketCacheSeries,
  cacheHitRate,
  extractError,
  formatPercent,
  type CacheGranularity,
} from '@/lib/utils'
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
import {
  CacheHitColumns,
  CacheHitTable,
  cacheSplitText,
} from '@/components/cache-hit-chart'
import { UsageBreakdown } from '@/components/usage-breakdown'

export const CACHE_RANGES = {
  '24h': { hours: 24, slots: 24, granularity: 'hour' as CacheGranularity, bucketSecs: 3600 },
  '7d': { hours: 7 * 24, slots: 7, granularity: 'day' as CacheGranularity, bucketSecs: 86400 },
  '30d': { hours: 30 * 24, slots: 30, granularity: 'day' as CacheGranularity, bucketSecs: 86400 },
} as const

export type CacheRangeKey = keyof typeof CACHE_RANGES

/** 默认看近 24 小时：先看今天怎么样，7 天与 30 天是往回翻。 */
export const DEFAULT_CACHE_RANGE: CacheRangeKey = '24h'

export function useCacheSeries(range: CacheRangeKey, enabled = true) {
  const preset = CACHE_RANGES[range]
  const query = useQuery({
    queryKey: ['cache-series', preset.hours, preset.bucketSecs],
    queryFn: () => getCacheSeries({ hours: preset.hours, bucketSecs: preset.bucketSecs }),
    enabled,
    refetchInterval: 60_000,
    placeholderData: keepPreviousData,
  })
  const slots = bucketCacheSeries(query.data?.points ?? [], preset.granularity, preset.slots)
  return {
    query,
    slots,
    granularity: preset.granularity,
    /** 整个窗口的三段合计。 */
    summary: query.data?.summary ?? null,
    /** 近 60 分钟的三段合计。 */
    recent: query.data?.recent ?? null,
  }
}

export function CacheHitTrendDialog({
  open,
  onOpenChange,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const { t } = useI18n()
  const titleRef = useRef<HTMLHeadingElement>(null)
  const [range, setRange] = useState<CacheRangeKey>(DEFAULT_CACHE_RANGE)
  const [view, setView] = useState<'chart' | 'table'>('chart')
  const { query, slots, granularity, summary, recent } = useCacheSeries(range, open)
  const hasTraffic = slots.some((s) => s.hasTraffic)
  const preset = CACHE_RANGES[range]
  const card = (label: string, p: { input_tokens: number; cached_tokens: number; written_tokens: number } | null) => {
    const empty = !p || p.input_tokens === 0
    return (
      <div className="rounded-xl border bg-muted/32 px-3 py-2.5 sm:px-4">
        <p className="text-2xs font-medium text-muted-foreground">{label}</p>
        <div className="mt-1 flex flex-wrap items-baseline gap-x-3 gap-y-1">
          <p className="text-2xl font-semibold leading-none tabular-nums">
            {empty ? '—' : formatPercent(cacheHitRate(p.input_tokens, p.cached_tokens))}
          </p>
          <p className="text-2xs text-muted-foreground tabular-nums">
            {empty ? t('无请求', 'No requests') : cacheSplitText(p, t)}
          </p>
        </div>
      </div>
    )
  }

  const rangeLabel: Record<CacheRangeKey, string> = {
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
              <AvatarFallback><DatabaseZapIcon /></AvatarFallback>
            </Avatar>
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-center gap-2">
                <DialogTitle ref={titleRef} tabIndex={-1}>
                  {t('缓存命中率趋势', 'Cache hit rate trend')}
                </DialogTitle>
                <Badge variant="info" aria-live="polite">{rangeLabel[range]}</Badge>
                {query.isFetching && !query.isPending && <Spinner />}
              </div>
              <DialogDescription className="mt-1">
                {t(
                  '全池按 token 加权，不是各账号命中率的平均。',
                  'Pooled and token-weighted, not an average of per-account rates.',
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
                if (next && next in CACHE_RANGES) setRange(next as CacheRangeKey)
              }}
              variant="outline"
              aria-label={t('回看跨度', 'Time range')}
            >
              {(Object.keys(CACHE_RANGES) as CacheRangeKey[]).map((key, i) => (
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

          {/* 左边整个窗口，右边近 1 小时：「现在」和「基线」并排。 */}
          <section className="grid gap-2 sm:grid-cols-2">
            {card(rangeLabel[range], summary)}
            {card(t('近 1 小时', 'Last hour'), recent)}
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
                <EmptyMedia variant="icon"><DatabaseZapIcon /></EmptyMedia>
                <EmptyTitle>{t('这段时间没有请求', 'No requests in this period')}</EmptyTitle>
                <EmptyDescription>
                  {t(
                    '命中率要有请求才谈得上。换个更长的跨度，或先跑几条请求。',
                    'A hit rate needs traffic. Try a longer range, or send some requests first.',
                  )}
                </EmptyDescription>
              </EmptyHeader>
            </Empty>
          ) : view === 'chart' ? (
            <CacheHitColumns
              slots={slots}
              granularity={granularity}
              refetching={query.isFetching && !query.isPending}
            />
          ) : (
            <CacheHitTable slots={slots} granularity={granularity} />
          )}

          <p className="text-2xs leading-4 text-muted-foreground">
            {t(
              '一根柱子叠三段：深色是命中（按十分之一计价）、浅色是写入（按 1.25 倍计价）、灰色是裸算。命中率就是深色那段的高度；写入多命中少是前缀每轮在变，两段都少是客户端没标断点。空着的格子是那个时段没有请求；柱子的深浅是那一格的 token 体量。请求明细只保留 30 天。',
              'Each bar stacks three parts: dark is cached (billed at a tenth), light is written (billed at 1.25×), grey is uncached. The hit rate is the dark part\'s height; lots written but little cached means the prefix changes every turn, little of both means the client sets no breakpoints. A gap means no traffic in that period; a bar\'s opacity reflects its token volume. Request logs are kept for 30 days.',
            )}
          </p>

          <UsageBreakdown hours={preset.hours} kind="cache" />
        </DialogPanel>
      </DialogPopup>
    </Dialog>
  )
}
