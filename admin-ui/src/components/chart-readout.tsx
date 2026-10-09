import type { ReactNode } from 'react'
import { cn } from '@/lib/utils'

/**
 * 柱状趋势图悬浮 / 聚焦某一格时浮在图顶的读数：第一行「主值 + 时段」，第二行明细。
 *
 * 缓存命中、首字时延、账号统计三张图原来各抄一份同样的绝对定位面板，这里收成一份。
 * 横向位置跟着那一格走；靠近两端（前后 20%）时改贴左 / 右边缘，否则居中会把面板挤出图外。
 * `pointer-events-none`：面板盖在柱子上，不能挡住指针去悬浮下一格。
 *
 * 需要放在 `relative` 的图区容器里。默认最宽 16rem，明细更长的图用 `className` 放宽
 * （`max-w-[min(18rem,100%)]`）。
 */
export function ChartReadout({
  index,
  count,
  value,
  when,
  detail,
  className,
}: {
  /** 当前那一格的下标。 */
  index: number
  /** 总格数。 */
  count: number
  value: ReactNode
  when: ReactNode
  detail: ReactNode
  className?: string
}) {
  const pos = (index + 0.5) / count
  const anchor = pos < 0.2 ? 'start' : pos > 0.8 ? 'end' : 'center'
  return (
    <div
      role="status"
      aria-live="off"
      className={cn(
        'pointer-events-none absolute top-1 z-10 rounded-lg border bg-popover px-2 py-1',
        'text-2xs leading-4 text-popover-foreground shadow-md',
        'w-max max-w-[min(16rem,100%)]',
        anchor === 'center' && '-translate-x-1/2',
        className,
      )}
      style={anchor === 'start' ? { left: 0 } : anchor === 'end' ? { right: 0 } : { left: `${pos * 100}%` }}
    >
      <p className="flex items-baseline gap-1.5 tabular-nums">
        <span className="font-semibold">{value}</span>
        <span className="text-muted-foreground">{when}</span>
      </p>
      <p className="text-muted-foreground tabular-nums">{detail}</p>
    </div>
  )
}
