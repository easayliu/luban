import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { SearchIcon } from 'lucide-react'
import { listCredentialUsage, listUsage, type UsageLog } from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import {
  cn, displayCredentialLabel, extractError, formatFullTime, formatUsd, parseSessionKey,
} from '@/lib/utils'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Dialog, DialogDescription, DialogHeader, DialogPanel, DialogPopup, DialogTitle,
} from '@/components/ui/dialog'
import { Empty, EmptyDescription, EmptyHeader, EmptyTitle } from '@/components/ui/empty'
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import { Form } from '@/components/ui/form'
import { Input } from '@/components/ui/input'
import { Spinner } from '@/components/ui/spinner'
import { RequestIdChip, statusVariant } from '@/components/usage-shared'

/**
 * 按请求 id 查一条流水。
 *
 * 排查路径：New API 日志里的 `upstream_request_id`（就是 luban 回在 `X-Oneapi-Request-Id`
 * 上的那个 `req_…`），贴进来直接看到它走的是哪个账号、模型、状态、用量与花费。
 * 不限账号——拿着 id 来的人不知道它落在哪个号上，这正是要查的东西。
 */
/**
 * 点进流水时带的筛选：某个模型、某个账号，或某条模拟会话，取最近几小时。
 *
 * 三者可叠（会话那条总是连着账号一起传，会话键本来就属于某个号）。
 */
export interface UsageDrillFilter {
  model?: string
  credId?: number
  /** 只看这条模拟会话的请求（`session_bindings.session_key`），见名额对话框的「看请求」。 */
  sessionKey?: string
  /** 展示名（模型名、账号 label，或会话的槽位与 id）。 */
  label: string
  hours: number
}

