import { useId, useState } from 'react'
import { useI18n } from '@/lib/i18n'
import { cacheHitRate, cn, formatPercent, formatTokens, type CacheGranularity, type CacheSlot } from '@/lib/utils'

function slotReadout(
  slot: CacheSlot,
  granularity: CacheGranularity,
  t: (zh: string, en: string) => string,
  locale: string,
): { when: string; axis: string; rate: string; detail: string } {
  const d = new Date(slot.ts * 1000)
  const p = (n: number) => String(n).padStart(2, '0')
  const day = `${d.getMonth() + 1}/${d.getDate()}`
  const when = granularity === 'hour' ? `${day} ${p(d.getHours())}:00` : day
  const axis = granularity === 'hour' ? `${p(d.getHours())}:00` : day
  if (!slot.hasTraffic) {
    return { when, axis, rate: '—', detail: t('这个时段没有请求', 'No requests in this period') }
  }
  return {
    when,
    axis,
    rate: formatPercent(cacheHitRate(slot.inputTokens, slot.cachedTokens)),
    detail: t(
      `命中 ${slot.cachedTokens.toLocaleString(locale)} / 输入 ${slot.inputTokens.toLocaleString(locale)} token`,
      `${slot.cachedTokens.toLocaleString(locale)} cached of ${slot.inputTokens.toLocaleString(locale)} input tokens`,
    ),
  }
}

function tickStep(slots: number): number {
  return Math.max(1, Math.ceil(slots / 7))
}

function volumeWeight(inputTokens: number, maxInputTokens: number): number {
  if (maxInputTokens <= 0) return 1
  return 0.3 + 0.7 * Math.sqrt(Math.min(1, inputTokens / maxInputTokens))
}

const DIP_MIN_VOLUME_SHARE = 0.1

export function CacheHitColumns({
  slots,
  granularity,
  refetching = false,
  className,
}: {
  slots: CacheSlot[]
  granularity: CacheGranularity
  refetching?: boolean
  className?: string
}) {
  const { t, locale } = useI18n()
  const [active, setActive] = useState<number | null>(null)
  const step = tickStep(slots.length)
  const readouts = slots.map((s) => slotReadout(s, granularity, t, locale))
  const maxInput = Math.max(0, ...slots.map((s) => s.inputTokens))
  const dipIndex = slots.reduce<number | null>((lowest, s, i) => {
    if (!s.hasTraffic || s.inputTokens < maxInput * DIP_MIN_VOLUME_SHARE) return lowest
    const rate = cacheHitRate(s.inputTokens, s.cachedTokens) ?? 1
    const best = lowest == null ? null : cacheHitRate(slots[lowest].inputTokens, slots[lowest].cachedTokens) ?? 1
    return best == null || rate < best ? i : lowest
  }, null)
  const dipWorthLabelling =
    dipIndex != null &&
    slots.length <= 12 &&
    slots.filter((s) => s.hasTraffic).length > 2 &&
    dipIndex > 0 &&
    dipIndex < slots.length - 1

  return (
    <div className={cn('transition-opacity', refetching && 'opacity-60', className)}>
      <div className="flex gap-2">
        <div className="flex h-40 w-8 shrink-0 flex-col justify-between py-0 text-end text-2xs text-muted-foreground tabular-nums">
          <span className="-translate-y-1/2">100%</span>
          <span>50%</span>
          <span className="translate-y-1/2">0%</span>
        </div>

        <div className="min-w-0 flex-1">
          <div className="relative h-40">
            {[0, 50, 100].map((pct) => (
              <div
                key={pct}
                aria-hidden
                className="absolute inset-x-0 border-t border-border"
                style={{ bottom: `${pct}%` }}
              />
            ))}
            <div className="absolute inset-0 flex items-end">
              {slots.map((slot, i) => {
                const rate = slot.hasTraffic ? cacheHitRate(slot.inputTokens, slot.cachedTokens) ?? 0 : null
                return (
                  <div
                    key={slot.ts}
                    role="img"
                    tabIndex={0}
                    aria-label={`${readouts[i].when} · ${readouts[i].rate} · ${readouts[i].detail}`}
                    onPointerEnter={() => setActive(i)}
                    onPointerLeave={() => setActive((cur) => (cur === i ? null : cur))}
                    onFocus={() => setActive(i)}
                    onBlur={() => setActive((cur) => (cur === i ? null : cur))}
                    className="group relative flex h-full flex-1 items-end justify-center px-px outline-none"
                  >
                    <span
                      aria-hidden
                      className={cn(
                        'absolute inset-0 transition-colors',
                        active === i && 'bg-muted/56',
                        'group-focus-visible:ring-2 group-focus-visible:ring-ring group-focus-visible:ring-inset',
                      )}
                    />
                    {rate == null ? (
                      <span
                        aria-hidden
                        className="relative h-0.5 w-full max-w-6 rounded-full bg-muted-foreground/24"
                      />
                    ) : (
                      <span
                        aria-hidden
                        className="relative w-full max-w-6 rounded-t bg-chart-1"
                        style={{
                          height: `max(0.125rem, ${rate * 100}%)`,
                          opacity: volumeWeight(slot.inputTokens, maxInput),
                        }}
                      />
                    )}
                    {dipWorthLabelling && i === dipIndex && (
                      <span
                        aria-hidden
                        className="absolute whitespace-nowrap text-2xs text-muted-foreground tabular-nums"
                        style={{ bottom: `calc(max(0.125rem, ${(rate ?? 0) * 100}%) + 0.25rem)` }}
                      >
                        {readouts[i].rate}
                      </span>
                    )}
                  </div>
                )
              })}
            </div>

            {active != null && (() => {
              const pos = (active + 0.5) / slots.length
              const anchor = pos < 0.2 ? 'start' : pos > 0.8 ? 'end' : 'center'
              return (
                <div
                  role="status"
                  aria-live="off"
                  className={cn(
                    'pointer-events-none absolute top-1 z-10 rounded-lg border bg-popover px-2 py-1',
                    'text-2xs leading-4 text-popover-foreground shadow-md',
                    'w-max max-w-[min(16rem,100%)]',
                    anchor === 'center' && '-translate-x-1/2',
                  )}
                  style={
                    anchor === 'start'
                      ? { left: 0 }
                      : anchor === 'end'
                        ? { right: 0 }
                        : { left: `${pos * 100}%` }
                  }
                >
                  <p className="flex items-baseline gap-1.5 tabular-nums">
                    <span className="font-semibold">{readouts[active].rate}</span>
                    <span className="text-muted-foreground">{readouts[active].when}</span>
                  </p>
                  <p className="text-muted-foreground tabular-nums">{readouts[active].detail}</p>
                </div>
              )
            })()}
          </div>

          <div className="relative mt-1.5 h-4" aria-hidden>
            {slots.map((slot, i) =>
              i % step === 0 ? (
                <span
                  key={slot.ts}
                  className="absolute -translate-x-1/2 whitespace-nowrap text-2xs text-muted-foreground tabular-nums"
                  style={{ left: `${((i + 0.5) / slots.length) * 100}%` }}
                >
                  {readouts[i].axis}
                </span>
              ) : null,
            )}
          </div>
        </div>
      </div>
    </div>
  )
}

