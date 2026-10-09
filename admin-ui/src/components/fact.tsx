import type { ReactNode } from 'react'
import { cn } from '@/lib/utils'

/**
 * 「字段名 / 值」一格，放在 `<dl>` 网格里用。账号详情与封号记录原来各写一份，字段名一边是
 * 12px、一边是 11px 全大写，同一类信息两副样子；这里统一成一份。
 *
 * 渲染成 `div > dt + dd`，外层必须是 `<dl>`——HTML 允许 `dl` 下用 `div` 给一对 dt/dd 分组。
 * 要跨列就把 `col-span-*` 传给 `className`，别再在外面套一层 div（那样 dt/dd 就不是 dl 的
 * 孙辈了）。字号随外层 `<dl>`。
 */
export function Fact({
  label,
  children,
  className,
}: {
  label: ReactNode
  children: ReactNode
  className?: string
}) {
  return (
    <div className={cn('min-w-0', className)}>
      <dt className="text-xs text-muted-foreground">{label}</dt>
      <dd className="mt-0.5 min-w-0 break-words tabular-nums">{children}</dd>
    </div>
  )
}
