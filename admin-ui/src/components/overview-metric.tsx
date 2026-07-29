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

  const content = (
    <>
      <span
        className={cn(
          'absolute inset-y-3 left-0 w-0.5 rounded-r-full opacity-0 transition-opacity',
          tone === 'ok' && 'bg-ok',
          tone === 'bad' && 'bg-bad',
          tone === 'warn' && 'bg-warn',
          tone === 'neutral' && 'bg-muted-foreground',
          active && 'opacity-100',
        )}
        aria-hidden
      />
      <div className="flex items-start justify-between gap-2">
        <p className="min-w-0 text-xs font-medium text-muted-foreground">{label}</p>
        <span className={cn('grid size-7 shrink-0 place-items-center rounded-md', iconClass)} aria-hidden>
          <Icon className="size-3.5" />
        </span>
      </div>
      <div className="mt-2 flex flex-wrap items-baseline gap-x-2 gap-y-1">
        <span className="text-xl font-semibold leading-none tracking-tight tnum sm:text-2xl xl:text-xl">
          {value}
        </span>
        {status && <span className={cn('text-xs font-medium', statusClass)}>{status}</span>}
      </div>
    </>
  )

  const rootClass = cn(
    'relative min-w-0 overflow-hidden rounded-lg border border-border/80 bg-card px-3 py-3 text-left shadow-card',
    onClick && 'cursor-pointer transition-[border-color,background-color,transform,box-shadow] hover:-translate-y-px hover:border-foreground/20 hover:shadow-elev focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring/60 focus-visible:ring-offset-2 focus-visible:ring-offset-background',
    active && 'border-foreground/25 bg-primary-soft/70 ring-2 ring-ring/10',
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
