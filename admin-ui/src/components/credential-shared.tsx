import { useEffect, useRef, useState } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import axios from 'axios'
import {
  ActivityIcon, ChevronDownIcon, ChevronUpIcon, CircleCheckIcon, CircleXIcon,
  PencilIcon, RefreshCwIcon, SmartphoneIcon, Trash2Icon,
} from 'lucide-react'
import {
  deleteCredential, probeCredential, refreshCredential, setDeviceLimit, setDisabled, setLabel,
  setPriority,
  type Credential, type ProbeQuota, type ProbeResult,
} from '@/api/credentials'
import { cn, extractError, formatClockTime, formatFullTime } from '@/lib/utils'
import {
  AlertDialog, AlertDialogClose, AlertDialogDescription, AlertDialogFooter,
  AlertDialogHeader, AlertDialogPopup, AlertDialogTitle,
} from '@/components/ui/alert-dialog'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import type { BadgeProps } from '@/components/ui/badge'
import { Badge } from '@/components/ui/badge'
import {
  Dialog, DialogDescription, DialogHeader, DialogPanel, DialogPopup, DialogTitle,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Empty, EmptyDescription, EmptyHeader, EmptyTitle } from '@/components/ui/empty'
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import { Form } from '@/components/ui/form'
import { Input } from '@/components/ui/input'
import { MenuItem, MenuPopup, MenuSeparator, MenuShortcut } from '@/components/ui/menu'
import { Spinner } from '@/components/ui/spinner'
import { toastManager } from '@/components/ui/toast'

/** 设备上限输入框的初值：跟随默认→空串；明确不限→0；独立上限→数值。 */
export function limitToInput(deviceLimit: number): string {
  if (deviceLimit === 0) return ''
  return deviceLimit < 0 ? '0' : String(deviceLimit)
}

/** 输入框内容 → 后端三态值：空=跟随全局默认(0)；0/负=该账号不限(-1)；正数=独立上限。 */
export function inputToLimit(v: string): number {
  const t = v.trim()
  if (t === '') return 0
  const n = Math.floor(Number(t))
  return Number.isFinite(n) && n > 0 ? n : -1
}

/**
 * 快照里**仍属于当前窗口**的使用率；窗口已过重置时刻的返回 null。
 *
 * 额度快照只在有请求经过时才更新（后端取该账号最后一条带限流头的日志，不设时间下限），
 * 所以账号闲下来之后百分比会一直停在最后一次的值。5h 窗口的 reset 时刻一过，那个值就
 * 跟现在毫无关系了——号早就满血，后台却还在报「额度将满」、红边、排在异常账号最前面。
 *
 * reset 缺失时无从判断新旧，保持原值（宁可虚报也不要漏报快满的号）。
 */
export function liveQuota(cred: Credential): { u5h: number | null; u7d: number | null } {
  const q = cred.quota
  if (!q) return { u5h: null, u7d: null }
  const now = Math.floor(Date.now() / 1000)
  const live = (u: number | null, reset: number | null) =>
    u != null && (reset == null || reset > now) ? u : null
  return {
    u5h: live(q.rl_5h_utilization, q.rl_5h_reset),
    u7d: live(q.rl_7d_utilization, q.rl_7d_reset),
  }
}

/** 账号是否处于「额度将满」（5h / 7d 任一 ≥90%，停用的不算；已重置的窗口不算）。 */
export function isNearLimit(cred: Credential): boolean {
  const { u5h, u7d } = liveQuota(cred)
  return !cred.disabled && Math.max(u5h ?? 0, u7d ?? 0) >= 0.9
}

/**
 * 账号是否异常——**只看被上游封禁**。
 *
 * `expired` 不算：那只是 access_token 到点了，而刷新是惰性的（选号之后、发请求之前必刷，
 * 见后端 `ensure_fresh_token`），所以闲置一夜的健康账号第二天必然是 `expired=true`，
 * 下一个请求会自动把它刷好。把它算成异常，等于每天早上给一批好号刷上红色、排到最前、
 * 塞进「需处理」——而这里真正要回答的是「refresh_token 还灵不灵」，那个答案在 `ban_reason`：
 * 刷新失败且判定为永久失效时，后端会 `mark_banned` 写进去。
 */
