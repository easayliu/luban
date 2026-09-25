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
import { ChartLegend } from '@/components/chart-legend'
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
    // 标签 / 数值 / 说明三行堆叠，而不是数值与说明并排 baseline 对齐：并排时两枚块一个装满、
    // 一个只有「无请求」，同一行的内容长度差把整行拉得参差；堆叠后两枚等高。
    // 大数字不带 `tabular-nums`——等宽数位是给需要竖向对齐的数字列用的，单独一个大号读数
    // 用等宽反而显得松散。
    return (
      <div className="rounded-xl border bg-muted/32 px-4 py-3">
        <p className="text-2xs font-medium text-muted-foreground">{label}</p>
        <p className="mt-1 text-2xl font-semibold leading-none">
          {empty ? '—' : formatPercent(cacheHitRate(p.input_tokens, p.cached_tokens))}
        </p>
        <p className="mt-1.5 text-2xs text-muted-foreground tabular-nums">
          {empty ? t('无请求', 'No requests') : cacheSplitText(p, t)}
        </p>
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
      <DialogPopup size="lg" initialFocus={titleRef}>
        <DialogHeader variant="panel">
          {/* 时间范围放在头部右端，和标题同一行——它影响这个弹窗里的全部内容，是「这张卡看的是
              哪一段」，不是正文里的某个局部开关。放在正文时它会和「图表/表格」「按模型/按账号」
              三组同款胶囊竖着排成一摞，谁管什么范围完全读不出来。
              `pr-12` 给右上角那枚关闭按钮让位（它在 end-2、宽 32px，占掉右边 40px）。 */}
          <div className="flex flex-wrap items-start justify-between gap-x-4 gap-y-3 pr-12">
          <div className="flex min-w-0 items-center gap-3">
            <Avatar>
              <AvatarFallback><DatabaseZapIcon /></AvatarFallback>
            </Avatar>
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-center gap-2">
                <DialogTitle ref={titleRef} tabIndex={-1}>
                  {t('缓存命中率趋势', 'Cache hit rate trend')}
                </DialogTitle>
                {/* 标题旁不再挂「近 24 小时」：右上角的范围切换已经标着选中的那一档，下面汇总卡的标签
                    又写了一遍，三处说同一个范围。 */}
                {query.isFetching && !query.isPending && <Spinner />}
              </div>
              <DialogDescription className="mt-1">
                {t(
                  '整个调度池按 token 加权计算，不是各账号命中率的平均值。',
                  'Token-weighted across the whole scheduling pool, not an average of per-account rates.',
                )}
              </DialogDescription>
            </div>
          </div>
            <ToggleGroup
              className="shrink-0"
              value={[range]}
              onValueChange={(values) => {
                const next = values[values.length - 1]
                if (next && next in CACHE_RANGES) setRange(next as CacheRangeKey)
              }}
              variant="outline"
              aria-label={t('时间范围', 'Time range')}
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
          </div>
        </DialogHeader>

        <DialogPanel className="space-y-3">
          {/* 左边整个窗口，右边近 1 小时：「现在」和「基线」并排。 */}
          <section className="grid gap-2 sm:grid-cols-2">
            {card(rangeLabel[range], summary)}
            {card(t('近 1 小时', 'Last hour'), recent)}
          </section>

          {/* 图例贴着图表上沿，右边是同一份数据的呈现形式切换。
              图例只在真画了图时出现——换成表格视图时颜色不再承载任何信息。 */}
          <div className="flex flex-wrap items-center justify-between gap-2">
            {view === 'chart' && hasTraffic && !query.isPending && !query.error ? (
              <ChartLegend
                items={[
                  { swatch: 'bg-chart-1', label: t('命中 ×0.1', 'Cached ×0.1') },
                  { swatch: 'bg-chart-1/40', label: t('写入 ×1.25', 'Written ×1.25') },
                  { swatch: 'bg-muted-foreground/24', label: t('未缓存 ×1', 'Uncached ×1') },
                ]}
                hint={t(
                  '命中率就是颜色最深那一段的高度。写入多、命中少，说明前缀每轮都在变；两段都少，说明客户端没有设置缓存断点。',
                  'The hit rate is the height of the darkest segment. Much written but little cached means the prefix changes every turn; little of both means the client sets no cache breakpoints.',
                )}
              />
            ) : (
              <span />
            )}
            <ToggleGroup
              className="shrink-0"
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
                    '有请求时命中率才有意义。换一个更长的时间范围，或先发几条请求。',
                    'A hit rate is only meaningful with traffic. Try a longer range, or send some requests first.',
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

          {/* 颜色的含义交给图例、计价倍率写进图例标签、诊断提示收进图例末尾那枚 info，
              这里只剩图例说不了的那两件事。原来这段是 100 字的 10px 灰字，铺满整个弹窗宽度。 */}
          <p className="text-2xs leading-4 text-muted-foreground">
            {t(
              '空着的格子表示该时段没有请求；柱子的深浅表示该格的 token 量。请求明细只保留 30 天。',
              'A gap means no traffic in that period; a bar’s opacity reflects its token volume. Request logs are kept for 30 days.',
            )}
          </p>

          <UsageBreakdown hours={preset.hours} kind="cache" />
        </DialogPanel>
      </DialogPopup>
    </Dialog>
  )
}
