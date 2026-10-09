import { useState } from 'react'
import { useI18n } from '@/lib/i18n'
import {
  cacheHitRate,
  cn,
  formatPercent,
  formatTokens,
  slotLabel,
  tickStep,
  type CacheGranularity,
  type CacheSlot,
} from '@/lib/utils'
import { ChartReadout } from '@/components/chart-readout'
import { ChartTable } from '@/components/chart-table'
import { TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'

function slotReadout(
  slot: CacheSlot,
  granularity: CacheGranularity,
  t: (zh: string, en: string) => string,
  locale: string,
): { when: string; axis: string; rate: string; detail: string } {
  const { when, axis } = slotLabel(slot.ts, granularity)
  if (!slot.hasTraffic) {
    return { when, axis, rate: '—', detail: t('该时段没有请求', 'No requests in this period') }
  }
  const uncached = uncachedTokens(slot)
  return {
    when,
    axis,
    rate: formatPercent(cacheHitRate(slot.inputTokens, slot.cachedTokens)),
    detail: t(
      `缓存读 ${slot.cachedTokens.toLocaleString(locale)} · 缓存写 ${slot.writtenTokens.toLocaleString(locale)} · 输入 ${uncached.toLocaleString(locale)} token`,
      `Cache read ${slot.cachedTokens.toLocaleString(locale)} · Cache write ${slot.writtenTokens.toLocaleString(locale)} · Input ${uncached.toLocaleString(locale)} tokens`,
    ),
  }
}

/** 三段之外的那部分：既没命中也没写入、按原价算的输入。 */
function uncachedTokens(slot: { inputTokens: number; cachedTokens: number; writtenTokens: number }): number {
  return Math.max(0, slot.inputTokens - slot.cachedTokens - slot.writtenTokens)
}

function volumeWeight(inputTokens: number, maxInputTokens: number): number {
  if (maxInputTokens <= 0) return 1
  return 0.3 + 0.7 * Math.sqrt(Math.min(1, inputTokens / maxInputTokens))
}

const DIP_MIN_VOLUME_SHARE = 0.1

/**
 * 叠柱里段与段之间那 2px 的留白——「用底色去分隔，而不是给每段描边」。
 *
 * 做法是给上面那段加一条 **透明的下边框**，再配 `bg-clip-padding`：背景不画到边框下面，
 * 于是那 2px 露出的是柱子背后的东西（包括悬浮时那层 `bg-muted/56` 的高亮），
 * 而不是某个写死的底色。边框走 border-box，占的是这一段自己已经分到的高度，
 * 三段的百分比之和仍然是 100%，柱子不会因此长高或被裁掉。
 *
 * 描边是不行的：那会给图里加进不属于数据的墨。
 */
const SEGMENT_GAP = 'border-b-2 border-transparent bg-clip-padding'

/**
 * 太薄的段不开缝：柱高 `h-40`（160px），4% 约合 6.4px，扣掉 2px 还剩 4.4px 涂色；
 * 再薄就让它实心——本来也没什么好分隔的，扣完反而把这一段自己抹掉了。
 */
const GAP_MIN_PCT = 4
const segmentGap = (pct: number) => (pct >= GAP_MIN_PCT ? SEGMENT_GAP : undefined)

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
                const share = (tokens: number) =>
                  slot.inputTokens > 0 ? (tokens / slot.inputTokens) * 100 : 0
                const uncachedPct = share(uncachedTokens(slot))
                const writtenPct = share(slot.writtenTokens)
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
                      // 一根柱子叠三段：底下深色是命中、中间浅色是写入、顶上灰色是未缓存，
                      // 三段加起来撑满——命中率就是深色那段的高度，写入多命中少一眼能看出来。
                      <span
                        aria-hidden
                        className="relative flex w-full max-w-6 flex-col justify-end overflow-hidden rounded-t"
                        style={{ height: '100%', opacity: volumeWeight(slot.inputTokens, maxInput) }}
                      >
                        <span
                          className={cn('w-full bg-muted-foreground/24', segmentGap(uncachedPct))}
                          style={{ height: `${uncachedPct}%` }}
                        />
                        <span
                          className={cn('w-full bg-chart-1/40', segmentGap(writtenPct))}
                          style={{ height: `${writtenPct}%` }}
                        />
                        {/* 最底下这段坐在基线上，下面没有东西要分隔，不开缝。 */}
                        <span className="w-full bg-chart-1" style={{ height: `max(0.125rem, ${rate * 100}%)` }} />
                      </span>
                    )}
                    {dipWorthLabelling && i === dipIndex && (
                      <span
                        aria-hidden
                        // 叠柱撑满整格，这个数字落在浅色 / 灰色段上，垫一层底色才读得清。
                        className="absolute whitespace-nowrap rounded bg-popover/85 px-0.5 text-2xs text-muted-foreground tabular-nums"
                        style={{ bottom: `calc(max(0.125rem, ${(rate ?? 0) * 100}%) + 0.25rem)` }}
                      >
                        {readouts[i].rate}
                      </span>
                    )}
                  </div>
                )
              })}
            </div>

            {active != null && (
              <ChartReadout
                index={active}
                count={slots.length}
                value={readouts[active].rate}
                when={readouts[active].when}
                detail={readouts[active].detail}
              />
            )}
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
  const rows = slots.filter((s) => s.hasTraffic)

  return (
    <ChartTable caption={t('缓存命中率按时段明细', 'Cache hit rate by period')}>
      <TableHeader>
        <TableRow>
          <TableHead>{granularity === 'hour' ? t('时段', 'Hour') : t('日期', 'Day')}</TableHead>
          <TableHead className="text-end">{t('命中率', 'Hit rate')}</TableHead>
          <TableHead className="text-end">{t('缓存读', 'Cache read')}</TableHead>
          <TableHead className="text-end">{t('缓存写', 'Cache write')}</TableHead>
          <TableHead className="text-end">{t('输入', 'Input')}</TableHead>
          <TableHead className="text-end">{t('输入合计', 'Input total')}</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {rows.map((slot) => {
          const r = slotReadout(slot, granularity, t, locale)
          return (
            <TableRow key={slot.ts}>
              <TableCell>{r.when}</TableCell>
              <TableCell className="text-end font-medium">{r.rate}</TableCell>
              <TableCell className="text-end">{slot.cachedTokens.toLocaleString(locale)}</TableCell>
              <TableCell className="text-end">{slot.writtenTokens.toLocaleString(locale)}</TableCell>
              <TableCell className="text-end">{uncachedTokens(slot).toLocaleString(locale)}</TableCell>
              <TableCell className="text-end text-muted-foreground">{slot.inputTokens.toLocaleString(locale)}</TableCell>
            </TableRow>
          )
        })}
      </TableBody>
    </ChartTable>
  )
}