export function isAbnormal(cred: Credential): boolean {
  return !!cred.ban_reason
}

// ---------- 排序 ----------
//
// 排序模型放这里，列表表头与工具栏下拉共用同一份定义，避免两处各写一套导致
// 「表头能排的维度和下拉里的对不上」。

export type SortKey =
  | 'priority' | 'status' | 'name' | 'tier'
  | 'usage5h' | 'devices' | 'cost' | 'recent' | 'created'

export type SortDir = 'asc' | 'desc'

/** 全部可排序维度（下拉菜单按此顺序渲染；表头列是其中的子集）。 */
export const SORTS: { key: SortKey; label: string }[] = [
  { key: 'priority', label: '优先级' },
  { key: 'status', label: '状态' },
  { key: 'name', label: '名称' },
  { key: 'tier', label: '套餐' },
  { key: 'usage5h', label: '5h 使用率' },
  { key: 'devices', label: '设备数' },
  { key: 'cost', label: '累计花费' },
  { key: 'recent', label: '最近使用' },
  { key: 'created', label: '添加时间' },
]

export const SORT_KEYS = SORTS.map((s) => s.key)

/**
 * 各维度首次选中时的默认方向——按「用户多半想先看什么」定：
 * 优先级/名称是升序（P0 在前、A→Z），其余都是降序（最严重、用得最多、最贵、最近的排前面）。
 * 再次点击同一维度会翻转方向，此处只决定初值。
 */
export const SORT_DIR_DEFAULT: Record<SortKey, SortDir> = {
  priority: 'asc',
  name: 'asc',
  status: 'desc',
  tier: 'desc',
  usage5h: 'desc',
  devices: 'desc',
  cost: 'desc',
  recent: 'desc',
  created: 'desc',
}

/** 套餐档位 → 序号（越大越高档）。按容量排而非字母序，`max_20x` 才会排在 `pro` 前面。 */
function tierRank(tier: string | null): number {
  const t = (tier ?? '').toLowerCase()
  if (t.includes('20x')) return 5
  if (t.includes('5x')) return 4
  if (t.includes('max')) return 3
  if (t.includes('pro')) return 2
  if (t.includes('free')) return 1
  return 0
}

/**
 * 状态 → 严重度（越大越需要关注）；降序即「先看有问题的」。
 *
 * 不含 `expired`：它不是「有问题」，见 [`isAbnormal`]。
 */
function statusRank(c: Credential): number {
  if (c.ban_reason) return 4
  if (!c.disabled && c.rate_limited_secs > 0) return 3
  if (isNearLimit(c)) return 2
  if (c.disabled) return 1
  return 0
}

/** 单维度的升序比较；方向由 [`sortCreds`] 统一套用，避免每个 case 都写两遍。 */
function compareBy(key: SortKey, a: Credential, b: Credential): number {
  switch (key) {
    case 'status':
      return statusRank(a) - statusRank(b)
    case 'name':
      return a.label.localeCompare(b.label, 'zh-CN')
    case 'tier':
      return tierRank(a.tier) - tierRank(b.tier)
    case 'usage5h':
      // 无额度数据、以及窗口已重置的（快照是上个周期的）一并垫底：升序时排最前、
      // 降序时排最后，不会混在真实数值中间。
      return (liveQuota(a).u5h ?? -1) - (liveQuota(b).u5h ?? -1)
    case 'devices':
      return a.device_count - b.device_count
    case 'cost':
      return (a.cost_total ?? 0) - (b.cost_total ?? 0)
    case 'recent':
      return (a.last_used ?? 0) - (b.last_used ?? 0)
    case 'created':
      return a.created_at - b.created_at
    case 'priority':
    default:
      return a.priority - b.priority
  }
}

/**
 * 按维度 + 方向排序（不改原数组）。
 *
 * 同值时一律按 id 升序兜底，保证顺序稳定——否则相同优先级的账号会在每次
 * 重新渲染时互相换位。
 */
export function sortCreds(list: Credential[], key: SortKey, dir: SortDir): Credential[] {
  const sign = dir === 'asc' ? 1 : -1
  return [...list].sort((a, b) => sign * compareBy(key, a, b) || a.id - b.id)
}

