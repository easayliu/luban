import { CopyIcon } from 'lucide-react'
import { useI18n } from '@/lib/i18n'
import { cn, copyText } from '@/lib/utils'
import { type BadgeProps } from '@/components/ui/badge'
import { toastManager } from '@/components/ui/toast'

/**
 * 流水表格与请求查询共用的两件小东西。
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
 * 请求 id：默认只显示尾部 8 位（来访沿用的 id 可能长达 128 位，表格里放不下），完整值在 title
 * 里。`full` 时整串显示（查询结果页有的是横向空间）。旧记录没有 id 时显示占位。
 *
 * 给了 `onOpen` 就拆成两颗按钮：id 本身点开这条请求的查询弹窗，后面那枚图标仍是复制——
 * 一个格子两种动作，都得是真按钮（键盘逐个 Tab 得到，读屏各念各的）。没给 `onOpen` 时整块
 * 就是复制按钮，与原来一样。
 *
 * 触屏上（`pointer-coarse`）两颗按钮都用「内边距撑大、负外边距抵回」放大命中区，布局一像素不动：
 * id 那颗是 `truncate`（overflow hidden），撑热区的 `::after` 会被自己裁掉，只能走内边距；复制
 * 那枚图标只有 12px，向上下与右侧各撑 12px，左侧只撑 4px——正好是两颗之间的间隙，不压到 id 上。
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
  const copy = async () => {
    const ok = await copyText(id)
    toastManager.add({
      title: ok ? t('已复制请求 ID', 'Request ID copied') : t('复制失败', 'Copy failed'),
      description: ok ? id : undefined,
      type: ok ? 'success' : 'error',
    })
  }
  if (!onOpen) {
    return (
      <button
        type="button"
        className="inline-flex max-w-full items-center gap-1 rounded font-mono text-xs hover:text-foreground hover:underline [overflow-wrap:anywhere] pointer-coarse:-my-3 pointer-coarse:py-3"
        title={`${id}\n${t('点击复制', 'Click to copy')}`}
        onClick={copy}
      >
        <span className={full ? '' : 'truncate'}>{shown}</span>
        <CopyIcon aria-hidden className="size-3 shrink-0 text-muted-foreground" />
      </button>
    )
  }
  return (
    <span className="inline-flex max-w-full items-center gap-1">
      <button
        type="button"
        className={cn(
          'min-w-0 rounded font-mono text-xs hover:text-foreground hover:underline pointer-coarse:-my-3 pointer-coarse:py-3',
          full ? '[overflow-wrap:anywhere] text-left' : 'truncate',
        )}
        title={`${id}\n${t('点击查看这条请求', 'Click to look up this request')}`}
        aria-label={t(`查看请求 ${id}`, `Look up request ${id}`)}
        aria-haspopup="dialog"
        onClick={() => onOpen(id)}
      >
        {shown}
      </button>
      <button
        type="button"
        className="shrink-0 rounded text-muted-foreground hover:text-foreground pointer-coarse:-my-3 pointer-coarse:-mr-3 pointer-coarse:-ml-1 pointer-coarse:py-3 pointer-coarse:pr-3 pointer-coarse:pl-1"
        title={`${id}\n${t('点击复制', 'Click to copy')}`}
        aria-label={t(`复制请求 ID ${id}`, `Copy request ID ${id}`)}
        onClick={copy}
      >
        <CopyIcon aria-hidden className="size-3" />
      </button>
    </span>
  )
}
