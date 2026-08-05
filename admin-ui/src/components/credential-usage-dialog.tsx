import { useRef, useState } from 'react'
import { keepPreviousData, useQuery, useQueryClient } from '@tanstack/react-query'
import { RefreshCwIcon, ScrollTextIcon } from 'lucide-react'
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
import {
  Pagination,
  PaginationContent,
  PaginationItem,
  PaginationNext,
  PaginationPrevious,
} from '@/components/ui/pagination'
import {
  Select,
  SelectItem,
  SelectPopup,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
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

/** 每页条数可选值。后端上限 200。 */
const PAGE_SIZES = [25, 50, 100] as const

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
  const qc = useQueryClient()
  const credentialLabel = displayCredentialLabel(cred.label, language)
  const [pageSize, setPageSize] = useState<number>(PAGE_SIZES[0])
  const [page, setPage] = useState(0)
  /**
   * 本轮翻页的锚点（首次响应给出，之后每页原样带回）。
   *
   * 放 ref 而不是 state，也不进 queryKey：它在一轮翻页里恒定，进 key 只会让第一页在拿到
   * 锚点后再白白重取一次。整轮钉住同一个快照，翻页期间新到的请求不会把记录往后挤。
   */
  const anchor = useRef<number | null>(null)

  const usage = useQuery({
    queryKey: ['credential-usage', cred.id, page, pageSize],
    queryFn: async () => {
      const res = await listCredentialUsage(cred.id, {
        limit: pageSize,
        offset: page * pageSize,
        until: anchor.current ?? undefined,
      })
      if (anchor.current == null) anchor.current = res.anchor
      return res
    },
    enabled: open,
    // 翻页时先留着上一页，避免表格整块闪成骨架屏。
    placeholderData: keepPreviousData,
  })

  const total = usage.data?.total ?? 0
  const totalPages = Math.max(1, Math.ceil(total / pageSize))
  const rows = usage.data?.logs ?? []
  // 页码越界（改了每页条数、或刷新后记录变少）时退回最后一页，而不是显示一页空白。
  const currentPage = Math.min(page, totalPages - 1)
  if (currentPage !== page) setPage(currentPage)
  const firstIndex = currentPage * pageSize + 1
  const lastIndex = currentPage * pageSize + rows.length

  /** 重新取一轮：丢掉锚点回到第一页，于是能看到刚发生的请求。 */
  const reload = () => {
    anchor.current = null
    setPage(0)
    void qc.invalidateQueries({ queryKey: ['credential-usage', cred.id] })
  }

  const handleOpenChange = (next: boolean) => {
    // 关掉即重置，下次打开是新的一轮（新锚点、第一页）。
    if (!next) {
      anchor.current = null
      setPage(0)
    }
    onOpenChange(next)
  }

  const status = usage.isPending
    ? { label: t('正在读取', 'Loading'), variant: 'secondary' as const }
    : usage.error
      ? { label: t('读取失败', 'Failed to load'), variant: 'error' as const }
      : {
          label: t(`共 ${total.toLocaleString(locale)} 条`, `${total.toLocaleString(locale)} total`),
          variant: 'info' as const,
        }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      {/* 比设备弹窗宽一档：这张表有十列，4xl 下 UA 与模型两列会被挤到只剩几个字符。 */}
      <DialogPopup className="max-w-5xl">
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
                  {t('明细合计花费', 'Cost across the log')}
                </p>
                <p className="mt-1 font-semibold text-sm tabular-nums">
                  {formatUsd(usage.data?.total_cost ?? 0)}
                </p>
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
              {Array.from({ length: 6 }, (_, index) => (
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
          ) : total === 0 ? (
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
              <div
                className={cn(
                  '-mx-1 overflow-x-auto px-1 transition-opacity',
                  usage.isFetching && 'opacity-60',
                )}
                aria-busy={usage.isFetching}
              >
                <UsageTable rows={rows} />
              </div>

              <div className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-3 text-xs sm:grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)]">
                <p className="min-w-0 text-muted-foreground tabular-nums">
                  {t(
                    `第 ${firstIndex}–${lastIndex} 条，共 ${total.toLocaleString(locale)} 条`,
                    `${firstIndex}–${lastIndex} of ${total.toLocaleString(locale)}`,
                  )}
                </p>
                <div className="row-start-1 flex items-center gap-2 justify-self-end sm:col-start-3">
                  <span className="whitespace-nowrap text-muted-foreground">{t('每页', 'Per page')}</span>
                  <Select
                    items={PAGE_SIZES.map((size) => ({ value: size, label: String(size) }))}
                    value={pageSize}
                    onValueChange={(value) => {
                      if (value == null) return
                      // 每页条数一变，原来的页码就没有意义了，回到第一页。
                      setPageSize(Number(value))
                      setPage(0)
                    }}
                  >
                    <SelectTrigger size="sm" aria-label={t('每页条数', 'Rows per page')}>
                      <SelectValue />
                    </SelectTrigger>
                    <SelectPopup>
                      {PAGE_SIZES.map((size) => (
                        <SelectItem key={size} value={size}>{size}</SelectItem>
                      ))}
                    </SelectPopup>
                  </Select>
                </div>

              {totalPages > 1 && (
                <Pagination className="col-span-2 row-start-2 justify-center sm:col-span-1 sm:col-start-2 sm:row-start-1">
                  <PaginationContent>
                    <PaginationItem>
                      <PaginationPrevious
                        href="#"
                        className={cn(
                          (usage.isFetching || currentPage === 0) && 'pointer-events-none opacity-50',
                        )}
                        aria-disabled={usage.isFetching || currentPage === 0}
                        onClick={(event) => {
                          event.preventDefault()
                          setPage((current) => Math.max(0, current - 1))
                        }}
                      />
                    </PaginationItem>
                    <PaginationItem>
                      <span className="whitespace-nowrap px-2 text-foreground text-xs tabular-nums" aria-live="polite">
                        {t(
                          `第 ${currentPage + 1} / ${totalPages} 页`,
                          `Page ${currentPage + 1} of ${totalPages}`,
                        )}
                      </span>
                    </PaginationItem>
                    <PaginationItem>
                      <PaginationNext
                        href="#"
                        className={cn(
                          (usage.isFetching || currentPage >= totalPages - 1) && 'pointer-events-none opacity-50',
                        )}
                        aria-disabled={usage.isFetching || currentPage >= totalPages - 1}
                        onClick={(event) => {
                          event.preventDefault()
                          setPage((current) => Math.min(totalPages - 1, current + 1))
                        }}
                      />
                    </PaginationItem>
                  </PaginationContent>
                </Pagination>
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
            onClick={reload}
            title={t('回到第一页并拉取最新记录', 'Jump back to the first page and fetch the newest records')}
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
          {/* 两份 UA 合在一列：绝大多数请求原样转发，两者是同一串，占两列纯浪费宽度。
              只在被改写时才多显示一行出站那份，见 UaCell。 */}
          <TableHead
            className="min-w-52 whitespace-nowrap"
            title={t(
              '来访客户端自报的 User-Agent；被改写时另起一行显示实际发给上游的那份',
              'User-Agent reported by the incoming client; when rewritten, the one actually sent upstream is shown on a second line',
            )}
          >
            {t('客户端 UA', 'Client UA')}
          </TableHead>
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
              <TableCell
                className="whitespace-nowrap text-right tabular-nums"
                title={log.sse_aggregated
                  ? t(
                      '这条请求本来是非流式，被改成流式发给上游后聚合回整段返回：首字耗时是上游的首字节，客户端则是在末尾一次性收到全部内容。',
                      'This request arrived non-streaming and was sent upstream as a stream, then reassembled into a single response: TTFT is the upstream first byte, while the client received everything at the end.',
                    )
                  : undefined}
              >
                {ms(log.ttft_ms)} / {ms(log.total_ms)}
                {log.sse_aggregated && (
                  <div className="text-muted-foreground text-[10px] font-normal">
                    {t('非流转流', 'stream-upgraded')}
                  </div>
                )}
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
              <UaCell ua={log.ua} uaOut={log.ua_out} />
            </TableRow>
          )
        })}
      </TableBody>
    </Table>
  )
}

