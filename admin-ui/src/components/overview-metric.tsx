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
    <div className="p-3 sm:p-4">
      <div className="flex items-center justify-between gap-3">
        <p className="min-w-0 text-xs font-medium text-muted-foreground">{label}</p>
        <Icon className={cn('size-4 shrink-0', iconClass)} aria-hidden />
      </div>
      <div className="mt-2.5 flex flex-wrap items-baseline gap-x-2 gap-y-1">
        <span className="text-xl font-semibold leading-none tracking-tight tnum">
          {value}
        </span>
        {status && <span className="text-xs text-muted-foreground">{status}</span>}
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
    <div className={cn('min-w-0 p-3 sm:p-4', className)}>
      <div className="flex items-center justify-between gap-3">
        <Skeleton className="h-4 w-20" />
        <Skeleton className="size-4 rounded-md" />
      </div>
      <div className="mt-2.5 flex items-end gap-2">
        <Skeleton className="h-5 w-14" />
        <Skeleton className="mb-0.5 h-3 w-16" />
      </div>
    </div>
  )
}
