import type { ReactNode } from 'react'
import { InfoIcon } from 'lucide-react'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'
import { Popover, PopoverPopup, PopoverTrigger } from '@/components/ui/popover'

/**
 * 全站统一的 ⓘ 说明：解释性的话收进浮层，看懂一次就不必每次占一行。
 *
 * 用 Popover 而不是 Tooltip：看到 ⓘ 的人习惯去点，Tooltip 点了反而收起、触屏上也打不开；
 * Popover 悬停与点击都能打开，浮层用 `tooltipStyle` 长得和 Tooltip 一样。
 * 之前图例、账号详情、设置分组、转发工具条各写了一份，触发器大小、触屏热区、能不能点开都不一样。
 */
export function InfoHint({
  children,
  label,
  side = 'top',
  className,
}: {
  children: ReactNode
  /** 读屏标签；缺省为「说明」。 */
  label?: string
  side?: 'top' | 'bottom' | 'left' | 'right'
  className?: string
}) {
  const { t } = useI18n()
  return (
    <Popover>
      <PopoverTrigger
        aria-label={label ?? t('说明', 'Explanation')}
        className={cn(
          'relative inline-flex shrink-0 rounded-full text-muted-foreground outline-none transition-colors hover:text-foreground focus-visible:ring-2 focus-visible:ring-ring pointer-coarse:after:absolute pointer-coarse:after:-inset-3',
          className,
        )}
        delay={0}
        openOnHover
      >
        <InfoIcon aria-hidden className="size-3.5" />
      </PopoverTrigger>
      <PopoverPopup className="max-w-72 whitespace-normal text-left font-normal leading-5" side={side} tooltipStyle>
        {children}
      </PopoverPopup>
    </Popover>
  )
}
