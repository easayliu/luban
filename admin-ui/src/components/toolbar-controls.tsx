import { useEffect, useMemo, useRef, useState, type ElementType, type ReactNode, type RefObject } from 'react'
import { CheckIcon, SearchIcon, SlidersHorizontalIcon, XIcon } from 'lucide-react'
import { useI18n } from '@/lib/i18n'
import { cn } from '@/lib/utils'
import { Button, buttonVariants } from '@/components/ui/button'
import { InputGroup, InputGroupAddon, InputGroupInput } from '@/components/ui/input-group'
import { Kbd } from '@/components/ui/kbd'
import {
  Menu,
  MenuGroup,
  MenuPopup,
  MenuRadioGroup,
  MenuRadioItem,
  MenuSeparator,
  MenuTrigger,
} from '@/components/ui/menu'
import { Popover, PopoverPopup, PopoverTrigger } from '@/components/ui/popover'

/**
 * 列表页工具条的两件公共控件：搜索框与菜单式下拉。账号池、费用等列表页都用这两件，不各写
 * 一份——各写一份时出过两次同样的问题：搜索框与下拉高度对不齐（一个默认档、一个小档），
 * 下拉弹层盖住触发器（基础 Select 默认把弹层对齐到选中项上）。两件都用默认档高度，下拉统一
 * 走 Menu：弹层在触发器下方、右对齐展开。
 */

/** 筛选生效时触发器的染色：一眼能看出哪个按钮正在缩小列表。 */
export const ACTIVE_FILTER_CLASS =
  'border-marine/40 bg-marine/10 text-marine-foreground hover:border-marine/40 hover:bg-marine/16 data-pressed:bg-marine/16'

/**
 * `/` 与 ⌘K / Ctrl+K 聚焦搜索框——列表型控制台的通用约定。
 *
 * 已经在输入的时候不抢键（否则打不出 `/`）；弹层/对话框打开时也不抢，
 * 否则焦点会跳到被遮住的输入框上，模态里反而按不动。
 */
function useSearchHotkey(ref: RefObject<HTMLInputElement | null>, enabled = true): void {
  useEffect(() => {
    if (!enabled) return
    const onKeyDown = (event: KeyboardEvent) => {
      const slash = event.key === '/' && !event.metaKey && !event.ctrlKey && !event.altKey
      const commandK = (event.key === 'k' || event.key === 'K') && (event.metaKey || event.ctrlKey)
      if (!slash && !commandK) return
      const target = event.target as HTMLElement | null
      if (target?.isContentEditable) return
      if (target && /^(input|textarea|select)$/i.test(target.tagName)) return
      if (target?.closest('[role="dialog"], [role="alertdialog"], [role="menu"], [role="listbox"]')) return
      const input = ref.current
      if (!input) return
      event.preventDefault()
      input.focus()
      input.select()
    }
    window.addEventListener('keydown', onKeyDown)
    return () => window.removeEventListener('keydown', onKeyDown)
  }, [ref, enabled])
}

/**
 * 工具条搜索框：放大镜在左；有内容时右侧是清除按钮，没内容时在指针设备上提示 `/` 快捷键。
 * Esc 先清空、再退出输入框——清空和失焦是两个不同的意图，一次按键只做一件。
 */
export function ToolbarSearch({
  value,
  onChange,
  placeholder,
  ariaLabel,
  className,
  size,
  hotkey = true,
}: {
  value: string
  onChange: (value: string) => void
  placeholder: string
  ariaLabel: string
  className?: string
  /** 缺省与工具条其余控件同为默认档；放进紧凑的列表区时可用 `sm`。 */
  size?: 'sm' | 'default'
  /** 接管 `/` 与 ⌘K。一页有两个搜索框时只给主的那个，否则按下去焦点落在后挂上的那个。 */
  hotkey?: boolean
}) {
  const { t } = useI18n()
  const ref = useRef<HTMLInputElement>(null)
  useSearchHotkey(ref, hotkey)
  return (
    <InputGroup className={className}>
      <InputGroupAddon><SearchIcon /></InputGroupAddon>
      <InputGroupInput
        ref={ref}
        value={value}
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={(event) => {
          if (event.key !== 'Escape') return
          event.preventDefault()
          if (value) onChange('')
          else event.currentTarget.blur()
        }}
        placeholder={placeholder}
        aria-label={ariaLabel}
        size={size}
      />
      <InputGroupAddon align="inline-end">
        {value ? (
          <Button size="icon-xs" variant="ghost" onClick={() => onChange('')} aria-label={t('清除搜索', 'Clear search')}>
            <XIcon />
          </Button>
        ) : hotkey ? (
          // 只在指针设备上提示：触屏没有物理按键，画个 kbd 只是噪声。
          <Kbd aria-hidden className="hidden pointer-fine:inline-flex">/</Kbd>
        ) : null}
      </InputGroupAddon>
    </InputGroup>
  )
}

