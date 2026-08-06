import type { ElementType } from 'react'
import { cn } from '@/lib/utils'
import { Skeleton } from '@/components/ui/skeleton'

export function OverviewMetric({
  label, value, status, icon: Icon, tone, active = false, onClick, className,
}: {
  label: string
  value: number | string
  status?: string
  icon: ElementType<{ className?: string }>
  tone: 'ok' | 'bad' | 'warn' | 'neutral'
  active?: boolean
  onClick?: () => void
  className?: string
}) {
  const iconClass = {
    ok: 'text-success-foreground',
    bad: 'text-destructive-foreground',
    warn: 'text-warning-foreground',
    neutral: 'text-muted-foreground',
  }[tone]
  const content = (
    <div className="flex min-h-16 items-center gap-3 px-3 py-2.5 sm:px-4">
      <span className="flex size-8 shrink-0 items-center justify-center rounded-lg bg-muted">
        <Icon className={cn('size-4', iconClass)} aria-hidden />
      </span>
      <div className="min-w-0 flex-1">
        <div className="flex min-w-0 items-baseline justify-between gap-2">
          <p className="min-w-0 truncate text-xs font-medium text-muted-foreground">{label}</p>
          <span className="shrink-0 text-lg font-semibold leading-none tracking-tight tnum">
            {value}
          </span>
        </div>
        {status && (
          <p className="mt-1 truncate text-2xs text-muted-foreground" title={status}>
            {status}
          </p>
        )}
      </div>
    </div>
  )

  const rootClass = cn(
    'min-w-0 text-left transition-colors',
    onClick && 'cursor-pointer hover:bg-muted/40 focus-visible:relative focus-visible:z-10 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring',
    active && 'bg-muted/72',
    className,
  )

  if (onClick) {
    return (
      <button type="button" className={rootClass} onClick={onClick} aria-pressed={active}>
        {content}
      </button>
    )
  }

  return <div className={rootClass}>{content}</div>
}

export function OverviewMetricSkeleton({ className }: { className?: string }) {
  return (
    <div className={cn('flex min-h-16 min-w-0 items-center gap-3 px-3 py-2.5 sm:px-4', className)}>
      <Skeleton className="size-8 shrink-0 rounded-lg" />
      <div className="min-w-0 flex-1">
        <div className="flex items-center justify-between gap-3">
          <Skeleton className="h-3 w-20" />
          <Skeleton className="h-5 w-10" />
        </div>
        <Skeleton className="mt-1.5 h-3 w-24 max-w-full" />
      </div>
    </div>
  )
}
