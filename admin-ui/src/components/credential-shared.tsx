import { useEffect, useRef, useState, type ReactNode } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import axios from 'axios'
import {
  ActivityIcon, ChevronDownIcon, ChevronUpIcon, CircleCheckIcon, CircleXIcon,
  GaugeIcon, GlobeIcon, PencilIcon, RefreshCwIcon, ScrollTextIcon, SmartphoneIcon, TimerOffIcon,
  Trash2Icon,
} from 'lucide-react'
import {
  clearCooldown, deleteCredential, probeCredential, refreshCredential, setDeviceLimit,
  setDisabled, setLabel, setPriority, setProxy, setRpmLimit,
  type Credential, type ProbeQuota, type ProbeResult,
} from '@/api/credentials'
import {
  cn, displayCredentialLabel, extractError, formatClockTime, formatFullTime, localizeBackendMessage,
} from '@/lib/utils'
import { localize, useI18n, type Language } from '@/lib/i18n'
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
import {
  Combobox, ComboboxItem, ComboboxPopup, ComboboxTrigger, ComboboxValue,
} from '@/components/ui/combobox'
import { Empty, EmptyDescription, EmptyHeader, EmptyTitle } from '@/components/ui/empty'
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import { Form } from '@/components/ui/form'
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

export type QuotaFreshness = 'current' | 'unknown' | 'expired'
export type QuotaLevel = 'empty' | 'ok' | 'warning' | 'critical'
export type OverageState = 'none' | 'active' | 'historical' | 'unknown'

export interface QuotaWindowMeta {
  /** 窗口名（`5h`/`7d`/`7d_oi` …）。上游说了算，不做白名单。 */
  name: string
  /** 上游对该窗口的判决（`allowed`/`allowed_warning`/`rejected`/`rate_limited`）。 */
  status: string | null
  /** 仍属于当前窗口的使用率；明确已重置时为空。 */
  utilization: number | null
  /** 上游快照里的原值，仅用于判断超额标记对应哪个窗口。 */
  rawUtilization: number | null
  /** 与界面显示、阈值判定共用的整数百分比。 */
  percentage: number | null
  resetAt: number | null
  freshness: QuotaFreshness
  level: QuotaLevel
  /**
   * 上游**是否报告过这个窗口**（使用率与重置时刻有其一即算）。
   *
   * 与「暂无数据」是两回事，界面必须分开说：没有快照是「还没跑过请求，等等就有」，
   * 而有快照却缺这个窗口，意味着这个账号的额度模型里压根没有它（例如只有 5h 没有 7d），
   * 再等也不会出现。混成一句「暂无数据」会让人一直等一个永远不来的数。
   */
  reported: boolean
}

/**
 * `overage` 为 `unknown` 时，说明**为什么**确认不了——两种成因的处置办法完全不同：
 *
 * - `null`：有窗口已经重置了，它多半就是当时的成因，这只是一条过期快照，等下一条请求即可；
 * - `'legacy-snapshot'`：这条快照落在「全窗口记录」上线之前，只有 5h/7d 两个窗口，
 *   吃满的那个（多为超额池）压根没被存下来。同样等下一条请求，但要说清是数据缺口不是账号问题；
 * - `'no-full-window'`：全部窗口都记下来了，也都还在当前周期，却没有一个是满的——
 *   上游按超额放行但没报告任何已满窗口，等再多请求也不会变清楚，只能去看原始额度头。
 */
export type OverageUnresolved = null | 'legacy-snapshot' | 'no-full-window'

export interface QuotaRiskMeta {
  h5: QuotaWindowMeta
  d7: QuotaWindowMeta
  /**
   * 上游报告的全部窗口。快照带了 `windows` 就用它（含 `7d_oi` 这类没有专用列的），
   * 老快照没有就退回 `[h5, d7]`——与升级前的判定完全一致。
   */
  windows: QuotaWindowMeta[]
  /** `windows` 里 5h/7d 之外的那些：卡片已经用专用进度条画了那两个，这里只补剩下的。 */
  extraWindows: QuotaWindowMeta[]
  /** 是否已有额度快照（`cred.quota != null`）；空态文案靠它区分「还没数据」与「无此窗口」。 */
  hasSnapshot: boolean
  nearLimit: boolean
  overage: OverageState
  /** `overage === 'unknown'` 的成因；其余状态下恒为 `null`。见 [`OverageUnresolved`]。 */
  overageUnresolved: OverageUnresolved
}

/**
 * 该窗口是否属于**超额/回补池**而非账号基础额度。
 *
 * 口径必须与后端 `store`/`proxy` 的 `is_overage_window` 一致（`_oi` 结尾或含 `overage`）：
 * 那边据此判定「超额池满不等于账号额度耗尽」，前端的「额度将满」预警若把超额池算进去，
 * 就会给一个基础额度还很空的号挂上红色，而后端明明照常在调度它。
 */
function isOverageWindow(name: string): boolean {
  return name.endsWith('_oi') || name.includes('overage')
}

export type CredentialStatusKind =
  | 'banned'
  | 'rate-limited'
  | 'disabled'
  | 'overage'
  | 'overage-unknown'
  | 'cooldown'
  | 'near-limit'
  | 'normal'

export interface CredentialStatusMeta {
  kind: CredentialStatusKind
  variant: BadgeProps['variant']
  label: string
  detail: string
  attention: boolean
  rank: number
}

export interface CredentialEvaluation {
  credential: Credential
  quota: QuotaRiskMeta
  status: CredentialStatusMeta
  schedulable: boolean
  nearLimit: boolean
  quotaRisk: boolean
  needsAttention: boolean
  /**
   * 是否有**模型级**冷却在生效。与 `schedulable` 刻意分开：这一档只挡住那几个模型，
   * 账号整体照常参与调度，把它算进「不可调度」会把一个还在正常服务的号显示成停摆。
   */
  modelCooling: boolean
}

const currentUnixSeconds = () => Math.floor(Date.now() / 1000)

