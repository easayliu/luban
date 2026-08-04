import { useInfiniteQuery } from '@tanstack/react-query'
import { ChevronDownIcon, RefreshCwIcon, ScrollTextIcon } from 'lucide-react'
import { listCredentialUsage, type Credential, type UsageLog } from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import {
  cn,
  displayCredentialLabel,
  extractError,
  formatFullTime,
  formatUsd,
} from '@/lib/utils'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Avatar, AvatarFallback } from '@/components/ui/avatar'
import { Badge, type BadgeProps } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Card, CardPanel } from '@/components/ui/card'
import {
  Dialog,
  DialogClose,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogPanel,
  DialogPopup,
  DialogTitle,
} from '@/components/ui/dialog'
import {
  Empty,
  EmptyDescription,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
} from '@/components/ui/empty'
import { Skeleton } from '@/components/ui/skeleton'
import { Spinner } from '@/components/ui/spinner'
import {
  Table,
  TableBody,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table'

/** 每页条数。后端上限 200，取 50 是一屏滚两下的量，翻页按钮就在表格下面。 */
const PAGE_SIZE = 50

/** 状态码 → 徽章配色。2xx 成功、429 单独一档（额度问题，不是错误），其余 4xx/5xx 红。 */
function statusVariant(status: number): BadgeProps['variant'] {
  if (status >= 200 && status < 300) return 'success'
  if (status === 429) return 'warning'
  if (status >= 400) return 'error'
  return 'secondary'
}

/**
 * 流水时间：日期 + 到秒的时钟。
 *
 * 这里不用 `relativeTime`——明细是按秒排布的，一屏「3 分钟前」看不出先后；也不用
 * `formatFullTime`，那个只到分钟，同一分钟内的连发请求会显示成同一时刻。
 */
function logTime(unixSecs: number): string {
  const d = new Date(unixSecs * 1000)
  const p = (n: number) => String(n).padStart(2, '0')
  return `${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`
}

/** 数字列的空值：**缺失**（没嗅探到 usage）与 0 是两回事，缺失显示 `—`。 */
function num(v: number | null | undefined, locale: string): string {
  return v == null ? '—' : v.toLocaleString(locale)
}

export function CredentialUsageDialog({
  cred,
  open,
  onOpenChange,
}: {
  cred: Credential
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const { t, language, locale } = useI18n()
  const credentialLabel = displayCredentialLabel(cred.label, language)
  const usage = useInfiniteQuery({
    queryKey: ['credential-usage', cred.id],
    queryFn: ({ pageParam }) =>
      listCredentialUsage(cred.id, { limit: PAGE_SIZE, before: pageParam }),
    initialPageParam: undefined as number | undefined,
    // 不足一页即到底；否则拿最后一条的 id 当游标（后端取 id 严格小于它的记录）。
    getNextPageParam: (lastPage: UsageLog[]) =>
      lastPage.length < PAGE_SIZE ? undefined : lastPage[lastPage.length - 1]?.id,
    enabled: open,
  })

  const rows = usage.data?.pages.flat() ?? []
  const loadedCount = rows.length.toLocaleString(locale)
  // 「已加载这些条」的合计，不是账号累计——流水只保留近 30 天，而卡片上的累计来自终身账本。
  const loadedCost = rows.reduce((sum, log) => sum + (log.cost_usd ?? 0), 0)
  const status = usage.isPending
    ? { label: t('正在读取', 'Loading'), variant: 'secondary' as const }
    : usage.error
      ? { label: t('读取失败', 'Failed to load'), variant: 'error' as const }
      : {
          label: t(`已加载 ${loadedCount} 条`, `${loadedCount} loaded`),
          variant: 'info' as const,
        }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogPopup className="max-w-4xl">
        <DialogHeader>
          <div className="flex items-start gap-3 pr-8">
            <Avatar>
              <AvatarFallback><ScrollTextIcon /></AvatarFallback>
            </Avatar>
            <div className="min-w-0 flex-1">
              <DialogTitle>{t('请求明细', 'Request log')}</DialogTitle>
              <DialogDescription className="mt-1 truncate" title={credentialLabel}>
                {credentialLabel}
              </DialogDescription>
              <div className="mt-2 flex flex-wrap items-center gap-2">
                <Badge variant="outline">#{cred.id}</Badge>
                <Badge variant={status.variant} aria-live="polite">{status.label}</Badge>
                {usage.isFetching && !usage.isPending && <Spinner />}
              </div>
            </div>
          </div>
        </DialogHeader>

        <DialogPanel className="space-y-4">
          <Card>
            <CardPanel className="flex flex-wrap items-baseline justify-between gap-x-6 gap-y-2">
              <div className="min-w-0">
                <p className="text-muted-foreground text-xs">
                  {t('已加载记录合计花费', 'Cost of loaded records')}
                </p>
                <p className="mt-1 font-semibold text-sm tabular-nums">{formatUsd(loadedCost)}</p>
              </div>
              <p className="min-w-0 flex-1 text-muted-foreground text-xs sm:text-right">
                {t(
                  '明细只保留最近 30 天，卡片上的累计花费来自终身账本，两者对不上是正常的。',
                  'The request log only keeps the last 30 days, while the total cost on the card comes from the lifetime ledger — the two are not meant to match.',
                )}
              </p>
            </CardPanel>
          </Card>

          {usage.isPending ? (
            <div
              className="space-y-2"
              role="status"
              aria-label={t('正在读取请求明细', 'Loading request log')}
            >
              {Array.from({ length: 4 }, (_, index) => (
                <Skeleton key={index} className="h-9 w-full" />
              ))}
            </div>
          ) : usage.error ? (
            <Alert variant="error">
              <AlertTitle>{t('请求明细读取失败', 'Failed to load request log')}</AlertTitle>
              <AlertDescription>
                <p className="break-words">{extractError(usage.error, language)}</p>
                <Button
                  type="button"
                  size="sm"
                  variant="destructive-outline"
                  onClick={() => { void usage.refetch() }}
                >
                  <RefreshCwIcon />
                  {t('重试', 'Retry')}
                </Button>
              </AlertDescription>
            </Alert>
          ) : rows.length === 0 ? (
            <Empty className="py-10">
              <EmptyHeader>
                <EmptyMedia variant="icon"><ScrollTextIcon /></EmptyMedia>
                <EmptyTitle className="text-base">{t('暂无请求记录', 'No requests yet')}</EmptyTitle>
                <EmptyDescription>
                  {t(
                    '此账号转发一次请求后就会出现在这里。',
                    'Requests forwarded through this account will show up here.',
                  )}
                </EmptyDescription>
              </EmptyHeader>
            </Empty>
          ) : (
            <>
              {/* 列多，窄屏靠横向滚动而不是换行——一条记录被折成两行反而看不出对应关系。 */}
              <div className="-mx-1 overflow-x-auto px-1">
                <UsageTable rows={rows} />
              </div>
              <div className="flex items-center justify-center">
                {usage.hasNextPage ? (
                  <Button
                    type="button"
                    variant="outline"
                    size="sm"
                    loading={usage.isFetchingNextPage}
                    onClick={() => { void usage.fetchNextPage() }}
                  >
                    <ChevronDownIcon />
                    {t(`再加载 ${PAGE_SIZE} 条`, `Load ${PAGE_SIZE} more`)}
                  </Button>
                ) : (
                  <p className="text-muted-foreground text-xs">
                    {t('已经到底了', 'End of the log')}
                  </p>
                )}
              </div>
            </>
          )}
        </DialogPanel>

        <DialogFooter>
          <Button
            type="button"
            variant="outline"
            className="mr-auto"
            disabled={usage.isFetching}
            onClick={() => { void usage.refetch() }}
          >
            <RefreshCwIcon className={usage.isFetching ? 'animate-spin' : undefined} />
            {t('刷新', 'Refresh')}
          </Button>
          <DialogClose render={<Button variant="outline" />}>{t('关闭', 'Close')}</DialogClose>
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  )
}

function UsageTable({ rows }: { rows: UsageLog[] }) {
  const { t, language, locale } = useI18n()
  return (
    <Table variant="card" className="text-xs">
      <TableHeader>
        <TableRow>
          <TableHead className="whitespace-nowrap">{t('时间', 'Time')}</TableHead>
          <TableHead className="whitespace-nowrap">{t('状态', 'Status')}</TableHead>
          <TableHead className="whitespace-nowrap">{t('模型', 'Model')}</TableHead>
          <TableHead className="whitespace-nowrap text-right">{t('输入', 'Input')}</TableHead>
          <TableHead className="whitespace-nowrap text-right">{t('输出', 'Output')}</TableHead>
          <TableHead className="whitespace-nowrap text-right">{t('缓存写/读', 'Cache w/r')}</TableHead>
          <TableHead className="whitespace-nowrap text-right">{t('首字 / 总耗时', 'TTFT / total')}</TableHead>
          <TableHead className="whitespace-nowrap text-right">{t('花费', 'Cost')}</TableHead>
          <TableHead className="whitespace-nowrap">{t('设备', 'Device')}</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {rows.map((log) => {
          const deviceId = log.device_id ?? '—'
          // 伪设备的 `sim:` 前缀要留着——截掉就和真实 device_id 混在一起分不出来了。
          const deviceShort = log.device_id
            ? log.device_id.startsWith('sim:')
              ? `sim:${log.device_id.slice(4, 12)}`
              : log.device_id.slice(0, 8)
            : '—'
          const ms = (v: number | null) => (v == null ? '—' : `${v.toLocaleString(locale)}ms`)
          return (
            <TableRow key={log.id}>
              <TableCell
                className="whitespace-nowrap tabular-nums"
                title={`${formatFullTime(log.ts, language)} · ${log.path}`}
              >
                {logTime(log.ts)}
              </TableCell>
              <TableCell>
                <Badge variant={statusVariant(log.status)} size="sm" className="tabular-nums">
                  {log.status}
                </Badge>
              </TableCell>
              <TableCell className="max-w-40 truncate" title={log.model ?? undefined}>
                {log.model ?? '—'}
              </TableCell>
              <TableCell className="whitespace-nowrap text-right tabular-nums">
                {num(log.input_tokens, locale)}
              </TableCell>
              <TableCell className="whitespace-nowrap text-right tabular-nums">
                {num(log.output_tokens, locale)}
              </TableCell>
              <TableCell className="whitespace-nowrap text-right tabular-nums">
                {num(log.cache_creation_tokens, locale)} / {num(log.cache_read_tokens, locale)}
              </TableCell>
              <TableCell className="whitespace-nowrap text-right tabular-nums">
                {ms(log.ttft_ms)} / {ms(log.total_ms)}
              </TableCell>
              <TableCell
                className={cn(
                  'whitespace-nowrap text-right tabular-nums',
                  log.cost_usd == null && 'text-muted-foreground',
                )}
                title={log.cost_usd == null
                  ? t('模型未在价目表内，无法估算', 'Model is not in the price table, cost cannot be estimated')
                  : undefined}
              >
                {log.cost_usd == null ? '—' : formatUsd(log.cost_usd)}
              </TableCell>
              <TableCell className="whitespace-nowrap font-mono text-xs" title={deviceId}>
                {deviceShort}
              </TableCell>
            </TableRow>
          )
        })}
      </TableBody>
    </Table>
  )
}
