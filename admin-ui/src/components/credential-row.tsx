import { useState } from 'react'
import {
  ArrowPathIcon, CheckIcon, XMarkIcon, EllipsisHorizontalIcon,
  ChevronDownIcon, ChevronUpIcon,
} from '@heroicons/react/24/outline'
import { type Credential } from '@/api/credentials'
import { cn, formatUsd, relativeTime } from '@/lib/utils'
import {
  CredentialMenuContent, expiryMeta, isAbnormal, isNearLimit, statusMeta, switchTitle,
  tierBadgeClass, useCredentialActions, type SortDir, type SortKey,
} from '@/components/credential-shared'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Switch } from '@/components/ui/switch'
import { TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { DropdownMenu, DropdownMenuTrigger } from '@/components/ui/dropdown-menu'

/**
 * 列宽与响应式收起断点的**唯一来源**，表头与数据行都引用同一份。
 *
 * 表头和单元格各写一套 class 是这类表格最常见的 bug（某一侧改了断点，窄屏下列就错位），
 * 所以这里集中定义；[`CredentialListHeader`] 与 [`CredentialRow`] 都从这里取，不可能漂移。
 * 断点用的是容器查询（外层 `main` 上有 `@container`），按容器宽度而非视口收起列。
 */
const COL = {
  /** 首列：仅承载左侧状态轨与批量勾选框；非批量模式下压成零宽，只剩 3px 色条。 */
  lead: 'w-0',
  status: 'w-px whitespace-nowrap',
  name: 'text-left',
  tier: 'hidden w-24 @lg:table-cell',
  priority: 'hidden w-16 text-right @md:table-cell',
  quota: 'hidden w-32 @xl:table-cell',
  devices: 'hidden w-20 text-right @md:table-cell',
  cost: 'hidden w-20 text-right @lg:table-cell',
  lastUsed: 'hidden w-24 text-right @2xl:table-cell',
  actions: 'w-20 text-right',
} as const

/**
 * 列表表头。**每一个数据列都可点击排序**：点未激活的列按该维度的默认方向排，
 * 再点当前列则翻转升/降序，箭头实时反映真实方向。
 *
 * 「添加时间」没有对应列，只在工具栏的排序下拉里出现——两者共用同一份 state，
 * 从表头点和从下拉选是等价的。
 */
export function CredentialListHeader({
  selectable, sort, dir, onSortChange, allSelected, onSelectAll,
}: {
  selectable?: boolean
  /** 当前排序维度；不是本表头任何一列时，所有列都显示为未激活。 */
  sort: SortKey
  dir: SortDir
  /** 点击列头。是否翻转方向由调用方决定（同一维度再点即翻转）。 */
  onSortChange: (key: SortKey) => void
  allSelected?: boolean
  onSelectAll?: (next: boolean) => void
}) {
  /** 渲染一个可排序列头：标签 + 方向箭头（未激活时占位不显形，避免 hover 抖动）。 */
  const sortable = (label: string, key: SortKey, align: 'left' | 'right' = 'left') => {
    const active = sort === key
    const Arrow = active && dir === 'asc' ? ChevronUpIcon : ChevronDownIcon
    return (
      <button
        onClick={() => onSortChange(key)}
        className={cn(
          'inline-flex w-full items-center gap-0.5 transition-colors hover:text-foreground',
          align === 'right' && 'justify-end',
          active && 'font-semibold text-foreground',
        )}
        title={active ? `按${label}排序（点击切换升/降序）` : `按${label}排序`}
      >
        {label}
        <Arrow className={cn('size-3 shrink-0', active ? 'opacity-100' : 'opacity-0')} />
      </button>
    )
  }
  /** 激活列要带 aria-sort，读屏才知道当前按哪列、以什么方向排。 */
  const sortProps = (key: SortKey) =>
    sort === key ? ({ 'aria-sort': dir === 'asc' ? 'ascending' : 'descending' } as const) : {}

  return (
    <TableHeader className="bg-surface-2/50 text-2xs text-muted-foreground">
      {/* 表头行不需要 hover 高亮 */}
      <TableRow className="hover:bg-transparent">
        <TableHead className={cn(COL.lead, selectable ? 'pr-0' : 'p-0')}>
          {selectable ? (
            <input
              type="checkbox"
              checked={!!allSelected}
              onChange={(e) => onSelectAll?.(e.target.checked)}
              className="size-4 align-middle accent-primary"
              aria-label="全选当前筛选结果"
            />
          ) : (
            <span className="sr-only">状态标识</span>
          )}
        </TableHead>
        <TableHead className={cn(COL.status, 'text-muted-foreground')} {...sortProps('status')}>
          {sortable('状态', 'status')}
        </TableHead>
        <TableHead className={cn(COL.name, 'text-muted-foreground')} {...sortProps('name')}>
          {sortable('账号', 'name')}
        </TableHead>
        <TableHead className={cn(COL.tier, 'text-muted-foreground')} {...sortProps('tier')}>
          {sortable('套餐', 'tier')}
        </TableHead>
        <TableHead className={cn(COL.priority, 'text-muted-foreground')} {...sortProps('priority')}>
          {sortable('优先级', 'priority', 'right')}
        </TableHead>
        <TableHead className={cn(COL.quota, 'text-muted-foreground')} {...sortProps('usage5h')}>
          {sortable('5h 额度', 'usage5h')}
        </TableHead>
        <TableHead className={cn(COL.devices, 'text-muted-foreground')} {...sortProps('devices')}>
          {sortable('设备', 'devices', 'right')}
        </TableHead>
        <TableHead className={cn(COL.cost, 'text-muted-foreground')} {...sortProps('cost')}>
          {sortable('花费', 'cost', 'right')}
        </TableHead>
        <TableHead className={cn(COL.lastUsed, 'text-muted-foreground')} {...sortProps('recent')}>
          {sortable('最近使用', 'recent', 'right')}
        </TableHead>
        <TableHead className={cn(COL.actions, 'text-muted-foreground')}>
          <span className="sr-only">操作</span>
        </TableHead>
      </TableRow>
    </TableHeader>
  )
}

/**
 * 紧凑列表的一行（一个 `<tr>`，配 [`CredentialListHeader`] 使用）。
 *
 * 信息取卡片视图的关键子集——状态灯、名称、档位/优先级、5h 额度条、设备、花费、最近使用，
 * 写操作（启用开关、⋯ 菜单、重命名）与卡片共用 [`useCredentialActions`]。
 * 设备上限这类低频配置留在卡片视图里改，列表只读展示。
 */
export function CredentialRow({
  cred, selectable = false, selected = false, onSelectedChange,
}: {
  cred: Credential
  selectable?: boolean
  selected?: boolean
  onSelectedChange?: (next: boolean) => void
}) {
  const [editing, setEditing] = useState(false)
  const [name, setName] = useState(cred.label)
  const actions = useCredentialActions(cred, () => setEditing(false))
  const { rename, toggle } = actions

  const nearLimit = isNearLimit(cred)
  const status = statusMeta(cred, nearLimit)
  const expiry = expiryMeta(cred)
  const util = cred.quota?.rl_5h_utilization ?? null

  return (
    <TableRow className={cn('text-xs', cred.disabled && 'opacity-60')}>
      {/* 首列：左侧状态轨（伪元素）+ 批量勾选框。轨挂在 `<td>` 而非 `<tr>`——
          `<tr>` 上的 position:relative 各浏览器支持不一。非批量模式下这格零宽只剩色条。 */}
      <TableCell
        className={cn(
          COL.lead,
          'relative before:absolute before:inset-y-0 before:left-0 before:w-[3px]',
          status.rail,
          selectable ? 'pr-0' : 'p-0',
        )}
      >
        {selectable && (
          <input
            type="checkbox"
            checked={selected}
            onChange={(e) => onSelectedChange?.(e.target.checked)}
            className="size-4 align-middle accent-primary"
            aria-label={`选择 ${cred.label}`}
          />
        )}
      </TableCell>

      {/* 状态：灯 + 文案（封禁/停用/过期/将满/剩余有效期）。窄容器下只留灯。 */}
      <TableCell className={COL.status}>
        <span className="flex items-center gap-1.5" title={cred.ban_reason ?? status.label}>
          <span
            className={cn('size-2 shrink-0 rounded-full', status.dot)}
            aria-label={status.label}
          />
          <span className={cn('hidden text-2xs @md:inline', expiry.className)}>
            {nearLimit && !isAbnormal(cred)
              ? `额度 ${Math.round(util != null ? util * 100 : 0)}%`
              : expiry.text}
          </span>
        </span>
      </TableCell>

      {/* 账号名（点击重命名） */}
      <TableCell className={COL.name}>
        {editing ? (
          <form
            className="flex min-w-0 items-center gap-1"
            onSubmit={(e) => { e.preventDefault(); rename.mutate(name.trim()) }}
          >
            <Input value={name} onChange={(e) => setName(e.target.value)} autoFocus className="h-7 w-44 text-xs" />
            <Button type="submit" size="icon" variant="ghost" className="size-7" disabled={rename.isPending}>
              {rename.isPending ? <ArrowPathIcon className="size-3.5 animate-spin" /> : <CheckIcon className="size-3.5" />}
            </Button>
            <Button type="button" size="icon" variant="ghost" className="size-7"
              onClick={() => { setEditing(false); setName(cred.label) }}>
              <XMarkIcon className="size-3.5" />
            </Button>
          </form>
        ) : (
          <button
            onClick={() => setEditing(true)}
            className="block max-w-full truncate text-left font-medium hover:underline"
            title={`${cred.label} · #${cred.id} · 点击重命名`}
          >
            {cred.label}
          </button>
        )}
      </TableCell>

      {/* 套餐（订阅等级）独立成列，不再跟账号名挤在一起 */}
      <TableCell className={COL.tier}>
        {cred.tier ? (
          <Badge
            variant="outline"
            className={cn('h-5 px-1.5 py-0 text-2xs font-medium', tierBadgeClass(cred.tier))}
          >
            {cred.tier}
          </Badge>
        ) : (
          <span className="text-2xs text-muted-foreground">—</span>
        )}
      </TableCell>

      {/* 调度优先级 */}
      <TableCell
        className={cn(COL.priority, 'font-mono text-2xs text-muted-foreground')}
        title="调度优先级（数值小者优先）"
      >
        P{cred.priority}
      </TableCell>

      {/* 5h 额度条 */}
      <TableCell className={COL.quota} title="5 小时额度使用率">
        {util != null ? (
          <div className="flex items-center gap-1.5">
            <div className="h-1.5 flex-1 overflow-hidden rounded-full bg-border/80">
              <div
                className={cn(
                  'h-full rounded-full',
                  util >= 0.9 ? 'bg-bad' : util >= 0.7 ? 'bg-warn' : 'bg-ok',
                )}
                style={{ width: `${Math.min(100, Math.max(0, Math.round(util * 100)))}%` }}
              />
            </div>
            <span className="w-8 shrink-0 text-right tnum text-2xs text-muted-foreground">
              {Math.round(util * 100)}%
            </span>
          </div>
        ) : (
          <span className="text-2xs text-muted-foreground">未知</span>
        )}
      </TableCell>

      {/* 设备 / 花费 / 最近使用 */}
      <TableCell
        className={cn(COL.devices, 'tnum text-muted-foreground')}
        title={cred.device_limit === 0 ? '设备数 / 上限（跟随全局默认）' : '设备数 / 上限'}
      >
        {cred.device_count}/{cred.device_limit_effective > 0 ? cred.device_limit_effective : '∞'}
      </TableCell>
      <TableCell className={cn(COL.cost, 'tnum text-muted-foreground')} title="累计等价 API 费用">
        {formatUsd(cred.cost_total)}
      </TableCell>
      <TableCell className={cn(COL.lastUsed, 'text-muted-foreground')} title="最近一次转发使用">
        {cred.last_used != null ? relativeTime(cred.last_used) : '未使用'}
      </TableCell>

      {/* 启用开关 + ⋯ 菜单 */}
      <TableCell className={COL.actions}>
        <div className="flex items-center justify-end gap-1">
          <span className="relative inline-flex shrink-0 items-center">
            <Switch
              variant="success"
              checked={!cred.disabled}
              onCheckedChange={(on) => toggle.mutate(!on)}
              disabled={toggle.isPending}
              title={switchTitle(cred)}
              className={cn(
                'scale-90',
                toggle.isPending && 'opacity-0',
                isAbnormal(cred) && 'data-[state=checked]:bg-muted-foreground/50',
              )}
            />
            {toggle.isPending && (
              <ArrowPathIcon className="absolute left-1/2 top-1/2 size-3.5 -translate-x-1/2 -translate-y-1/2 animate-spin text-muted-foreground" />
            )}
          </span>
          <DropdownMenu>
            <DropdownMenuTrigger asChild>
              <Button size="icon" variant="ghost" className="size-7 shrink-0 text-muted-foreground">
                <EllipsisHorizontalIcon className="size-4" />
              </Button>
            </DropdownMenuTrigger>
            <CredentialMenuContent cred={cred} actions={actions} onRename={() => setEditing(true)} />
          </DropdownMenu>
        </div>
      </TableCell>
    </TableRow>
  )
}