/**
 * 使用率统一截断成界面展示的整数百分比，颜色与告警只读这个值。
 * 这样 89.9% 会显示为 89% 且不告警，真正达到 90% 时才同时变红并进入额度风险。
 */
export function quotaPercentage(utilization: number | null): number | null {
  if (utilization == null || !Number.isFinite(utilization)) return null
  const clamped = Math.min(1, Math.max(0, utilization))
  return Math.floor(clamped * 100 + 1e-9)
}

export function quotaLevel(utilization: number | null): QuotaLevel {
  const percentage = quotaPercentage(utilization)
  if (percentage == null) return 'empty'
  if (percentage >= 90) return 'critical'
  if (percentage >= 70) return 'warning'
  return 'ok'
}

function evaluateQuotaWindow(
  name: string,
  utilization: number | null,
  resetAt: number | null,
  now: number,
  status: string | null = null,
): QuotaWindowMeta {
  const freshness: QuotaFreshness = resetAt == null
    ? 'unknown'
    : resetAt <= now
      ? 'expired'
      : 'current'
  const liveUtilization = freshness === 'expired' ? null : utilization
  return {
    name,
    status,
    utilization: liveUtilization,
    rawUtilization: utilization,
    percentage: quotaPercentage(liveUtilization),
    resetAt,
    freshness,
    level: quotaLevel(liveUtilization),
    reported: utilization != null || resetAt != null,
  }
}

/**
 * 将最新额度快照解释成当前可展示的窗口与风险。
 *
 * `overage_in_use` 也是快照，不是实时订阅：有满额窗口仍在当前周期才算「正在用 Usage credits」；
 * 所有已报告窗口都明确重置时降级成「近期用过」；其余不确定情况保守标为待确认。
 *
 * **术语**：Anthropic 官方管这个叫 **usage credits**（旧称 extra usage）——套餐包含的用量
 * 用完后不拦你，而是切成按标准 API 价的按量计费继续跑。上游头里的字段名才叫 `overage`，
 * 界面上不要写成「超额计费」，那是我们自己编的说法。官方帮助中心只有英文版，故术语保留英文。
 */
export function quotaRiskMeta(cred: Credential, now = currentUnixSeconds()): QuotaRiskMeta {
  const q = cred.quota
  const h5 = evaluateQuotaWindow('5h', q?.rl_5h_utilization ?? null, q?.rl_5h_reset ?? null, now)
  const d7 = evaluateQuotaWindow('7d', q?.rl_7d_utilization ?? null, q?.rl_7d_reset ?? null, now)
  // 后端落了全窗口就以它为准（含 7d_oi 这类没有专用列的）；老快照的 windows 是空数组，
  // 退回 [h5, d7]——与升级前逐字一致，不会因为升级把老数据的判定改掉。
  const hasWindowList = (q?.windows?.length ?? 0) > 0
  const windows: QuotaWindowMeta[] = hasWindowList
    ? q!.windows.map((w) =>
        evaluateQuotaWindow(w.name, w.utilization ?? null, w.reset ?? null, now, w.status ?? null))
    : [h5, d7]
  const extraWindows = windows.filter((w) => w.name !== '5h' && w.name !== '7d')

  // 「额度将满」只看基础窗口：超额池满了不代表账号额度耗尽（后端同一口径，见 isOverageWindow），
  // 把它算进来会给一个基础额度还很空、后端照常在调度的号挂上红色预警。
  const nearLimit = windows.some(
    (window) => !isOverageWindow(window.name) && (window.percentage ?? -1) >= 90,
  )

  let overage: OverageState = 'none'
  let overageUnresolved: OverageUnresolved = null
  if (q?.overage_in_use === true) {
    const reportedWindows = windows.filter((window) => window.reported)
    // 判满不要求 freshness 必须是 `current`，只排除**明确已重置**的：上游给了 7d_oi=1.02
    // 却没给它的 reset（实测形态，重置时刻走的是 retry-after），要求 current 会让这条
    // 证据被丢掉，于是又落回「待确认」——而那正是记录全窗口要解决的问题。
    const fullWindow = reportedWindows.some(
      (window) => (window.rawUtilization ?? -1) >= 1 && window.freshness !== 'expired',
    )
    // 只有所有已报告窗口都明确过期，前端才能确认这是一条历史快照；若仍有窗口未重置、
    // 但没有可对应上的满额窗口，则保守标成「待确认」，不把快照误报成仍在实时计费。
    overage = fullWindow
      ? 'active'
      : reportedWindows.length > 0
        && reportedWindows.every((window) => window.freshness === 'expired')
        ? 'historical'
        : 'unknown'
    if (overage === 'unknown') {
      // 有窗口已经重置了 → 它多半就是当时的成因，这只是条过期快照，等下条请求即可（null）。
      // 全都还在当前周期 → 分两种：老快照压根没存全窗口，还是存全了也确实没有满的。
      const allCurrent = reportedWindows.length > 0
        && reportedWindows.every((window) => window.freshness === 'current')
      overageUnresolved = !allCurrent
        ? null
        : hasWindowList ? 'no-full-window' : 'legacy-snapshot'
    }
  }

  return {
    h5, d7, windows, extraWindows,
    hasSnapshot: q != null,
    nearLimit, overage, overageUnresolved,
  }
}

function quotaWarningDetail(quota: QuotaRiskMeta, language: Language): string {
  const windows = [
    quota.h5.percentage != null && quota.h5.percentage >= 90
      ? localize(language, `5 小时 ${quota.h5.percentage}%`, `5-hour ${quota.h5.percentage}%`)
      : '',
    quota.d7.percentage != null && quota.d7.percentage >= 90
      ? localize(language, `7 天 ${quota.d7.percentage}%`, `7-day ${quota.d7.percentage}%`)
      : '',
  ].filter(Boolean)
  return localize(
    language,
    `${windows.join('、')}，已达到额度预警线`,
    `${windows.join(', ')} reached the quota warning threshold`,
  )
}