/** 菜单式下拉里的一项。 */
export interface MenuSelectItem<T extends string> {
  value: T
  label: ReactNode
}

/**
 * 菜单式下拉：描边按钮（图标 + 当前值）做触发器，弹层在下方右对齐展开，单选。`groups` 之间
 * 画分隔线；`children` 追加在最后一组之后（排序菜单用它放升 / 降序）。`active` 为真时按钮
 * 染色，表示这一项正在缩小列表。
 */
export function ToolbarMenuSelect<T extends string>({
  icon: Icon,
  label,
  ariaLabel,
  value,
  groups,
  onChange,
  active = false,
  className,
  children,
}: {
  icon: ElementType<{ className?: string }>
  label: ReactNode
  ariaLabel: string
  value: T
  groups: MenuSelectItem<T>[][]
  onChange: (value: T) => void
  active?: boolean
  className?: string
  children?: ReactNode
}) {
  return (
    <Menu>
      <MenuTrigger
        aria-label={ariaLabel}
        className={cn(
          buttonVariants({ variant: 'outline' }),
          'w-full min-w-0 justify-between max-sm:[&_svg]:hidden sm:w-auto',
          active && ACTIVE_FILTER_CLASS,
          className,
        )}
      >
        <Icon />
        {typeof label === 'string' ? <span className="min-w-0 truncate">{label}</span> : label}
      </MenuTrigger>
      <MenuPopup align="end" className="w-48">
        <MenuRadioGroup value={value} onValueChange={(next) => onChange(next as T)}>
          {groups.map((items, index) => (
            <MenuGroup key={items[0]?.value ?? index}>
              {index > 0 && <MenuSeparator />}
              {items.map((item) => (
                // 内容列收成 minmax(0,1fr)、包裹层 min-w-0：选项名过长（代理名称）时截断自身，
                // 不把右侧计数挤出弹层。
                <MenuRadioItem
                  key={item.value}
                  value={item.value}
                  className="grid-cols-[.75rem_minmax(0,1fr)] *:last:min-w-0"
                >
                  {item.label}
                </MenuRadioItem>
              ))}
            </MenuGroup>
          ))}
        </MenuRadioGroup>
        {children && (
          <>
            <MenuSeparator />
            {children}
          </>
        )}
      </MenuPopup>
    </Menu>
  )
}

/** 「筛选」弹层里一节的一个选项。 */
export interface FacetFilterOption {
  value: string
  label: string
  count: number
  /** 挂在上一项下面的子项（成员筛选里队下的人），缩进一档。 */
  nested?: boolean
}

/** 「筛选」弹层里的一节：一个维度，单选，第一项约定是「全部」。 */
export interface FacetFilterSection {
  key: string
  title: string
  value: string
  /** 「全部」那一项的值，用来判断这一节是否生效。 */
  allValue: string
  groups: FacetFilterOption[][]
  onChange: (value: string) => void
  /** 选项多时（代理）在这一节顶上放个搜索框，只过滤 `allValue` 以外的项。 */
  searchable?: boolean
  searchPlaceholder?: string
}

/** 选项多于这个数才出这一节的搜索框，少的时候一眼就看完了。 */
const FACET_SEARCH_THRESHOLD = 6

function FacetSection({ section }: { section: FacetFilterSection }) {
  const { t } = useI18n()
  const [query, setQuery] = useState('')
  const total = section.groups.reduce((sum, group) => sum + group.length, 0)
  const showSearch = section.searchable && total > FACET_SEARCH_THRESHOLD
  const groups = useMemo(() => {
    const q = query.trim().toLowerCase()
    if (!q) return section.groups
    return section.groups
      .map((group) => group.filter((item) => item.value === section.allValue || item.label.toLowerCase().includes(q)))
      .filter((group) => group.length > 0)
  }, [section.groups, section.allValue, query])
  const titleId = `facet-${section.key}`
  return (
    <div className="space-y-1" role="radiogroup" aria-labelledby={titleId}>
      <p id={titleId} className="px-2 text-xs font-medium text-muted-foreground">{section.title}</p>
      {showSearch && (
        <div className="px-1 pb-1">
          <ToolbarSearch
            size="sm"
            hotkey={false}
            value={query}
            onChange={setQuery}
            placeholder={section.searchPlaceholder ?? t('搜索…', 'Search…')}
            ariaLabel={section.searchPlaceholder ?? t('搜索', 'Search')}
          />
        </div>
      )}
      {groups.map((items, index) => (
        <div key={items[0]?.value ?? index} className={cn(index > 0 && 'mt-1 border-t pt-1')}>
          {items.map((item) => {
            const checked = item.value === section.value
            return (
              <button
                key={item.value}
                type="button"
                role="radio"
                aria-checked={checked}
                onClick={() => section.onChange(item.value)}
                className={cn(
                  'grid min-h-8 w-full cursor-default grid-cols-[.75rem_minmax(0,1fr)_auto] items-center gap-2 rounded-sm ps-2 pe-2 text-left text-sm outline-none hover:bg-accent focus-visible:bg-accent sm:min-h-7',
                  checked && 'font-medium',
                )}
              >
                {checked ? <CheckIcon className="size-3.5" /> : <span />}
                <span className={cn('min-w-0 truncate', item.nested && 'ps-3')}>{item.label}</span>
                <span className="tnum text-xs text-muted-foreground">{item.count.toLocaleString()}</span>
              </button>
            )
          })}
        </div>
      ))}
      {groups.length === 0 && (
        <p className="px-2 py-1.5 text-sm text-muted-foreground">{t('无匹配结果', 'No matches')}</p>
      )}
    </div>
  )
}

