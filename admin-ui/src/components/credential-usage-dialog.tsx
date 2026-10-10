import { useRef, useState } from 'react'
import { keepPreviousData, useQuery, useQueryClient } from '@tanstack/react-query'
import { RefreshCwIcon, ScrollTextIcon } from 'lucide-react'
import { listCredentialUsage, type Credential, type UsageLog } from '@/api/credentials'
import { useI18n, type Language } from '@/lib/i18n'
import { useMediaQuery } from '@/lib/use-media-query'
import {
  cn,
  displayCredentialLabel,
  extractError,
  formatFullTime,
  formatUsd,
  parseSessionKey,
  sideClassLabel,
} from '@/lib/utils'
import { PaginationBar } from '@/components/pagination-bar'
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
import { Skeleton } from '@/components/ui/skeleton'
import { Spinner } from '@/components/ui/spinner'
import { Hint } from '@/components/ui/tooltip'
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
  const [pageSize, setPageSize] = useState<(typeof PAGE_SIZES)[number]>(PAGE_SIZES[0])
  const [page, setPage] = useState(0)
  /**
   * 第几轮翻页。重新取一轮（刷新、关掉再开）时加一。轮次进 queryKey：各轮的查询缓存互不相干，
   * 上一轮迟到的响应只落进上一轮的缓存，新一轮翻到同一页不会命中它。
   */
  const [round, setRound] = useState(0)
  const wideEnoughForTable = useMediaQuery('(min-width: 64rem)')
  /**
   * 点开了哪条请求的查询弹窗（请求 id）。
   *
   * 记 id 而不是一个开关：换一条就是换一次挂载，弹窗按初值直接开查，不必再同步内部状态
   * （见 [RequestLookupDialog] 的 `initialId`）。关掉即置空，不常驻。
   */
  const [lookupId, setLookupId] = useState<string | null>(null)

  /**
   * 本轮第一页：不带锚点取，响应里的锚点与总条数、总花费就是这一轮的快照，后续页把锚点原样带回、
   * 统计沿用它（后续页后端不再回统计）。快照直接从这条查询的数据里取——命中缓存、queryFn
   * 没跑也一样有，不另存一份。
   *
   * 一轮之内不再重取（`staleTime: Infinity`）：重取会换一个新锚点，与已经翻到的后续页错开。
   * 要看新请求就开新一轮（[reload]）。
   */
  const first = useQuery({
    // 首页与后续页的请求语义不同（首页不带锚点、回统计），查询身份分开：共用 key 时禁用着的
    // 后续页查询也会改写这条查询的选项，失效重取时首页就会误带锚点、丢了统计。
    queryKey: ['credential-usage', cred.id, round, 'first', pageSize],
    queryFn: () => listCredentialUsage(cred.id, { limit: pageSize, offset: 0 }),
    enabled: open,
    staleTime: Infinity,
    // 换每页条数时先留着上一份，避免表格整块闪成骨架屏。
    placeholderData: keepPreviousData,
  })
  const snapshot =
    first.data && !first.isPlaceholderData && first.data.total != null
      ? { anchor: first.data.anchor, total: first.data.total, cost: first.data.total_cost ?? 0 }
      : null
  /** 后续页：等本轮快照定下来再取，带着它的锚点；锚点之下的记录不会再变，同样不重取。 */
  const later = useQuery({
    // 锚点进 key：首页被显式失效重取、换了锚点时，后续页跟着按新锚点重取，不沿用旧锚点的缓存。
    queryKey: ['credential-usage', cred.id, round, 'page', snapshot?.anchor ?? null, page, pageSize],
    queryFn: () =>
      listCredentialUsage(cred.id, {
        limit: pageSize,
        offset: page * pageSize,
        until: snapshot?.anchor ?? undefined,
      }),
    enabled: open && page > 0 && snapshot != null,
    staleTime: Infinity,
    // 翻页时先留着上一页，避免表格整块闪成骨架屏。
    placeholderData: keepPreviousData,
  })
  const usage = page === 0 ? first : later

  // 显示用：换每页条数、新快照还在路上时，先沿用占位的上一份统计，别闪成「没有记录」。
  // 取后续页只认真正的 `snapshot`。
  const shown = snapshot ?? (first.data?.total != null
    ? { total: first.data.total, cost: first.data.total_cost ?? 0 }
    : null)
  const total = shown?.total ?? 0
  const totalPages = Math.max(1, Math.ceil(total / pageSize))
  const rows = usage.data?.logs ?? []
  // 页码越界（改了每页条数、或刷新后记录变少）时退回最后一页，而不是显示一页空白。
  const currentPage = Math.min(page, totalPages - 1)
  if (currentPage !== page) setPage(currentPage)
  const retentionNoteId = `credential-usage-retention-${cred.id}`

  /**
   * 重新取一轮：丢掉锚点回到第一页，于是能看到刚发生的请求。旧轮的查询取消并清掉，轮次加一
   * （新 queryKey，必定重取）。关掉对话框也走这里，下次打开是新的一轮。
   */
  const reload = () => {
    void qc.cancelQueries({ queryKey: ['credential-usage', cred.id] })
    qc.removeQueries({ queryKey: ['credential-usage', cred.id] })
    setRound((r) => r + 1)
    setPage(0)
  }

  const handleOpenChange = (next: boolean) => {
    // 关掉即重置，下次打开是新的一轮（新锚点、第一页）。
    if (!next) reload()
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
                <Hint label={credentialLabel}><span className="truncate">{credentialLabel}</span></Hint>
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
                {t('近 8 天明细花费', 'Request cost, last 8 days')}
              </p>
              <p className="font-semibold text-sm tabular-nums sm:mt-0.5">
                {shown ? formatUsd(shown.cost) : '—'}
              </p>
            </div>
            <p id={retentionNoteId} className="min-w-0 text-2xs leading-4 text-muted-foreground sm:text-right">
              {/* 「近 8 天」左边的标签已经写着，这里只说它和累计花费为什么对不上。这个对话框也从详情页
                  打开，那里没有卡片，所以不说「卡片上的」。 */}
              {t(
                '累计花费取自累计账本，与此处的明细合计不一定相等。',
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
                    '该账号转发请求后，记录将显示在此处。',
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

              <PaginationBar
                className="border-t pt-3"
                total={total}
                page={currentPage + 1}
                pageCount={totalPages}
                pageSize={pageSize}
                pageSizes={PAGE_SIZES}
                pageRowCount={rows.length}
                pageSizeLabel={t('每页条数', 'Rows per page')}
                disabled={usage.isFetching}
                onPageChange={(next) => setPage(next - 1)}
                onPageSizeChange={(size) => {
                  // 每页条数一变，原来的页码就没有意义了，回到第一页。
                  setPageSize(size)
                  setPage(0)
                }}
              />
            </>
          )}
        </DialogPanel>

        <DialogFooter>
          <Hint label={t('回到第一页并拉取最新记录', 'Jump back to the first page and fetch the newest records')}>
            <Button
              type="button"
              variant="outline"
              className="mr-auto"
              disabled={usage.isFetching}
              onClick={reload}
            >
              <RefreshCwIcon className={usage.isFetching ? 'animate-spin' : undefined} />
              {t('刷新', 'Refresh')}
            </Button>
          </Hint>
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
              <Hint label={formatFullTime(log.ts, language)}>
                <span className="shrink-0 font-medium tabular-nums">
                  {logTime(log.ts)}
                </span>
              </Hint>
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
            <Hint label={log.model}>
              <p className="mt-1 truncate text-muted-foreground">
                {log.model ?? '—'}
              </p>
            </Hint>
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
                <Hint label={deviceTitle(log)}>
                  <span className="font-mono">
                    {deviceShort}
                    {log.device_id_out && <span className="text-muted-foreground">→{log.device_id_out.slice(0, 8)}</span>}
                  </span>
                </Hint>
              </LogFact>
              <LogFact label={t('请求 ID', 'Request ID')}>
                <RequestIdChip id={log.request_id} onOpen={onLookup} />
              </LogFact>
              {/* 会话：模拟路径且没有设备身份的请求才有对话键，其余退到上游那个 session_id
                  （按槽位派生、对话之间复用），两者都在悬浮提示里。 */}
              <LogFact label={t('会话', 'Session')}>
                <Hint label={sessionTitle(log)}>
                  <span className="font-mono">
                    {sessionShort(log, t, language)}
                  </span>
                </Hint>
              </LogFact>
            </dl>
            {(log.ua || log.ua_out) && (
              <Hint label={log.ua_out && log.ua_out !== log.ua ? `${log.ua ?? '—'}\n→ ${log.ua_out}` : log.ua}>
                <p className="mt-2 truncate border-t pt-1.5 text-2xs text-muted-foreground">
                  {log.ua ?? t('无（luban 自身发起）', 'None (sent by luban itself)')}
                </p>
              </Hint>
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
          <Hint label={t('客户端请求自带的 device_id（设备绑定与设备上限均据此计算）', 'device_id carried by the inbound client request (device bindings and device limits are based on it)')}>
            <TableHead className="whitespace-nowrap">
              {t('入站设备', 'Inbound device')}
            </TableHead>
          </Hint>
          <Hint
            label={t(
              '实际发给上游的 device_id（按账号派生），上游给出的设备 ID 对应此列；旧记录为空',
              'device_id actually sent upstream (derived per account); a device ID quoted by upstream corresponds to this column. Empty for older rows',
            )}
          >
            <TableHead className="whitespace-nowrap">
              {t('出站设备', 'Outbound device')}
            </TableHead>
          </Hint>
          <Hint
            label={t(
              'luban 在响应头 X-Oneapi-Request-Id 中返回的请求 ID，New API 日志中称为 upstream_request_id；点击 ID 可查看该请求，点击旁边的图标可复制',
              'Request ID luban returns in the X-Oneapi-Request-Id response header (upstream_request_id in New API logs); click the ID to look up the request, or the icon next to it to copy',
            )}
          >
            <TableHead className="whitespace-nowrap">
              {t('请求 ID', 'Request ID')}
            </TableHead>
          </Hint>
          {/* 两份 UA 合在一列：绝大多数请求原样转发，两者是同一串，占两列纯浪费宽度。
              只在被改写时才多显示一行出站那份，见 UaCell。 */}
          <Hint
            label={t(
              '客户端自报的 User-Agent；被改写时，另起一行显示实际发给上游的 User-Agent',
              'User-Agent reported by the client; when rewritten, the one actually sent upstream is shown on a second line',
            )}
          >
            <TableHead className="min-w-52 whitespace-nowrap">
              {t('客户端 UA', 'Client UA')}
            </TableHead>
          </Hint>
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
              <Hint label={`${formatFullTime(log.ts, language)} · ${log.path}`}>
                <TableCell className="whitespace-nowrap tabular-nums">
                  {logTime(log.ts)}
                </TableCell>
              </Hint>
              <TableCell>
                <Badge variant={statusVariant(log.status)} size="sm" className="tabular-nums">
                  {log.status}
                </Badge>
              </TableCell>
              <Hint label={log.model}>
                <TableCell className="max-w-40 truncate">
                  {log.model ?? '—'}
                </TableCell>
              </Hint>
              <TableCell className="whitespace-nowrap text-right tabular-nums">
                {num(log.input_tokens, locale)}
              </TableCell>
              <TableCell className="whitespace-nowrap text-right tabular-nums">
                {num(log.output_tokens, locale)}
              </TableCell>
              <TableCell className="whitespace-nowrap text-right tabular-nums">
                {num(log.cache_creation_tokens, locale)} / {num(log.cache_read_tokens, locale)}
              </TableCell>
              <Hint
                label={log.sse_aggregated
                  ? t(
                      '该请求原为非流式，以流式发往上游后再聚合为完整响应返回：首字耗时取自上游首字节，客户端则在结束时一次性收到全部内容。',
                      'This request arrived non-streaming and was sent upstream as a stream, then reassembled into a single response: TTFT is the upstream first byte, while the client received everything at the end.',
                    )
                  : undefined}
              >
                <TableCell className="whitespace-nowrap text-right tabular-nums">
                  {ms(log.ttft_ms)} / {ms(log.total_ms)}
                  {log.sse_aggregated && (
                    <div className="text-muted-foreground text-[10px] font-normal">
                      {t('非流转流', 'stream-upgraded')}
                    </div>
                  )}
                </TableCell>
              </Hint>
              <Hint
                label={log.cost_usd == null
                  ? t('模型不在价目表内，无法估算花费', 'Model is not in the price table, so the cost cannot be estimated')
                  : undefined}
              >
                <TableCell
                  className={cn(
                    'whitespace-nowrap text-right tabular-nums',
                    log.cost_usd == null && 'text-muted-foreground',
                  )}
                >
                  {log.cost_usd == null ? '—' : formatUsd(log.cost_usd)}
                </TableCell>
              </Hint>
              <Hint label={log.device_id}>
                <TableCell className="whitespace-nowrap font-mono text-xs">
                  {deviceShort}
                </TableCell>
              </Hint>
              <Hint label={log.device_id_out}>
                <TableCell className="whitespace-nowrap font-mono text-xs">
                  {log.device_id_out?.slice(0, 8) ?? '—'}
                </TableCell>
              </Hint>
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
 * 悬浮提示里。被改写时第二行显示实际发给上游的值。
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
    <Hint label={rewritten ? `${incoming}\n→ ${uaOut}` : incoming}>
      <TableCell className="align-top leading-4">
        <span className={cn('block truncate', !ua && 'text-muted-foreground')}>
          {incoming}
        </span>
        {rewritten && (
          <span className="mt-0.5 block truncate text-muted-foreground">
            → {uaOut}
          </span>
        )}
      </TableCell>
    </Hint>
  )
}

/**
 * 设备格的悬停全文：来访原始 id 与出站派生 id 各一行。上游侧（工单、封号通知）给出的
 * device_id 对的是第二行——第一行是客户端自己的 id，上游从没见过。
 */
/** 卡片里那一格会话：有对话键就显示「来源 + 前 8 位」（匿名侧查询再接类别），否则退到上游 session_id，都没有是 '—'。 */
function sessionShort(
  log: UsageLog,
  t: (zh: string, en: string) => string,
  language: Language,
): string {
  if (log.session_key) {
    const { source, value, sideClass } = parseSessionKey(log.session_key)
    const short = `${source === 'pfx' ? t('前缀', 'prefix') : t('自带', 'client')} ${value.slice(0, 8)}`
    return sideClass ? `${short} · ${sideClassLabel(sideClass, language)}` : short
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
