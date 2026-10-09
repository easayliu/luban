import { useMemo } from 'react'
import { useI18n } from '@/lib/i18n'
import { Button } from '@/components/ui/button'
import {
  Pagination,
  PaginationContent,
  PaginationItem,
  PaginationNext,
  PaginationPrevious,
} from '@/components/ui/pagination'
import { Select, SelectItem, SelectPopup, SelectTrigger, SelectValue } from '@/components/ui/select'

/**
 * 列表底部的分页条：左计数、中翻页、右每页。账号池、用量明细、封禁记录原来各抄一份，
 * 每页选择框是 sm、翻页按钮却是默认高度，同一行高矮不齐；这里统一成 sm 一档。
 *
 * 任何宽度都排成一行：窄屏时计数缩成「1–10 / 29」、页码缩成「1 / 3」、藏掉「每页」二字，
 * 翻页在手机上是两个方形图标按钮，腾出宽度让三栏并排——原先翻页落到第二行，与左右两栏对不上。
 * 只有一页时不出翻页，只留计数和每页。
 *
 * `page` 从 1 起算。`pageRowCount` 给了就按本页实际行数算区间末尾（服务端总数可能比本页
 * 落后一拍），不给就按 `page × pageSize` 与总数取小。
 */
export function PaginationBar<S extends number>({
  total,
  page,
  pageCount,
  pageSize,
  pageSizes,
  pageRowCount,
  onPageChange,
  onPageSizeChange,
  unit = 'row',
  pageSizeLabel,
  disabled = false,
  className,
}: {
  total: number
  page: number
  pageCount: number
  pageSize: S
  pageSizes: readonly S[]
  pageRowCount?: number
  onPageChange: (page: number) => void
  onPageSizeChange: (pageSize: S) => void
  /** 计数的量词：`row` 是「条」，`account` 是「个」。 */
  unit?: 'row' | 'account'
  /** 每页选择框的读屏名称。 */
  pageSizeLabel: string
  /** 翻页按钮额外禁用（比如正在取数时）。 */
  disabled?: boolean
  className?: string
}) {
  const { locale, t } = useI18n()
  const numberFormatter = useMemo(() => new Intl.NumberFormat(locale), [locale])
  const format = (value: number) => numberFormatter.format(value)
  const pageSizeItems = useMemo(
    () => pageSizes.map((size) => ({ value: String(size), label: numberFormatter.format(size) })),
    [pageSizes, numberFormatter],
  )
  const from = (page - 1) * pageSize + 1
  const to = pageRowCount === undefined ? Math.min(page * pageSize, total) : (page - 1) * pageSize + pageRowCount
  const unitZh = unit === 'account' ? '个' : '条'
  const prevDisabled = disabled || page <= 1
  const nextDisabled = disabled || page >= pageCount

  return (
    <div className={`grid grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)] items-center gap-2 sm:gap-3 text-xs ${className ?? ''}`}>
      <p className="min-w-0 text-muted-foreground tabular-nums">
        <span className="max-sm:hidden">
          {t(
            `第 ${format(from)}–${format(to)} ${unitZh}，共 ${format(total)} ${unitZh}`,
            `${format(from)}–${format(to)} of ${format(total)}`,
          )}
        </span>
        <span className="sm:hidden">{`${format(from)}–${format(to)} / ${format(total)}`}</span>
      </p>
      <div className="col-start-3 row-start-1 flex items-center gap-2 justify-self-end">
        <span className="whitespace-nowrap text-muted-foreground max-sm:hidden">{t('每页', 'Per page')}</span>
        <Select
          items={pageSizeItems}
          value={String(pageSize)}
          onValueChange={(value) => {
            const next = pageSizes.find((size) => String(size) === value)
            if (next !== undefined) onPageSizeChange(next)
          }}
        >
          <SelectTrigger size="sm" className="w-auto min-w-16 sm:min-w-20" aria-label={pageSizeLabel}>
            <SelectValue />
          </SelectTrigger>
          {/* 挨着翻页按钮的一行控件，弹层往下展开，别盖住触发器和旁边的按钮。 */}
          <SelectPopup alignItemWithTrigger={false}>
            {pageSizeItems.map((item) => (
              <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>
            ))}
          </SelectPopup>
        </Select>
      </div>
      {pageCount > 1 && (
        <Pagination className="col-start-2 row-start-1 justify-center">
          <PaginationContent>
            <PaginationItem>
              <PaginationPrevious
                render={<Button variant="ghost" size="sm" disabled={prevDisabled} />}
                aria-disabled={prevDisabled}
                onClick={() => onPageChange(Math.max(1, page - 1))}
              />
            </PaginationItem>
            <PaginationItem>
              <span className="whitespace-nowrap px-2 text-xs text-foreground tabular-nums" aria-live="polite">
                <span className="max-sm:hidden">
                  {t(`第 ${format(page)} / ${format(pageCount)} 页`, `Page ${format(page)} of ${format(pageCount)}`)}
                </span>
                <span className="sm:hidden">{`${format(page)} / ${format(pageCount)}`}</span>
              </span>
            </PaginationItem>
            <PaginationItem>
              <PaginationNext
                render={<Button variant="ghost" size="sm" disabled={nextDisabled} />}
                aria-disabled={nextDisabled}
                onClick={() => onPageChange(Math.min(pageCount, page + 1))}
              />
            </PaginationItem>
          </PaginationContent>
        </Pagination>
      )}
    </div>
  )
}