/**
 * 卡片视图与列表视图共用的写操作。各视图自行管理编辑态（重命名、设备上限输入框），
 * 这里只封装请求与失败提示，避免两处重复维护同一套 mutation。
 */
export function useCredentialActions(cred: Credential, onRenamed?: () => void, onLimitSaved?: () => void) {
  const qc = useQueryClient()
  const invalidate = () => qc.invalidateQueries({ queryKey: ['credentials'] })
  const failure = (title: string, error: unknown) => toastManager.add({
    title,
    description: extractError(error),
    type: 'error',
  })

  const rename = useMutation({
    mutationFn: (label: string) => setLabel(cred.id, label),
    onSuccess: () => { onRenamed?.(); invalidate() },
    onError: (e) => failure('重命名失败', e),
  })
  const toggle = useMutation({
    mutationFn: (disabled: boolean) => setDisabled(cred.id, disabled),
    onSuccess: invalidate,
    onError: (e) => failure('操作失败', e),
  })
  const prio = useMutation({
    mutationFn: (p: number) => setPriority(cred.id, p),
    onSuccess: invalidate,
    onError: (e) => failure('设置优先级失败', e),
  })
  const limit = useMutation({
    mutationFn: (n: number) => setDeviceLimit(cred.id, n),
    onSuccess: () => { onLimitSaved?.(); invalidate() },
    onError: (e) => failure('设置设备上限失败', e),
  })
  const refresh = useMutation({
    mutationFn: () => refreshCredential(cred.id),
    onSuccess: () => { toastManager.add({ title: '已刷新', type: 'success' }); invalidate() },
    onError: (e) => failure('刷新失败', e),
  })
  const remove = useMutation({
    mutationFn: () => deleteCredential(cred.id),
    onSuccess: () => { toastManager.add({ title: '已删除', type: 'success' }); invalidate() },
    onError: (e) => failure('删除失败', e),
  })

  return { rename, toggle, prio, limit, refresh, remove }
}

export type CredentialActions = ReturnType<typeof useCredentialActions>

/**
 * ⋯ 菜单内容（刷新 / 重命名 / 优先级调整 / 删除），卡片与列表共用。
 *
 * 删除只往外抛意图，确认框由调用方渲染在菜单之外——菜单一关，挂在它里面的弹窗会跟着
 * 卸载，确认框根本来不及显示。
 */
export function CredentialMenuContent({
  cred, actions, onRename, onDeviceLimit, onTest, onRequestDelete,
}: {
  cred: Credential
  actions: CredentialActions
  onRename: () => void
  onDeviceLimit: () => void
  onTest: () => void
  onRequestDelete: () => void
}) {
  const { refresh, prio } = actions
  return (
    <MenuPopup align="end">
      <MenuItem onClick={() => refresh.mutate()} disabled={refresh.isPending}>
        <RefreshCwIcon className={refresh.isPending ? 'animate-spin' : undefined} />
        刷新 token
      </MenuItem>
      <MenuItem onClick={onTest}>
        <ActivityIcon />
        连通性测试
      </MenuItem>
      <MenuItem onClick={onRename}>
        <PencilIcon />
        重命名
      </MenuItem>
      <MenuItem onClick={onDeviceLimit}>
        <SmartphoneIcon />
        设备上限
      </MenuItem>
      <MenuSeparator />
      <MenuItem
        onClick={() => prio.mutate(cred.priority - 1)}
        disabled={prio.isPending}
        title="数值越小，调度优先级越高"
      >
        <ChevronUpIcon />
        提高优先级
        <MenuShortcut>P{cred.priority - 1}</MenuShortcut>
      </MenuItem>
      <MenuItem
        onClick={() => prio.mutate(cred.priority + 1)}
        disabled={prio.isPending}
        title="数值越大，调度优先级越低"
      >
        <ChevronDownIcon />
        降低优先级
        <MenuShortcut>P{cred.priority + 1}</MenuShortcut>
      </MenuItem>
      <MenuSeparator />
      <MenuItem variant="destructive" onClick={onRequestDelete}>
        <Trash2Icon />
        删除
      </MenuItem>
    </MenuPopup>
  )
}

