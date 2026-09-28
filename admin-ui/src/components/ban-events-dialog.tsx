import React, { useState } from 'react'
import { keepPreviousData, useQuery } from '@tanstack/react-query'
import {
  ChevronDownIcon, ChevronRightIcon, CopyIcon, DownloadIcon, RefreshCwIcon,
} from 'lucide-react'
import {
  fetchAllBanEventLogs, listBanEventLogs, listBanEvents,
  type BanEvent, type UsageLog, type ValueCount,
} from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import {
  cn, copyText, displayCredentialLabel, downloadJson, extractError, fileStamp, formatFullTime,
  formatUsd,
} from '@/lib/utils'
import { ClampedDescription } from '@/components/settings-group'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Dialog, DialogDescription, DialogHeader, DialogPanel, DialogPopup, DialogTitle,
} from '@/components/ui/dialog'
import { Empty, EmptyDescription, EmptyHeader, EmptyTitle } from '@/components/ui/empty'
import {
  Pagination, PaginationContent, PaginationItem, PaginationNext, PaginationPrevious,
} from '@/components/ui/pagination'
import {
  Select, SelectItem, SelectPopup, SelectTrigger, SelectValue,
} from '@/components/ui/select'
import { Spinner } from '@/components/ui/spinner'
import { toastManager } from '@/components/ui/toast'
import {
  Table, TableBody, TableCell, TableHead, TableHeader, TableRow,
} from '@/components/ui/table'
import { RequestIdChip, statusVariant } from '@/components/usage-shared'

/** 冻结流水每页条数可选值。后端上限 1000。 */
const PAGE_SIZES = [25, 50, 100] as const

/** 触发来源 → 人话。 */
export function sourceLabel(source: string, t: (zh: string, en: string) => string): string {
  switch (source) {
    case 'forward': return t('转发 4xx', 'Forward 4xx')
    case 'forward_401': return t('转发 401 后换账号', 'Forward 401, account switch')
    case 'probe': return t('连通性测试', 'Connectivity test')
    case 'keepalive': return t('保活端点 401/403', 'Keepalive 401/403')
    case 'refresh': return t('refresh token 已吊销', 'Refresh token revoked')
    case 'proxy': return t('代理不可用', 'Proxy unusable')
    default: return source
  }
}

function Distribution({ items, empty }: { items: ValueCount[]; empty: string }) {
  if (items.length === 0) return <span className="text-muted-foreground">{empty}</span>
  return (
    <div className="flex flex-wrap gap-1">
      {items.map((it) => (
        <Badge key={it.value} variant="outline" className="max-w-72 font-mono text-[11px]" title={it.value}>
          <span className="truncate">{it.value}</span>
          <span className="ml-1 text-muted-foreground">×{it.count}</span>
        </Badge>
      ))}
    </div>
  )
}

function Fact({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="min-w-0">
      <div className="text-[11px] uppercase tracking-wide text-muted-foreground">{label}</div>
      <div className="mt-0.5 break-words text-sm">{children}</div>
    </div>
  )
}

