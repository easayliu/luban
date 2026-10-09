import type { ReactNode } from 'react'
import { Table, TableCaption } from '@/components/ui/table'

/**
 * 趋势弹窗里「表格」视图的外壳：限高滚动、表头吸顶、紧凑行距。
 *
 * 缓存命中与首字时延两张明细表原来是裸 `table`，表头那串 class 逐字抄两遍。这里落到
 * `Table` 上，紧凑那一档用后代选择器一次压下来（行高 28px、`text-xs`），调用方只写
 * `TableHead` / `TableCell` 和对齐方式。
 *
 * - 滚动容器就是 `Table` 自己的容器（经 `render` 换掉）：若再套一层 `overflow-auto`，
 *   里层那个 `overflow-x-auto` 也是滚动容器，吸顶的表头会粘在它身上而不是外层，滚动时跟着跑掉。
 * - 表头不转大写、不加字距：列名里有 `p50` / `p95`，转成 `P50` 就不是惯用写法了。
 * - 分隔线画在 `th` 上而不是 `tr` 上：合并边框模式下，`tr` 的边框不跟着吸顶的表头走。
 * - 六列在手机宽度上放不下，横向滚动而不是把表撑出对话框。
 */
export function ChartTable({
  caption,
  children,
}: {
  /** 只给读屏的表格标题。 */
  caption?: string
  children: ReactNode
}) {
  return (
    <Table
      className="text-xs [&_td]:px-3 [&_td]:py-1.5 [&_td]:leading-4 [&_td]:tabular-nums [&_th]:h-7 [&_th]:border-b [&_th]:px-3 [&_th]:font-medium [&_th]:normal-case [&_th]:tracking-normal [&_thead]:sticky [&_thead]:top-0 [&_thead]:z-1 [&_thead]:bg-surface-subtle [&_thead_tr]:border-b-0 [&_thead_tr]:hover:bg-transparent"
      render={<div className="max-h-64 overflow-auto rounded-xl border" />}
    >
      {caption && <TableCaption className="sr-only">{caption}</TableCaption>}
      {children}
    </Table>
  )
}
