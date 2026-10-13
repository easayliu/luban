import type { ElementType, ReactNode } from 'react'
import { useI18n } from '@/lib/i18n'
import { InfoHint } from '@/components/info-hint'
import { Frame, FrameHeader, FramePanel, FrameTitle } from '@/components/ui/frame'
import { cn } from '@/lib/utils'

/**
 * 页面级分组：图标、标题字号、表头版式与设置页的 SettingsGroup 一致（同一副 Frame 底座、说明同样
 * 收进标题旁的 ⓘ），只多两个口子——头部右侧的操作槽（「查看全部」「刷新」），以及标题后的一句读数。
 */
export function DetailSection({
  icon: Icon,
  title,
  description,
  summary,
  action,
  children,
  className,
  panelClassName,
}: {
  icon: ElementType<{ className?: string }>
  title: string
  /** 「这块讲什么」：收进标题旁的 ⓘ 浮层，不常显。 */
  description?: ReactNode
  /**
   * 跟在标题后的一句读数（条数、费用、「暂无快照」这类状态）。它是数据不是说明，所以常显，
   * 不收进 ⓘ；写短，一眼读完。
   */
  summary?: ReactNode
  action?: ReactNode
  children: ReactNode
  /** 给 Frame 的类名；并排的两块要等高时传 `h-full`，再给面板 `flex-1` 让它吃掉多出来的高度。 */
  className?: string
  panelClassName?: string
}) {
  const { t } = useI18n()
  return (
    <Frame className={className}>
      <FrameHeader className="flex-row flex-wrap items-center justify-between gap-x-4 gap-y-2 py-2.5 sm:py-3">
        <div className="flex min-w-0 flex-1 flex-wrap items-center gap-x-2 gap-y-0.5">
          <Icon aria-hidden="true" className="size-4 text-muted-foreground" />
          <FrameTitle className="text-base">
            <h2>{title}</h2>
          </FrameTitle>
          {description && (
            <InfoHint label={t(`${title}：说明`, `About ${title}`)}>{description}</InfoHint>
          )}
          {summary && (
            <span className="min-w-0 text-xs leading-5 text-muted-foreground tabular-nums">{summary}</span>
          )}
        </div>
        {action && <div className={cn('flex shrink-0 items-center gap-2')}>{action}</div>}
      </FrameHeader>
      <FramePanel className={cn('p-0 sm:p-0', panelClassName)}>{children}</FramePanel>
    </Frame>
  )
}
