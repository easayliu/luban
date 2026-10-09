import type { ReactNode } from 'react'
import { RefreshCwIcon, TriangleAlertIcon } from 'lucide-react'
import { useI18n } from '@/lib/i18n'
import { cn, extractError } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Empty, EmptyContent, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from '@/components/ui/empty'
import { Spinner } from '@/components/ui/spinner'

/**
 * 加载中 / 读取失败的占位，全站同一副样子。此前设置页、用户管理、费用页各写一份：有的是
 * 居中转圈、有的是一行灰字，出错有的是 Empty、有的是一句红字。
 */
export function LoadingState({ label, className }: { label?: ReactNode; className?: string }) {
  const { t } = useI18n()
  return (
    <div className={cn('flex min-h-40 items-center justify-center gap-2 text-sm text-muted-foreground', className)} role="status">
      <Spinner className="size-4" />
      {label ?? t('正在加载', 'Loading')}
    </div>
  )
}

/** 读取失败：标题 + 错误原文 + 重试。`title` 缺省「暂时无法读取」。 */
export function ErrorState({
  error,
  title,
  onRetry,
  retrying,
}: {
  error: unknown
  title?: ReactNode
  onRetry?: () => void
  retrying?: boolean
}) {
  const { t, language } = useI18n()
  return (
    <Empty role="alert">
      <EmptyHeader>
        <EmptyMedia variant="icon"><TriangleAlertIcon /></EmptyMedia>
        <EmptyTitle>{title ?? t('暂时无法读取', 'Unable to load')}</EmptyTitle>
        <EmptyDescription className="break-words">{extractError(error, language)}</EmptyDescription>
      </EmptyHeader>
      {onRetry && (
        <EmptyContent>
          <Button loading={retrying} variant="outline" onClick={onRetry}>
            <RefreshCwIcon />
            {t('重新加载', 'Reload')}
          </Button>
        </EmptyContent>
      )}
    </Empty>
  )
}