/** 删除单个账号的确认框。卡片与列表共用，免得两处各写一遍后果说明。 */
export function DeleteCredentialDialog({
  cred, actions, open, onOpenChange,
}: {
  cred: Credential
  actions: CredentialActions
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  return (
    <AlertDialog open={open} onOpenChange={onOpenChange}>
      <AlertDialogPopup>
        <AlertDialogHeader>
          <AlertDialogTitle>删除账号</AlertDialogTitle>
          <AlertDialogDescription>
            删除「<span className="font-medium text-foreground">{cred.label}</span>」后，
            历史用量与设备绑定将一并清除，且无法恢复。
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogClose render={<Button variant="outline" />}>取消</AlertDialogClose>
          <Button
            variant="destructive"
            loading={actions.remove.isPending}
            onClick={() => actions.remove.mutate()}
          >
            删除
          </Button>
        </AlertDialogFooter>
      </AlertDialogPopup>
    </AlertDialog>
  )
}

/**
 * 测试弹窗里的备选模型。**只是快捷入口，不是白名单**——输入框可以随便填，后端也原样发给
 * 上游，故官方上新模型时不改这里也能测，只是少一个可点的按钮。
 *
 * 取现役四个模型族各一个（基座与 beta 串按族分家，见后端 `cc_system_base`/`cc_beta_seed`），
 * 这样点一遍就覆盖了四条不同的模拟路径。
 */
const PROBE_MODELS = [
  'claude-opus-5',
  'claude-sonnet-5',
  'claude-haiku-4-5',
  'claude-fable-5',
] as const

/** 一条测试记录：同一个弹窗里连测多次时按时间倒序累积，方便横向比较不同模型。 */
interface ProbeEntry {
  /** 自增序号，仅用作列表 key。 */
  seq: number
  /** 本次请求的模型名（`result.model` 是上游回报的，可能不同）。 */
  model: string
  result: ProbeResult
}

/** 一次在途探测。session 用来丢弃关闭弹窗后才到达的旧结果。 */
interface ProbeRequest {
  model: string
  controller: AbortController
  session: number
}

/** 耗时展示：1 秒以内用毫秒，超过用秒（保留一位小数）。 */
function formatLatency(ms: number): string {
  return ms < 1000 ? `${Math.round(ms)} ms` : `${(ms / 1000).toFixed(1)} s`
}

/**
 * 连通性测试弹窗：用**这一个**账号向上游发一条最小请求，测它能不能用某个模型。
 *
 * 卡片与列表共用。请求形态、代价与副作用见后端 `proxy::probe`——一句话：不选号、不占设备
 * 名额、失败也不自动停用账号，但会写一条用量日志（卡片上的额度与花费据此更新），
 * 也真的会打到上游、花掉一点点订阅额度。
 *
 * 结果列表在弹窗内累积（关掉即清空）：连测几个模型时，「opus 429 而 haiku 200」这种对照
 * 只有并排看才成立，一次只留最后一条就得靠人脑记。
 */
export function ConnectivityTestDialog({
  cred, open, onOpenChange,
}: {
  cred: Credential
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const qc = useQueryClient()
  const [model, setModel] = useState<string>(PROBE_MODELS[0])
  const [entries, setEntries] = useState<ProbeEntry[]>([])
  const seq = useRef(0)
  const session = useRef(0)
  const activeProbe = useRef<AbortController | null>(null)

  const probe = useMutation({
    mutationKey: ['credential-probe', cred.id],
    // 管理页即使被浏览器判为 offline，也应立即请求本机后端并得到明确失败，而不是静默 paused。
    networkMode: 'always',
    mutationFn: ({ model: requestModel, controller }: ProbeRequest) =>
      probeCredential(cred.id, requestModel, controller.signal),
    onSuccess: (result, request) => {
      if (request.session !== session.current) return
      const entrySeq = ++seq.current
      setEntries((prev) => [{ seq: entrySeq, model: request.model, result }, ...prev])
      // 测试是真实流量，账号状态照真实口径更新：可能刷新了过期 token（有效期变了）、
      // 可能停用了命中封号特征的号（ban_reason 变了）——卡片得跟着变，别让弹窗一个说法、
      // 列表另一个说法。上游拒绝也走 onSuccess（接口恒 200 带结果），所以这里就够了。
      qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    // 这条是「请求没发出去」（账号已被删、管理密码失效等），与「上游拒绝」不同：
    // 后者是 200 + 一份带状态码的结果，会进上面的列表。
    onError: (e, request) => {
      if (request.session !== session.current || axios.isCancel(e)) return
      toastManager.add({ title: '测试失败', description: extractError(e), type: 'error' })
    },
    onSettled: (_result, _error, request) => {
      if (activeProbe.current === request.controller) activeProbe.current = null
    },
  })

  const submit = () => {
    const m = model.trim()
    // mutation state 要到下一次 render 才更新；ref 同步挡住双击/连续回车造成的重复扣费。
    if (!m || activeProbe.current) return
    const controller = new AbortController()
    activeProbe.current = controller
    probe.mutate({ model: m, controller, session: session.current })
  }

  const cancelProbe = () => {
    session.current += 1
    activeProbe.current?.abort()
    activeProbe.current = null
    probe.reset()
  }

  // 账号因筛选、分页或重新排序离开页面时，终止前端请求并丢弃旧结果，避免重开后继承 pending。
  useEffect(() => () => {
    session.current += 1
    activeProbe.current?.abort()
  }, [])

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next) {
          cancelProbe()
          seq.current = 0
          setEntries([])
        }
        onOpenChange(next)
      }}
    >
      <DialogPopup>
        <DialogHeader>
          <DialogTitle>连通性测试</DialogTitle>
          <DialogDescription>
            使用「<span className="font-medium text-foreground">{cred.label}</span>」向上游发送最小请求；
            会消耗少量订阅额度并按实际用量计入该账号。测试结果与真实流量同等对待：限流会进入冷却，检测到封禁会自动停用。
          </DialogDescription>
        </DialogHeader>
        <DialogPanel className="space-y-4">
          <Form
            className="space-y-3"
            onSubmit={(e) => { e.preventDefault(); submit() }}
          >
            <Field>
              <FieldLabel>测试模型</FieldLabel>
              <div className="flex flex-wrap gap-2">
                {PROBE_MODELS.map((m) => (
                  <Button
                    key={m}
                    type="button"
                    size="xs"
                    variant={model === m ? 'secondary' : 'outline'}
                    aria-pressed={model === m}
                    onClick={() => setModel(m)}
                  >
                    <span>{m}</span>
                  </Button>
                ))}
              </div>
              <FieldDescription>也可以直接输入尚未列出的模型名称。</FieldDescription>
            </Field>
            <Field>
              <FieldLabel htmlFor={`probe-model-${cred.id}`}>模型名称</FieldLabel>
              <div className="flex w-full items-center gap-2">
                <Input
                  id={`probe-model-${cred.id}`}
                  value={model}
                  onChange={(e) => setModel(e.target.value)}
                  placeholder="如 claude-opus-5"
                  className="min-w-0 flex-1"
                />
                <Button
                  type={probe.isPending ? 'button' : 'submit'}
                  variant={probe.isPending ? 'outline' : 'default'}
                  disabled={!probe.isPending && !model.trim()}
                  onClick={probe.isPending ? cancelProbe : undefined}
                >
                  {probe.isPending ? <Spinner /> : <ActivityIcon />}
                  {probe.isPending ? '取消测试' : '开始测试'}
                </Button>
              </div>
            </Field>
          </Form>

          {entries.length === 0 ? (
            <Empty className="py-8">
              <EmptyHeader>
                <EmptyTitle>尚无测试结果</EmptyTitle>
                <EmptyDescription>选择模型开始测试，结果会显示实时额度或上游错误。</EmptyDescription>
              </EmptyHeader>
            </Empty>
          ) : (
            <ul className="space-y-2">
              {entries.map((e) => (
                <ProbeEntryRow key={e.seq} entry={e} />
              ))}
            </ul>
          )}
        </DialogPanel>
      </DialogPopup>
    </Dialog>
  )
}

