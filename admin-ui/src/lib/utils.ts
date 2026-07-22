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

/** 秒数格式化为「x 分钟 / x 小时」。 */
export function formatDuration(secs: number): string {
  if (secs <= 0) return '已过期'
  const min = Math.floor(secs / 60)
  if (min < 60) return `${min} 分钟`
  const hours = Math.floor(min / 60)
  const rem = min % 60
  return rem ? `${hours} 小时 ${rem} 分钟` : `${hours} 小时`
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