export function CacheHitTable({
  slots,
  granularity,
}: {
  slots: CacheSlot[]
  granularity: CacheGranularity
}) {
  const { t, locale } = useI18n()
  const captionId = useId()
  const rows = slots.filter((s) => s.hasTraffic)

  return (
    <div className="max-h-64 overflow-y-auto rounded-xl border">
      <table className="w-full text-xs" aria-describedby={captionId}>
        <caption id={captionId} className="sr-only">
          {t('缓存命中率按时段明细', 'Cache hit rate by period')}
        </caption>
        <thead className="sticky top-0 bg-muted/96 backdrop-blur-sm">
          <tr className="[&>th]:h-7 [&>th]:border-b [&>th]:px-3 [&>th]:text-2xs [&>th]:font-medium [&>th]:text-muted-foreground">
            <th scope="col" className="text-start">
              {granularity === 'hour' ? t('时段', 'Hour') : t('日期', 'Day')}
            </th>
            <th scope="col" className="text-end">{t('命中率', 'Hit rate')}</th>
            <th scope="col" className="text-end">{t('命中 token', 'Cached')}</th>
            <th scope="col" className="text-end">{t('输入 token', 'Input')}</th>
          </tr>
        </thead>
        <tbody>
          {rows.map((slot) => {
            const r = slotReadout(slot, granularity, t, locale)
            return (
              <tr key={slot.ts} className="[&>td]:border-b [&>td]:px-3 [&>td]:py-1.5 last:[&>td]:border-b-0">
                <td className="whitespace-nowrap tabular-nums">{r.when}</td>
                <td className="whitespace-nowrap text-end font-medium tabular-nums">{r.rate}</td>
                <td className="whitespace-nowrap text-end tabular-nums">
                  {slot.cachedTokens.toLocaleString(locale)}
                </td>
                <td className="whitespace-nowrap text-end tabular-nums">
                  {slot.inputTokens.toLocaleString(locale)}
                </td>
              </tr>
            )
          })}
        </tbody>
      </table>
    </div>
  )
}

export function CacheHitSparkline({ slots, className }: { slots: CacheSlot[]; className?: string }) {
  const maxInput = Math.max(0, ...slots.map((s) => s.inputTokens))
  return (
    <span aria-hidden className={cn('flex h-5 items-end gap-px', className)}>
      {slots.map((slot, i) => {
        const rate = slot.hasTraffic ? cacheHitRate(slot.inputTokens, slot.cachedTokens) ?? 0 : null
        const last = i === slots.length - 1
        return (
          <span
            key={slot.ts}
            className={cn('w-1.5 rounded-t', rate == null ? 'bg-muted-foreground/24' : 'bg-chart-1')}
            style={{
              height: rate == null ? '0.125rem' : `max(0.125rem, ${rate * 100}%)`,
              opacity:
                rate == null ? undefined : (last ? 1 : 0.4) * volumeWeight(slot.inputTokens, maxInput),
            }}
          />
        )
      })}
    </span>
  )
}

export function aggregateCacheHitRate(slots: CacheSlot[]): {
  rate: number | null
  inputTokens: number
  cachedTokens: number
} {
  const inputTokens = slots.reduce((sum, s) => sum + s.inputTokens, 0)
  const cachedTokens = slots.reduce((sum, s) => sum + s.cachedTokens, 0)
  return { rate: cacheHitRate(inputTokens, cachedTokens), inputTokens, cachedTokens }
}

export function cacheTotalsText(
  cachedTokens: number,
  inputTokens: number,
  t: (zh: string, en: string) => string,
): string {
  return t(
    `命中 ${formatTokens(cachedTokens)} / 输入 ${formatTokens(inputTokens)}`,
    `${formatTokens(cachedTokens)} of ${formatTokens(inputTokens)} input`,
  )
}
