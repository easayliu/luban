import type { ElementType } from 'react'
import { cn } from '@/lib/utils'
import { Card, CardPanel } from '@/components/ui/card'
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
    <CardPanel className="p-4 sm:p-5">
      <div className="flex items-center justify-between gap-3">
        <p className="min-w-0 text-sm font-medium text-muted-foreground">{label}</p>
        <Icon className={cn('size-5 shrink-0', iconClass)} aria-hidden />
      </div>
      <div className="mt-3 flex flex-wrap items-baseline gap-x-2 gap-y-1">
        <span className="text-2xl font-semibold leading-none tracking-tight tnum">
          {value}
        </span>
        {status && <span className="text-xs text-muted-foreground">{status}</span>}
      </div>
    </CardPanel>
  )

  const rootClass = cn(
    'min-w-0 text-left',
    onClick && 'cursor-pointer transition-colors hover:bg-muted/40 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-1 focus-visible:ring-offset-background',
    active && 'border-primary bg-muted/56 ring-1 ring-primary/16',
    className,
  )

  return (
    <Card
      className={rootClass}
      render={onClick ? <button type="button" onClick={onClick} aria-pressed={active} /> : undefined}
    >
      {content}
    </Card>
  )
}

export function OverviewMetricSkeleton({ className }: { className?: string }) {
  return (
    <Card className={cn('min-w-0', className)}>
      <CardPanel className="p-4 sm:p-5">
        <div className="flex items-center justify-between gap-3">
          <Skeleton className="h-4 w-20" />
          <Skeleton className="size-5 rounded-md" />
        </div>
        <div className="mt-3 flex items-end gap-2">
          <Skeleton className="h-6 w-14" />
          <Skeleton className="mb-0.5 h-3 w-16" />
        </div>
      </CardPanel>
    </Card>
  )
}
