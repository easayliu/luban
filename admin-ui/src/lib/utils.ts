import { clsx, type ClassValue } from 'clsx'
import { twMerge } from 'tailwind-merge'

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs))
}

/** 从 axios / Error 中提取用户友好的错误信息。 */
export function extractError(error: unknown): string {
  if (error && typeof error === 'object') {
    const e = error as {
      response?: { data?: unknown }
      message?: string
    }
    if (typeof e.response?.data === 'string' && e.response.data.trim()) {
      return e.response.data
    }
    if (e.message) return e.message
  }
  return '未知错误'
}

const DURATION_UNITS = [
  ['天', 86400],
  ['小时', 3600],
  ['分钟', 60],
  ['秒', 1],
] as const

/**
 * 时长（秒）格式化为「1 天 6 小时」，用于配置项回显这类**静态时长**。
 *
 * 取最高的两个非零单位、不丢余数：这里是给用户核对自己填的秒数的，宁可啰嗦也不能糊
 * （108000 显示成「1 天」会让人以为填对了整天）。倒计时不要用它——倒计时得自己走，
 * 否则渲染完就冻住；界面上的到期/重置一律用 {@link formatClockTime} 显示绝对时刻。
 */
export function formatDuration(secs: number): string {
  if (secs <= 0) return '0 秒'
  let rest = Math.floor(secs)
  const parts: string[] = []
  for (const [name, size] of DURATION_UNITS) {
    const n = Math.floor(rest / size)
    rest -= n * size
    if (n > 0) parts.push(`${n} ${name}`)
    if (parts.length === 2) break
  }
  return parts.join(' ')
}

const WEEKDAYS = ['周日', '周一', '周二', '周三', '周四', '周五', '周六']

const pad = (n: number) => String(n).padStart(2, '0')

/** 相差多少个自然日（按本地时区的午夜切分，不是 24 小时整除）。 */
function calendarDaysFromNow(d: Date): number {
  const now = new Date()
  const a = new Date(now.getFullYear(), now.getMonth(), now.getDate()).getTime()
  const b = new Date(d.getFullYear(), d.getMonth(), d.getDate()).getTime()
  return Math.round((b - a) / 86_400_000)
}

/**
 * Unix 秒 → 本地时区的绝对时刻，粒度随距离放宽：
 * 今天 `21:30`／明天 `明天 03:00`／一周内 `周三 09:00`／更远 `8/5 09:00`。
 *
 * 用绝对时刻而非倒计时：既省掉了 ticker，也避开了浏览器时钟偏差——歪的是本地时钟时，
 * 倒计时会直接算错，而绝对时刻只是按本地时区渲染同一个瞬间，仍然对得上用户的表。
 */
export function formatClockTime(unixSecs: number): string {
  const d = new Date(unixSecs * 1000)
  const hm = `${pad(d.getHours())}:${pad(d.getMinutes())}`
  const days = calendarDaysFromNow(d)
  if (days === 0) return hm
  if (days === 1) return `明天 ${hm}`
  // 7 天开外星期几会重名（「周三」可能是今天也可能是下周三），改用日期。
  if (days > 1 && days < 7) return `${WEEKDAYS[d.getDay()]} ${hm}`
  return `${d.getMonth() + 1}/${d.getDate()} ${hm}`
}

/** Unix 秒 → 完整本地时间，给 title 兜底（粗粒度显示看不出具体是哪天）。 */
export function formatFullTime(unixSecs: number): string {
  const d = new Date(unixSecs * 1000)
  return `${d.getFullYear()}-${pad(d.getMonth() + 1)}-${pad(d.getDate())} ${pad(d.getHours())}:${pad(d.getMinutes())}`
}

/** 复制文本到剪贴板：安全上下文用现代 API，否则回退 execCommand（http/局域网可用）。 */
export async function copyText(text: string): Promise<boolean> {
  if (!text) return false
  if (navigator.clipboard && window.isSecureContext) {
    try {
      await navigator.clipboard.writeText(text)
      return true
    } catch {
      // 继续走回退
    }
  }
  try {
    const ta = document.createElement('textarea')
    ta.value = text
    ta.setAttribute('readonly', '')
    ta.style.position = 'fixed'
    ta.style.left = '-9999px'
    document.body.appendChild(ta)
    ta.select()
    ta.setSelectionRange(0, text.length)
    const ok = document.execCommand('copy')
    document.body.removeChild(ta)
    return ok
  } catch {
    return false
  }
}

/** 美元金额格式化：极小额多留几位小数，便于看清单次费用。 */
export function formatUsd(v: number): string {
  if (!v || v <= 0) return '$0.00'
  if (v < 0.01) return `$${v.toFixed(4)}`
  if (v < 1) return `$${v.toFixed(3)}`
  return `$${v.toFixed(2)}`
}

/** Unix 秒时间戳 → 相对当前的「x 前」。 */
export function relativeTime(unixSecs: number): string {
  const diff = Math.floor(Date.now() / 1000) - unixSecs
  if (diff < 60) return '刚刚'
  const min = Math.floor(diff / 60)
  if (min < 60) return `${min} 分钟前`
  const hours = Math.floor(min / 60)
  if (hours < 24) return `${hours} 小时前`
  return `${Math.floor(hours / 24)} 天前`
}
