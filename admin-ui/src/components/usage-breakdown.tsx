import { Fragment, useState } from 'react'
import { keepPreviousData, useQuery } from '@tanstack/react-query'
import { getUsageBreakdown, type BreakdownBy, type BreakdownRow } from '@/api/metrics'
import { useI18n } from '@/lib/i18n'
import { cacheHitRate, cn, extractError, formatPercent, formatTokens, formatUsd } from '@/lib/utils'
import { formatMs, formatTokensPerSec } from '@/components/ttft-trend-dialog'
import { AccountTierBadge } from '@/components/credential-shared'
import { RequestLookupDialog, type UsageDrillFilter } from '@/components/request-lookup-dialog'
import {
  COMPACT_TABLE_BODY_CLASS,
  COMPACT_TABLE_HEADER_CLASS,
  COMPACT_TABLE_LINK_CLASS,
} from '@/components/usage-shared'
import { Button } from '@/components/ui/button'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Skeleton } from '@/components/ui/skeleton'
import { Spinner } from '@/components/ui/spinner'
import { Table, TableBody, TableCell, TableFooter, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { ToggleGroup, ToggleGroupItem, ToggleGroupSeparator } from '@/components/ui/toggle-group'
import { Hint } from '@/components/ui/tooltip'

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
  // 行属于哪个维度以返回数据为准：切换维度后新数据回来前，表里还是旧维度的行
  // （keepPreviousData），按按钮上的 by 解读会把模型名当账号 id 去查。
  const rowsBy = query.data?.by ?? by
  // 点某一行 → 打开请求明细，带上模型 / 账号与时间范围：看到某个模型 p95 飙高之后，
  // 排查是两步而不是再去请求日志页翻。
  const [drill, setDrill] = useState<UsageDrillFilter | null>(null)
  const openRow = (row: BreakdownRow) => setDrill(
    rowsBy === 'model'
      ? { model: row.key, label: row.label || row.key, hours }
      : { credId: Number(row.key), label: row.label, hours },
  )
  const byLabel: Record<BreakdownBy, string> = {
    model: t('按模型', 'By model'),
    account: t('按账号', 'By account'),
  }

  return (
    <section className="space-y-2" aria-label={t('分维度明细', 'Breakdown')}>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div className="flex items-center gap-2">
          {/* 提到 text-sm semibold：原来是 text-xs 的灰字，比同一行那两枚胶囊按钮还小还淡，
              标题被自己的控件压了过去。现在是「区块标题 14px / 表头 10px」两档。 */}
          <h3 className="text-sm font-semibold tracking-tight">
            {kind === 'latency' ? t('延迟拆分', 'Latency breakdown') : t('缓存命中拆分', 'Cache hit breakdown')}
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
        <p className="rounded-xl border px-4 py-6 text-center text-xs text-muted-foreground">
          {t('所选时间范围内没有请求', 'No requests in this period')}
        </p>
      ) : (
        // 滚动容器当 Table 的外壳传进 `render`，sticky 表头才粘得住，见 COMPACT_TABLE_HEADER_CLASS。
        // 缓存表 9 列，数字列左右留白从 12px 收到 8px，弹窗里一屏放得下「API 价」；首列（名称）不动。
        <Table
          className={cn('text-xs', kind === 'cache' && '[&_td:not(:first-child)]:px-2 [&_th:not(:first-child)]:px-2')}
          render={<div className={cn('max-h-64 overflow-auto rounded-xl border transition-opacity', query.isFetching && !query.isPending && 'opacity-60')} />}
        >
          <TableHeader className={COMPACT_TABLE_HEADER_CLASS}>
            <TableRow>
              <TableHead>{rowsBy === 'model' ? t('模型', 'Model') : t('账号', 'Account')}</TableHead>
              <TableHead className="text-end">{t('请求', 'Requests')}</TableHead>
              {kind === 'latency' ? (
                <>
                  <TableHead className="text-end">p50</TableHead>
                  <TableHead className="text-end">p95</TableHead>
                  <TableHead className="text-end">{t('吞吐', 'Throughput')}</TableHead>
                  <TableHead className="text-end">{t('命中率', 'Hit rate')}</TableHead>
                </>
              ) : (
                <>
                  <TableHead className="text-end">{t('命中率', 'Hit rate')}</TableHead>
                  {/* token 各列按官方定价表的顺序与叫法：基础输入 → 5m 缓存写 → 1h 缓存写 → 缓存读 → 输出，
                      与价目从便宜到贵、从左到右对得上。表头默认大写，TTL 前缀得保留小写（5m 不是 5M）。 */}
                  <TableHead className="text-end">{t('输入', 'Input')}</TableHead>
                  <TableHead className="text-end normal-case">{t('5m 缓存写', '5m cache writes')}</TableHead>
                  <TableHead className="text-end normal-case">{t('1h 缓存写', '1h cache writes')}</TableHead>
                  <TableHead className="text-end">{t('缓存读', 'Cache reads')}</TableHead>
                  <TableHead className="text-end">{t('输出', 'Output')}</TableHead>
                  <TableHead className="text-end">{t('API 价', 'API cost')}</TableHead>
                </>
              )}
            </TableRow>
          </TableHeader>
          <TableBody className={COMPACT_TABLE_BODY_CLASS}>
            {rows.map((row) => (
              <BreakdownTr key={row.key} row={row} kind={kind} locale={locale} onOpen={() => openRow(row)} />
            ))}
          </TableBody>
          {/* 合计是表格的一部分，落在 tfoot 里、和「API 价」那一列对齐；口径收进悬浮层。 */}
          {kind === 'cache' && query.data && (
            <TableFooter className="[&>tr>*]:border-t [&>tr>*]:bg-surface-subtle [&>tr>*]:px-3 [&>tr>*]:py-1.5 [&>tr>*]:font-medium">
              <tr>
                <th className="text-start" scope="row">{t('合计', 'Total')}</th>
                {/* 缓存表共 9 列：模型 / 请求 / 命中率 / 输入 / 5m 缓存写 / 1h 缓存写 / 缓存读 / 输出 / API 价。
                    合计只有「API 价」这一列有值，中间 7 列留空。 */}
                <td colSpan={7} />
                <td className="text-end">
                  <Hint
                    label={t(
                      '按官方 API 价目估算的等价费用，含输入、输出与缓存；缓存写 5m 按输入价 1.25 倍、1h 按 2 倍计，缓存读按各模型的折扣价计。',
                      'Equivalent cost at official API list prices, covering input, output and cache; 5m cache writes at 1.25× input, 1h at 2×, cache reads at each model\'s discounted rate.',
                    )}
                  >
                    <span className="cursor-help rounded-sm tabular-nums underline decoration-dotted underline-offset-4">
                      {formatUsd(query.data.cost_usd_total)}
                    </span>
                  </Hint>
                </td>
              </tr>
            </TableFooter>
          )}
        </Table>
      )}
      <RequestLookupDialog
        open={drill != null}
        onOpenChange={(open) => { if (!open) setDrill(null) }}
        filter={drill ?? undefined}
      />
    </section>
  )
}