/**
 * 「筛选」按钮 + 分节弹层：把用得不那么勤的几个维度（成员、套餐、代理）收进一处，工具条上只占
 * 一个按钮，按钮上标出已生效几项。选一项不关弹层——常常是连着设两三个维度。每节单选，第一项是
 * 「全部」。生效的条件另由 [ActiveFilterChips] 摊在工具条下面，一眼看得出筛了什么。
 */
export function ToolbarFacetFilter({
  sections,
  className,
}: {
  sections: FacetFilterSection[]
  className?: string
}) {
  const { t } = useI18n()
  const active = sections.filter((section) => section.value !== section.allValue).length
  const label = t('筛选', 'Filter')
  return (
    <Popover>
      <PopoverTrigger
        aria-label={active > 0 ? t(`${label}，已生效 ${active} 项`, `${label}, ${active} active`) : label}
        className={cn(
          buttonVariants({ variant: 'outline' }),
          'w-full min-w-0 justify-between max-sm:[&_svg]:hidden sm:w-auto',
          active > 0 && ACTIVE_FILTER_CLASS,
          className,
        )}
      >
        <SlidersHorizontalIcon />
        <span className="flex min-w-0 items-baseline gap-1">
          <span className="min-w-0 truncate">{label}</span>
          {active > 0 && <span className="tnum shrink-0 text-xs">· {active}</span>}
        </span>
      </PopoverTrigger>
      <PopoverPopup align="end" className="w-72 [--viewport-inline-padding:--spacing(1.5)] *:data-[slot=popover-viewport]:py-2">
        <div className="space-y-2 [&>*+*]:border-t [&>*+*]:pt-2">
          {sections.map((section) => <FacetSection key={section.key} section={section} />)}
        </div>
      </PopoverPopup>
    </Popover>
  )
}

/** 工具条下方的一枚生效条件。 */
export interface ActiveFilterChip {
  key: string
  /** 维度名，如「代理」。 */
  dimension: string
  /** 当前值，如「#1 香港」。 */
  value: string
  onRemove: () => void
}

/**
 * 生效条件的标签行：「维度：值 ✕」逐个列出，末尾一个「清除全部」。没有生效条件时不渲染，
 * 不占高度。
 */
export function ActiveFilterChips({
  chips,
  onClearAll,
  className,
}: {
  chips: ActiveFilterChip[]
  onClearAll: () => void
  className?: string
}) {
  const { t } = useI18n()
  if (chips.length === 0) return null
  return (
    <div className={cn('flex flex-wrap items-center gap-1.5', className)}>
      {chips.map((chip) => (
        <span
          key={chip.key}
          className="inline-flex h-7 max-w-full min-w-0 items-center gap-1 rounded-md border border-marine/30 bg-marine/8 ps-2 pe-0.5 text-xs text-marine-foreground"
        >
          <span className="shrink-0 text-muted-foreground">{chip.dimension}</span>
          <span className="min-w-0 truncate font-medium">{chip.value}</span>
          <Button
            size="icon-xs"
            variant="ghost"
            className="size-6 shrink-0"
            onClick={chip.onRemove}
            aria-label={t(`移除筛选：${chip.dimension} ${chip.value}`, `Remove filter: ${chip.dimension} ${chip.value}`)}
          >
            <XIcon />
          </Button>
        </span>
      ))}
      <Button size="xs" variant="ghost" className="text-muted-foreground" onClick={onClearAll}>
        {t('清除全部', 'Clear all')}
      </Button>
    </div>
  )
}
