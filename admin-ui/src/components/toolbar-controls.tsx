import { useEffect, useRef, type ElementType, type ReactNode, type RefObject } from 'react'
import { SearchIcon, XIcon } from 'lucide-react'
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
function useSearchHotkey(ref: RefObject<HTMLInputElement | null>): void {
  useEffect(() => {
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
  }, [ref])
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
}: {
  value: string
  onChange: (value: string) => void
  placeholder: string
  ariaLabel: string
  className?: string
  /** 缺省与工具条其余控件同为默认档；放进紧凑的列表区时可用 `sm`。 */
  size?: 'sm' | 'default'
}) {
  const { t } = useI18n()
  const ref = useRef<HTMLInputElement>(null)
  useSearchHotkey(ref)
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
        ) : (
          // 只在指针设备上提示：触屏没有物理按键，画个 kbd 只是噪声。
          <Kbd aria-hidden className="hidden pointer-fine:inline-flex">/</Kbd>
        )}
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
                <MenuRadioItem key={item.value} value={item.value}>
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