/**
 * UA 单元格：来访那份为主，被改写时另起一行显示实际发给上游的那份。
 *
 * **折行而不是截断**：真实 UA 动辄六七十字符
 * （`claude-cli/2.1.219 (external, local-agent, agent-sdk/0.3.219)`），截断后靠 title 悬停才能
 * 看全，而这一列的用处正是「一眼认出谁在发」。折行让它整串可见，代价只是行高。
 *
 * 两者相同（原样转发）时只显示一份——绝大多数请求都是这种，重复显示等于白占半屏。
 * 都为空的是 0.2.60 之前的旧记录，不是「没有客户端」。
 */
function UaCell({ ua, uaOut }: { ua: string | null; uaOut: string | null }) {
  const { t } = useI18n()
  const rewritten = !!uaOut && uaOut !== ua
  return (
    <TableCell className="min-w-52 max-w-72 whitespace-normal break-all align-top">
      <span className={cn(!ua && 'text-muted-foreground')}>
        {ua ?? (uaOut
          ? t('无（luban 自身发起）', 'None (sent by luban itself)')
          : '—')}
      </span>
      {rewritten && (
        <span
          className="mt-0.5 block text-muted-foreground"
          title={t('实际发给上游的 User-Agent', 'User-Agent actually sent upstream')}
        >
          → {uaOut}
        </span>
      )}
    </TableCell>
  )
}