function statusFromQuota(
  cred: Credential,
  quota: QuotaRiskMeta,
  language: Language,
): CredentialStatusMeta {
  // 限流暂停必须排在封禁之前判：两者都是 disabled + ban_reason，只有 resume_at 能区分。
  // 漏了这一档的话，一个只是额度用完、几小时后自己就回来的号会被显示成「已封禁」，
  // 而封禁在这套界面里的含义是「需要人工介入，这个号可能废了」——两回事。
  if (cred.resume_at != null) {
    return {
      kind: 'rate-limited', variant: 'warning',
      label: localize(language, '限流暂停', 'Rate limited'),
      detail: localize(
        language,
        `账号额度已用尽，已移出调度池，${formatFullTime(cred.resume_at, language)} 自动恢复；也可手动启用或做一次连通性测试立即恢复`,
        `Quota exhausted; removed from the scheduling pool and resuming automatically at ${formatFullTime(cred.resume_at, language)}. You can also enable it manually or run a connectivity test to restore it now`,
      ),
      attention: true, rank: 6,
    }
  }
  if (cred.ban_reason) {
    return {
      kind: 'banned', variant: 'error',
      label: localize(language, '已封禁', 'Banned'),
      detail: localizeBackendMessage(cred.ban_reason, language),
      attention: true, rank: 7,
    }
  }
  if (cred.disabled) {
    return {
      kind: 'disabled', variant: 'secondary',
      label: localize(language, '已停用', 'Disabled'),
      detail: localize(language, '账号已停用，不参与调度', 'This account is disabled and excluded from scheduling'),
      attention: false, rank: 1,
    }
  }
  const snapshotTime = cred.quota
    ? formatFullTime(cred.quota.ts, language)
    : localize(language, '未知时间', 'unknown time')
  if (quota.overage === 'active') {
    return {
      kind: 'overage', variant: 'error',
      label: localize(language, 'Usage credits 生效中', 'Usage credits active'),
      detail: localize(
        language,
        `额度快照（${snapshotTime}）显示该账号已用完套餐包含的用量，正由 Usage credits 按标准 API 价继续放行请求`,
        `The quota snapshot (${snapshotTime}) shows this account has used up its plan's included usage and is being served by usage credits at standard API rates`,
      ),
      attention: true, rank: 5,
    }
  }
  if (quota.overage === 'unknown') {
    return {
      kind: 'overage-unknown', variant: 'warning',
      label: localize(language, 'Usage credits 待确认', 'Usage credits unconfirmed'),
      // 三种成因的处置办法完全不同，不能共用一句「需等待新请求确认」——见 OverageUnresolved。
      detail: quota.overageUnresolved === 'no-full-window'
        ? localize(
            language,
            `额度快照（${snapshotTime}）显示上游动用了 Usage credits，但它报告的窗口没有一个是满的。等新请求也不会更清楚，做一次连通性测试看上游此刻的原始额度头`,
            `The quota snapshot (${snapshotTime}) shows the upstream drawing on usage credits, yet none of the windows it reported is full. Waiting for new requests will not clarify this — run a connectivity test to see the upstream's current raw quota headers`,
          )
        : quota.overageUnresolved === 'legacy-snapshot'
          ? localize(
              language,
              `额度快照（${snapshotTime}）早于「记录全部额度窗口」这次升级，只存了 5h / 7d 两个窗口，吃满的那个（多为超额池）没被存下来。下一条带额度头的请求会自动补齐`,
              `The quota snapshot (${snapshotTime}) predates the full-window recording upgrade and only stored the 5h / 7d windows, so the exhausted one (typically the overage pool) was not kept. The next request carrying quota headers will fill it in`,
            )
          : localize(
              language,
              `额度快照（${snapshotTime}）记录了 Usage credits，但现有窗口信息不足以确认当前是否仍在计费，需等待新请求确认`,
              `The quota snapshot (${snapshotTime}) recorded usage credits, but the available window data cannot confirm whether they are still in use; wait for a new request to verify`,
            ),
      attention: true, rank: 4,
    }
  }
  if (cred.rate_limited_secs > 0) {
    const minutes = Math.max(1, Math.ceil(cred.rate_limited_secs / 60))
    return {
      kind: 'cooldown', variant: 'warning',
      label: localize(language, '冷却中', 'Cooling down'),
      detail: localize(
        language,
        `账号约 ${minutes} 分钟后恢复调度`,
        `Scheduling resumes in about ${minutes} ${minutes === 1 ? 'minute' : 'minutes'}`,
      ),
      attention: true, rank: 3,
    }
  }
  if (quota.nearLimit) {
    return {
      kind: 'near-limit', variant: 'warning',
      label: localize(language, '额度将满', 'Quota nearly full'),
      detail: quotaWarningDetail(quota, language), attention: true, rank: 2,
    }
  }
  return {
    kind: 'normal', variant: 'success',
    label: localize(language, '运行正常', 'Healthy'),
    detail: localize(language, '账号运行正常，可参与调度', 'This account is healthy and available for scheduling'),
    attention: false, rank: 0,
  }
}

/** 卡片、列表、概览、筛选和排序共同消费的一份账号状态解释。 */
export function evaluateCredential(
  cred: Credential,
  now = currentUnixSeconds(),
  language: Language = 'zh-CN',
): CredentialEvaluation {
  const quota = quotaRiskMeta(cred, now)
  const status = statusFromQuota(cred, quota, language)
  const nearLimit = !cred.disabled && quota.nearLimit
  const quotaRisk = !cred.disabled && (
    nearLimit || quota.overage === 'active' || quota.overage === 'unknown'
  )
  // 只看账号级冷却：模型级那档挡的是「这个号的某几个模型」，账号本身照常在调度池里。
  const schedulable = !cred.disabled && !cred.ban_reason && cred.rate_limited_secs <= 0
  const modelCooling = (cred.rate_limited_models?.length ?? 0) > 0
  return {
    credential: cred,
    quota,
    status,
    schedulable,
    nearLimit,
    quotaRisk,
    needsAttention: status.attention,
    modelCooling,
  }
}