export function RequestLookupDialog({
  open,
  onOpenChange,
  filter,
  initialId,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  /** 给了就不显示请求 id 的输入框，直接列这个模型 / 账号最近的 50 条。 */
  filter?: UsageDrillFilter
  /**
   * 开着就直接查这个 id（请求明细里点某一行的请求 id 带进来的），输入框仍留着，查完还能
   * 顺手改成别的 id 或会话 id 再查。
   *
   * 只当初值读一次：调用方按「要查谁」挂载这个对话框（`{id && <RequestLookupDialog …/>}`），
   * 换一个 id 就是换一次挂载，不必再拿 effect 去同步。
   */
  initialId?: string
}) {
  const { t, language, locale } = useI18n()
  const [draft, setDraft] = useState(initialId ?? '')
  const [submitted, setSubmitted] = useState(initialId ?? '')
  const query = useQuery({
    queryKey: filter
      ? [
          'request-drill', filter.model ?? null, filter.credId ?? null,
          filter.sessionKey ?? null, filter.hours,
        ]
      : ['request-lookup', submitted],
    queryFn: () => {
      // 贴进来的是 uuid 就按**会话 id** 查（出站与来访两侧任一命中，见后端 session_id_in）：
      // 走模拟路径时上游看到的会话 id 与客户端自己那个不是同一个，而来查的人手里通常只有
      // 客户端那个。其余按请求 id 精确查。
      if (!filter) {
        return looksLikeUuid(submitted)
          ? listUsage({ session_id: submitted, limit: 50 })
          : listUsage({ request_id: submitted, limit: 50 })
      }
      const params = {
        model: filter.model,
        session_key: filter.sessionKey,
        hours: filter.hours,
        limit: 50,
      }
      return filter.credId != null
        ? listCredentialUsage(filter.credId, params)
        : listUsage(params)
    },
    enabled: open && (filter != null || submitted !== ''),
  })
  const rows = query.data?.logs ?? []
  const drillTitle = filter
    ? t(
        `${filter.label} · 近 ${filter.hours} 小时 · 最近 ${rows.length.toLocaleString(locale)} 条`,
        `${filter.label} · last ${filter.hours}h · latest ${rows.length.toLocaleString(locale)}`,
      )
    : ''

  const submit = () => {
    const id = draft.trim()
    if (!id) return
    // 同一个 id 再点一次也要真的再查：首次没查到、后台刚落库、上次网络失败，都是重查的理由。
    // key 没变时 setState 是空操作，得显式 refetch。
    if (id === submitted) void query.refetch()
    else setSubmitted(id)
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogPopup size="lg">
        <DialogHeader>
          <DialogTitle>{filter ? t('请求明细', 'Request details') : t('请求查询', 'Request lookup')}</DialogTitle>
          <DialogDescription>
            {filter
              ? drillTitle
              : t(
                  '粘贴 luban 在响应头 X-Request-Id / X-Oneapi-Request-Id 中返回的请求 ID（New API 日志里叫 upstream_request_id，报错信息里叫 luban request id），查看它在这里的流水。也可以粘贴一个会话 ID（uuid），查看该会话的全部请求：客户端自己的会话 ID 和上游看到的会话 ID 都能识别。',
                  'Paste the request ID luban returned in the X-Request-Id / X-Oneapi-Request-Id response header (upstream_request_id in New API logs, "luban request id" in error messages) to find its record here. You can also paste a session ID (uuid) to list all of that session\'s requests; both the client\'s own session ID and the one upstream sees are recognized.',
                )}
          </DialogDescription>
        </DialogHeader>
        <DialogPanel className="space-y-4">
          {!filter && <Form onSubmit={(e) => { e.preventDefault(); submit() }}>
            <Field>
              <FieldLabel htmlFor="request-lookup-id">
                {t('请求 ID 或会话 ID', 'Request ID or session ID')}
              </FieldLabel>
              <div className="flex gap-2">
                <Input
                  id="request-lookup-id"
                  autoFocus
                  autoComplete="off"
                  spellCheck={false}
                  className="font-mono"
                  placeholder="req_Q3k9ZpL2mNv7Xb1c"
                  value={draft}
                  onChange={(e) => setDraft(e.target.value)}
                />
                <Button type="submit" disabled={!draft.trim() || query.isFetching}>
                  {query.isFetching ? <Spinner /> : <SearchIcon />}
                  {t('查询', 'Search')}
                </Button>
              </div>
              <FieldDescription>
                {looksLikeUuid(draft.trim())
                  ? t(
                      '按会话 ID 查询：客户端自报的和上游看到的会话 ID 都会匹配，列出该会话最近 50 条请求。',
                      'Looking up by session ID: matches both the client-reported and the upstream-visible session ID, listing the session\'s latest 50 requests.',
                    )
                  : t('精确匹配；流水只保留最近 30 天。', 'Exact match; logs are retained for 30 days.')}
              </FieldDescription>
            </Field>
          </Form>}

          {!filter && submitted === '' ? null : query.isPending ? (
            <div className="flex items-center gap-2 py-6 text-sm text-muted-foreground">
              <Spinner />{t('正在查询', 'Searching')}
            </div>
          ) : query.isError ? (
            <Alert variant="error">
              <AlertTitle>{t('查询失败', 'Lookup failed')}</AlertTitle>
              <AlertDescription>{extractError(query.error, language)}</AlertDescription>
            </Alert>
          ) : rows.length === 0 ? (
            <Empty className="py-8">
              <EmptyHeader>
                <EmptyTitle className="text-base">{filter ? t('这段时间没有请求', 'No requests in this period') : t('没有找到这条请求', 'No request found')}</EmptyTitle>
                <EmptyDescription>
                  {t(
                    '请确认 ID 完整（请求 ID 形如 req_ 加 16 位随机字符，会话 ID 是一个 uuid）；超过 30 天的流水已被清理。0.3.139 之前的记录没有客户端侧的会话 ID，只能用上游看到的会话 ID 查询。',
                    'Check that the ID is complete (request IDs look like req_ plus 16 random characters; a session ID is a uuid). Records older than 30 days have been pruned. Records from before 0.3.139 have no client-side session ID; look those up with the upstream one.',
                  )}
                </EmptyDescription>
              </EmptyHeader>
            </Empty>
          ) : (
            <ul className="space-y-2" aria-label={t('查询结果', 'Results')}>
              {rows.map((log) => <LookupRow key={log.id} log={log} locale={locale} />)}
            </ul>
          )}
        </DialogPanel>
      </DialogPopup>
    </Dialog>
  )
}

/** 形态判据与后端 `looks_like_uuid` 同口径：8-4-4-4-12 的十六进制。 */
function looksLikeUuid(v: string): boolean {
  return /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i.test(v)
}

/**
 * 悬浮里把两侧的会话 id 都给全。走模拟路径时它们是两个不同的 uuid：来访那个是客户端自己
 * 知道的，出站那个是上游看到的（按槽位派生或按账号钉住）。本地拒绝的只有来访那一侧。
 */
function sessionTitle(log: UsageLog): string | undefined {
  if (!log.session_id && !log.session_id_in) return undefined
  return `in:  ${log.session_id_in ?? '—'}\nout: ${log.session_id ?? '—'}`
}

