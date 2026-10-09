import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'
import { CopyButton } from '@/components/copy-button'
import { type BadgeProps } from '@/components/ui/badge'
import { Hint } from '@/components/ui/tooltip'

/**
 * 流水表格与请求查询共用的几件小东西。
 *
 * 单独成文件是为了断开引用环：请求明细（credential-usage-dialog）要能点开请求查询
 * （request-lookup-dialog），而请求查询的结果里又要画请求 id 徽标——两边直接互相 import
 * 就成了环。放在这个谁也不依赖的模块里，两边各取所需。
 */

/** 状态码 → 徽章配色。2xx 成功、429 单独一档（额度问题，不是错误），其余 4xx/5xx 红。 */
export function statusVariant(status: number): BadgeProps['variant'] {
  if (status >= 200 && status < 300) return 'success'
  if (status === 429) return 'warning'
  if (status >= 400) return 'error'
  return 'secondary'
}

/**
 * 对话框里的紧凑统计表（趋势对话框的分维度明细、账号详情的用量表）：表头 28px、单元格上下
 * 6px，比列表页 Table 的默认密度低一档。表头吸顶，边框写在 th 上——border-collapse 下 tr 的
 * 边框吸顶后会留在原处。z-10 盖住行：TableRow 是 `relative`，不抬一层的话滚过去的行会压在表头上。
 *
 * 滚动容器要当 Table 的外壳传进 `render`：Table 自带一层 `overflow-x-auto` 的 div，外面再套
 * 一层滚动 div 的话，sticky 表头会粘在里面那层上，竖着滚时跟着一起滚走。
 */
export const COMPACT_TABLE_HEADER_CLASS = 'sticky top-0 z-10 bg-surface-subtle [&_th]:h-7 [&_th]:border-b [&_th]:px-3'
export const COMPACT_TABLE_BODY_CLASS = '[&_td]:px-3 [&_td]:py-1.5 [&_td]:leading-4'
/** 紧凑表里的下钻名称：链接样式的按钮，去掉按钮的高度、内边距与边框，与旁边的纯文本行高一致。 */
export const COMPACT_TABLE_LINK_CLASS =
  'block h-auto min-w-0 max-w-full truncate rounded-sm border-0 p-0 text-start font-normal text-xs sm:h-auto'

/** 复制按钮缩成与 `text-xs` 正文同高：图标 12px、不要按钮的方块尺寸与内边距。触屏热区靠 Button 自带的 `::after` 撑大。 */
const INLINE_COPY_CLASS =
  "h-auto gap-1 rounded border-0 p-0 font-mono font-normal text-xs sm:h-auto sm:text-xs [&_svg:not([class*='size-'])]:size-3 sm:[&_svg:not([class*='size-'])]:size-3"

/**
 * 请求 id：默认只显示尾部 8 位（来访沿用的 id 可能长达 128 位，表格里放不下），完整值在悬浮
 * 提示里（触屏长按）。`full` 时整串显示（查询结果页有的是横向空间）。旧记录没有 id 时显示占位。
 *
 * 给了 `onOpen` 就拆成两颗按钮：id 本身点开这条请求的查询弹窗，后面那枚图标仍是复制——
 * 一个格子两种动作，都得是真按钮（键盘逐个 Tab 得到，读屏各念各的）。没给 `onOpen` 时整块
 * 就是复制按钮。复制走公共的 [CopyButton]：成功时图标换对勾、悬浮提示换「已复制」，失败才弹提示。
 *
 * 触屏上（`pointer-coarse`）id 那颗用「内边距撑大、负外边距抵回」放大命中区，布局一像素不动：
 * 它是 `truncate`（overflow hidden），撑热区的 `::after` 会被自己裁掉，只能走内边距。复制按钮
 * 是 Button，热区由它自带的 `::after` 撑到 44px。
 */
export function RequestIdChip({
  id,
  full = false,
  onOpen,
}: {
  id: string | null
  full?: boolean
  /** 点 id 时带着它去查这条请求；不传则 id 本身也是复制。 */
  onOpen?: (id: string) => void
}) {
  const { t } = useI18n()
  if (!id) return <span className="text-muted-foreground">—</span>
  const shown = full || id.length <= 12 ? id : `…${id.slice(-8)}`
  if (!onOpen) {
    return (
      <CopyButton
        text={id}
        label={`${id}\n${t('点击复制', 'Click to copy')}`}
        variant="link"
        className={cn(INLINE_COPY_CLASS, 'max-w-full', full && 'whitespace-normal text-left [overflow-wrap:anywhere]')}
      >
        <span className={full ? '' : 'truncate'}>{shown}</span>
      </CopyButton>
    )
  }
  return (
    <span className="inline-flex max-w-full items-center gap-1">
      <Hint label={`${id}\n${t('点击查看这条请求', 'Click to look up this request')}`}>
        <button
          type="button"
          className={cn(
            'min-w-0 rounded font-mono text-xs hover:text-foreground hover:underline pointer-coarse:-my-3 pointer-coarse:py-3',
            full ? '[overflow-wrap:anywhere] text-left' : 'truncate',
          )}
          aria-label={t(`查看请求 ${id}`, `Look up request ${id}`)}
          aria-haspopup="dialog"
          onClick={() => onOpen(id)}
        >
          {shown}
        </button>
      </Hint>
      <CopyButton
        text={id}
        label={t(`复制请求 ID ${id}`, `Copy request ID ${id}`)}
        copiedLabel={t('已复制请求 ID', 'Request ID copied')}
        className={cn(INLINE_COPY_CLASS, 'size-auto shrink-0 text-muted-foreground hover:bg-transparent hover:text-foreground sm:size-auto')}
      />
    </span>
  )
}