/** 一条测试结果：成败徽章 + 模型名 + 状态码/耗时，失败时附上游错误原文。 */
function ProbeEntryRow({ entry }: { entry: ProbeEntry }) {
  const { model, result } = entry
  const Icon = result.ok ? CircleCheckIcon : CircleXIcon
  return (
    <li>
      <Alert variant={result.ok ? 'success' : 'error'}>
        <Icon aria-hidden />
        <AlertTitle className="flex flex-wrap items-center gap-2">
          <span>{model}</span>
          <Badge variant={result.ok ? 'success' : 'error'} size="sm">
            {result.status > 0 ? `HTTP ${result.status}` : '未送达上游'}
          </Badge>
          <span className="font-normal text-muted-foreground">{formatLatency(result.latency_ms)}</span>
          {result.model && result.model !== model && (
            <span className="font-normal text-muted-foreground" title="上游实际使用的模型">
              → {result.model}
            </span>
          )}
        </AlertTitle>
        <AlertDescription>
          {result.error && (
            <p className="break-words">
              {result.error_type && (
                <span className="mr-1 text-destructive-foreground">{result.error_type}</span>
              )}
              {result.error}
            </p>
          )}
          {result.quota && <ProbeQuotaLine quota={result.quota} />}
        </AlertDescription>
      </Alert>
    </li>
  )
}

