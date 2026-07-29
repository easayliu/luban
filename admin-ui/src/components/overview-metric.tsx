import type { ElementType } from 'react'
import { cn } from '@/lib/utils'

export function OverviewMetric({
  label, value, status, icon: Icon, tone, className,
}: {
  label: string
  value: number | string
  status?: string
  icon: ElementType<{ className?: string }>
  tone: 'ok' | 'bad' | 'warn' | 'neutral'
  className?: string
}) {
  const iconClass = {
    ok: 'bg-ok-soft text-ok',
    bad: 'bg-bad-soft text-bad',
    warn: 'bg-warn-soft text-warn',
    neutral: 'bg-muted text-muted-foreground',
  }[tone]
  const statusClass = {
    ok: 'text-ok',
    bad: 'text-bad',
    warn: 'text-warn',
    neutral: 'text-muted-foreground',
  }[tone]

  return (
    <div className={cn('min-w-0 px-2 py-3 sm:px-4 sm:py-3.5', className)}>
      <div className="flex min-h-10 items-center gap-3">
        <span className={cn('grid size-8 shrink-0 place-items-center rounded-md', iconClass)} aria-hidden>
          <Icon className="size-4" />
        </span>
        <div className="min-w-0 flex-1">
          <p className="text-2xs font-medium text-muted-foreground">{label}</p>
          <div className="mt-1 flex flex-wrap items-baseline gap-x-2 gap-y-0.5">
            <span className="text-xl font-semibold leading-none tracking-tight tnum">{value}</span>
            {status && <span className={cn('text-2xs font-medium', statusClass)}>{status}</span>}
          </div>
        </div>
      </div>
    </div>
  )
}
