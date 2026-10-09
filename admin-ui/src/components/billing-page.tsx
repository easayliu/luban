import { useEffect, useMemo, useState } from 'react'
import { keepPreviousData, useQuery } from '@tanstack/react-query'
import { ReceiptIcon, XIcon } from 'lucide-react'
import { getBilling, type BillingDim, type BillingRow } from '@/api/billing'
import { useI18n } from '@/lib/i18n'
import { useMe } from '@/lib/role'
import { cn, extractError, formatTokens, formatUsd } from '@/lib/utils'
import { AppFooter } from '@/components/app-footer'
import { AppHeader, MainNav, PreferencesMenu, type MainSection } from '@/components/app-header'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from '@/components/ui/empty'
import { Skeleton } from '@/components/ui/skeleton'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { ToggleGroup, ToggleGroupItem } from '@/components/ui/toggle-group'

type RangeKey = 'today' | '7d' | '30d' | 'month' | 'last-month'

/** 时间范围（本地时区）：`[from, to)` 的 Unix 秒。 */
function rangeOf(key: RangeKey): { from: number; to: number } {
  const now = new Date()
  const startOfDay = new Date(now.getFullYear(), now.getMonth(), now.getDate())
  const secs = (d: Date) => Math.floor(d.getTime() / 1000)
  const tomorrow = secs(startOfDay) + 86400
  switch (key) {
    case 'today':
      return { from: secs(startOfDay), to: tomorrow }
    case '7d':
      return { from: tomorrow - 7 * 86400, to: tomorrow }
    case '30d':
      return { from: tomorrow - 30 * 86400, to: tomorrow }
    case 'month':
      return { from: secs(new Date(now.getFullYear(), now.getMonth(), 1)), to: tomorrow }
    case 'last-month':
      return {
        from: secs(new Date(now.getFullYear(), now.getMonth() - 1, 1)),
        to: secs(new Date(now.getFullYear(), now.getMonth(), 1)),
      }
  }
}

/** 正在查看的那个人（下钻）。 */
interface Focus {
  id: number
  name: string
}

/**
 * 费用：只计费、不扣费，看号跑出了多少等价 API 费用。
 *
 * 可看的范围由后端按身份收窄，这里只决定给哪些拆分维度：
 * - 管理员 / 访客：按人、号、模型、接入 Key、分组、日；点某个人下钻看他一个人的；
 * - 代理：默认按人看自己与名下用户的汇总；点自己能拆到号、模型、分组；点下属只看按日走势
 *   （代理看不到下属的号）；
 * - 用户：只看自己，按号、模型、分组、日。
 */
