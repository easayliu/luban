import { useState, type ReactNode } from 'react'
import type { LucideIcon } from 'lucide-react'
import { ChevronDownIcon } from 'lucide-react'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'
import {
  Frame,
  FrameDescription,
  FrameHeader,
  FramePanel,
  FrameTitle,
} from '@/components/ui/frame'

/**
 * 超过这个长度的说明默认收起到两行。
 *
 * 现有分组的说明多在 20~100 字，只有「从上游学到的规则」那条 370 余字会撑掉大半屏；
 * 按长度自动判定，短说明保持原样，日后再写长的也不必逐处改调用。
 */
const CLAMP_THRESHOLD = 140

/**
 * 系统设置页的统一分组容器。
 *
 * Frame 是仓库中来自 Coss UI 的设置面板底座；这里仅补齐标题语义、图标和说明，
 * 避免接入、设备与转发页面各自再造一套卡片样式。
 */
export function SettingsGroup({
  icon: Icon,
  title,
  description,
  children,
}: {
  icon?: LucideIcon
  title: string
  description?: string
  children: ReactNode
}) {
  const { t } = useI18n()
  const [open, setOpen] = useState(false)
  const clampable = !!description && description.length > CLAMP_THRESHOLD

  return (
    <Frame>
      <FrameHeader className="gap-1.5">
        <div className="flex items-center gap-2">
          {Icon && <Icon aria-hidden="true" className="size-4 text-muted-foreground" />}
          <FrameTitle>
            <h3>{title}</h3>
          </FrameTitle>
        </div>
        {description && (
          <FrameDescription className={cn('text-xs leading-5', Icon && 'pl-6')}>
            <p className={cn(clampable && !open && 'line-clamp-2')}>{description}</p>
            {clampable && (
              <button
                aria-expanded={open}
                className="mt-1 inline-flex items-center gap-0.5 text-xs text-foreground/70 underline-offset-4 transition-colors hover:text-foreground hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-1 focus-visible:ring-offset-background"
                type="button"
                onClick={() => setOpen((v) => !v)}
              >
                {open ? t('收起', 'Show less') : t('了解更多', 'Learn more')}
                <ChevronDownIcon
                  aria-hidden="true"
                  className={cn('size-3 transition-transform', open && 'rotate-180')}
                />
              </button>
            )}
          </FrameDescription>
        )}
      </FrameHeader>
      <FramePanel className="divide-y p-0">{children}</FramePanel>
    </Frame>
  )
}
