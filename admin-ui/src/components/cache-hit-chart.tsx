import { useState } from 'react'
import { useI18n } from '@/lib/i18n'
import {
  cacheHitRate,
  cn,
  formatPercent,
  formatTokens,
  formatUsd,
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
  const n = (v: number) => v.toLocaleString(locale)
  if (slot.inputTokens <= 0) {
    return { when, axis, rate: '—', detail: noInputText(slot.requests, t, locale) }
  }
  const w5 = written5mTokens(slot)
  return {
    when,
    axis,
    rate: formatPercent(cacheHitRate(slot.inputTokens, slot.cachedTokens)),
    // 按官方 usage / 定价表的顺序：输入 → 5m 缓存写 → 1h 缓存写 → 缓存读。
    detail: t(
      `输入 ${n(uncachedTokens(slot))} · 5m 缓存写 ${n(w5)} · 1h 缓存写 ${n(slot.written1hTokens)} · 缓存读 ${n(slot.cachedTokens)} token`,
      `Input ${n(uncachedTokens(slot))} · 5m cache writes ${n(w5)} · 1h cache writes ${n(slot.written1hTokens)} · Cache reads ${n(slot.cachedTokens)} tokens`,
    ),
  }
}

/** 5 分钟档写入：合计减 1 小时档（没有细分的整笔算 5 分钟档，与计价同口径）。 */
function written5mTokens(slot: { writtenTokens: number; written1hTokens: number }): number {
  return Math.max(0, slot.writtenTokens - slot.written1hTokens)
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
    if (!s.hasTraffic || s.inputTokens <= 0 || s.inputTokens < maxInput * DIP_MIN_VOLUME_SHARE) return lowest
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
                // 有请求但没有输入 token（全是失败 / 本地拒绝）时命中率算不出，与空格一样画基线。
                const rate = slot.hasTraffic ? cacheHitRate(slot.inputTokens, slot.cachedTokens) : null
                const share = (tokens: number) =>
                  slot.inputTokens > 0 ? (tokens / slot.inputTokens) * 100 : 0
                const uncachedPct = share(uncachedTokens(slot))
                const written5mPct = share(written5mTokens(slot))
                const written1hPct = share(slot.written1hTokens)
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
                      // 一根柱子叠四段，自上而下正是官方的顺序：灰色未缓存输入、浅色 5m 写入、
                      // 稍深的 1h 写入、深色命中，加起来撑满——命中率就是深色那段的高度。
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
                          className={cn('w-full bg-chart-1/32', segmentGap(written5mPct))}
                          style={{ height: `${written5mPct}%` }}
                        />
                        <span
                          className={cn('w-full bg-chart-1/56', segmentGap(written1hPct))}
                          style={{ height: `${written1hPct}%` }}
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
      {/* 与下面「缓存命中拆分」同一套列与写法：token 按官方定价表的顺序与叫法、缩写成 K / M，
          悬停看完整数值；表头默认大写，TTL 前缀保留小写。命中率的分母（全部输入）不再单列。 */}
      <TableHeader>
        <TableRow>
          <TableHead>{granularity === 'hour' ? t('时段', 'Hour') : t('日期', 'Day')}</TableHead>
          <TableHead className="text-end">{t('请求', 'Requests')}</TableHead>
          <TableHead className="text-end">{t('命中率', 'Hit rate')}</TableHead>
          <TableHead className="text-end">{t('输入', 'Input')}</TableHead>
          <TableHead className="text-end normal-case">{t('5m 缓存写', '5m cache writes')}</TableHead>
          <TableHead className="text-end normal-case">{t('1h 缓存写', '1h cache writes')}</TableHead>
          <TableHead className="text-end">{t('缓存读', 'Cache reads')}</TableHead>
          <TableHead className="text-end">{t('输出', 'Output')}</TableHead>
          <TableHead className="text-end">{t('API 价', 'API cost')}</TableHead>
        </TableRow>
      </TableHeader>
      <TableBody>
        {rows.map((slot) => {
          const r = slotReadout(slot, granularity, t, locale)
          const tokens = (v: number) => (
            <TableCell className="text-end" title={v.toLocaleString(locale)}>{formatTokens(v)}</TableCell>
          )
          return (
            <TableRow key={slot.ts}>
              <TableCell>{r.when}</TableCell>
              <TableCell className="text-end">{slot.requests.toLocaleString(locale)}</TableCell>
              <TableCell className="text-end font-medium">{r.rate}</TableCell>
              {tokens(uncachedTokens(slot))}
              {tokens(written5mTokens(slot))}
              {tokens(slot.written1hTokens)}
              {tokens(slot.cachedTokens)}
              {tokens(slot.outputTokens)}
              <TableCell className="text-end">{formatUsd(slot.costUsd)}</TableCell>
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
        const rate = slot.hasTraffic ? cacheHitRate(slot.inputTokens, slot.cachedTokens) : null
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

/** 四段写全，按官方顺序：输入 · 5m 缓存写 · 1h 缓存写 · 缓存读。 */
export function cacheSplitText(
  p: { input_tokens: number; cached_tokens: number; written_tokens: number; written_1h_tokens: number },
  t: (zh: string, en: string) => string,
): string {
  const uncached = Math.max(0, p.input_tokens - p.cached_tokens - p.written_tokens)
  const w5 = Math.max(0, p.written_tokens - p.written_1h_tokens)
  return t(
    `输入 ${formatTokens(uncached)} · 5m 缓存写 ${formatTokens(w5)} · 1h 缓存写 ${formatTokens(p.written_1h_tokens)} · 缓存读 ${formatTokens(p.cached_tokens)}`,
    `Input ${formatTokens(uncached)} · 5m writes ${formatTokens(w5)} · 1h writes ${formatTokens(p.written_1h_tokens)} · Cache reads ${formatTokens(p.cached_tokens)}`,
  )
}

/** 有请求、但没有输入 token（全是失败 / 本地拒绝，上游没回 usage）：命中率算不出。 */
export function noInputText(requests: number, t: (zh: string, en: string) => string, locale: string): string {
  const n = requests.toLocaleString(locale)
  return t(
    `${n} 个请求，均无 token 用量（多为失败请求），命中率无法计算`,
    `${n} requests without token usage (mostly failed), so no hit rate`,
  )
}
