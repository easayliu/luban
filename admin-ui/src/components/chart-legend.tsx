import type { ReactNode } from 'react'
import { InfoIcon } from 'lucide-react'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'
import { Tooltip, TooltipPopup, TooltipTrigger } from '@/components/ui/tooltip'

export type ChartLegendItem = {
  /** 色块的 className，必须与图里那一段的填充完全一致（如 `bg-chart-1/40`）。 */
  swatch: string
  label: string
  /** 记号的形状要跟着图里那个标记走：填充段落用方块，细横线标记（如 p95）用线。 */
  shape?: 'block' | 'line'
}

/**
 * 图表图例。两个及以上系列一律要有——这是「哪段是什么」唯一可靠的通道。
 *
 * 之前这两张堆叠图没有图例，三段颜色的含义写在图下面一段 100 字的 10px 灰字里，
 * 读的人得一边读文字一边回头对颜色。图例把色块和名字放在同一个视线落点上，
 * 那段文字也就只剩下「颜色之外的话」要说了。
 *
 * 文字一律用文本色，颜色只由旁边那枚色块承担——浅色系列当文字是读不清的。
 *
 * `hint` 是可选的诊断性说明（「写入多命中少说明什么」这类），挂在末尾一枚 info 上：
 * 它对会看的人有用、对其他人是噪声，适合收进悬浮层而不是平铺在图下。
 */
export function ChartLegend({
  items,
  hint,
  className,
}: {
  items: ChartLegendItem[]
  hint?: ReactNode
  className?: string
}) {
  const { t } = useI18n()

  return (
    <ul className={cn('flex min-w-0 flex-wrap items-center gap-x-3 gap-y-1', className)}>
      {items.map((item) => (
        <li className="flex items-center gap-1.5 text-2xs text-muted-foreground" key={item.label}>
          <span
            aria-hidden="true"
            className={cn(
              'shrink-0',
              item.shape === 'line' ? 'h-0.5 w-3 rounded-full' : 'size-2.5 rounded-[3px]',
              item.swatch,
            )}
          />
          {item.label}
        </li>
      ))}
      {hint && (
        <li className="flex items-center">
          <Tooltip>
            <TooltipTrigger
              aria-label={t('怎么读这张图', 'How to read this chart')}
              className="rounded-sm text-muted-foreground transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/60"
              render={<button type="button" />}
            >
              <InfoIcon className="size-3.5" />
            </TooltipTrigger>
            <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">
              {hint}
            </TooltipPopup>
          </Tooltip>
        </li>
      )}
    </ul>
  )
}