function LookupRow({ log, locale }: { log: UsageLog; locale: string }) {
  const { t, language } = useI18n()
  const num = (v: number | null) => (v == null ? '—' : v.toLocaleString(locale))
  const ms = (v: number | null) => (v == null ? '—' : `${v.toLocaleString(locale)}ms`)
  const deviceShort = log.device_id
    ? log.device_id.startsWith('sim:') ? `sim:${log.device_id.slice(4, 12)}` : log.device_id.slice(0, 8)
    : '—'
  // 会话两项是两回事：`session_id` 是上游看到的那个（按「账号 + 槽位」派生、对话之间复用），
  // `session_key` 才是这条对话自己的身份（名额对话框里点「看请求」筛的就是它）。带设备身份的
  // 来访与非模拟路径没有键，0.3.139 之前的旧记录也没有。
  const key = log.session_key ? parseSessionKey(log.session_key) : null
  return (
    <li className="rounded-lg border bg-card p-3 text-xs">
      <div className="flex flex-wrap items-center gap-2">
        <Badge variant={statusVariant(log.status)} size="sm" className="tabular-nums">{log.status}</Badge>
        <span className="font-medium">
          {log.cred_label
            ? displayCredentialLabel(log.cred_label, language)
            : log.cred_id == null
              ? t('未转发（本地拒绝）', 'Not forwarded (rejected locally)')
              : t('（账号已删除）', '(account deleted)')}
        </span>
        {log.cred_id != null && <span className="tabular-nums text-muted-foreground">#{log.cred_id}</span>}
        <span className="ml-auto tabular-nums text-muted-foreground">{formatFullTime(log.ts, language)}</span>
      </div>
      <dl className="mt-2 grid grid-cols-2 gap-x-4 gap-y-1.5 sm:grid-cols-3">
        <Fact label={t('模型', 'Model')}><span title={log.model ?? undefined}>{log.model ?? '—'}</span></Fact>
        <Fact label={t('输入 / 输出', 'In / out')}>{num(log.input_tokens)} / {num(log.output_tokens)}</Fact>
        <Fact label={t('缓存写 / 读', 'Cache w/r')}>{num(log.cache_creation_tokens)} / {num(log.cache_read_tokens)}</Fact>
        <Fact label={t('首字 / 总耗时', 'TTFT / total')}>{ms(log.ttft_ms)} / {ms(log.total_ms)}</Fact>
        <Fact label={t('花费', 'Cost')}>
          <span className={cn(log.cost_usd == null && 'text-muted-foreground')}>
            {log.cost_usd == null ? '—' : formatUsd(log.cost_usd)}
          </span>
        </Fact>
        <Fact label={t('设备（入站）', 'Device (inbound)')}><span className="font-mono" title={log.device_id ?? undefined}>{deviceShort}</span></Fact>
        <Fact label={t('设备（出站）', 'Device (outbound)')}><span className="font-mono" title={log.device_id_out ?? undefined}>{log.device_id_out?.slice(0, 8) ?? '—'}</span></Fact>
        <Fact label={t('请求 ID', 'Request ID')}><RequestIdChip id={log.request_id} full /></Fact>
        <Fact label={t('上游 request-id', 'Upstream request-id')}><RequestIdChip id={log.upstream_request_id} full /></Fact>
        <Fact label={t('路径', 'Path')}><span className="font-mono" title={log.path}>{log.path}</span></Fact>
        <Fact label={t('会话（入站 → 出站）', 'Session (inbound → outbound)')}>
          <span className="font-mono" title={sessionTitle(log)}>
            {log.session_id_in?.slice(0, 8) ?? '—'}
            <span className="text-muted-foreground">→{log.session_id?.slice(0, 8) ?? '—'}</span>
          </span>
        </Fact>
        <Fact label={t('会话键', 'Session key')}>
          <span className="font-mono" title={log.session_key ?? undefined}>
            {key ? `${key.source === 'pfx' ? t('前缀', 'prefix') : t('自带', 'client')} ${key.value.slice(0, 8)}` : '—'}
          </span>
        </Fact>
      </dl>
      {(log.ua || log.ua_out) && (
        <p className="mt-2 truncate border-t pt-1.5 text-2xs text-muted-foreground" title={log.ua_out && log.ua_out !== log.ua ? `${log.ua ?? '—'}\n→ ${log.ua_out}` : (log.ua ?? undefined)}>
          {log.ua ?? t('无（luban 自身发起）', 'None (sent by luban itself)')}
        </p>
      )}
      <ForensicTags log={log} />
      {log.response_excerpt && (
        <div className="mt-2 rounded-md border border-warning/40 bg-warning/10 px-2.5 py-2">
          <p className="text-2xs font-medium">
            {t('上游回复（截取开头）', 'Upstream reply (excerpt)')}
          </p>
          <pre className="mt-1 max-h-40 overflow-y-auto whitespace-pre-wrap break-all font-mono text-2xs text-muted-foreground">
            {log.response_excerpt}
          </pre>
        </div>
      )}
      {(log.error_type || log.error_message) && (
        <Alert variant="error" className="mt-2 py-2">
          <AlertTitle className="font-mono text-2xs">
            {log.error_type ?? t('错误', 'Error')}
          </AlertTitle>
          {log.error_message && (
            <AlertDescription className="max-h-40 overflow-y-auto whitespace-pre-wrap break-words text-2xs">
              {log.error_message}
            </AlertDescription>
          )}
        </Alert>
      )}
    </li>
  )
}

/** 已知改写/结局标签的可读名；认不出的原样显示。 */
/** 走模拟路径的原因（流水 `sim_reason` 列）：三道判据里没过的第一道。 */
function simReasonLabel(reason: string, t: (zh: string, en: string) => string): string {
  switch (reason) {
    case 'not_cc_client':
      return t('UA 不是可信 CC 版本', 'UA is not a trusted CC version')
    case 'identity_malformed':
      return t('身份字段格式错误', 'Malformed identity fields')
    case 'not_cc_shaped':
      return t('system 缺少身份句和 billing header', 'No identity line or billing header in system')
    case 'no_base_prompt':
      return t('缺少基座提示词', 'Missing base prompt')
    case 'tools_not_cc':
      return t('tools 中没有官方工具名', 'No official tool names in tools')
    case 'probe':
      return t('luban 探测', 'luban probe')
    default:
      return reason
  }
}

function rewriteLabel(tag: string, t: (zh: string, en: string) => string): string {
  switch (tag) {
    case 'rejected_locally':
      return t('本地拒绝，未转发', 'Rejected locally, not forwarded')
    case 'refusal_replay':
      return t('本地回放已学到的上游拒答，未转发', 'Replayed a learned upstream refusal locally, not forwarded')
    case 'app_refusal_replay':
      return t('本地回放按应用学到的上游拒答，未转发', 'Replayed a learned app-level upstream refusal locally, not forwarded')
    case 'probe_reply':
      return t('命中探针特征，本地回复最小的 200，未转发', 'Probe signature matched, answered locally with a minimal 200, not forwarded')
    case 'upstream_401':
      return t('上游 401，没有可换的账号', 'Upstream 401, no account to switch to')
    case 'model_unsupported':
      return t('套餐不含该模型，可换的账号已用尽', 'Model not in plan, no accounts left to try')
    case 'connection_error':
      return t('上游连接失败', 'Upstream connection failed')
    case 'empty_reply':
      return t('上游返回 200 但没有输出', 'Upstream returned 200 with no output')
    case 'refusal':
      return t('上游拒答（refusal）', 'Upstream refusal')
    case 'served_by_fallback':
      return t('拒答后由 fallback 模型作答', 'Refused, then answered by the fallback model')
    case 'no_fallbacks':
      return t('上游不接受 fallback 目标，去掉后重试', 'Upstream rejected the fallback target; retried without fallbacks')
    case 'demoted_thinking':
      return t('降级 thinking 后重试', 'Retried with thinking demoted')
    case 'no_prefill':
      return t('去掉 prefill 后重试', 'Retried without prefill')
    case 'injected_tool_called':
      return t('模型调用了注入的 CC 工具（客户端未声明）', 'Model called an injected CC tool the client never declared')
    case 'tools_filled':
      return t('客户端请求未带工具，已补上官方工具', 'Client request had no tools; official tools were added')
    default:
      return tag
  }
}

/**
 * 取证标签一行：这条请求在 luban 里经历了什么（改写/重试/结局）、走了模拟没有、上游判成第三方
 * 没有、出口代理是哪个。没有一项时整行不出现。
 */
function ForensicTags({ log }: { log: UsageLog }) {
  const { t } = useI18n()
  const tags = (log.rewrites ?? '').split(',').map((x) => x.trim()).filter(Boolean)
  if (tags.length === 0 && !log.simulated && !log.third_party && !log.proxy) return null
  return (
    <div className="mt-2 flex flex-wrap items-center gap-1.5">
      {tags.map((tag) => (
        <Badge
          key={tag}
          variant={tag === 'rejected_locally' || tag === 'refusal_replay' || tag === 'app_refusal_replay' || tag === 'probe_reply' ? 'secondary' : 'outline'}
          size="sm"
          title={tag}
        >
          {rewriteLabel(tag, t)}
        </Badge>
      ))}
      {log.simulated && (
        <Badge variant="info" size="sm" title={log.sim_reason ?? undefined}>
          {t('模拟路径', 'Simulated')}
          {log.sim_reason ? ` · ${simReasonLabel(log.sim_reason, t)}` : ''}
        </Badge>
      )}
      {log.third_party && <Badge variant="error" size="sm">{t('上游判为第三方', 'Flagged as third-party')}</Badge>}
      {log.proxy && (
        <span className="truncate font-mono text-2xs text-muted-foreground" title={log.proxy}>
          {t('出口', 'Egress')} {log.proxy}
        </span>
      )}
    </div>
  )
}

function Fact({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div className="min-w-0">
      <dt className="text-2xs text-muted-foreground">{label}</dt>
      <dd className="truncate tabular-nums">{children}</dd>
    </div>
  )
}