export function BillingPage({
  onNavigate,
  onSignOut,
}: {
  onNavigate: (section: MainSection) => void
  onSignOut: () => void
}) {
  const { t, language, locale } = useI18n()
  const me = useMe().data
  const role = me?.role
  const [range, setRange] = useState<RangeKey>('month')
  const [focus, setFocus] = useState<Focus | null>(null)
  const [dim, setDim] = useState<BillingDim | null>(null)

  useEffect(() => {
    const previousTitle = document.title
    document.title = `${t('费用', 'Billing')} · Luban`
    return () => {
      document.title = previousTitle
    }
  }, [t])

  // 当前身份、当前下钻下能用哪些拆分维度。
  const dims: BillingDim[] = useMemo(() => {
    if (role === 'admin' || role === 'viewer') {
      return focus ? ['cred', 'model', 'group', 'key', 'day'] : ['owner', 'cred', 'model', 'key', 'group', 'day']
    }
    if (role === 'agent') {
      if (!focus) return ['owner', 'day']
      return focus.id === me?.id ? ['cred', 'model', 'group', 'day'] : ['day']
    }
    return ['cred', 'model', 'group', 'day']
  }, [role, focus, me?.id])
  const activeDim = dim && dims.includes(dim) ? dim : dims[0]
  const { from, to } = rangeOf(range)

  const query = useQuery({
    queryKey: ['billing', from, to, activeDim, focus?.id ?? null],
    queryFn: () => getBilling({ from, to, by: activeDim, owner_id: focus?.id }),
    enabled: !!role,
    placeholderData: keepPreviousData,
    refetchInterval: 60_000,
  })
  const rows = query.data?.rows ?? []
  const total = query.data?.total
  const maxCost = Math.max(...rows.map((r) => r.cost_usd), 0)

  const dimLabel: Record<BillingDim, string> = {
    owner: t('按人', 'By person'),
    cred: t('按号', 'By account'),
    model: t('按模型', 'By model'),
    key: t('按接入 Key', 'By access key'),
    group: t('按分组', 'By group'),
    day: t('按日', 'By day'),
  }
  const columnLabel: Record<BillingDim, string> = {
    owner: t('人', 'Person'),
    cred: t('号', 'Account'),
    model: t('模型', 'Model'),
    key: t('接入 Key', 'Access key'),
    group: t('分组', 'Group'),
    day: t('日期', 'Date'),
  }
  const rangeLabel: Record<RangeKey, string> = {
    today: t('今天', 'Today'),
    '7d': t('近 7 天', 'Last 7 days'),
    '30d': t('近 30 天', 'Last 30 days'),
    month: t('本月', 'This month'),
    'last-month': t('上月', 'Last month'),
  }
  const dayFormatter = useMemo(
    () => new Intl.DateTimeFormat(locale, { month: '2-digit', day: '2-digit', weekday: 'short' }),
    [locale],
  )
  const nameOf = (row: BillingRow): string => {
    switch (activeDim) {
      case 'owner':
        return row.key === '0' ? t('未归属', 'Unassigned') : (row.label ?? t(`已删除的账号 #${row.key}`, `Deleted account #${row.key}`))
      case 'cred':
        return row.label ?? t(`已删除的号 #${row.key}`, `Deleted account #${row.key}`)
      case 'key':
        return row.key === '0'
          ? t('环境变量 / 未配置 Key', 'Environment / no key')
          : (row.label || t(`已删除的 Key #${row.key}`, `Deleted key #${row.key}`))
      case 'group':
        return row.key === '0' ? t('无分组', 'No group') : (row.label ?? t(`已删除的分组 #${row.key}`, `Deleted group #${row.key}`))
      case 'model':
        return row.key || t('未知模型', 'Unknown model')
      case 'day':
        return dayFormatter.format(new Date(Number(row.key) * 1000))
    }
  }
  // 按人那一表点一行下钻：管理员 / 访客谁都能点，代理能点自己与下属。
  const canDrill = activeDim === 'owner' && !focus
  const drill = (row: BillingRow) => {
    if (!canDrill || row.key === '0') return
    setFocus({ id: Number(row.key), name: nameOf(row) })
    setDim(null)
  }
  const number = (n: number) => n.toLocaleString(locale)

  return (
    <div className="app-shell flex min-h-dvh flex-col text-foreground">
      <AppHeader
        actions={<PreferencesMenu onSignOut={onSignOut} />}
        nav={<MainNav current="billing" onNavigate={onNavigate} />}
        onNavigateHome={() => onNavigate('pool')}
      />

      <main className="page-frame relative flex-1 py-5 pb-8 sm:py-8 sm:pb-12">
        <div className="space-y-5 sm:space-y-6">
          <section className="flex flex-wrap items-end justify-between gap-3">
            <div className="max-w-2xl">
              <h1 className="text-xl font-semibold tracking-tight sm:text-2xl">{t('费用', 'Billing')}</h1>
              <p className="mt-1.5 text-sm leading-6 text-muted-foreground max-sm:sr-only">
                {role === 'agent'
                  ? t('按官方 API 价格折算的等价费用。你和名下用户的费用汇总在这里；下属用户只显示到人。', 'Equivalent cost at official API prices for you and your users; your users are shown per person only.')
                  : role === 'user'
                    ? t('按官方 API 价格折算的等价费用，统计你名下的号。', 'Equivalent cost at official API prices for the accounts you added.')
                    : t('按官方 API 价格折算的等价费用。只计费，不扣费。', 'Equivalent cost at official API prices. Metered only; nothing is charged.')}
              </p>
            </div>
            <ToggleGroup
              aria-label={t('时间范围', 'Time range')}
              value={[range]}
              variant="outline"
              onValueChange={(values) => {
                const next = values[values.length - 1] as RangeKey | undefined
                if (next) setRange(next)
              }}
            >
              {(Object.keys(rangeLabel) as RangeKey[]).map((key) => (
                <ToggleGroupItem key={key} size="sm" value={key}>{rangeLabel[key]}</ToggleGroupItem>
              ))}
            </ToggleGroup>
          </section>

          <section className="grid grid-cols-2 gap-3 lg:grid-cols-4">
            {[
              { label: t('等价费用', 'Equivalent cost'), value: total ? formatUsd(total.cost_usd) : '—' },
              { label: t('请求数', 'Requests'), value: total ? number(total.requests) : '—' },
              {
                label: t('输入 / 输出 token', 'Input / output tokens'),
                value: total ? `${formatTokens(total.input_tokens)} / ${formatTokens(total.output_tokens)}` : '—',
              },
              {
                label: t('缓存写 / 缓存读 token', 'Cache write / read tokens'),
                value: total ? `${formatTokens(total.cache_write_tokens)} / ${formatTokens(total.cache_read_tokens)}` : '—',
              },
            ].map((tile) => (
              <div className="rounded-xl border bg-card px-4 py-3" key={tile.label}>
                <div className="text-xs text-muted-foreground">{tile.label}</div>
                <div className="mt-1 text-lg font-semibold tabular-nums">{tile.value}</div>
              </div>
            ))}
          </section>

          <section className="space-y-3">
            <div className="flex flex-wrap items-center gap-2">
              {focus && (
                <Badge className="gap-1.5" variant="secondary">
                  {t(`正在查看：${focus.name}`, `Viewing: ${focus.name}`)}
                  <button
                    aria-label={t('返回汇总', 'Back to summary')}
                    className="cursor-pointer rounded-sm opacity-72 hover:opacity-100"
                    type="button"
                    onClick={() => { setFocus(null); setDim(null) }}
                  >
                    <XIcon className="size-3.5" />
                  </button>
                </Badge>
              )}
              {dims.length > 1 && (
                <ToggleGroup
                  aria-label={t('拆分方式', 'Breakdown')}
                  value={[activeDim]}
                  variant="outline"
                  onValueChange={(values) => {
                    const next = values[values.length - 1] as BillingDim | undefined
                    if (next) setDim(next)
                  }}
                >
                  {dims.map((d) => (
                    <ToggleGroupItem key={d} size="sm" value={d}>{dimLabel[d]}</ToggleGroupItem>
                  ))}
                </ToggleGroup>
              )}
              {canDrill && rows.length > 0 && (
                <span className="text-xs text-muted-foreground">{t('点击某个人查看明细', 'Click a person for details')}</span>
              )}
            </div>

            {query.isPending ? (
              <div className="space-y-2">
                {Array.from({ length: 5 }, (_, i) => <Skeleton className="h-10 w-full" key={i} />)}
              </div>
            ) : query.isError ? (
              <div className="flex items-center gap-3 text-sm">
                <span className="text-destructive-foreground">{extractError(query.error, language)}</span>
                <Button size="sm" variant="outline" onClick={() => void query.refetch()}>{t('重试', 'Retry')}</Button>
              </div>
            ) : rows.length === 0 ? (
              <Empty>
                <EmptyHeader>
                  <EmptyMedia variant="icon"><ReceiptIcon /></EmptyMedia>
                  <EmptyTitle>{t('这段时间没有费用', 'No usage in this period')}</EmptyTitle>
                  <EmptyDescription>{t('换个时间范围看看。', 'Try another time range.')}</EmptyDescription>
                </EmptyHeader>
              </Empty>
            ) : (
              <div className="overflow-x-auto rounded-xl border">
                <Table>
                  <TableHeader>
                    <TableRow>
                      <TableHead>{columnLabel[activeDim]}</TableHead>
                      <TableHead className="text-right">{t('请求数', 'Requests')}</TableHead>
                      <TableHead className="text-right">{t('输入', 'Input')}</TableHead>
                      <TableHead className="text-right">{t('输出', 'Output')}</TableHead>
                      <TableHead className="text-right">{t('缓存写', 'Cache write')}</TableHead>
                      <TableHead className="text-right">{t('缓存读', 'Cache read')}</TableHead>
                      <TableHead className="min-w-40 text-right">{t('等价费用', 'Cost')}</TableHead>
                    </TableRow>
                  </TableHeader>
                  <TableBody>
                    {rows.map((row) => {
                      const drillable = canDrill && row.key !== '0'
                      return (
                        <TableRow
                          className={cn(drillable && 'cursor-pointer')}
                          key={row.key}
                          onClick={() => drill(row)}
                        >
                          <TableCell className="max-w-64 truncate font-medium">
                            {drillable ? (
                              <button className="cursor-pointer text-left underline-offset-4 hover:underline" type="button">
                                {nameOf(row)}
                              </button>
                            ) : nameOf(row)}
                          </TableCell>
                          <TableCell className="text-right tabular-nums">{number(row.requests)}</TableCell>
                          <TableCell className="text-right tabular-nums">{formatTokens(row.input_tokens)}</TableCell>
                          <TableCell className="text-right tabular-nums">{formatTokens(row.output_tokens)}</TableCell>
                          <TableCell className="text-right tabular-nums">{formatTokens(row.cache_write_tokens)}</TableCell>
                          <TableCell className="text-right tabular-nums">{formatTokens(row.cache_read_tokens)}</TableCell>
                          <TableCell className="text-right">
                            <div className="flex items-center justify-end gap-2">
                              <div aria-hidden="true" className="hidden h-1.5 w-20 overflow-hidden rounded-full bg-muted sm:block">
                                <div
                                  className="h-full rounded-full bg-primary/72"
                                  style={{ width: `${maxCost > 0 ? (row.cost_usd / maxCost) * 100 : 0}%` }}
                                />
                              </div>
                              <span className="font-medium tabular-nums">{formatUsd(row.cost_usd)}</span>
                            </div>
                          </TableCell>
                        </TableRow>
                      )
                    })}
                  </TableBody>
                </Table>
              </div>
            )}
          </section>
        </div>
      </main>
      <AppFooter />
    </div>
  )
}