/** 一条事件的详情：账号侧快照 + 冻结流水时间线。 */
export function BanEventDetail({ ev }: { ev: BanEvent }) {
  const { t, language, locale } = useI18n()
  const [pageSize, setPageSize] = useState<number>(PAGE_SIZES[0])
  const [page, setPage] = useState(0)
  /** 导出（复制/下载）正在连着翻页拉整份：期间两个按钮都锁上，免得拉出半份。 */
  const [exporting, setExporting] = useState(false)
  const logs = useQuery({
    queryKey: ['ban-event-logs', ev.id, page, pageSize],
    queryFn: () => listBanEventLogs(ev.id, { limit: pageSize, offset: page * pageSize }),
    // 翻页时先留着上一页，避免时间线整块闪成 spinner。
    placeholderData: keepPreviousData,
  })
  const rows = logs.data?.logs ?? []
  // 冻结表写完就不再变，总条数用接口给的；接口还没回来时先用事件里记着的那个数。
  const total = logs.data?.total ?? ev.frozen_rows
  const totalPages = Math.max(1, Math.ceil(total / pageSize))
  // 页码越界（改了每页条数）时退回最后一页，而不是显示一页空白。
  const currentPage = Math.min(page, totalPages - 1)
  if (currentPage !== page) setPage(currentPage)
  const firstIndex = currentPage * pageSize + 1
  const lastIndex = currentPage * pageSize + rows.length
  const dash = '—'

  /**
   * 取证包要的是**整份**，而页面只按页拉——导出时现连着翻完再拼。
   *
   * 拼在前端而不是给后端开一个「全量」口子：整份常有几十 MB，一次性生成、传输、驻留在
   * 内存里的代价只有真按了导出的人该付，翻着看的人不该跟着等。
   */
  const collect = async (): Promise<{ event: BanEvent; logs: UsageLog[] } | null> => {
    setExporting(true)
    try {
      return { event: ev, logs: await fetchAllBanEventLogs(ev.id) }
    } catch (err) {
      toastManager.add({
        type: 'error',
        title: t('获取完整流水失败', 'Failed to fetch the full timeline'),
        description: extractError(err, language),
      })
      return null
    } finally {
      setExporting(false)
    }
  }

  const copyAll = async () => {
    const pack = await collect()
    if (!pack) return
    const ok = await copyText(JSON.stringify(pack, null, 2))
    toastManager.add({
      type: ok ? 'success' : 'error',
      title: ok ? t('已复制事件与流水 JSON', 'Event and logs copied as JSON') : t('复制失败', 'Copy failed'),
    })
  }

  // 复盘要的是**整份**：一次封号常带上千行流水（封前 7 天全量），复制到剪贴板经常在半路
  // 断掉——超长文本、非安全上下文下的 execCommand 回退都会。存成文件不吃这两样限制，拿到
  // 的一定是完整的那份。
  const downloadAll = async () => {
    const pack = await collect()
    if (!pack) return
    downloadJson(`luban-ban-${ev.id}-${fileStamp(ev.ts)}.json`, pack)
    toastManager.add({
      type: 'success',
      title: t(`已保存为文件（${pack.logs.length} 条流水）`, `Saved to a file (${pack.logs.length} rows)`),
    })
  }

  const ageDays = Math.max(0, Math.floor((ev.ts - ev.account_created_at) / 86400))

  return (
    // whitespace-normal：详情挂在表格单元格里，表格默认不折行，长的上游原文会直接压到右边那一格上。
    <div className="space-y-4 whitespace-normal border-t bg-muted/30 px-4 py-4">
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <Fact label={t('上游原文', 'Upstream message')}>
          <div className="font-mono text-xs [overflow-wrap:anywhere]">
            {/* `size="xs"`：这枚是内联嵌在一行 `text-xs` 正文里的，默认档带视口 ramp
                （手机 14px），会比它夹在中间的文字还大。 */}
            {ev.error_type && <Badge size="xs" variant="outline" className="mr-1">{ev.error_type}</Badge>}
            {ev.error_message ?? ev.reason}
          </div>
        </Fact>
        <Fact label={t('请求 ID', 'Request IDs')}>
          <div className="flex flex-wrap gap-1">
            {ev.request_id ? <RequestIdChip id={ev.request_id} /> : dash}
            {ev.upstream_request_id && <RequestIdChip id={ev.upstream_request_id} />}
          </div>
        </Fact>
        <Fact label={t('账号', 'Account')}>
          {[ev.tier, ev.org_type].filter(Boolean).join(' / ') || dash}
          <span className="ml-2 text-muted-foreground">
            {t(`账龄 ${ageDays} 天`, `${ageDays} days old`)}
          </span>
        </Fact>
        <Fact label={t('当时的代理', 'Proxy at the time')}>
          <span className="font-mono text-xs">{ev.proxy ?? t('直连', 'direct')}</span>
        </Fact>
        <Fact label={t('累计', 'Lifetime')}>
          {t(`${ev.lifetime_requests} 次请求 · ${formatUsd(ev.lifetime_cost_usd)}`,
            `${ev.lifetime_requests} requests · ${formatUsd(ev.lifetime_cost_usd)}`)}
        </Fact>
        {/* 「封前 7 天」不在这里再列：展开它的那一行已经写着「N 次 · 入站 N → 出站 N 台」。 */}
        <Fact label={t('最后额度状态', 'Last quota status')}>
          {ev.last_unified_status ?? dash}
          {ev.last_overage_in_use && (
            <Badge variant="error" className="ml-1">{t('正在使用超额用量', 'extra usage in use')}</Badge>
          )}
        </Fact>
        <Fact label={t('冻结流水', 'Frozen rows')}>{ev.frozen_rows}</Fact>
        <div className="sm:col-span-2">
          <Fact label={t('模型分布（7 天）', 'Models (7d)')}>
            <Distribution items={ev.models_7d} empty={dash} />
          </Fact>
        </div>
        <div className="sm:col-span-2">
          <Fact label={t('客户端 UA 分布（7 天）', 'Client UAs (7d)')}>
            <Distribution items={ev.uas_7d} empty={dash} />
          </Fact>
        </div>
        <div className="sm:col-span-2 lg:col-span-4">
          <Fact label={t('发给 Anthropic 的设备 ID 分布（7 天）', 'Device IDs sent to Anthropic (7d)')}>
            <Distribution items={ev.device_ids_out_7d} empty={t('旧记录不含此项', 'not recorded in older rows')} />
          </Fact>
        </div>
        <div className="sm:col-span-2 lg:col-span-4">
          <Fact label={t('出口代理分布（7 天）', 'Proxies (7d)')}>
            <Distribution items={ev.proxies_7d} empty={t('全部直连，或旧记录不含此项', 'all direct, or not recorded in older rows')} />
          </Fact>
        </div>
      </div>

      <div className="flex items-center justify-between gap-2">
        <div className="text-sm font-medium">
          {t('封号前流水时间线', 'Traffic timeline before the ban')}
          <span className="ml-2 text-xs text-muted-foreground">
            {t('封号前 7 天至封号后 10 分钟内到达的请求，包括触发封号的请求；表格分页显示，导出内容为完整流水', 'Requests that arrived from 7 days before to 10 minutes after the ban, including the triggering one; the table is paginated, while the export includes all of them')}
          </span>
        </div>
        <div className="flex shrink-0 gap-2">
          <Button size="sm" variant="outline" onClick={copyAll} disabled={exporting || total === 0}>
            <CopyIcon />{t('复制 JSON', 'Copy JSON')}
          </Button>
          <Button size="sm" variant="outline" onClick={downloadAll} disabled={exporting || total === 0}>
            {exporting ? <Spinner className="size-4" /> : <DownloadIcon />}
            {t('下载 JSON', 'Download JSON')}
          </Button>
        </div>
      </div>

      {logs.isPending ? (
        <div className="flex justify-center py-6"><Spinner /></div>
      ) : logs.isError ? (
        <Alert variant="error">
          <AlertTitle>{t('读取失败', 'Failed to load')}</AlertTitle>
          <AlertDescription>{extractError(logs.error, language)}</AlertDescription>
        </Alert>
      ) : total === 0 ? (
        <div className="py-4 text-center text-sm text-muted-foreground">
          {t('本次封号前没有流水记录（可能已被清理，或账号添加后随即被封）', 'No traffic was recorded before this ban (it may have been pruned, or the account was banned right after being added)')}
        </div>
      ) : (
        <>
          <div className="overflow-x-auto rounded-md border bg-background">
            <Table className="text-xs">
              <TableHeader>
                <TableRow>
                  <TableHead className="whitespace-nowrap">{t('时间', 'Time')}</TableHead>
                  <TableHead>{t('状态', 'Status')}</TableHead>
                  <TableHead>{t('模型', 'Model')}</TableHead>
                  <TableHead>{t('设备（入站 → 出站）', 'Device (inbound → outbound)')}</TableHead>
                  <TableHead>{t('客户端 UA', 'Client UA')}</TableHead>
                  <TableHead>{t('出口', 'Proxy')}</TableHead>
                  <TableHead>{t('标记', 'Flags')}</TableHead>
                  <TableHead className="text-right">{t('输入/输出', 'In/Out')}</TableHead>
                  <TableHead className="text-right">{t('花费', 'Cost')}</TableHead>
                  <TableHead>{t('上游报错', 'Upstream error')}</TableHead>
                  <TableHead>{t('形态', 'Shape')}</TableHead>
                </TableRow>
              </TableHeader>
              <TableBody>
                {rows.map((log) => <FrozenRow key={log.id} log={log} trigger={log.request_id != null && log.request_id === ev.request_id} />)}
              </TableBody>
            </Table>
          </div>

          <div className="grid grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)] items-center gap-2 sm:gap-3 border-t pt-3 text-xs">
            <p className="min-w-0 text-muted-foreground tabular-nums">
              <span className="max-sm:hidden">
                {t(
                  `第 ${firstIndex}–${lastIndex} 条，共 ${total.toLocaleString(locale)} 条`,
                  `${firstIndex}–${lastIndex} of ${total.toLocaleString(locale)}`,
                )}
              </span>
              <span className="sm:hidden">{`${firstIndex}–${lastIndex} / ${total.toLocaleString(locale)}`}</span>
            </p>
            {/* 窄屏也排成一行：计数缩成「1–10 / 29」、翻页只写「1 / 3」、藏掉「每页」二字，
              三栏放得下，不再把翻页挤到第二行。 */}
            <div className="col-start-3 row-start-1 flex items-center gap-2 justify-self-end">
              <span className="whitespace-nowrap text-muted-foreground max-sm:hidden">{t('每页', 'Per page')}</span>
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
                <SelectTrigger size="sm" className="w-auto min-w-16 sm:min-w-20" aria-label={t('每页条数', 'Rows per page')}>
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
              <Pagination className="col-start-2 row-start-1 justify-center">
                <PaginationContent>
                  <PaginationItem>
                    <PaginationPrevious
                      render={<Button variant="ghost" disabled={logs.isFetching || currentPage === 0} />}
                      aria-disabled={logs.isFetching || currentPage === 0}
                      onClick={() => setPage((current) => Math.max(0, current - 1))}
                    />
                  </PaginationItem>
                  <PaginationItem>
                    <span className="whitespace-nowrap px-2 text-xs text-foreground tabular-nums" aria-live="polite">
                      <span className="max-sm:hidden">{t(`第 ${currentPage + 1} / ${totalPages} 页`, `Page ${currentPage + 1} of ${totalPages}`)}</span>
                      <span className="sm:hidden">{`${currentPage + 1} / ${totalPages}`}</span>
                    </span>
                  </PaginationItem>
                  <PaginationItem>
                    <PaginationNext
                      render={<Button variant="ghost" disabled={logs.isFetching || currentPage >= totalPages - 1} />}
                      aria-disabled={logs.isFetching || currentPage >= totalPages - 1}
                      onClick={() => setPage((current) => Math.min(totalPages - 1, current + 1))}
                    />
                  </PaginationItem>
                </PaginationContent>
              </Pagination>
            )}
          </div>
        </>
      )}
    </div>
  )
}