/** 把模型级冷却写成一句人话，如「fable-5 还有 5 分钟」。剩余不足一分钟时按秒说。 */
export function modelCooldownSummary(cred: Credential, language: Language): string {
  return (cred.rate_limited_models ?? [])
    .map(({ model, secs }) =>
      secs >= 60
        ? localize(
            language,
            `${model} 还有 ${Math.ceil(secs / 60)} 分钟`,
            `${model} in ${Math.ceil(secs / 60)} min`,
          )
        : localize(language, `${model} 还有 ${secs} 秒`, `${model} in ${secs}s`),
    )
    .join(localize(language, '、', ', '))
}

/** 上游整体额度判决的稳定枚举；未知新值原样保留，避免误译。 */
export function unifiedQuotaStatusLabel(status: string, language: Language): string {
  switch (status) {
    case 'allowed':
      return localize(language, '已放行', 'Allowed')
    case 'allowed_warning':
      return localize(language, '已放行（预警）', 'Allowed with warning')
    case 'rejected':
      return localize(language, '已拒绝', 'Rejected')
    case 'rate_limited':
      return localize(language, '已限流', 'Rate limited')
    default:
      return status
  }
}

/** 兼容卡片与紧凑列表：返回仍属于当前窗口的原始使用率。 */
export function liveQuota(
  cred: Credential,
  now = currentUnixSeconds(),
): { u5h: number | null; u7d: number | null } {
  const quota = quotaRiskMeta(cred, now)
  return { u5h: quota.h5.utilization, u7d: quota.d7.utilization }
}

/** 账号是否处于「额度将满」（停用的不算；已重置的窗口不算）。 */
export function isNearLimit(cred: Credential, now = currentUnixSeconds()): boolean {
  return evaluateCredential(cred, now).nearLimit
}

/**
 * 账号是否异常——**只看被上游封禁**。
 *
 * `expired` 不算：那只是 access_token 到点了，而刷新是惰性的（选号之后、发请求之前必刷，
 * 见后端 `ensure_fresh_token`），所以闲置一夜的健康账号第二天必然是 `expired=true`，
 * 下一个请求会自动把它刷好。把它算成异常，等于每天早上给一批好号刷上红色、排到最前、
 * 塞进「需处理」——而这里真正要回答的是「refresh_token 还灵不灵」，那个答案在 `ban_reason`：
 * 刷新失败且判定为永久失效时，后端会 `mark_banned` 写进去。
 *
 * 限流暂停（`resume_at != null`）同样不算：那时 `ban_reason` 里写的是「额度用尽，几点恢复」，
 * token 好好的、号也好好的，到点自己就回调度池了，不需要任何人处理。
 */
export function isAbnormal(cred: Credential): boolean {
  return !!cred.ban_reason && cred.resume_at == null
}

// ---------- 排序 ----------
//
// 排序模型放这里，列表表头与工具栏下拉共用同一份定义，避免两处各写一套导致
// 「表头能排的维度和下拉里的对不上」。

export type SortKey =
  | 'priority' | 'status' | 'name' | 'tier'
  | 'usage5h' | 'usage7d' | 'devices' | 'cost' | 'recent' | 'created' | 'rpm'

export type SortDir = 'asc' | 'desc'

