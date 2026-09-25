import type { ElementType, ReactNode } from 'react'
import { ClampedDescription } from '@/components/settings-group'
import { Frame, FrameDescription, FrameHeader, FramePanel, FrameTitle } from '@/components/ui/frame'
import { cn } from '@/lib/utils'

/**
 * 页面级分组：图标、标题字号、说明折叠与设置页的 SettingsGroup 完全一致（同一副 Frame 底座），
 * 只多一个头部右侧的操作槽——SettingsGroup 没有这个口子，详情页的「查看全部」「刷新」要放那儿。
 */
export function DetailSection({
  icon: Icon,
  title,
  description,
  mobileDescription,
  action,
  children,
  className,
  panelClassName,
}: {
  icon: ElementType<{ className?: string }>
  title: string
  description?: ReactNode
  /**
   * 手机上显示的短说明。不传则手机上把 `description` 收进读屏文本——窄屏上一段说明要占两三行，
   * 还常被右边的操作挤成竖条（设置页的页面描述同样这么处理）；带数据的那几句（更新时刻、条数）
   * 值得留，就传一条短的进来。
   */
  mobileDescription?: ReactNode
  action?: ReactNode
  children: ReactNode
  /** 给 Frame 的类名；并排的两块要等高时传 `h-full`，再给面板 `flex-1` 让它吃掉多出来的高度。 */
  className?: string
  panelClassName?: string
}) {
  return (
    <Frame className={className}>
      <FrameHeader className="flex-row flex-wrap items-start justify-between gap-x-4 gap-y-2">
        <div className="min-w-0 flex-1 space-y-1">
          <div className="flex items-center gap-2">
            <Icon aria-hidden="true" className="size-4 text-muted-foreground" />
            <FrameTitle className="text-base">
              <h2>{title}</h2>
            </FrameTitle>
          </div>
          {description && (
            <FrameDescription className="text-sm leading-5 max-sm:sr-only sm:pl-6">
              {typeof description === 'string' ? <ClampedDescription text={description} /> : description}
            </FrameDescription>
          )}
          {mobileDescription && (
            <FrameDescription className="text-xs leading-5 sm:hidden">{mobileDescription}</FrameDescription>
          )}
        </div>
        {action && <div className="flex shrink-0 items-center gap-2">{action}</div>}
      </FrameHeader>
      <FramePanel className={cn('p-0', panelClassName)}>{children}</FramePanel>
    </Frame>
  )
}