/**
 * 本次响应带回的额度：5h / 7d 使用率 + 各自的重置时刻，429 时另标出上游要求的等待时长。
 *
 * 这是**这一刻**上游的说法，不是卡片上那份按用量日志存下来的快照——测试不写日志，所以看完
 * 就没了，页面上别处不会跟着变。
 */
function ProbeQuotaLine({ quota }: { quota: ProbeQuota }) {
  const win = (label: string, util: number | null, reset: number | null) => {
    if (util == null && reset == null) return null
    const pct = util == null ? null : Math.round(util * 100)
    return (
      <span
        key={label}
        className="tnum"
        title={reset != null ? `${label} 窗口 ${formatFullTime(reset)} 重置` : undefined}
      >
        {label}{' '}
        {pct == null ? (
          '—'
        ) : (
          <span className={cn('font-medium', quotaTone(util))}>{pct}%</span>
        )}
        {reset != null && ` · ${formatClockTime(reset)} 重置`}
      </span>
    )
  }
  return (
    <div className="mt-1 flex flex-wrap items-center gap-x-2.5 gap-y-1 text-2xs text-muted-foreground">
      {win('5h', quota.rl_5h_utilization, quota.rl_5h_reset)}
      {win('7d', quota.rl_7d_utilization, quota.rl_7d_reset)}
      {/* 429 才有。它是上游对**这次**拒绝给出的等待时间，比窗口 reset 更直接。 */}
      {quota.retry_after_secs != null && (
        <span className="text-destructive-foreground" title={`上游 retry-after: ${quota.retry_after_secs} 秒`}>
          需等待 {formatWait(quota.retry_after_secs)}
        </span>
      )}
      {/* allowed 是常态，不占地方；warning/rejected 才值得说一句。 */}
      {quota.unified_status && quota.unified_status !== 'allowed' && (
        <span
          className={cn(
            quota.unified_status === 'rejected'
              ? 'text-destructive-foreground'
              : 'text-warning-foreground',
          )}
          title={
            quota.rl_representative
              ? `上游整体额度状态（当前由 ${quota.rl_representative} 窗口决定）`
              : '上游整体额度状态'
          }
        >
          {quota.unified_status}
        </span>
      )}
    </div>
  )
}

/** 使用率配色，阈值与卡片额度条一致（≥90% 红、≥70% 橙）。 */
function quotaTone(util: number | null): string {
  if (util == null) return 'text-foreground/80'
  if (util >= 0.9) return 'text-destructive-foreground'
  return util >= 0.7 ? 'text-warning-foreground' : 'text-foreground/80'
}

/** 等待时长：分钟以内给秒，一天以内给小时，再长给天（上游真给过 63 小时）。 */
function formatWait(secs: number): string {
  if (secs < 60) return `${secs} 秒`
  if (secs < 3600) return `${Math.round(secs / 60)} 分钟`
  if (secs < 86400) return `${(secs / 3600).toFixed(1)} 小时`
  return `${(secs / 86400).toFixed(1)} 天`
}

