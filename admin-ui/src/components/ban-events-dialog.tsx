import React, { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { ChevronDownIcon, ChevronRightIcon, CopyIcon, RefreshCwIcon } from 'lucide-react'
import {
  listBanEventLogs, listBanEvents, type BanEvent, type UsageLog, type ValueCount,
} from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import {
  cn, copyText, displayCredentialLabel, extractError, formatFullTime, formatUsd,
} from '@/lib/utils'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Dialog, DialogDescription, DialogHeader, DialogPanel, DialogPopup, DialogTitle,
} from '@/components/ui/dialog'
import { Empty, EmptyDescription, EmptyHeader, EmptyTitle } from '@/components/ui/empty'
import { Spinner } from '@/components/ui/spinner'
import { toastManager } from '@/components/ui/toast'
import {
  Table, TableBody, TableCell, TableHead, TableHeader, TableRow,
} from '@/components/ui/table'
import { RequestIdChip, statusVariant } from '@/components/credential-usage-dialog'

/** 触发来源 → 人话。 */
function sourceLabel(source: string, t: (zh: string, en: string) => string): string {
  switch (source) {
    case 'forward': return t('转发 4xx', 'Forward 4xx')
    case 'forward_401': return t('转发 401 换号', 'Forward 401 swap')
    case 'probe': return t('连通性测试', 'Connectivity test')
    case 'refresh': return t('刷新 token 被作废', 'Refresh revoked')
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
function BanEventDetail({ ev }: { ev: BanEvent }) {
  const { t, language } = useI18n()
  const logs = useQuery({
    queryKey: ['ban-event-logs', ev.id],
    queryFn: () => listBanEventLogs(ev.id),
  })
  const rows = logs.data ?? []
  const dash = '—'

  const copyAll = async () => {
    const ok = await copyText(JSON.stringify({ event: ev, logs: rows }, null, 2))
    toastManager.add({
      type: ok ? 'success' : 'error',
      title: ok ? t('已复制事件与流水 JSON', 'Copied event + logs as JSON') : t('复制失败', 'Copy failed'),
    })
  }

  const ageDays = Math.max(0, Math.floor((ev.ts - ev.account_created_at) / 86400))

  return (
    <div className="space-y-4 border-t bg-muted/30 px-4 py-4">
      <div className="grid gap-3 sm:grid-cols-2 lg:grid-cols-4">
        <Fact label={t('上游原文', 'Upstream message')}>
          <div className="font-mono text-xs">
            {ev.error_type && <Badge variant="outline" className="mr-1">{ev.error_type}</Badge>}
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
        <Fact label={t('当时代理', 'Proxy at the time')}>
          <span className="font-mono text-xs">{ev.proxy ?? t('直连', 'direct')}</span>
        </Fact>
        <Fact label={t('终身', 'Lifetime')}>
          {t(`${ev.lifetime_requests} 次请求 · ${formatUsd(ev.lifetime_cost_usd)}`,
            `${ev.lifetime_requests} requests · ${formatUsd(ev.lifetime_cost_usd)}`)}
        </Fact>
        <Fact label={t('封前 7 天', 'Last 7 days')}>
          {t(`${ev.requests_7d} 次请求 · 来访 ${ev.devices_7d} 台 → 出站 ${ev.devices_out_7d} 台`,
            `${ev.requests_7d} requests · ${ev.devices_7d} client devices → ${ev.devices_out_7d} sent upstream`)}
        </Fact>
        <Fact label={t('最后额度状态', 'Last quota status')}>
          {ev.last_unified_status ?? dash}
          {ev.last_overage_in_use && (
            <Badge variant="error" className="ml-1">{t('在烧 credits', 'overage in use')}</Badge>
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
            <Distribution items={ev.device_ids_out_7d} empty={t('旧记录未记', 'older rows without the column')} />
          </Fact>
        </div>
        <div className="sm:col-span-2 lg:col-span-4">
          <Fact label={t('出口代理分布（7 天）', 'Proxies (7d)')}>
            <Distribution items={ev.proxies_7d} empty={t('全部直连或旧记录未记', 'all direct, or older rows without the column')} />
          </Fact>
        </div>
      </div>

      <div className="flex items-center justify-between gap-2">
        <div className="text-sm font-medium">
          {t('封前流水时间线', 'Traffic timeline before the ban')}
          <span className="ml-2 text-xs text-muted-foreground">
            {t('封前 7 天 + 封后 10 分钟内到达的请求，含触发那一发', 'Requests from 7 days before to 10 minutes after, including the triggering one')}
          </span>
        </div>
        <Button size="sm" variant="outline" onClick={copyAll} disabled={logs.isPending}>
          <CopyIcon />{t('复制 JSON', 'Copy JSON')}
        </Button>
      </div>

      {logs.isPending ? (
        <div className="flex justify-center py-6"><Spinner /></div>
      ) : logs.isError ? (
        <Alert variant="error">
          <AlertTitle>{t('读取失败', 'Failed to load')}</AlertTitle>
          <AlertDescription>{extractError(logs.error, language)}</AlertDescription>
        </Alert>
      ) : rows.length === 0 ? (
        <div className="py-4 text-center text-sm text-muted-foreground">
          {t('这次封号之前没有留下流水（可能已被裁剪，或账号刚加进来就被封）', 'No traffic was recorded before this ban (pruned, or the account was banned right after being added)')}
        </div>
      ) : (
        <div className="overflow-x-auto rounded-md border bg-background">
          <Table className="text-xs">
            <TableHeader>
              <TableRow>
                <TableHead className="whitespace-nowrap">{t('时间', 'Time')}</TableHead>
                <TableHead>{t('状态', 'Status')}</TableHead>
                <TableHead>{t('模型', 'Model')}</TableHead>
                <TableHead>{t('设备（来访→出站）', 'Device (in→out)')}</TableHead>
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
      <DialogPopup className="max-w-6xl">
        <DialogHeader>
          <DialogTitle>{t('封号记录', 'Ban events')}</DialogTitle>
          <DialogDescription>
            {t(
              '每次自动封停一条：上游原话、触发请求、账号当时的等级/代理/用量快照，以及封前 7 天的全部流水（含取证列）。解封、删号、流水裁剪都不会抹掉这里的记录。',
              'One row per automatic disable: the upstream message, the triggering request, a snapshot of tier/proxy/usage at the time, and every request from the 7 days before (with forensic columns). Re-enabling, deleting the account, or pruning logs never removes these.',
            )}
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
                <EmptyTitle>{t('还没有封号记录', 'No ban events yet')}</EmptyTitle>
                <EmptyDescription>
                  {t('账号被上游自动封停时会在这里留一条，之后再解封或删号也不会消失。', 'When an account is auto-disabled by an upstream error a row lands here and stays, even after re-enabling or deletion.')}
                </EmptyDescription>
              </EmptyHeader>
            </Empty>
          ) : (
            <div className="overflow-hidden rounded-md border">
              <Table>
                <TableHeader>
                  <TableRow>
                    <TableHead className="w-8" />
                    <TableHead className="whitespace-nowrap">{t('时间', 'Time')}</TableHead>
                    <TableHead>{t('账号', 'Account')}</TableHead>
                    <TableHead>{t('来源', 'Source')}</TableHead>
                    <TableHead>{t('状态', 'Status')}</TableHead>
                    <TableHead>{t('原因', 'Reason')}</TableHead>
                    <TableHead className="text-right">{t('封前 7 天', '7d')}</TableHead>
                  </TableRow>
                </TableHeader>
                <TableBody>
                  {events.map((ev) => {
                    const isOpen = expanded === ev.id
                    return (
                      <React.Fragment key={ev.id}>
                        <TableRow
                          className="cursor-pointer"
                          onClick={() => setExpanded(isOpen ? null : ev.id)}
                        >
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
                            {t(`${ev.requests_7d} 次 / 出站 ${ev.devices_out_7d} 台`, `${ev.requests_7d} req / ${ev.devices_out_7d} dev out`)}
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
