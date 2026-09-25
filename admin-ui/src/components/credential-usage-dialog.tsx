import { useRef, useState } from 'react'
import { keepPreviousData, useQuery, useQueryClient } from '@tanstack/react-query'
import { RefreshCwIcon, ScrollTextIcon } from 'lucide-react'
import { listCredentialUsage, type Credential, type UsageLog } from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import { useMediaQuery } from '@/lib/use-media-query'
import {
  cn,
  displayCredentialLabel,
  extractError,
  formatFullTime,
  formatUsd,
  parseSessionKey,
} from '@/lib/utils'
import { RequestLookupDialog } from '@/components/request-lookup-dialog'
import { RequestIdChip, statusVariant } from '@/components/usage-shared'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Avatar, AvatarFallback } from '@/components/ui/avatar'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
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
  TableCaption,
  TableCell,
  TableHead,
  TableHeader,
  TableRow,
} from '@/components/ui/table'

/** 每页条数可选值。后端上限 200。 */
const PAGE_SIZES = [25, 50, 100] as const

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
  const titleRef = useRef<HTMLHeadingElement>(null)
  const [pageSize, setPageSize] = useState<number>(PAGE_SIZES[0])
  const [page, setPage] = useState(0)
  /**
   * 本轮翻页的锚点（首次响应给出，之后每页原样带回）。
   *
   * 放 ref 而不是 state，也不进 queryKey：它在一轮翻页里恒定，进 key 只会让第一页在拿到
   * 锚点后再白白重取一次。整轮钉住同一个快照，翻页期间新到的请求不会把记录往后挤。
   */
  const anchor = useRef<number | null>(null)
  const wideEnoughForTable = useMediaQuery('(min-width: 64rem)')
  /**
   * 点开了哪条请求的查询弹窗（请求 id）。
   *
   * 记 id 而不是一个开关：换一条就是换一次挂载，弹窗按初值直接开查，不必再同步内部状态
   * （见 [RequestLookupDialog] 的 `initialId`）。关掉即置空，不常驻。
   */
  const [lookupId, setLookupId] = useState<string | null>(null)

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
  const retentionNoteId = `credential-usage-retention-${cred.id}`

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
      <DialogPopup size="full" initialFocus={titleRef}>
        <DialogHeader variant="panel">
          <div className="flex items-center gap-3 pr-8">
            <Avatar>
              <AvatarFallback><ScrollTextIcon /></AvatarFallback>
            </Avatar>
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-center gap-2">
                <DialogTitle ref={titleRef} tabIndex={-1}>{t('请求明细', 'Request log')}</DialogTitle>
                {/* 只在读取中 / 失败时挂徽章：「共 N 条」底部分页那行已经写着，标题旁再报一遍是重复。 */}
                {(usage.isPending || usage.error) && (
                  <Badge variant={status.variant} aria-live="polite">{status.label}</Badge>
                )}
                {usage.isFetching && !usage.isPending && <Spinner />}
              </div>
              <DialogDescription className="mt-1 flex min-w-0 items-center gap-1.5">
                <span className="truncate" title={credentialLabel}>{credentialLabel}</span>
                <span aria-hidden="true">·</span>
                <span className="shrink-0 tabular-nums">#{cred.id}</span>
              </DialogDescription>
            </div>
          </div>
        </DialogHeader>

        <DialogPanel className="space-y-3">
          <section className="grid gap-2 rounded-xl border bg-muted/32 px-4 py-3 sm:grid-cols-[auto_minmax(0,1fr)] sm:items-center sm:gap-5">
            <div className="flex items-baseline justify-between gap-4 sm:block">
              <p className="text-2xs font-medium text-muted-foreground">
                {t('近 30 天明细花费', 'Request cost, last 30 days')}
              </p>
              <p className="font-semibold text-sm tabular-nums sm:mt-0.5">
                {usage.data ? formatUsd(usage.data.total_cost) : '—'}
              </p>
            </div>
            <p id={retentionNoteId} className="min-w-0 text-2xs leading-4 text-muted-foreground sm:text-right">
              {/* 「近 30 天」左边的标签已经写着，这里只说它和累计花费为什么对不上。这个对话框也从详情页
                  打开，那里没有卡片，所以不说「卡片上的」。 */}
              {t(
                '累计花费来自终身账本，与这里的明细合计不一定相等。',
                'The total cost comes from the lifetime ledger and is not expected to match this sum.',
              )}
            </p>
          </section>

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
              {/* 十列的宽表在窄屏只能横向拖着看，等于没法用；lg 以下换成一条一张的堆叠卡片。
                  这里用媒体查询二选一而不是 CSS 隐藏：一页最多 100 条，两套都建出来是双倍节点。 */}
              {wideEnoughForTable ? (
                <UsageTable
                  rows={rows}
                  credentialLabel={credentialLabel}
                  descriptionId={retentionNoteId}
                  loading={usage.isFetching}
                  onLookup={setLookupId}
                />
              ) : (
                <UsageCards
                  rows={rows}
                  credentialLabel={credentialLabel}
                  loading={usage.isFetching}
                  onLookup={setLookupId}
                />
              )}

              <div className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-3 border-t pt-3 text-xs sm:grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)]">
                <p className="min-w-0 text-muted-foreground tabular-nums">
                  {t(
                    `第 ${firstIndex}–${lastIndex} 条，共 ${total.toLocaleString(locale)} 条`,
                    `${firstIndex}–${lastIndex} of ${total.toLocaleString(locale)}`,
                  )}
                </p>
                {/* `col-start-2` 不能省：这一格是「行确定、列自动」，而 CSS 网格会把这类项**先于**纯自动项
                  放置（放置算法第 2 步早于第 4 步），不钉列它就会抢到第 1 列、和左边那句计数调个个儿。
                  sm 起三列时它本来就有 `col-start-3`，只有窄屏这一档踩坑。 */}
                <div className="col-start-2 row-start-1 flex items-center gap-2 justify-self-end sm:col-start-3">
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
                    <SelectTrigger size="sm" className="w-auto min-w-20" aria-label={t('每页条数', 'Rows per page')}>
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
                        render={<Button variant="ghost" disabled={usage.isFetching || currentPage === 0} />}
                        aria-disabled={usage.isFetching || currentPage === 0}
                        onClick={() => {
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
                        render={<Button variant="ghost" disabled={usage.isFetching || currentPage >= totalPages - 1} />}
                        aria-disabled={usage.isFetching || currentPage >= totalPages - 1}
                        onClick={() => {
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
      {/* 点某一行的请求 id 开出来的查询弹窗：直接查那一条（`initialId`），输入框仍在，看完
          可以顺手换个 id 或贴一个会话 id 再查。没点过就不挂。 */}
      {lookupId && (
        <RequestLookupDialog
          open
          initialId={lookupId}
          onOpenChange={(next) => { if (!next) setLookupId(null) }}
        />
      )}
    </Dialog>
  )
}

/**
 * 窄屏下的请求明细：一条请求一张卡片。
 *
 * 字段顺序按排查时的读法排：先看什么时候、成没成、花了多少，再看模型与 token，
 * 最后才是耗时和来源。UA 只留一行截断——真要看全的场景基本都在桌面端。
 */
export function UsageCards({
  rows,
  credentialLabel,
  loading,
  onLookup,
  scroll = true,
}: {
  rows: UsageLog[]
  credentialLabel: string
  loading: boolean
  /** 对话框里限高、自己滚；详情页里整页滚，再套一层内滚动在手机上很难滑，传 false。 */
  scroll?: boolean
  /** 点某一行的请求 id：带着它开请求查询弹窗，见 [CredentialUsageDialog] 里那段。 */
  onLookup: (id: string) => void
}) {
  const { t, language, locale } = useI18n()
  const ms = (v: number | null) => (v == null ? '—' : `${v.toLocaleString(locale)}ms`)

  return (
    <ul
      className={cn('space-y-2', scroll && 'max-h-[26rem] overflow-y-auto overscroll-contain')}
      aria-label={t(`${credentialLabel} 的请求明细`, `Request log for ${credentialLabel}`)}
      aria-busy={loading}
    >
      {rows.map((log) => {
        const deviceShort = log.device_id
          ? log.device_id.startsWith('sim:')
            ? `sim:${log.device_id.slice(4, 12)}`
            : log.device_id.slice(0, 8)
          : '—'
        return (
          <li key={log.id} className="rounded-lg border bg-card px-4 py-2.5 text-xs">
            <div className="flex min-w-0 items-center gap-2">
              <span className="shrink-0 font-medium tabular-nums" title={formatFullTime(log.ts, language)}>
                {logTime(log.ts)}
              </span>
              <Badge variant={statusVariant(log.status)} size="sm" className="tabular-nums">
                {log.status}
              </Badge>
              <span
                className={cn(
                  'ml-auto shrink-0 font-medium tabular-nums',
                  log.cost_usd == null && 'font-normal text-muted-foreground',
                )}
              >
                {log.cost_usd == null ? '—' : formatUsd(log.cost_usd)}
              </span>
            </div>
            <p className="mt-1 truncate text-muted-foreground" title={log.model ?? undefined}>
              {log.model ?? '—'}
            </p>
            <dl className="mt-2 grid grid-cols-2 gap-x-4 gap-y-1.5">
              <LogFact label={t('输入 / 输出', 'In / out')}>
                {num(log.input_tokens, locale)} / {num(log.output_tokens, locale)}
              </LogFact>
              <LogFact label={t('缓存写 / 读', 'Cache w/r')}>
                {num(log.cache_creation_tokens, locale)} / {num(log.cache_read_tokens, locale)}
              </LogFact>
              <LogFact label={t('首字 / 总耗时', 'TTFT / total')}>
                {ms(log.ttft_ms)} / {ms(log.total_ms)}
                {log.sse_aggregated && (
                  <span className="ml-1 text-[10px] text-muted-foreground">
                    {t('非流转流', 'stream-upgraded')}
                  </span>
                )}
              </LogFact>
              <LogFact label={t('设备', 'Device')}>
                <span className="font-mono" title={deviceTitle(log)}>
                  {deviceShort}
                  {log.device_id_out && <span className="text-muted-foreground">→{log.device_id_out.slice(0, 8)}</span>}
                </span>
              </LogFact>
              <LogFact label={t('请求 ID', 'Request ID')}>
                <RequestIdChip id={log.request_id} onOpen={onLookup} />
              </LogFact>
              {/* 会话：模拟路径且没有设备身份的请求才有对话键，其余退到上游那个 session_id
                  （按槽位派生、对话之间复用），两者都在悬浮提示里。 */}
              <LogFact label={t('会话', 'Session')}>
                <span className="font-mono" title={sessionTitle(log)}>
                  {sessionShort(log, t)}
                </span>
              </LogFact>
            </dl>
            {(log.ua || log.ua_out) && (
              <p
                className="mt-2 truncate border-t pt-1.5 text-2xs text-muted-foreground"
                title={log.ua_out && log.ua_out !== log.ua ? `${log.ua ?? '—'}\n→ ${log.ua_out}` : (log.ua ?? undefined)}
              >
                {log.ua ?? t('无（luban 自身发起）', 'None (sent by luban itself)')}
              </p>
            )}
          </li>
        )
      })}
    </ul>
  )
}

function LogFact({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="min-w-0">
      <dt className="text-2xs text-muted-foreground">{label}</dt>
      <dd className="truncate tabular-nums">{children}</dd>
    </div>
  )
}

export function UsageTable({
  rows,
  credentialLabel,
  descriptionId,
  loading,
  onLookup,
}: {
  rows: UsageLog[]
  credentialLabel: string
  descriptionId: string
  loading: boolean
  /** 点某一行的请求 id：带着它开请求查询弹窗，见 [CredentialUsageDialog] 里那段。 */
  onLookup: (id: string) => void
}) {
  const { t, language, locale } = useI18n()
  return (
    <Table
      render={(
        <div
          className="max-h-[32rem] rounded-xl border bg-card outline-none overscroll-contain focus-visible:ring-2 focus-visible:ring-ring sm:max-h-[min(52vh,32rem)]"
          role="region"
          aria-label={t(`${credentialLabel} 的请求明细表`, `Request log table for ${credentialLabel}`)}
          aria-busy={loading}
          tabIndex={0}
        />
      )}
      className="min-w-[87rem] table-fixed text-xs"
      aria-describedby={descriptionId}
    >
      <TableCaption className="sr-only">
        {t(`${credentialLabel} 的请求明细`, `Request log for ${credentialLabel}`)}
      </TableCaption>
      <colgroup>
        <col className="w-[7.5rem]" />
        <col className="w-[4rem]" />
        <col className="w-[9rem]" />
        <col className="w-[4.25rem]" />
        <col className="w-[4.25rem]" />
        <col className="w-[6.5rem]" />
        <col className="w-[7.25rem]" />
        <col className="w-[5rem]" />
        <col className="w-[6.5rem]" />
        {/* 出站设备：上游实际看到的 device_id 前 8 位。 */}
        <col className="w-[6.5rem]" />
        {/* 请求 ID：尾 8 位加复制图标。固定布局下每列都得在这里登记，漏一列会把后面的列挤成 0 宽。 */}
        <col className="w-[8rem]" />
        <col className="w-[18rem]" />
      </colgroup>
      <TableHeader className="sticky top-0 z-10 bg-surface-subtle">
        <TableRow className="bg-muted/72 [&>th]:border-b [&>th]:text-2xs">
          <TableHead scope="colgroup" colSpan={3} className="h-7 text-center">{t('请求', 'Request')}</TableHead>
          <TableHead scope="colgroup" colSpan={3} className="h-7 text-center">Token</TableHead>
          <TableHead scope="colgroup" className="h-7 text-center">{t('性能', 'Performance')}</TableHead>
          <TableHead scope="colgroup" className="h-7 text-center">{t('费用', 'Billing')}</TableHead>
          <TableHead scope="colgroup" colSpan={4} className="h-7 text-center">{t('来源', 'Source')}</TableHead>
        </TableRow>
        <TableRow className="bg-muted/96">
          <TableHead className="whitespace-nowrap">{t('时间', 'Time')}</TableHead>
          <TableHead className="whitespace-nowrap">{t('状态', 'Status')}</TableHead>
          <TableHead className="whitespace-nowrap">{t('模型', 'Model')}</TableHead>
          <TableHead className="whitespace-nowrap text-right">{t('输入', 'Input')}</TableHead>
          <TableHead className="whitespace-nowrap text-right">{t('输出', 'Output')}</TableHead>
          <TableHead className="whitespace-nowrap text-right">{t('缓存写/读', 'Cache w/r')}</TableHead>
          <TableHead className="whitespace-nowrap text-right">{t('首字 / 总耗时', 'TTFT / total')}</TableHead>
          <TableHead className="whitespace-nowrap text-right">{t('花费', 'Cost')}</TableHead>
          <TableHead
            className="whitespace-nowrap"
            title={t('客户端请求自带的 device_id（设备绑定与设备上限都按它计算）', 'device_id carried by the inbound client request (device bindings and device limits are based on it)')}
          >
            {t('入站设备', 'Inbound device')}
          </TableHead>
          <TableHead
            className="whitespace-nowrap"
            title={t(
              '实际发给上游的 device_id（按账号派生）；上游给出的设备 ID 对应的是这一列。旧记录为空',
              'device_id actually sent upstream (derived per account); a device ID quoted by upstream corresponds to this column. Empty for older rows',
            )}
          >
            {t('出站设备', 'Outbound device')}
          </TableHead>
          <TableHead
            className="whitespace-nowrap"
            title={t(
              'luban 在响应头 X-Oneapi-Request-Id 中返回的请求 ID，New API 日志里叫 upstream_request_id；点击 ID 查看这条请求，点击旁边的图标复制',
              'Request ID luban returns in the X-Oneapi-Request-Id response header (upstream_request_id in New API logs); click the ID to look up the request, or the icon next to it to copy',
            )}
          >
            {t('请求 ID', 'Request ID')}
          </TableHead>
          {/* 两份 UA 合在一列：绝大多数请求原样转发，两者是同一串，占两列纯浪费宽度。
              只在被改写时才多显示一行出站那份，见 UaCell。 */}
          <TableHead
            className="min-w-52 whitespace-nowrap"
            title={t(
              '客户端自报的 User-Agent；被改写时，另起一行显示实际发给上游的 User-Agent',
              'User-Agent reported by the client; when rewritten, the one actually sent upstream is shown on a second line',
            )}
          >
            {t('客户端 UA', 'Client UA')}
          </TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {rows.map((log) => {
          // 伪设备的 `sim:` 前缀要留着——截掉就和真实 device_id 混在一起分不出来了。
          const deviceShort = log.device_id
            ? log.device_id.startsWith('sim:')
              ? `sim:${log.device_id.slice(4, 12)}`
              : log.device_id.slice(0, 8)
            : '—'
          const ms = (v: number | null) => (v == null ? '—' : `${v.toLocaleString(locale)}ms`)
          return (
            <TableRow key={log.id} className="[&>td]:px-2.5 [&>td]:py-2">
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
                  ? t('模型不在价目表内，无法估算花费', 'Model is not in the price table, so the cost cannot be estimated')
                  : undefined}
              >
                {log.cost_usd == null ? '—' : formatUsd(log.cost_usd)}
              </TableCell>
              <TableCell className="whitespace-nowrap font-mono text-xs" title={log.device_id ?? undefined}>
                {deviceShort}
              </TableCell>
              <TableCell className="whitespace-nowrap font-mono text-xs" title={log.device_id_out ?? undefined}>
                {log.device_id_out?.slice(0, 8) ?? '—'}
              </TableCell>
              <TableCell className="whitespace-nowrap">
                <RequestIdChip id={log.request_id} onOpen={onLookup} />
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
 * 真实 UA 动辄六七十字符。表格内最多展示两行，避免一条长 UA 把整行撑高；完整内容保留在
 * title 里。被改写时第二行显示实际发给上游的值。
 *
 * 两者相同（原样转发）时只显示一份——绝大多数请求都是这种，重复显示等于白占半屏。
 * 都为空的是 0.2.60 之前的旧记录，不是「没有客户端」。
 */
function UaCell({ ua, uaOut }: { ua: string | null; uaOut: string | null }) {
  const { t } = useI18n()
  const rewritten = !!uaOut && uaOut !== ua
  const incoming = ua ?? (uaOut
    ? t('无（luban 自身发起）', 'None (sent by luban itself)')
    : '—')
  return (
    <TableCell className="align-top leading-4" title={rewritten ? `${incoming}\n→ ${uaOut}` : incoming}>
      <span className={cn('block truncate', !ua && 'text-muted-foreground')}>
        {incoming}
      </span>
      {rewritten && (
        <span className="mt-0.5 block truncate text-muted-foreground">
          → {uaOut}
        </span>
      )}
    </TableCell>
  )
}

/**
 * 设备格的悬停全文：来访原始 id 与出站派生 id 各一行。上游侧（工单、封号通知）给出的
 * device_id 对的是第二行——第一行是客户端自己的 id，上游从没见过。
 */
/** 卡片里那一格会话：有对话键就显示「来源 + 前 8 位」，否则退到上游 session_id，都没有是 '—'。 */
function sessionShort(log: UsageLog, t: (zh: string, en: string) => string): string {
  if (log.session_key) {
    const { source, value } = parseSessionKey(log.session_key)
    return `${source === 'pfx' ? t('前缀', 'prefix') : t('自带', 'client')} ${value.slice(0, 8)}`
  }
  return log.session_id_in?.slice(0, 8) ?? log.session_id?.slice(0, 8) ?? '—'
}

/** 悬浮里两者都给全：对话键是这条对话的身份，session_id 是上游看到的那个（槽位会被复用）。 */
function sessionTitle(log: UsageLog): string | undefined {
  if (!log.session_key && !log.session_id && !log.session_id_in) return undefined
  return `key: ${log.session_key ?? '—'}\nin:  ${log.session_id_in ?? '—'}\nout: ${log.session_id ?? '—'}`
}

function deviceTitle(log: UsageLog): string | undefined {
  if (!log.device_id && !log.device_id_out) return undefined
  return `in:  ${log.device_id ?? '—'}\nout: ${log.device_id_out ?? '—'}`
}