/**
 * 概览那一格里的迷你趋势。总宽固定 5rem、柱子按格数均分：格数从 7 天的 7 格改成 24 小时的
 * 24 格之后，每柱定宽会把整条线撑到 170px、压到隔壁那格的图标上。
 */
export function CacheHitSparkline({ slots, className }: { slots: CacheSlot[]; className?: string }) {
  const maxInput = Math.max(0, ...slots.map((s) => s.inputTokens))
  return (
    <span aria-hidden className={cn('flex h-5 min-w-0 max-w-20 flex-1 items-end gap-px', className)}>
      {slots.map((slot, i) => {
        const rate = slot.hasTraffic ? cacheHitRate(slot.inputTokens, slot.cachedTokens) ?? 0 : null
        const last = i === slots.length - 1
        return (
          <span
            key={slot.ts}
            className={cn('min-w-0 flex-1 rounded-t', rate == null ? 'bg-muted-foreground/24' : 'bg-chart-1')}
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

/** 三段写全：缓存读 · 缓存写 · 输入。 */
export function cacheSplitText(
  p: { input_tokens: number; cached_tokens: number; written_tokens: number },
  t: (zh: string, en: string) => string,
): string {
  const uncached = Math.max(0, p.input_tokens - p.cached_tokens - p.written_tokens)
  return t(
    `缓存读 ${formatTokens(p.cached_tokens)} · 缓存写 ${formatTokens(p.written_tokens)} · 输入 ${formatTokens(uncached)}`,
    `Cache read ${formatTokens(p.cached_tokens)} · Cache write ${formatTokens(p.written_tokens)} · Input ${formatTokens(uncached)}`,
  )
}
