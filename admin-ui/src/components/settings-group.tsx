import { useState, type ReactNode } from 'react'
import type { LucideIcon } from 'lucide-react'
import { ChevronDownIcon } from 'lucide-react'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import {
  Frame,
  FrameDescription,
  FrameHeader,
  FramePanel,
  FrameTitle,
} from '@/components/ui/frame'

/**
 * 超过这个长度的说明默认收起到两行。
 *
 * 现有分组的说明多在 20~100 字，只有「从上游学到的规则」那条 370 余字会撑掉大半屏；
 * 按长度自动判定，短说明保持原样，日后再写长的也不必逐处改调用。
 */
const CLAMP_THRESHOLD = 140

/**
 * 手机上再低一档：60 字以上就收到两行。375px 上说明一行只放得下二十几个字，60–140 字的说明
 * 要摞四五行（设备策略页那几条「模拟会话…」都是），而桌面上它们一两行就完了，桌面照旧铺开。
 */
const MOBILE_CLAMP_THRESHOLD = 60

/**
 * 长说明的统一处理：收起到两行，末尾挂一枚「了解更多」。分组标题下的说明与**每一行参数**
 * 的说明共用这一套——同一页里不该有两种「文案太长怎么办」。
 *
 * 短说明（不到 [CLAMP_THRESHOLD]）原样铺开，不挂按钮：那一枚按钮本身也是噪声，只在真需要时出现。
 *
 * 渲染成 `span` + `button` 而不是 `p`：调用方多半已经把它放在 `FieldDescription` 里了，
 * 那是一个 `p`，里面再套 `p` 是非法嵌套；这两个都是短语内容，放进去是合法的。
 */
export function ClampedDescription({ text, className }: { text: string; className?: string }) {
  const { t } = useI18n()
  const [open, setOpen] = useState(false)
  const clampable = text.length > CLAMP_THRESHOLD
  // 只在窄屏收起的那一档：收起用 `max-sm:line-clamp-2`、按钮 `sm:hidden`，桌面上既不截也不挂按钮。
  const mobileOnly = !clampable && text.length > MOBILE_CLAMP_THRESHOLD

  return (
    <>
      <span
        className={cn(
          'block',
          className,
          !open && (clampable ? 'line-clamp-2' : mobileOnly && 'max-sm:line-clamp-2'),
        )}
      >
        {text}
      </span>
      {(clampable || mobileOnly) && (
        <button
          aria-expanded={open}
          // 不写死字号：跟着外面那段说明走（卡片描述 text-sm、行描述 text-xs），
          // 否则卡片头里这枚按钮会比它跟着的那段说明小一号。
          className={cn(
            'mt-1 inline-flex items-center gap-0.5 text-foreground/70 underline-offset-4 transition-colors hover:text-foreground hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-1 focus-visible:ring-offset-background',
            mobileOnly && 'sm:hidden',
          )}
          type="button"
          onClick={() => setOpen((v) => !v)}
        >
          {open ? t('收起', 'Show less') : t('了解更多', 'Learn more')}
          <ChevronDownIcon
            aria-hidden="true"
            className={cn('size-3 transition-transform', open && 'rotate-180')}
          />
        </button>
      )}
    </>
  )
}

/**
 * 设置页里「一行参数」的统一骨架：说明在左、控件在右。
 *
 * 之前这一行的写法有两套、各抄十几遍——access 与迁移页是
 * `grid gap-4 p-5 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-center sm:gap-x-6`（14 处逐字重复），
 * 转发页是 `p-5` + `flex w-full items-start justify-between gap-4`，还有一处干脆用裸 `div` + 裸 `p`
 * 手搓。抄出来的差异全落在对齐上：同一页里控件有 `items-center`、`items-start`、`items-end` 三种基线，
 * 其中 `items-center` 那档最难看——左边是「标签 + 两行说明 + 徽章」近 90px、右边是 36px 的输入框，
 * 居中之后输入框浮在半空，和它自己的标签差了一大截。
 *
 * 这里只认一条规矩：**控件与标签首行对齐**（`items-start`），说明写多长都不挪控件。
 *
 * 各槽位：
 * - `badge` 跟在标签右边（状态类，如「严格模式」）；
 * - `note` 落在说明下面（读数类，如「闲置 2 小时后释放名额」）；
 * - `children` 是右边那一格的控件，窄屏整块落到第二行并撑满；
 * - `footer` 是整行底下的全宽附加块（如「影响与限制」那个折叠）；
 * - `inlineControl`：控件本身很小（开关）时传它，窄屏上也不换行，贴在标题右侧。
 *
 * 窄屏默认把控件整块换到说明下面并撑满——输入框、分段选择、保存按钮这样才有地方放。但开关
 * 照这个规矩会**独占一行**：标题、说明、开关、「影响与限制」四段摞起来，转发页三十多个开关
 * 每个多占 40px，而且看标题时看不到它开没开。手机设置页的通用做法是开关跟标题同一行、靠右。
 */
