import { Fragment, useState } from 'react'
import { keepPreviousData, useQuery } from '@tanstack/react-query'
import { getUsageBreakdown, type BreakdownBy, type BreakdownRow } from '@/api/metrics'
import { useI18n } from '@/lib/i18n'
import { cacheHitRate, cn, extractError, formatPercent, formatTokens } from '@/lib/utils'
import { formatMs, formatTokensPerSec } from '@/components/ttft-trend-dialog'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Skeleton } from '@/components/ui/skeleton'
import { Spinner } from '@/components/ui/spinner'
import { ToggleGroup, ToggleGroupItem, ToggleGroupSeparator } from '@/components/ui/toggle-group'

/**
 * 趋势对话框下面那张「谁在拖后腿」的表：这段时间按模型或按账号拆开，按请求数降序取前 12。
 * 延迟对话框里看 p50 / p95 / 吞吐，缓存对话框里看三段 token 与命中率——同一个接口，两套列。
 */
export function UsageBreakdown({ hours, kind }: { hours: number; kind: 'latency' | 'cache' }) {
  const { t, locale } = useI18n()
  const [by, setBy] = useState<BreakdownBy>('model')
  const query = useQuery({
    queryKey: ['usage-breakdown', hours, by],
    queryFn: () => getUsageBreakdown(hours, by),
    refetchInterval: 60_000,
    placeholderData: keepPreviousData,
  })
  const rows = query.data?.rows ?? []
  const byLabel: Record<BreakdownBy, string> = {
    model: t('按模型', 'By model'),
    account: t('按账号', 'By account'),
  }

  return (
    <section className="space-y-2" aria-label={t('分维度明细', 'Breakdown')}>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex items-center gap-2">
          <h3 className="text-xs font-medium text-muted-foreground">
            {kind === 'latency' ? t('谁在拖慢', 'Who is slow') : t('谁没命中', 'Who misses the cache')}
          </h3>
          {query.isFetching && !query.isPending && <Spinner />}
        </div>
        <ToggleGroup
          value={[by]}
          onValueChange={(values) => {
            const next = values[values.length - 1]
            if (next === 'model' || next === 'account') setBy(next)
          }}
          variant="outline"
          aria-label={t('拆分维度', 'Breakdown dimension')}
        >
          {(['model', 'account'] as BreakdownBy[]).map((key, i) => (
            <Fragment key={key}>
              {i > 0 && <ToggleGroupSeparator />}
              <ToggleGroupItem value={key} aria-label={byLabel[key]}>{byLabel[key]}</ToggleGroupItem>
            </Fragment>
          ))}
        </ToggleGroup>
      </div>

      {query.error ? (
        <Alert variant="error">
          <AlertTitle>{t('读取失败', 'Failed to load')}</AlertTitle>
          <AlertDescription>{extractError(query.error)}</AlertDescription>
        </Alert>
      ) : query.isPending ? (
        <Skeleton className="h-32 w-full rounded-xl" />
      ) : rows.length === 0 ? (
        <p className="rounded-xl border px-3 py-6 text-center text-xs text-muted-foreground">
          {t('这段时间没有请求', 'No requests in this period')}
        </p>
      ) : (
        <div className={cn('max-h-64 overflow-y-auto rounded-xl border transition-opacity', query.isFetching && !query.isPending && 'opacity-60')}>
          <table className="w-full text-xs">
            <thead className="sticky top-0 bg-surface-subtle">
              <tr className="[&>th]:h-7 [&>th]:border-b [&>th]:px-3 [&>th]:text-2xs [&>th]:font-medium [&>th]:text-muted-foreground">
                <th scope="col" className="text-start">{by === 'model' ? t('模型', 'Model') : t('账号', 'Account')}</th>
                <th scope="col" className="text-end">{t('请求', 'Requests')}</th>
                {kind === 'latency' ? (
                  <>
                    <th scope="col" className="text-end">p50</th>
                    <th scope="col" className="text-end">p95</th>
                    <th scope="col" className="text-end">{t('吞吐', 'Throughput')}</th>
                    <th scope="col" className="text-end">{t('命中率', 'Hit rate')}</th>
                  </>
                ) : (
                  <>
                    <th scope="col" className="text-end">{t('命中率', 'Hit rate')}</th>
                    <th scope="col" className="text-end">{t('命中', 'Cached')}</th>
                    <th scope="col" className="text-end">{t('写入', 'Written')}</th>
                    <th scope="col" className="text-end">{t('裸算', 'Uncached')}</th>
                  </>
                )}
              </tr>
            </thead>
            <tbody>
              {rows.map((row) => <BreakdownTr key={row.key} row={row} kind={kind} locale={locale} />)}
            </tbody>
          </table>
        </div>
      )}
    </section>
  )
}

function BreakdownTr({ row, kind, locale }: { row: BreakdownRow; kind: 'latency' | 'cache'; locale: string }) {
  const rate = cacheHitRate(row.cache.input_tokens, row.cache.cached_tokens)
  const uncached = Math.max(0, row.cache.input_tokens - row.cache.cached_tokens - row.cache.written_tokens)
  const noLatency = row.latency.count === 0
  return (
    <tr className="[&>td]:border-b [&>td]:px-3 [&>td]:py-1.5 last:[&>td]:border-b-0">
      <td className="max-w-48 truncate" title={row.label}>{row.label || '—'}</td>
      <td className="whitespace-nowrap text-end tabular-nums">{row.requests.toLocaleString(locale)}</td>
      {kind === 'latency' ? (
        <>
          <td className={cn('whitespace-nowrap text-end font-medium tabular-nums', noLatency && 'text-muted-foreground')}>
            {noLatency ? '—' : formatMs(row.latency.p50_ms)}
          </td>
          <td className={cn('whitespace-nowrap text-end tabular-nums', noLatency && 'text-muted-foreground')}>
            {noLatency ? '—' : formatMs(row.latency.p95_ms)}
          </td>
          <td className="whitespace-nowrap text-end tabular-nums">{formatTokensPerSec(row.latency.tokens_per_sec)}</td>
          <td className="whitespace-nowrap text-end tabular-nums">{formatPercent(rate)}</td>
        </>
      ) : (
        <>
          <td className="whitespace-nowrap text-end font-medium tabular-nums">{formatPercent(rate)}</td>
          <td className="whitespace-nowrap text-end tabular-nums">{formatTokens(row.cache.cached_tokens)}</td>
          <td className="whitespace-nowrap text-end tabular-nums">{formatTokens(row.cache.written_tokens)}</td>
          <td className="whitespace-nowrap text-end tabular-nums">{formatTokens(uncached)}</td>
        </>
      )}
    </tr>
  )
}
