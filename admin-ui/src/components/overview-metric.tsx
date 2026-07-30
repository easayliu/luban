import type { ElementType } from 'react'
import { cn } from '@/lib/utils'

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
    ok: 'text-ok',
    bad: 'text-bad',
    warn: 'text-warn',
    neutral: 'text-muted-foreground',
  }[tone]
  const statusClass = {
    ok: 'text-ok',
    bad: 'text-bad',
    warn: 'text-warn',
    neutral: 'text-muted-foreground',
  }[tone]

  const content = (
    <>
      <div className="flex items-center justify-between gap-3">
        <p className="min-w-0 text-sm font-medium text-muted-foreground">{label}</p>
        <Icon className={cn('size-5 shrink-0', iconClass)} aria-hidden />
      </div>
      <div className="mt-3 flex flex-wrap items-baseline gap-x-2 gap-y-1">
        <span className="text-2xl font-semibold leading-none tracking-tight tnum">
          {value}
        </span>
        {status && <span className={cn('text-xs', statusClass)}>{status}</span>}
      </div>
    </>
  )

  const rootClass = cn(
    'relative min-w-0 bg-card px-4 py-4 text-left sm:px-5 sm:py-5',
    onClick && 'cursor-pointer transition-colors hover:bg-muted/35 focus-visible:z-10 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring/60',
    active && 'bg-primary-soft/80',
    className,
  )

  if (onClick) {
    return (
      <button type="button" className={rootClass} onClick={onClick} aria-pressed={active}>
        {content}
      </button>
    )
  }

  return (
    <div className={rootClass}>
      {content}
    </div>
  )
}