/** 设备格的悬停全文：来访原始 id 与出站派生 id 各一行，上游侧拿到的 id 对的是第二行。 */
function deviceTitle(log: UsageLog): string | undefined {
  if (!log.device_id && !log.device_id_out) return undefined
  return `in:  ${log.device_id ?? '—'}\nout: ${log.device_id_out ?? '—'}`
}

function FrozenRow({ log, trigger }: { log: UsageLog; trigger: boolean }) {
  const { t } = useI18n()
  const [shapeOpen, setShapeOpen] = useState(false)
  const flags: string[] = []
  if (log.simulated) flags.push(t('模拟', 'sim'))
  if (log.third_party) flags.push(t('判为第三方', '3rd-party'))
  if (log.sse_aggregated) flags.push('sse→json')
  if (log.rewrites) flags.push(...log.rewrites.split(','))
  const shape = log.shape ? safeParse(log.shape) : null
  return (
    <>
      <TableRow className={cn(trigger && 'bg-destructive/10')}>
        <TableCell className="whitespace-nowrap tabular-nums">
          {formatFullTime(log.ts)}
          {trigger && <Badge variant="error" className="ml-1">{t('触发', 'trigger')}</Badge>}
        </TableCell>
        <TableCell><Badge variant={statusVariant(log.status)}>{log.status}</Badge></TableCell>
        <TableCell className="max-w-36 truncate" title={log.model ?? undefined}>{log.model ?? '—'}</TableCell>
        <TableCell className="whitespace-nowrap font-mono" title={deviceTitle(log)}>
          {log.device_id?.slice(0, 8) ?? '—'}
          <span className="text-muted-foreground">→{log.device_id_out?.slice(0, 8) ?? '—'}</span>
        </TableCell>
        <TableCell className="max-w-44 truncate font-mono" title={log.ua ?? undefined}>{log.ua ?? '—'}</TableCell>
        <TableCell className="max-w-40 truncate font-mono" title={log.proxy ?? undefined}>{log.proxy ?? t('直连', 'direct')}</TableCell>
        <TableCell>
          <div className="flex flex-wrap gap-1">
            {flags.map((f) => <Badge key={f} variant="outline">{f}</Badge>)}
          </div>
        </TableCell>
        <TableCell className="whitespace-nowrap text-right tabular-nums">
          {log.input_tokens ?? '—'} / {log.output_tokens ?? '—'}
        </TableCell>
        <TableCell className="whitespace-nowrap text-right tabular-nums">
          {log.cost_usd == null ? '—' : formatUsd(log.cost_usd)}
        </TableCell>
        <TableCell className="max-w-64">
          {log.error_type || log.error_message ? (
            <span className="font-mono" title={log.error_message ?? undefined}>
              {log.error_type && <span className="text-muted-foreground">{log.error_type}: </span>}
              <span className="line-clamp-2">{log.error_message}</span>
            </span>
          ) : '—'}
        </TableCell>
        <TableCell>
          {shape ? (
            <Button size="sm" variant="ghost" className="h-6 px-1 font-mono" onClick={() => setShapeOpen((v) => !v)}>
              {shapeOpen ? <ChevronDownIcon /> : <ChevronRightIcon />}
              {shapeBrief(shape)}
            </Button>
          ) : '—'}
        </TableCell>
      </TableRow>
      {shapeOpen && shape && (
        <TableRow>
          <TableCell colSpan={11} className="bg-muted/40">
            <pre className="max-h-80 overflow-auto whitespace-pre-wrap break-all font-mono text-[11px]">
              {JSON.stringify(shape, null, 2)}
            </pre>
          </TableCell>
        </TableRow>
      )}
    </>
  )
}

