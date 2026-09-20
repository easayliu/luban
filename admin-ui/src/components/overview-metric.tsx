import type { ElementType, ReactNode } from 'react'
import { ArrowUpRightIcon } from 'lucide-react'
import { cn } from '@/lib/utils'
import { Skeleton } from '@/components/ui/skeleton'
import { Tooltip, TooltipPopup, TooltipTrigger } from '@/components/ui/tooltip'

export function OverviewMetric({
  label, value, status, statusHint, trend, icon: Icon, tone, active = false, opensDetail = false, onClick, className,
}: {
  label: string
  value: number | string
  status?: string
  statusHint?: string
  trend?: ReactNode
  icon: ElementType<{ className?: string }>
  tone: 'ok' | 'bad' | 'warn' | 'neutral'
  active?: boolean
  /**
   * 这一格点开的是详情弹窗，而不是筛选列表。
   *
   * 六格长得一模一样，点下去却分三种结果：筛选列表、开趋势弹窗、什么都不做。带角标的那几枚
   * 是「点开看详情」，选中态（左侧竖条 + 淡底）的那几枚是「正在按它筛选」，剩下没反应的那枚
   * 两样都没有——不用点一遍也能分出来。
   */
  opensDetail?: boolean
  onClick?: () => void
  className?: string
}) {
  /**
   * 告警色上在**数值**上，图标只在真出事时才跟着变色。
   *
   * 原来反过来：色调只上在左边那枚图标上，`需处理 2` 里的 2 还是普通前景色。可这一排格子里眼睛
   * 先落到的就是那个大数字，图标是最后才看的——真有 2 个号要处理时，最醒目的位置反而最安静。
   *
   * 常态（ok / neutral）一律不着色：概览是用来找异常的，「一切正常」不需要拿颜色来说，否则
   * 六格里四格都带色，真正该跳出来的那一格就淹了。这也是 5h / 7d 计量条改成常态 marine 的同一条理由。
   */
  const iconClass = {
    ok: 'text-muted-foreground',
    bad: 'text-destructive-foreground',
    warn: 'text-warning-foreground',
    neutral: 'text-muted-foreground',
  }[tone]
  const valueClass = {
    ok: 'text-foreground',
    bad: 'text-destructive-foreground',
    warn: 'text-warning-foreground',
    neutral: 'text-foreground',
  }[tone]
  const content = (
    <div className="flex min-h-16 items-center gap-3 px-4 py-2.5 sm:px-5" title={statusHint}>
      {/* 图标方块（32px + 12px 间距）在手机上要走 44px 横向，而一格只有半个屏宽：正文被压到
          约 103px，`99.7%` 加上 80px 的迷你线要 140px，于是迷你线要么换行把整格撑高、要么被压成
          一条虚线。手机上改成把同一枚图标挂到标签前（14px），正文回到 147px，数值（约 28px）
          加迷你线（80px）与中间 8px 的间距共 116px，一行装得下。
          （375px 屏、两列、每格 171px，扣掉 `px-4` 两侧共 32px 得 139px 正文——上面按 `gap-3`
          的图标形态算是 103px、按标签内联形态算是 147px。）
          sm 起格子宽裕，方块照旧——它是这排概览的视觉锚点。色调仍由图标颜色承担，两种形态共用。 */}
      <span className="hidden size-8 shrink-0 items-center justify-center rounded-lg bg-muted sm:flex">
        <Icon className={cn('size-4', iconClass)} aria-hidden />
      </span>
      <div className="min-w-0 flex-1">
        <p className="flex min-w-0 items-center gap-1.5 text-xs font-medium text-muted-foreground">
          <Icon className={cn('size-3.5 shrink-0 sm:hidden', iconClass)} aria-hidden />
          <span className="min-w-0 truncate">{label}</span>
          {opensDetail && <ArrowUpRightIcon className="size-3 shrink-0 opacity-64" aria-hidden />}
        </p>
        {/* 一行排数值、小字、迷你线，但这两样不会同时出现：带迷你线的两格不再放小字（三样在
            任何宽度下都挤不开，见 credential-workspace 那两处注释）。迷你线放最后、按剩余宽度
            伸缩，手机上一格不到 190px 也缩得进来。
            小字是 12px 而不是 11px：手机半格正文 139px，最长的「2 暂不可用」在 12px 下约 58px，
            与 18px 的数值（约 28px）并排共 94px，仍有富余；11px 只是把字压小，并没有换来位置。 */}
        <div className="mt-1 flex min-w-0 items-baseline gap-2">
          <span className={cn('shrink-0 text-lg font-semibold leading-none tracking-tight tnum', valueClass)}>
            {value}
          </span>
          {/* 状态小字（「2 暂不可用」「2 封禁」）手机上也显示。v0.3.142 曾把它设成 sr-only，
              理由是「一格半屏宽再塞一句解释会把数值挤扁」；重算过是装得下的：375px 屏两列、
              一格 171px，扣掉 `px-4` 得 139px 正文，数值（约 28px）+ 间距 8px + 最长的
              「2 暂不可用」（12px 下约 58px）= 94px。
              真放不下时由 `min-w-0 shrink truncate` 截尾，不会把数值挤扁——数值是 `shrink-0`，
              被截的永远是这句小字；完整说法仍在整格的悬浮提示与点开的筛选结果里。
              带迷你趋势线的两格本来就不传 status，三样挤一行的问题不存在。 */}
          {status && (
            <Tooltip>
              <TooltipTrigger className="min-w-0 shrink truncate text-xs text-muted-foreground">
                {status}
              </TooltipTrigger>
              <TooltipPopup>{status}</TooltipPopup>
            </Tooltip>
          )}
          {trend}
        </div>
      </div>
    </div>
  )

  const rootClass = cn(
    'min-w-0 text-left transition-colors',
    onClick && 'cursor-pointer hover:bg-muted/40 focus-visible:relative focus-visible:z-10 focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-inset focus-visible:ring-ring',
    // 选中不只是淡底：再压一条 2px 的左侧竖条。淡底在深色主题下几乎看不出来，而这三格是
    // 列表当前的筛选条件，看不出选没选就会对着一份被筛过的列表发愣。
    active && 'bg-marine/10 shadow-[inset_2px_0_0_0_var(--marine)] hover:bg-marine/14',
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

/**
 * 概览里的实时格：和 [OverviewMetric] 同一套图标 / 字号 / 排布，区别只在数值后面跟着单位与在途数。
 *
 * 它曾经在手机上占满一整行，于是排成「标签贴左、数值贴右」的横条把整行用满；现在六格在手机上一律
 * 两列（见 credential-workspace 的概览网格），它也只有半格宽，横条那套就没有意义了——退回竖排，
 * 与左右邻居的标签、数值落在同一条基线上。
 */
export function LiveTrafficMetric({
  label, value, unit, detail, live, hint, icon: Icon, className,
}: {
  label: string
  value: number | string
  unit: string
  detail: string
  /** 有在途请求：图标转成成功色并点亮呼吸点，静默时保持中性，避免恒亮的绿色变成背景噪声。 */
  live: boolean
  hint: string
  icon: ElementType<{ className?: string }>
  className?: string
}) {
  return (
    <Tooltip>
      <TooltipTrigger
        render={<div />}
        className={cn(
          'flex min-h-16 min-w-0 cursor-help items-center gap-3 px-4 py-2.5 sm:px-5 text-left',
          className,
        )}
      >
        {/* 图标的两种形态与 [OverviewMetric] 完全一致，理由见那边的注。 */}
        <span className="hidden size-8 shrink-0 items-center justify-center rounded-lg bg-muted sm:flex">
          <Icon className={cn('size-4', live ? 'text-success-foreground' : 'text-muted-foreground')} aria-hidden />
        </span>
        <div className="min-w-0 flex-1">
          <p className="flex min-w-0 items-center gap-1.5 text-xs font-medium text-muted-foreground">
            <Icon
              className={cn('size-3.5 shrink-0 sm:hidden', live ? 'text-success-foreground' : 'text-muted-foreground')}
              aria-hidden
            />
            <span className="min-w-0 truncate">{label}</span>
          </p>
          <div className="mt-1 flex min-w-0 items-baseline gap-1.5">
            <span className="shrink-0 text-lg font-semibold leading-none tracking-tight tnum">
              {value}
            </span>
            <span className="shrink-0 text-xs text-muted-foreground tracking-wide">{unit}</span>
            {/* 在途数手机上也显示。v0.3.142 曾按「与状态小字同一处理」把它设成 sr-only，
                只留呼吸点；但算下来它装得下：375px 屏两列、一格 171px，扣掉 `px-4` 得 139px
                正文，而「0 RPM · 0 在途」约 99px。数大时靠 `min-w-0 truncate` 收尾，
                不会把格子撑开。整格的悬浮提示里仍有完整说法。
                静默时补一个中点当分隔；有在途时呼吸点自己就是分隔符，再补中点只是噪声。 */}
            <span className="flex min-w-0 items-baseline gap-1.5 text-xs text-muted-foreground">
              {live ? (
                <span className="relative flex size-1.5 shrink-0 translate-y-[-1px]" aria-hidden>
                  <span className="absolute inline-flex size-full animate-ping rounded-full bg-success opacity-60 motion-reduce:hidden" />
                  <span className="relative inline-flex size-1.5 rounded-full bg-success" />
                </span>
              ) : (
                <span aria-hidden>·</span>
              )}
              <span className="min-w-0 truncate">{detail}</span>
            </span>
          </div>
        </div>
      </TooltipTrigger>
      <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">{hint}</TooltipPopup>
    </Tooltip>
  )
}

export function OverviewMetricSkeleton({ className }: { className?: string }) {
  return (
    <div className={cn('flex min-h-16 min-w-0 items-center gap-3 px-4 py-2.5 sm:px-5', className)}>
      <Skeleton className="hidden size-8 shrink-0 rounded-lg sm:block" />
      <div className="min-w-0 flex-1">
        <Skeleton className="h-3 w-20" />
        <div className="mt-1.5 flex items-center gap-2">
          <Skeleton className="h-5 w-10 shrink-0" />
          <Skeleton className="h-3 w-20 max-w-full" />
        </div>
      </div>
    </div>
  )
}