/** 账号档位使用官方 Badge 变体，避免业务层复制一套徽章色板。 */
export function tierBadgeVariant(tier: string): BadgeProps['variant'] {
  const t = tier.toLowerCase()
  if (t.includes('20x') || t.includes('5x') || t.includes('max')) return 'info'
  if (t.includes('pro')) return 'secondary'
  return 'outline'
}

/**
 * 凭证综合状态 → 状态灯颜色 + 左侧轨道色 + 文案。优先级：封禁 > 停用 > 冷却 > 额度将满 > 正常。
 *
 * **access_token 的有效期不参与判色**（既没有「已过期」也没有「即将过期」）：token 到点会在
 * 下次使用时自动刷新，而 `expires_in <= 300` 恰恰就是后端的刷新窗口——把「马上就要被刷新」
 * 画成琥珀色警告，是在提醒一件系统自己会处理、且用户也做不了什么的事。真正的凭证问题
 * （refresh_token 失效）走封禁那一支。有效期本身仍在底栏如实展示，见 [`credentialExpiryMeta`]。
 */
export function statusMeta(
  cred: Credential,
  nearLimit: boolean,
): { variant: BadgeProps['variant']; label: string } {
  if (cred.ban_reason) return { variant: 'error', label: '已封禁' }
  if (cred.disabled) return { variant: 'secondary', label: '已停用' }
  if (cred.rate_limited_secs > 0) return { variant: 'warning', label: '冷却中' }
  if (nearLimit) return { variant: 'warning', label: '额度将满' }
  return { variant: 'success', label: '运行正常' }
}

/**
 * 凭证自身的到期时间 → 元信息行的文案、配色与 title。
 *
 * 正常态给的是过期时刻而非「剩余 x 小时 y 分钟」：倒计时不自己走就是个假数字，
 * 而 token 到点会自动刷新，用户真正要判断的是「几点」，不是还剩多久。
 * 这里不混入停用、封禁、冷却等账号状态，避免底栏出现「凭证有效期：已停用」。
 *
 * 已过期的说成「待刷新」且保持中性色：刷新是惰性的（下次被调度时才刷），闲置久了必然到这个
 * 状态，它说明的是「这个号最近没被用过」，不是「这个号坏了」。
 */
export function credentialExpiryMeta(cred: Credential): {
  text: string
  className: string
  title?: string
} {
  if (cred.expired) {
    return {
      text: '待刷新',
      className: 'text-muted-foreground',
      title: `access_token 已于 ${formatFullTime(cred.expires_at)} 过期 · 下次被调度时自动刷新，不影响可用性`,
    }
  }
  return {
    text: `${formatClockTime(cred.expires_at)} 过期`,
    className: 'text-muted-foreground',
    title: `${formatFullTime(cred.expires_at)} 过期 · 到点自动刷新`,
  }
}

/** 列表紧凑态的综合说明：优先展示会影响账号调度的状态，再回退到真实有效期。 */
export function expiryMeta(cred: Credential): {
  text: string
  className: string
  title?: string
} {
  if (cred.ban_reason) {
    return {
      text: '已封禁',
      className: 'font-medium text-destructive-foreground',
      title: cred.ban_reason,
    }
  }
  if (cred.disabled) return { text: '已停用', className: 'text-muted-foreground' }
  if (cred.rate_limited_secs > 0) {
    const minutes = Math.max(1, Math.ceil(cred.rate_limited_secs / 60))
    return {
      text: `冷却约 ${minutes} 分钟`,
      className: 'font-medium text-warning-foreground',
      title: '账号级限流冷却中，结束后会自动恢复调度',
    }
  }
  return credentialExpiryMeta(cred)
}

/** 启用开关的 hover 提示：封禁态说明「已被上游封禁」并提示仍可手动停用。 */
export function switchTitle(cred: Credential): string {
  if (cred.disabled) return '已停用（点击启用）'
  if (cred.ban_reason) return `${cred.ban_reason} · 点击可手动停用`
  return '已启用（点击停用）'
}