function safeParse(text: string): Record<string, unknown> | null {
  try {
    const v: unknown = JSON.parse(text)
    return v && typeof v === 'object' ? (v as Record<string, unknown>) : null
  } catch {
    return null
  }
}

/** 形态摘要的一行速览：system 哈希、工具数、消息数。 */
function shapeBrief(shape: Record<string, unknown>): string {
  const sys = shape.system as { sha?: string; blocks?: unknown[] } | undefined
  const tools = shape.tools as { count?: number } | undefined
  const msgs = shape.messages as { count?: number } | undefined
  const parts: string[] = []
  if (sys?.sha) parts.push(`sys:${sys.sha.slice(0, 6)}×${sys.blocks?.length ?? 0}`)
  if (tools) parts.push(`tools:${tools.count ?? 0}`)
  if (msgs) parts.push(`msgs:${msgs.count ?? 0}`)
  return parts.join(' ') || '{…}'
}

/**
 * 封号记录：每次自动停用一条，带封号当时的账号侧快照与封前流水。
 *
 * 上游的封号文案对谁都是同一句，「为什么」只能从封前流量反推：这里把被封的号封前 7 天
 * 走了什么出口、用了什么客户端、请求体什么形状、有没有被判成第三方一次摆开，拿它和活着的号
 * 对照。事件不随解封、删号、流水裁剪消失。
 */
