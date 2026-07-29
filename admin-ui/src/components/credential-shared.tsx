import { useMutation, useQueryClient } from '@tanstack/react-query'
import {
  ArrowPathIcon, TrashIcon, PencilIcon, ChevronUpIcon, ChevronDownIcon,
} from '@heroicons/react/24/outline'
import { toast } from 'sonner'
import {
  deleteCredential, refreshCredential, setDeviceLimit, setDisabled, setLabel, setPriority,
  type Credential,
} from '@/api/credentials'
import { extractError, formatClockTime, formatFullTime } from '@/lib/utils'
import {
  DropdownMenuContent, DropdownMenuItem, DropdownMenuSeparator,
} from '@/components/ui/dropdown-menu'
import { ConfirmDialog } from '@/components/ui/confirm-dialog'
import { Button } from '@/components/ui/button'

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

/** 账号是否异常：被上游封禁或凭证已过期。 */
export function isAbnormal(cred: Credential): boolean {
  return !!cred.ban_reason || cred.expired
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

/** 状态 → 严重度（越大越需要关注）；降序即「先看有问题的」。 */
function statusRank(c: Credential): number {
  if (c.ban_reason) return 4
  if (c.expired) return 3
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

  const rename = useMutation({
    mutationFn: (label: string) => setLabel(cred.id, label),
    onSuccess: () => { onRenamed?.(); invalidate() },
    onError: (e) => toast.error('重命名失败', { description: extractError(e) }),
  })
  const toggle = useMutation({
    mutationFn: (disabled: boolean) => setDisabled(cred.id, disabled),
    onSuccess: invalidate,
    onError: (e) => toast.error('操作失败', { description: extractError(e) }),
  })
  const prio = useMutation({
    mutationFn: (p: number) => setPriority(cred.id, p),
    onSuccess: invalidate,
    onError: (e) => toast.error('设置优先级失败', { description: extractError(e) }),
  })
  const limit = useMutation({
    mutationFn: (n: number) => setDeviceLimit(cred.id, n),
    onSuccess: () => { onLimitSaved?.(); invalidate() },
    onError: (e) => toast.error('设置设备上限失败', { description: extractError(e) }),
  })
  const refresh = useMutation({
    mutationFn: () => refreshCredential(cred.id),
    onSuccess: () => { toast.success('已刷新'); invalidate() },
    onError: (e) => toast.error('刷新失败', { description: extractError(e) }),
  })
  const remove = useMutation({
    mutationFn: () => deleteCredential(cred.id),
    onSuccess: () => { toast.success('已删除'); invalidate() },
    onError: (e) => toast.error('删除失败', { description: extractError(e) }),
  })

  return { rename, toggle, prio, limit, refresh, remove }
}

export type CredentialActions = ReturnType<typeof useCredentialActions>

/**
 * ⋯ 菜单内容（刷新 / 重命名 / 优先级步进 / 删除），卡片与列表共用。
 *
 * 删除只往外抛意图，确认框由调用方渲染在菜单之外——菜单一关，挂在它里面的弹窗会跟着
 * 卸载，确认框根本来不及显示。
 */
export function CredentialMenuContent({
  cred, actions, onRename, onRequestDelete,
}: {
  cred: Credential
  actions: CredentialActions
  onRename: () => void
  onRequestDelete: () => void
}) {
  const { refresh, prio } = actions
  return (
    <DropdownMenuContent align="end">
      <DropdownMenuItem onClick={() => refresh.mutate()} disabled={refresh.isPending}>
        <ArrowPathIcon className={refresh.isPending ? 'animate-spin' : undefined} />
        刷新 token
      </DropdownMenuItem>
      <DropdownMenuItem onClick={onRename}>
        <PencilIcon />
        重命名
      </DropdownMenuItem>
      <DropdownMenuSeparator />
      {/* 调度优先级：内联步进器，选中不关闭菜单，可连续调（数值小者优先）。 */}
      <div className="flex items-center justify-between gap-2 px-2 py-1.5 text-sm">
        <span className="text-muted-foreground">调度优先级</span>
        <div
          className="flex items-center overflow-hidden rounded-md border border-border bg-surface-2/40"
          title="数值小者优先被调度"
        >
          <Button
            type="button"
            size="icon"
            variant="ghost"
            className="size-6 rounded-none"
            onClick={(e) => { e.preventDefault(); prio.mutate(cred.priority - 1) }}
            disabled={prio.isPending}
            aria-label="提升优先级"
          >
            <ChevronUpIcon className="size-3.5" />
          </Button>
          <span className="w-7 border-x border-border bg-card text-center text-xs font-medium tnum leading-6">
            {cred.priority}
          </span>
          <Button
            type="button"
            size="icon"
            variant="ghost"
            className="size-6 rounded-none"
            onClick={(e) => { e.preventDefault(); prio.mutate(cred.priority + 1) }}
            disabled={prio.isPending}
            aria-label="降低优先级"
          >
            <ChevronDownIcon className="size-3.5" />
          </Button>
        </div>
      </div>
      <DropdownMenuSeparator />
      <DropdownMenuItem className="text-bad focus:bg-bad-soft" onClick={onRequestDelete}>
        <TrashIcon />
        删除
      </DropdownMenuItem>
    </DropdownMenuContent>
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
    <ConfirmDialog
      open={open}
      onOpenChange={onOpenChange}
      title="删除账号"
      confirmText="删除"
      pending={actions.remove.isPending}
      onConfirm={() => actions.remove.mutate()}
      description={
        <>
          确定删除「<span className="font-medium text-foreground">{cred.label}</span>」？
          该账号的历史用量记录与设备绑定将一并清除，不可恢复。
        </>
      }
    />
  )
}

/** 账号档位徽章配色：Max 20x/5x/Max/Pro/Free 用冷色系区分（避开到期徽章的绿/橙/红）。 */
export function tierBadgeClass(tier: string): string {
  const t = tier.toLowerCase()
  if (t.includes('20x'))
    return 'border-violet-200 bg-violet-100 text-violet-700 dark:border-violet-500/30 dark:bg-violet-500/15 dark:text-violet-300'
  if (t.includes('5x'))
    return 'border-indigo-200 bg-indigo-100 text-indigo-700 dark:border-indigo-500/30 dark:bg-indigo-500/15 dark:text-indigo-300'
  if (t.includes('max'))
    return 'border-blue-200 bg-blue-100 text-blue-700 dark:border-blue-500/30 dark:bg-blue-500/15 dark:text-blue-300'
  if (t.includes('pro'))
    return 'border-sky-200 bg-sky-100 text-sky-700 dark:border-sky-500/30 dark:bg-sky-500/15 dark:text-sky-300'
  if (t.includes('free'))
    return 'border-border bg-muted text-muted-foreground'
  return 'border-border bg-secondary text-secondary-foreground'
}

/** 凭证综合状态 → 状态灯颜色 + 左侧轨道色 + 文案。优先级：封禁 > 停用 > 过期 > 将满/将过期 > 正常。 */
export function statusMeta(
  cred: Credential,
  nearLimit: boolean,
): { dot: string; rail: string; label: string } {
  if (cred.ban_reason) return { dot: 'bg-bad', rail: 'before:bg-bad', label: '已封禁' }
  if (cred.disabled) return { dot: 'bg-muted-foreground/50', rail: 'before:bg-transparent', label: '已停用' }
  if (cred.expired) return { dot: 'bg-bad', rail: 'before:bg-bad', label: '已过期' }
  if (nearLimit) return { dot: 'bg-warn', rail: 'before:bg-warn', label: '额度将满' }
  if (cred.expires_in <= 300) return { dot: 'bg-warn', rail: 'before:bg-warn', label: '即将过期' }
  return { dot: 'bg-ok', rail: 'before:bg-transparent', label: '运行正常' }
}

/**
 * 凭证状态/有效期 → 元信息行的文案、配色与 title。异常态着色，正常态保持中性。
 *
 * 正常态给的是过期时刻而非「剩余 x 小时 y 分钟」：倒计时不自己走就是个假数字，
 * 而 token 到点会自动刷新，用户真正要判断的是「几点」，不是还剩多久。
 */
export function expiryMeta(cred: Credential): {
  text: string
  className: string
  title?: string
} {
  if (cred.ban_reason) return { text: '已封禁', className: 'font-medium text-bad', title: cred.ban_reason }
  if (cred.disabled) return { text: '已停用', className: 'text-muted-foreground' }
  if (cred.expired) return { text: '已过期', className: 'font-medium text-bad' }
  if (cred.expires_in <= 300) {
    return { text: '即将过期', className: 'font-medium text-warn', title: `${formatFullTime(cred.expires_at)} 过期` }
  }
  return {
    text: `${formatClockTime(cred.expires_at)} 过期`,
    className: 'text-muted-foreground',
    title: `${formatFullTime(cred.expires_at)} 过期 · 到点自动刷新`,
  }
}

/** 启用开关的 hover 提示：封禁态说明「已被上游封禁」并提示仍可手动停用。 */
export function switchTitle(cred: Credential): string {
  if (cred.disabled) return '已停用（点击启用）'
  if (cred.ban_reason) return `${cred.ban_reason} · 点击可手动停用`
  if (cred.expired) return '凭证已过期 · 点击可手动停用'
  return '已启用（点击停用）'
}