export function SettingsRow({
  label,
  htmlFor,
  badge,
  description,
  note,
  children,
  footer,
  className,
  disabled,
  inlineControl = false,
}: {
  label: ReactNode
  htmlFor?: string
  badge?: ReactNode
  description?: ReactNode
  note?: ReactNode
  children: ReactNode
  footer?: ReactNode
  className?: string
  disabled?: boolean
  inlineControl?: boolean
}) {
  // 槽宽分两档：手机 16px、≥640px 20px。`page-frame` 在手机上已占去两侧各 16px，
  // 卡片再加 20px 正文就只剩 303px；收一档回来给内容。
  return (
    <Field className={cn('p-4 sm:p-5', className)} disabled={disabled}>
      <div
        className={cn(
          'flex w-full items-start justify-between gap-x-6 gap-y-3',
          inlineControl ? 'max-sm:gap-x-4' : 'flex-wrap',
        )}
      >
        <div className={cn('min-w-0 flex-1 space-y-1.5', !inlineControl && 'basis-72')}>
          <div className="flex flex-wrap items-center gap-2">
            <FieldLabel htmlFor={htmlFor}>{label}</FieldLabel>
            {badge}
          </div>
          {/* 说明统一封在 max-w-xl：内容列 56rem 的一行放得下 800px 出头的正文，
              一行 80 个中文字读不动，这是设置页说明该有的行长。 */}
          {/* 纯文字说明走 ClampedDescription（手机上 60 字起收两行）；带 footer 的行（转发页那些
              挂着「影响与限制」的开关）不再套第二个展开器，原样铺开，理由见 ForwardingToggle。 */}
          {description && (
            <FieldDescription className="max-w-xl leading-5">
              {typeof description === 'string' && !footer ? <ClampedDescription text={description} /> : description}
            </FieldDescription>
          )}
          {note}
        </div>
        {/* 窄屏 `max-sm:w-full`：控件整块换行并撑满，与原来 `sm:grid-cols-…` 那套的手机形态一致。 */}
        <div className={cn('flex shrink-0 items-center gap-2', !inlineControl && 'max-sm:w-full')}>{children}</div>
      </div>
      {footer}
    </Field>
  )
}

/**
 * 系统设置页的统一分组容器。
 *
 * Frame 是仓库中来自 Coss UI 的设置面板底座；这里仅补齐标题语义、图标和说明，
 * 避免接入、设备与转发页面各自再造一套卡片样式。
 */
export function SettingsGroup({
  icon: Icon,
  title,
  description,
  children,
}: {
  icon?: LucideIcon
  title: string
  description?: string
  children: ReactNode
}) {
  return (
    <Frame>
      <FrameHeader className="gap-1.5">
        <div className="flex items-center gap-2">
          {Icon && <Icon aria-hidden="true" className="size-4 text-muted-foreground" />}
          {/* 比行标签（FieldLabel，text-sm/medium）大一档：底座给的 text-sm + semibold 与行标签
              只差一个字重，一张卡扫下来分不出「组」和「项」。这里是 16 / 14 / 12 三档的中间那档。 */}
          <FrameTitle className="text-base">
            <h3>{title}</h3>
          </FrameTitle>
        </div>
        {/* 卡片描述 text-sm、行描述 text-xs：同为 text-xs 时卡片头读起来就是又一行普通说明。
            与标题对齐（让过图标那 24px）只在 sm 起做：窄屏上这 24px 会把本来就要换行的说明
            再多挤出一行，还带出一道悬挂缩进；手机上让它从卡片槽宽起排。 */}
        {description && (
          <FrameDescription className={cn('text-sm leading-5', Icon && 'sm:pl-6')}>
            <ClampedDescription text={description} />
          </FrameDescription>
        )}
      </FrameHeader>
      <FramePanel className="divide-y p-0">{children}</FramePanel>
    </Frame>
  )
}