export function BanEventsDialog({
  open,
  onOpenChange,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const { t, language } = useI18n()
  const [expanded, setExpanded] = useState<number | null>(null)
  const query = useQuery({
    queryKey: ['ban-events'],
    queryFn: () => listBanEvents({ limit: 200 }),
    enabled: open,
  })
  const events = query.data ?? []

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogPopup size="full">
        <DialogHeader>
          <DialogTitle>{t('封号记录', 'Ban events')}</DialogTitle>
          <DialogDescription>
            {/* 长说明默认收两行、末尾「了解更多」，同设置页的 ClampedDescription：超过 140 字各宽度都收，60–140 字只在手机上收。 */}
            <ClampedDescription text={t(
              '每次自动封停生成一条记录，包含上游原始返回信息、触发请求、账号当时的等级 / 代理 / 用量快照，以及封号前 7 天的全部流水（含取证列）。解封、删除账号或清理流水均不会删除这些记录。',
              'One row per automatic disable: the upstream message, the triggering request, a snapshot of the account’s tier / proxy / usage at the time, and every request from the 7 days before (with forensic columns). Re-enabling or deleting the account, or pruning logs, never removes these records.',
            )} />
          </DialogDescription>
        </DialogHeader>
        <DialogPanel className="max-h-[70vh] overflow-y-auto">
          <div className="mb-2 flex justify-end">
            <Button size="sm" variant="ghost" onClick={() => query.refetch()} disabled={query.isFetching}>
              <RefreshCwIcon className={cn(query.isFetching && 'animate-spin')} />{t('刷新', 'Refresh')}
            </Button>
          </div>
          {query.isPending ? (
            <div className="flex justify-center py-10"><Spinner /></div>
          ) : query.isError ? (
            <Alert variant="error">
              <AlertTitle>{t('读取失败', 'Failed to load')}</AlertTitle>
              <AlertDescription>{extractError(query.error, language)}</AlertDescription>
            </Alert>
          ) : events.length === 0 ? (
            <Empty>
              <EmptyHeader>
                <EmptyTitle>{t('暂无封号记录', 'No ban events yet')}</EmptyTitle>
                <EmptyDescription>
                  {t('账号因上游错误被自动封停时，此处会新增一条记录；此后即使解封或删除该账号，记录仍会保留。', 'When an account is auto-disabled because of an upstream error, a row is added here and stays, even after the account is re-enabled or deleted.')}
                </EmptyDescription>
              </EmptyHeader>
            </Empty>
          ) : (
            <div className="overflow-hidden rounded-md border">
              <Table>
                {/* 手机上换成堆叠卡片，表头跟着一起藏——7 列的表头配单格的堆叠行没有意义。 */}
                <TableHeader className="max-lg:hidden">
                  <TableRow>
                    <TableHead className="w-8" />
                    <TableHead className="whitespace-nowrap">{t('时间', 'Time')}</TableHead>
                    <TableHead>{t('账号', 'Account')}</TableHead>
                    <TableHead>{t('来源', 'Source')}</TableHead>
                    <TableHead>{t('状态', 'Status')}</TableHead>
                    <TableHead>{t('原因', 'Reason')}</TableHead>
                    <TableHead className="text-right">{t('封号前 7 天', '7d')}</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {events.map((ev) => {
                    const isOpen = expanded === ev.id
                    // 整行可点之外，键盘也得能展开：Tab 聚焦到行，Enter / 空格切换。
                    const toggleProps = {
                      tabIndex: 0,
                      'aria-expanded': isOpen,
                      onClick: () => setExpanded(isOpen ? null : ev.id),
                      onKeyDown: (event: React.KeyboardEvent) => {
                        if (event.target !== event.currentTarget) return
                        if (event.key !== 'Enter' && event.key !== ' ') return
                        event.preventDefault()
                        setExpanded(isOpen ? null : ev.id)
                      },
                    }
                    return (
                      <React.Fragment key={ev.id}>
                        {/* 手机到平板：堆叠成一张卡片。7 列的表在 lg 以下只能左右拖，
                            而这张表每一行都要读「谁、什么时候、为什么」三件事，横滚读不了。
                            与账号列表的窄屏行同一个做法（见 credential-row 的 `xl:hidden`）。 */}
                        <TableRow className="cursor-pointer lg:hidden" {...toggleProps}>
                          <TableCell colSpan={7} className="w-full max-w-0 whitespace-normal p-0">
                            <article className="flex min-w-0 items-start gap-2 px-4 py-3">
                              <span className="mt-0.5 shrink-0 text-muted-foreground" aria-hidden>
                                {isOpen ? <ChevronDownIcon className="size-4" /> : <ChevronRightIcon className="size-4" />}
                              </span>
                              <div className="min-w-0 flex-1 space-y-1.5">
                                <div className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1">
                                  <span className="min-w-0 truncate font-medium text-sm">
                                    {displayCredentialLabel(ev.cred_label, language)}
                                  </span>
                                  <span className="shrink-0 font-mono text-xs text-muted-foreground">#{ev.cred_id}</span>
                                  {ev.status != null && (
                                    <Badge size="sm" variant={statusVariant(ev.status)}>{ev.status}</Badge>
                                  )}
                                </div>
                                {/* 表头藏了，所以「封前 7 天」这类列名在这里要自己带上。 */}
                                <p className="flex min-w-0 flex-wrap items-center gap-x-2 gap-y-1 text-xs text-muted-foreground">
                                  <span className="tabular-nums">{formatFullTime(ev.ts)}</span>
                                  <Badge size="sm" variant="outline">{sourceLabel(ev.source, t)}</Badge>
                                  <span className="tabular-nums">
                                    {t(
                                      `封号前 7 天 ${ev.requests_7d} 次请求 · 入站 ${ev.devices_7d} → 出站 ${ev.devices_out_7d} 台`,
                                      `7d before: ${ev.requests_7d} req · ${ev.devices_7d} in → ${ev.devices_out_7d} out`,
                                    )}
                                  </span>
                                </p>
                                <p className="line-clamp-2 break-all font-mono text-xs text-muted-foreground">
                                  {ev.reason}
                                </p>
                              </div>
                            </article>
                          </TableCell>
                        </TableRow>
                        <TableRow className="hidden cursor-pointer lg:table-row" {...toggleProps}>
                          <TableCell className="text-muted-foreground">
                            {isOpen ? <ChevronDownIcon className="size-4" /> : <ChevronRightIcon className="size-4" />}
                          </TableCell>
                          <TableCell className="whitespace-nowrap tabular-nums">{formatFullTime(ev.ts)}</TableCell>
                          <TableCell className="whitespace-nowrap">
                            {displayCredentialLabel(ev.cred_label, language)}
                            <span className="ml-1 font-mono text-xs text-muted-foreground">#{ev.cred_id}</span>
                          </TableCell>
                          <TableCell className="whitespace-nowrap"><Badge variant="outline">{sourceLabel(ev.source, t)}</Badge></TableCell>
                          <TableCell>{ev.status == null ? '—' : <Badge variant={statusVariant(ev.status)}>{ev.status}</Badge>}</TableCell>
                          <TableCell className="max-w-md truncate font-mono text-xs" title={ev.reason}>{ev.reason}</TableCell>
                          <TableCell className="whitespace-nowrap text-right tabular-nums">
                            {/* 表格这一格不折行，写全「入站 3 → 出站 2 台」会把整张表撑宽、挤掉右侧的导出按钮；
                                这里用「3→2 台」，完整说法在 title 里。窄屏的堆叠行能折行，照写全称。 */}
                            <span
                              title={t(
                                `封号前 7 天：入站 ${ev.devices_7d} 台 → 出站 ${ev.devices_out_7d} 台`,
                                `7 days before: ${ev.devices_7d} inbound → ${ev.devices_out_7d} outbound devices`,
                              )}
                            >
                              {t(
                                `${ev.requests_7d} 次 · ${ev.devices_7d}→${ev.devices_out_7d} 台`,
                                `${ev.requests_7d} req · ${ev.devices_7d}→${ev.devices_out_7d} dev`,
                              )}
                            </span>
                          </TableCell>
                        </TableRow>
                        {isOpen && (
                          <TableRow>
                            <TableCell colSpan={7} className="p-0">
                              <BanEventDetail ev={ev} />
                            </TableCell>
                          </TableRow>
                        )}
                      </React.Fragment>
                    )
                  })}
                </TableBody>
              </Table>
            </div>
          )}
        </DialogPanel>
      </DialogPopup>
    </Dialog>
  )
}