/** 全部可排序维度（下拉菜单按此顺序渲染；表头列是其中的子集）。 */
export const SORTS: { key: SortKey; label: string }[] = [
  { key: 'priority', label: '优先级' },
  { key: 'status', label: '状态' },
  { key: 'name', label: '名称' },
  { key: 'tier', label: '套餐' },
  { key: 'usage5h', label: '5h 使用率' },
  { key: 'usage7d', label: '7d 使用率' },
  { key: 'devices', label: '设备数' },
  { key: 'rpm', label: '当前 RPM' },
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
  usage7d: 'desc',
  devices: 'desc',
  rpm: 'desc',
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

/** 单维度排序值；额度与状态使用同一时钟快照，避免跨过 reset 后卡片和排序口径分叉。 */
function sortValue(key: SortKey, credential: Credential, now: number): number | string {
  switch (key) {
    case 'status':
      return evaluateCredential(credential, now).status.rank
    case 'name':
      return credential.label
    case 'tier':
      return tierRank(credential.tier)
    case 'usage5h':
      return quotaRiskMeta(credential, now).h5.percentage ?? -1
    case 'usage7d':
      return quotaRiskMeta(credential, now).d7.percentage ?? -1
    case 'devices':
      return credential.device_count
    case 'rpm':
      return credential.rpm ?? 0
    case 'cost':
      return credential.cost_total ?? 0
    case 'recent':
      return credential.last_used ?? 0
    case 'created':
      return credential.created_at
    case 'priority':
    default:
      return credential.priority
  }
}

/**
 * 按维度 + 方向排序（不改原数组）。
 *
 * 同值时一律按 id 升序兜底，保证顺序稳定——否则相同优先级的账号会在每次
 * 重新渲染时互相换位。
 */
export function sortCreds(
  list: Credential[],
  key: SortKey,
  dir: SortDir,
  now = currentUnixSeconds(),
  language: Language = 'zh-CN',
): Credential[] {
  const sign = dir === 'asc' ? 1 : -1
  const values = new Map(list.map((credential) => [credential.id, sortValue(key, credential, now)]))
  return [...list].sort((a, b) => {
    const aValue = values.get(a.id)!
    const bValue = values.get(b.id)!
    const compared = typeof aValue === 'string' && typeof bValue === 'string'
      ? aValue.localeCompare(bValue, language)
      : Number(aValue) - Number(bValue)
    return sign * compared || a.id - b.id
  })
}

/**
 * 卡片视图与列表视图共用的写操作。各视图自行管理编辑态（重命名、设备上限输入框），
 * 这里只封装请求与失败提示，避免两处重复维护同一套 mutation。
 */
export function useCredentialActions(cred: Credential, onRenamed?: () => void, onLimitSaved?: () => void) {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  const invalidate = () => qc.invalidateQueries({ queryKey: ['credentials'] })
  const failure = (title: string, error: unknown) => toastManager.add({
    title,
    description: extractError(error, language),
    type: 'error',
  })

  const rename = useMutation({
    mutationFn: (label: string) => setLabel(cred.id, label),
    onSuccess: () => { onRenamed?.(); invalidate() },
    onError: (e) => failure(t('重命名失败', 'Rename failed'), e),
  })
  const toggle = useMutation({
    mutationFn: (disabled: boolean) => setDisabled(cred.id, disabled),
    onMutate: async (disabled) => {
      await qc.cancelQueries({ queryKey: ['credentials'] })
      const previous = qc.getQueryData<Credential[]>(['credentials'])
      qc.setQueryData<Credential[]>(['credentials'], (current) => current?.map((item) => (
        item.id === cred.id ? { ...item, disabled } : item
      )))
      return { previous }
    },
    onError: (e, _disabled, context) => {
      if (context?.previous) qc.setQueryData(['credentials'], context.previous)
      failure(t('操作失败', 'Operation failed'), e)
    },
    onSettled: () => invalidate(),
  })
  const prio = useMutation({
    mutationFn: (p: number) => setPriority(cred.id, p),
    onSuccess: invalidate,
    onError: (e) => failure(t('设置优先级失败', 'Failed to set priority'), e),
  })
  const limit = useMutation({
    mutationFn: (n: number) => setDeviceLimit(cred.id, n),
    onSuccess: () => { onLimitSaved?.(); invalidate() },
    onError: (e) => failure(t('设置设备上限失败', 'Failed to set device limit'), e),
  })
  // RPM 上限与设备上限分开两个 mutation：两者的失败提示不一样，共用一个的话，
  // 改 RPM 失败会弹出「设置设备上限失败」。
  const rpmLimit = useMutation({
    mutationFn: (n: number) => setRpmLimit(cred.id, n),
    onSuccess: () => {
      toastManager.add({ title: t('已保存 RPM 上限', 'RPM limit saved'), type: 'success' })
      invalidate()
    },
    onError: (e) => failure(t('设置 RPM 上限失败', 'Failed to set the RPM limit'), e),
  })
  const proxy = useMutation({
    mutationFn: (url: string | null) => setProxy(cred.id, url),
    onSuccess: () => {
      toastManager.add({ title: t('已保存出站代理', 'Outbound proxy saved'), type: 'success' })
      invalidate()
    },
    onError: (e) => failure(t('设置出站代理失败', 'Failed to set the outbound proxy'), e),
  })
  const refresh = useMutation({
    mutationFn: () => refreshCredential(cred.id),
    onSuccess: () => { toastManager.add({ title: t('已刷新', 'Refreshed'), type: 'success' }); invalidate() },
    onError: (e) => failure(t('刷新失败', 'Refresh failed'), e),
  })
  const remove = useMutation({
    mutationFn: () => deleteCredential(cred.id),
    onSuccess: () => { toastManager.add({ title: t('已删除', 'Deleted'), type: 'success' }); invalidate() },
    onError: (e) => failure(t('删除失败', 'Delete failed'), e),
  })
  const cooldown = useMutation({
    mutationFn: () => clearCooldown(cred.id),
    onSuccess: () => { toastManager.add({ title: t('已解除冷却', 'Cooldown cleared'), type: 'success' }); invalidate() },
    onError: (e) => failure(t('解除冷却失败', 'Failed to clear cooldown'), e),
  })

  return { rename, toggle, prio, limit, rpmLimit, proxy, refresh, remove, cooldown }
}

export type CredentialActions = ReturnType<typeof useCredentialActions>

/**
 * ⋯ 菜单内容（刷新 / 重命名 / 优先级调整 / 删除），卡片与列表共用。
 *
 * 删除只往外抛意图，确认框由调用方渲染在菜单之外——菜单一关，挂在它里面的弹窗会跟着
 * 卸载，确认框根本来不及显示。
 */
export function CredentialMenuContent({
  cred, actions, onRename, onDeviceLimit, onRpmLimit, onProxy, onUsage, onTest, onRequestDelete,
}: {
  cred: Credential
  actions: CredentialActions
  onRename: () => void
  onDeviceLimit: () => void
  onRpmLimit: () => void
  onProxy: () => void
  onUsage: () => void
  onTest: () => void
  onRequestDelete: () => void
}) {
  const { t } = useI18n()
  const { refresh, prio, cooldown } = actions
  return (
    <MenuPopup align="end">
      <MenuItem onClick={() => refresh.mutate()} disabled={refresh.isPending}>
        <RefreshCwIcon className={refresh.isPending ? 'animate-spin' : undefined} />
        {t('刷新 token', 'Refresh token')}
      </MenuItem>
      <MenuItem onClick={onTest}>
        <ActivityIcon />
        {t('连通性测试', 'Connectivity test')}
      </MenuItem>
      {/* 账号级或模型级任一在冷却都该给出口——后端那个接口本来就是两档一起清的。 */}
      {(cred.rate_limited_secs > 0 || (cred.rate_limited_models?.length ?? 0) > 0) && (
        <MenuItem onClick={() => cooldown.mutate()} disabled={cooldown.isPending}>
          <TimerOffIcon />
          {t('解除冷却', 'Clear cooldown')}
        </MenuItem>
      )}
      <MenuItem onClick={onRename}>
        <PencilIcon />
        {t('重命名', 'Rename')}
      </MenuItem>
      <MenuItem onClick={onDeviceLimit}>
        <SmartphoneIcon />
        {t('设备上限', 'Device limit')}
      </MenuItem>
      <MenuItem onClick={onRpmLimit}>
        <GaugeIcon />
        {t('RPM 上限', 'RPM limit')}
      </MenuItem>
      <MenuItem onClick={onProxy}>
        <GlobeIcon />
        {t('出站代理', 'Outbound proxy')}
      </MenuItem>
      <MenuItem onClick={onUsage}>
        <ScrollTextIcon />
        {t('请求明细', 'Request log')}
      </MenuItem>
      <MenuSeparator />
      <MenuItem
        onClick={() => prio.mutate(cred.priority - 1)}
        disabled={prio.isPending}
        title={t('数值越小，调度优先级越高', 'Lower values are scheduled first')}
      >
        <ChevronUpIcon />
        {t('提高优先级', 'Increase priority')}
        <MenuShortcut>P{cred.priority - 1}</MenuShortcut>
      </MenuItem>
      <MenuItem
        onClick={() => prio.mutate(cred.priority + 1)}
        disabled={prio.isPending}
        title={t('数值越大，调度优先级越低', 'Higher values are scheduled later')}
      >
        <ChevronDownIcon />
        {t('降低优先级', 'Decrease priority')}
        <MenuShortcut>P{cred.priority + 1}</MenuShortcut>
      </MenuItem>
      <MenuSeparator />
      <MenuItem variant="destructive" onClick={onRequestDelete}>
        <Trash2Icon />
        {t('删除', 'Delete')}
      </MenuItem>
    </MenuPopup>
  )
}

/**
 * 首次打开之后才真正挂载里面的内容，之后一直保留（保住关闭动画和已填状态）。
 *
 * 每个账号后面都跟着一串对话框，一页 50 个账号就是几百棵永远关着的组件树；
 * 它们的查询虽然已经用 `enabled: open` 挡住了，组件本身的挂载与更新仍然照跑。
 */
export function DeferredMount({ open, children }: { open: boolean; children: ReactNode }) {
  const [mounted, setMounted] = useState(open)
  // 渲染期直接改自身 state 是 React 认可的「派生状态」写法，会在同一帧内重跑，
  // 不像放进 effect 那样晚一帧——晚一帧会让对话框错过入场动画。
  if (open && !mounted) setMounted(true)
  return mounted ? <>{children}</> : null
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
  const { t, language } = useI18n()
  const credentialLabel = displayCredentialLabel(cred.label, language)
  return (
    <AlertDialog open={open} onOpenChange={onOpenChange}>
      <AlertDialogPopup>
        <AlertDialogHeader>
          <AlertDialogTitle>{t('删除账号', 'Delete account')}</AlertDialogTitle>
          <AlertDialogDescription>
            {t('删除', 'Deleting ')}
            {t('「', '"')}<span className="font-medium text-foreground [overflow-wrap:anywhere]">{credentialLabel}</span>{t('」后，', '" will ')}
            {t('历史用量与设备绑定将一并清除，且无法恢复。', 'permanently remove its usage history and device bindings. This cannot be undone.')}
          </AlertDialogDescription>
        </AlertDialogHeader>
        <AlertDialogFooter>
          <AlertDialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</AlertDialogClose>
          <Button
            variant="destructive"
            loading={actions.remove.isPending}
            onClick={() => actions.remove.mutate()}
          >
            {t('删除', 'Delete')}
          </Button>
        </AlertDialogFooter>
      </AlertDialogPopup>
    </AlertDialog>
  )
}

/**
 * 测试弹窗里的模型选项。取现役四个模型族各一个（基座与 beta 串按族分家，见后端
 * `cc_system_base`/`cc_beta_seed`），这样逐项测试可以覆盖四条不同的模拟路径。
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
  const { t, language } = useI18n()
  const credentialLabel = displayCredentialLabel(cred.label, language)
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
      toastManager.add({
        title: t('测试失败', 'Test failed'),
        description: extractError(e, language),
        type: 'error',
      })
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
          <DialogTitle>{t('连通性测试', 'Connectivity test')}</DialogTitle>
          <DialogDescription>
            {t('使用「', 'Send a minimal request through "')}
            <span className="font-medium text-foreground [overflow-wrap:anywhere]">{credentialLabel}</span>
            {t('」发送一条最小请求，验证所选模型是否可用。', '" to verify that the selected model is available.')}
          </DialogDescription>
        </DialogHeader>
        <DialogPanel className="space-y-4">
          <Form
            className="space-y-4"
            onSubmit={(e) => { e.preventDefault(); submit() }}
          >
            <Field>
              <FieldLabel htmlFor={`probe-model-${cred.id}`}>{t('测试模型', 'Model to test')}</FieldLabel>
              <div className="flex w-full flex-col gap-2 sm:flex-row sm:items-center">
                <Combobox
                  items={PROBE_MODELS}
                  value={model}
                  onValueChange={(value) => value && setModel(value)}
                  disabled={probe.isPending}
                >
                  <ComboboxTrigger id={`probe-model-${cred.id}`} className="min-w-0 flex-1">
                    <ComboboxValue placeholder={t('选择模型', 'Select a model')} />
                  </ComboboxTrigger>
                  <ComboboxPopup
                    aria-label={t('选择测试模型', 'Select a model to test')}
                    inputPlaceholder={t('搜索模型', 'Search models')}
                    emptyText={t('没有匹配的模型', 'No matching models')}
                  >
                    {(item: string) => (
                      <ComboboxItem key={item} value={item}>{item}</ComboboxItem>
                    )}
                  </ComboboxPopup>
                </Combobox>
                <Button
                  type={probe.isPending ? 'button' : 'submit'}
                  variant={probe.isPending ? 'outline' : 'default'}
                  className="w-full sm:w-auto sm:shrink-0"
                  onClick={probe.isPending ? cancelProbe : undefined}
                >
                  {probe.isPending ? <Spinner /> : <ActivityIcon />}
                  {probe.isPending ? t('取消测试', 'Cancel test') : t('开始测试', 'Start test')}
                </Button>
              </div>
              <FieldDescription>
                {t(
                  '每次测试会消耗少量订阅额度，并计入该账号当前周期的请求数与花费。',
                  'Each test uses a small amount of subscription quota and counts toward this account’s current-period requests and cost.',
                )}
              </FieldDescription>
            </Field>
          </Form>

          {entries.length === 0 ? (
            <Empty className="py-8">
              <EmptyHeader>
                <EmptyTitle className="text-base">{t('尚无测试结果', 'No test results yet')}</EmptyTitle>
                <EmptyDescription>
                  {t(
                    '选择模型并开始测试，结果会显示实时额度或上游错误。',
                    'Select a model and start a test to see live quota data or upstream errors.',
                  )}
                </EmptyDescription>
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
  const { t, language } = useI18n()
  const { model, result } = entry
  const Icon = result.ok ? CircleCheckIcon : CircleXIcon
  return (
    <li>
      <Alert variant={result.ok ? 'success' : 'error'}>
        <Icon aria-hidden />
        <AlertTitle className="flex min-w-0 flex-wrap items-center gap-2">
          <span className="min-w-0 [overflow-wrap:anywhere]" title={model}>{model}</span>
          <Badge variant={result.ok ? 'success' : 'error'} size="sm">
            {result.status > 0 ? `HTTP ${result.status}` : t('未送达上游', 'Not sent upstream')}
          </Badge>
          <span className="font-normal text-muted-foreground">{formatLatency(result.latency_ms)}</span>
          {result.model && result.model !== model && (
            <span
              className="min-w-0 font-normal text-muted-foreground [overflow-wrap:anywhere]"
              title={t(`上游实际使用的模型：${result.model}`, `Model actually used upstream: ${result.model}`)}
            >
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
              {localizeBackendMessage(result.error, language)}
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
 * 这是**本次测试响应**里上游直接返回的说法；测试完成后也会写用量日志并刷新账号列表，
 * 但这里保留逐次结果，方便对照不同模型的状态与等待时间。
 */
function ProbeQuotaLine({ quota }: { quota: ProbeQuota }) {
  const { t, language } = useI18n()
  const win = (label: string, util: number | null, reset: number | null) => {
    if (util == null && reset == null) return null
    const pct = quotaPercentage(util)
    return (
      <span
        key={label}
        className="tnum"
        title={reset != null
          ? t(
              `${label} 窗口 ${formatFullTime(reset, language)} 重置`,
              `${label} window resets at ${formatFullTime(reset, language)}`,
            )
          : undefined}
      >
        {label}{' '}
        {pct == null ? (
          '—'
        ) : (
          <span className={cn('font-medium', quotaToneClass(util))}>{pct}%</span>
        )}
        {reset != null && t(
          ` · ${formatClockTime(reset, language)} 重置`,
          ` · resets ${formatClockTime(reset, language)}`,
        )}
      </span>
    )
  }
  return (
    <div className="mt-1 flex flex-wrap items-center gap-x-2.5 gap-y-1 text-xs text-muted-foreground">
      {win('5h', quota.rl_5h_utilization, quota.rl_5h_reset)}
      {win('7d', quota.rl_7d_utilization, quota.rl_7d_reset)}
      {/* 429 才有。它是上游对**这次**拒绝给出的等待时间，比窗口 reset 更直接。 */}
      {quota.retry_after_secs != null && (
        <span
          className="text-destructive-foreground"
          title={t(
            `上游 retry-after: ${quota.retry_after_secs} 秒`,
            `Upstream retry-after: ${quota.retry_after_secs} ${quota.retry_after_secs === 1 ? 'second' : 'seconds'}`,
          )}
        >
          {t('需等待', 'Wait')} {formatWait(quota.retry_after_secs, language)}
        </span>
      )}
      {/* 套餐额度已满但上游动用 Usage credits 放行：不 429、请求照常成功，只有这里能看出在花钱。 */}
      {quota.overage_in_use && (
        <span
          className="text-destructive-foreground"
          title={t(
            '本次请求由 Usage credits（上游头里的 overage）放行：套餐包含的用量已用完，正按标准 API 价计费',
            'This request was served by usage credits (`overage` in the upstream headers): the plan\'s included usage is exhausted and standard API rates now apply',
          )}
        >
          {t('Usage credits 放行', 'Served by usage credits')}
        </span>
      )}
      {/* allowed 是常态，不占地方；warning/rejected 才值得说一句。 */}
      {quota.unified_status && quota.unified_status !== 'allowed' && (
        <span
          className={cn(
            quota.unified_status === 'rejected' || quota.unified_status === 'rate_limited'
              ? 'text-destructive-foreground'
              : 'text-warning-foreground',
          )}
          title={
            quota.rl_representative
              ? t(
                  `上游整体额度状态（当前由 ${quota.rl_representative} 窗口决定）`,
                  `Overall upstream quota status (currently determined by the ${quota.rl_representative} window)`,
                )
              : t('上游整体额度状态', 'Overall upstream quota status')
          }
        >
          {unifiedQuotaStatusLabel(quota.unified_status, language)}
        </span>
      )}
    </div>
  )
}

/** 使用率配色，阈值与卡片额度条一致（≥90% 红、≥70% 橙）。 */
function quotaToneClass(util: number | null): string {
  const level = quotaLevel(util)
  if (level === 'critical') return 'text-destructive-foreground'
  if (level === 'warning') return 'text-warning-foreground'
  return 'text-foreground/80'
}

/** 等待时长：分钟以内给秒，一天以内给小时，再长给天（上游真给过 63 小时）。 */
function formatWait(secs: number, language: Language): string {
  if (secs < 60) {
    return localize(language, `${secs} 秒`, `${secs} ${secs === 1 ? 'second' : 'seconds'}`)
  }
  if (secs < 3600) {
    const minutes = Math.round(secs / 60)
    return localize(language, `${minutes} 分钟`, `${minutes} ${minutes === 1 ? 'minute' : 'minutes'}`)
  }
  if (secs < 86400) {
    const hours = (secs / 3600).toFixed(1)
    return localize(language, `${hours} 小时`, `${hours} hours`)
  }
  const days = (secs / 86400).toFixed(1)
  return localize(language, `${days} 天`, `${days} days`)
}

/** 账号档位使用官方 Badge 变体，避免业务层复制一套徽章色板。 */
/** 个人账号的组织类型。这三个之外的一律当组织号，见 [isOrgAccount]。 */
const PERSONAL_ORG_TYPES = ['claude_max', 'claude_pro', 'claude_free']

/**
 * 是不是组织号（团队/企业/其它非个人形态）。判据是 profile 的
 * `organization.organization_type`——团队号的 `account.has_claude_max`/`has_claude_pro`
 * 实测都是 false，光看等级分不出来（团队号的等级显示成 `Max 5x`，与个人 Max 号一模一样）。
 *
 * **用个人类型做白名单，而不是列举团队类型**：反过来写的话，将来冒出一个没见过的组织类型
 * （`claude_edu` 之类）会被静默当成个人号，标记就没了。宁可给未知类型打个标（最多是多显示
 * 一个徽章），也不要漏标。
 *
 * 旧库里在这一列存在之前加的号 `org_type` 为 null——**不猜**，不打标；刷新一次凭证即可补上。
 */
export function isOrgAccount(cred: Pick<Credential, 'org_type'>): boolean {
  const t = cred.org_type?.trim().toLowerCase()
  return !!t && !PERSONAL_ORG_TYPES.includes(t)
}

/**
 * 组织号的标记文案：组织类型原值剥掉 `claude_` 前缀后首字母大写（`claude_team` → `Team`）。
 *
 * **不做中英翻译**：等级徽章本来就是 `Max 5x`/`Pro` 这类原文，组织标记跟着一致；而且这样
 * 没见过的新类型（`claude_edu` → `Edu`）天然就显示得出来，不必逐个补词条。
 */
export function orgBadgeLabel(cred: Pick<Credential, 'org_type'>): string {
  const bare = (cred.org_type?.trim().toLowerCase() ?? '').replace(/^claude_/, '')
  return bare.charAt(0).toUpperCase() + bare.slice(1)
}

export function tierBadgeVariant(tier: string): BadgeProps['variant'] {
  const t = tier.toLowerCase()
  if (t.includes('20x') || t.includes('5x') || t.includes('max')) return 'info'
  if (t.includes('pro')) return 'secondary'
  return 'outline'
}

/** 凭证综合状态；access_token 到期不参与判色，因为下次调度会自动刷新。 */
export function statusMeta(
  cred: Credential,
  now = currentUnixSeconds(),
  language: Language = 'zh-CN',
): CredentialStatusMeta {
  return evaluateCredential(cred, now, language).status
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
export function credentialExpiryMeta(
  cred: Credential,
  language: Language = 'zh-CN',
): {
  text: string
  className: string
  title?: string
} {
  if (cred.expired) {
    return {
      text: localize(language, '待刷新', 'Refresh pending'),
      className: 'text-muted-foreground',
      title: localize(
        language,
        `access_token 已于 ${formatFullTime(cred.expires_at, language)} 过期 · 下次被调度时自动刷新，不影响可用性`,
        `The access token expired at ${formatFullTime(cred.expires_at, language)} · It refreshes automatically the next time this account is scheduled and does not affect availability`,
      ),
    }
  }
  return {
    text: localize(
      language,
      `${formatClockTime(cred.expires_at, language)} 过期`,
      `Expires ${formatClockTime(cred.expires_at, language)}`,
    ),
    className: 'text-muted-foreground',
    title: localize(
      language,
      `${formatFullTime(cred.expires_at, language)} 过期 · 到点自动刷新`,
      `Expires ${formatFullTime(cred.expires_at, language)} · Refreshes automatically when due`,
    ),
  }
}

/** 列表紧凑态的综合说明：优先展示会影响账号调度的状态，再回退到真实有效期。 */
export function expiryMeta(cred: Credential, language: Language = 'zh-CN'): {
  text: string
  className: string
  title?: string
} {
  // 同 `statusFromQuota`：限流暂停要排在封禁之前，否则会被显示成「已封禁」。
  if (cred.resume_at != null) {
    return {
      text: localize(language, '限流暂停', 'Rate limited'),
      className: 'font-medium text-warning-foreground',
      title: localize(
        language,
        `额度用尽，${formatFullTime(cred.resume_at, language)} 自动恢复调度`,
        `Quota exhausted; scheduling resumes at ${formatFullTime(cred.resume_at, language)}`,
      ),
    }
  }
  if (cred.ban_reason) {
    return {
      text: localize(language, '已封禁', 'Banned'),
      className: 'font-medium text-destructive-foreground',
      title: localizeBackendMessage(cred.ban_reason, language),
    }
  }
  if (cred.disabled) {
    return {
      text: localize(language, '已停用', 'Disabled'),
      className: 'text-muted-foreground',
    }
  }
  if (cred.rate_limited_secs > 0) {
    const minutes = Math.max(1, Math.ceil(cred.rate_limited_secs / 60))
    return {
      text: localize(
        language,
        `冷却约 ${minutes} 分钟`,
        `Cooling down · about ${minutes} ${minutes === 1 ? 'minute' : 'minutes'}`,
      ),
      className: 'font-medium text-warning-foreground',
      title: localize(
        language,
        '账号级限流冷却中，结束后会自动恢复调度',
        'Account-level rate-limit cooldown; scheduling resumes automatically when it ends',
      ),
    }
  }
  return credentialExpiryMeta(cred, language)
}

/** 启用开关的 hover 提示：封禁态说明「已被上游封禁」并提示仍可手动停用。 */
export function switchTitle(cred: Credential, language: Language = 'zh-CN'): string {
  if (cred.disabled) return localize(language, '已停用（点击启用）', 'Disabled (click to enable)')
  if (cred.ban_reason) {
    const reason = localizeBackendMessage(cred.ban_reason, language)
    return localize(
      language,
      `${reason} · 点击可手动停用`,
      `${reason} · Click to disable manually`,
    )
  }
  return localize(language, '已启用（点击停用）', 'Enabled (click to disable)')
}