function BreakdownTr({
  row,
  kind,
  locale,
  onOpen,
}: {
  row: BreakdownRow
  kind: 'latency' | 'cache'
  locale: string
  onOpen: () => void
}) {
  const { t } = useI18n()
  const rate = cacheHitRate(row.cache.input_tokens, row.cache.cached_tokens)
  const uncached = Math.max(0, row.cache.input_tokens - row.cache.cached_tokens - row.cache.written_tokens)
  const written5m = Math.max(0, row.cache.written_tokens - row.cache.written_1h_tokens)
  const noLatency = row.latency.count === 0
  return (
    <TableRow className="cursor-pointer hover:bg-muted/40" onClick={onOpen}>
      {/* 宽度上限写在里面这层：auto 布局的表格不认 td 的 max-width，名称 + 徽章会把列撑开或溢出。 */}
      <TableCell>
        <span className="flex w-max min-w-0 max-w-64 items-center gap-1.5">
          {/* 「点击查看该组最近的请求」原先挂在整行上；行里不再套提示，并到这枚名称按钮上，顺带补上被截断的全名。 */}
          <Hint
            label={row.label
              ? `${row.label}\n${t('点击查看该组最近的请求', 'Click to view recent requests in this group')}`
              : t('点击查看该组最近的请求', 'Click to view recent requests in this group')}
          >
            <Button variant="link" className={COMPACT_TABLE_LINK_CLASS} onClick={onOpen}>
              {row.label || '—'}
            </Button>
          </Hint>
          {/* 按账号拆时带套餐：Max 号和 Pro 号上游的排队本来就不同，混着比延迟没有意义。 */}
          {/* 与账号卡片同一枚徽章：组织号带楼形图标、套餐名去掉重复的组织前缀（Team Standard → Standard）。 */}
          {row.plan && <span className="shrink-0"><AccountTierBadge cred={row.plan} size="xs" /></span>}
        </span>
      </TableCell>
      <TableCell className="whitespace-nowrap text-end tabular-nums">{row.requests.toLocaleString(locale)}</TableCell>
      {kind === 'latency' ? (
        <>
          <TableCell className={cn('whitespace-nowrap text-end font-medium tabular-nums', noLatency && 'text-muted-foreground')}>
            {noLatency ? '—' : formatMs(row.latency.p50_ms)}
          </TableCell>
          <TableCell className={cn('whitespace-nowrap text-end tabular-nums', noLatency && 'text-muted-foreground')}>
            {noLatency ? '—' : formatMs(row.latency.p95_ms)}
          </TableCell>
          <TableCell className="whitespace-nowrap text-end tabular-nums">{formatTokensPerSec(row.latency.tokens_per_sec)}</TableCell>
          <TableCell className="whitespace-nowrap text-end tabular-nums">{formatPercent(rate)}</TableCell>
        </>
      ) : (
        <>
          <TableCell className="whitespace-nowrap text-end font-medium tabular-nums">{formatPercent(rate)}</TableCell>
          <TableCell className="whitespace-nowrap text-end tabular-nums">{formatTokens(uncached)}</TableCell>
          <TableCell className="whitespace-nowrap text-end tabular-nums">{formatTokens(written5m)}</TableCell>
          <TableCell className="whitespace-nowrap text-end tabular-nums">{formatTokens(row.cache.written_1h_tokens)}</TableCell>
          <TableCell className="whitespace-nowrap text-end tabular-nums">{formatTokens(row.cache.cached_tokens)}</TableCell>
          <TableCell className="whitespace-nowrap text-end tabular-nums">{formatTokens(row.output_tokens)}</TableCell>
          <TableCell className="whitespace-nowrap text-end tabular-nums">{formatUsd(row.cost_usd)}</TableCell>
        </>
      )}
    </TableRow>
  )
}
