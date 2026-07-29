import { type ReactNode, useState } from 'react'
import {
  ArrowPathIcon, CheckIcon, XMarkIcon, EllipsisHorizontalIcon,
  ChevronDownIcon, ChevronUpIcon, ClockIcon, DevicePhoneMobileIcon, PencilIcon, WalletIcon,
} from '@heroicons/react/24/outline'
import { type Credential } from '@/api/credentials'
import { cn, formatUsd, relativeTime } from '@/lib/utils'
import {
  ConnectivityTestDialog, CredentialMenuContent, DeleteCredentialDialog, expiryMeta, isAbnormal,
  isNearLimit, inputToLimit, limitToInput, liveQuota, statusMeta, switchTitle, tierBadgeClass,
  useCredentialActions,
  type CredentialActions, type SortDir, type SortKey,
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
 * 手机端使用独立的信息行；桌面表格统一使用视口断点，避免与页面响应式规则脱节。
 */
const COL = {
  /** 首列：仅承载左侧状态轨与批量勾选框；非批量模式下压成零宽，只剩 3px 色条。 */
  lead: 'w-0',
  account: 'w-[30%] pl-4 text-left',
  schedule: 'w-[12%] px-3 text-left',
  quota: 'w-[28%] px-3 text-left',
  devices: 'w-[12%] px-3 text-left',
  activity: 'w-[14%] px-3 text-left',
  actions: 'w-12 pr-3 text-right',
} as const

/**
 * 列表表头。核心数据列支持点击排序：点未激活的列按该维度的默认方向排，
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
  const sortable = (label: string, key: SortKey, align: 'left' | 'center' | 'right' = 'left') => {
    const active = sort === key
    const Arrow = active && dir === 'asc' ? ChevronUpIcon : ChevronDownIcon
    return (
      <Button
        type="button"
        size="sm"
        variant="ghost"
        onClick={() => onSortChange(key)}
        className={cn(
          'h-8 w-full justify-start gap-0.5 rounded px-0 text-2xs font-medium text-muted-foreground hover:bg-transparent hover:text-foreground',
          align === 'center' && 'justify-center',
          align === 'right' && 'justify-end',
          active && 'font-semibold text-foreground',
        )}
        title={active ? `按${label}排序（点击切换升/降序）` : `按${label}排序`}
      >
        {label}
        <Arrow className={cn('size-3 shrink-0', active ? 'opacity-100' : 'opacity-0')} />
      </Button>
    )
  }
  /** 激活列要带 aria-sort，读屏才知道当前按哪列、以什么方向排。 */
  const sortProps = (key: SortKey) =>
    sort === key ? ({ 'aria-sort': dir === 'asc' ? 'ascending' : 'descending' } as const) : {}

  return (
    <TableHeader className="hidden bg-transparent text-2xs text-muted-foreground lg:table-header-group">
      {/* 表头行不需要 hover 高亮 */}
      <TableRow className="hover:bg-transparent">
        <TableHead className={cn(COL.lead, selectable ? 'pr-0' : 'p-0')}>
          {selectable ? (
            <div className="flex h-10 items-center">
              <input
                type="checkbox"
                checked={!!allSelected}
                onChange={(e) => onSelectAll?.(e.target.checked)}
                className="size-4 rounded border-border accent-primary"
                aria-label="全选当前筛选结果"
              />
            </div>
          ) : (
            <span className="sr-only">状态标识</span>
          )}
        </TableHead>
        <TableHead className={cn(COL.account, 'text-muted-foreground')} {...sortProps('name')}>
          {sortable('账号', 'name')}
        </TableHead>
        <TableHead className={cn(COL.schedule, 'text-muted-foreground')} {...sortProps('priority')}>
          {sortable('调度', 'priority')}
        </TableHead>
        <TableHead className={cn(COL.quota, 'text-muted-foreground')} {...sortProps('usage5h')}>
          {sortable('额度', 'usage5h')}
        </TableHead>
        <TableHead className={cn(COL.devices, 'text-muted-foreground')} {...sortProps('devices')}>
          {sortable('设备', 'devices')}
        </TableHead>
        <TableHead className={cn(COL.activity, 'text-muted-foreground')} {...sortProps('recent')}>
          {sortable('活动', 'recent')}
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
 * 桌面端按“账号、调度、额度、设备、活动”聚合信息，减少跨列扫描；
 * 手机端使用同一层级的紧凑信息卡。写操作与卡片视图共用 [`useCredentialActions`]。
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
  const [editingPriority, setEditingPriority] = useState(false)
  const [priorityValue, setPriorityValue] = useState(String(cred.priority))
  const [editingLimit, setEditingLimit] = useState(false)
  const [limitValue, setLimitValue] = useState(limitToInput(cred.device_limit))
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [testing, setTesting] = useState(false)
  const actions = useCredentialActions(
    cred,
    () => setEditing(false),
    () => setEditingLimit(false),
  )
  const { rename, prio, limit } = actions

  const nearLimit = isNearLimit(cred)
  const status = statusMeta(cred, nearLimit)
  const expiry = expiryMeta(cred)
  const { u5h, u7d } = liveQuota(cred)
  const quotaMax = Math.max(u5h ?? 0, u7d ?? 0)
  const initial = cred.label.trim().charAt(0).toUpperCase() || '?'
  const deviceLimit = cred.device_limit_effective > 0 ? cred.device_limit_effective : '∞'
  const startPriorityEdit = () => {
    setPriorityValue(String(cred.priority))
    setEditingPriority(true)
  }
  const startLimitEdit = () => {
    setLimitValue(limitToInput(cred.device_limit))
    setEditingLimit(true)
  }
  const savePriority = () => {
    const next = Math.floor(Number(priorityValue) || 0)
    prio.mutate(next, { onSuccess: () => setEditingPriority(false) })
  }
  const saveLimit = () => limit.mutate(inputToLimit(limitValue))

  return (
    <>
      <TableRow
        className={cn('hover:bg-transparent lg:hidden', cred.disabled && 'bg-muted/15')}
        data-state={selected ? 'selected' : undefined}
      >
        <TableCell
          colSpan={7}
          className={cn(
            'relative whitespace-normal p-0 before:absolute before:inset-y-0 before:left-0 before:w-[3px]',
            status.rail,
          )}
        >
          <div className="p-3.5 pl-[1.125rem]">
            <div className="flex items-start gap-3">
              {selectable && (
                <div className="grid size-9 shrink-0 place-items-center">
                  <input
                    type="checkbox"
                    checked={selected}
                    onChange={(e) => onSelectedChange?.(e.target.checked)}
                    className="size-4 rounded border-border accent-primary"
                    aria-label={`选择 ${cred.label}`}
                  />
                </div>
              )}
              <div className="relative shrink-0">
                <div
                  className={cn(
                    'grid size-9 place-items-center rounded-full border border-border text-xs font-semibold',
                    cred.disabled
                      ? 'bg-muted/70 text-muted-foreground/70'
                      : 'bg-muted text-foreground',
                  )}
                  aria-hidden
                >
                  {initial}
                </div>
                <span
                  className={cn('absolute -bottom-0.5 -right-0.5 size-3 rounded-full ring-2 ring-card', status.dot)}
                  title={status.label}
                  aria-label={status.label}
                  role="img"
                />
              </div>

              <div className="min-w-0 flex-1">
                <EditableCredentialName
                  cred={cred}
                  editing={editing}
                  name={name}
                  pending={rename.isPending}
                  mobile
                  onNameChange={setName}
                  onStart={() => setEditing(true)}
                  onCancel={() => { setEditing(false); setName(cred.label) }}
                  onSubmit={() => rename.mutate(name.trim())}
                />
                {!editing && (
                  <div className="mt-2 flex flex-wrap items-center gap-1.5 text-2xs text-muted-foreground">
                    {status.label !== '运行正常' && (
                      <ListCredentialStatus
                        status={status}
                        expiry={expiry}
                        nearLimit={nearLimit}
                        abnormal={isAbnormal(cred)}
                        quotaMax={quotaMax}
                        compact
                        showLabel
                      />
                    )}
                    {cred.tier && (
                      <Badge variant="outline" className={cn('h-5 px-1.5 py-0 text-2xs', tierBadgeClass(cred.tier))}>
                        {cred.tier}
                      </Badge>
                    )}
                    <span className="inline-flex h-5 items-center px-1.5 font-mono">
                      P{cred.priority}
                    </span>
                  </div>
                )}
              </div>

              <div className="flex h-9 items-center">
                <CredentialRowActions
                  cred={cred}
                  actions={actions}
                  onRename={() => setEditing(true)}
                  onDeviceLimit={startLimitEdit}
                  onTest={() => setTesting(true)}
                  onRequestDelete={() => setConfirmDelete(true)}
                />
              </div>
            </div>

            <div className="mt-3.5 grid grid-cols-2 gap-4 px-0.5">
              <div>
                <ListQuotaMeter label="5h" util={u5h} reset={cred.quota?.rl_5h_reset ?? null} />
              </div>
              <div>
                <ListQuotaMeter label="7d" util={u7d} reset={cred.quota?.rl_7d_reset ?? null} />
              </div>
            </div>

            <div className="mt-3 flex flex-wrap items-center justify-between gap-x-3 gap-y-2 text-2xs text-muted-foreground">
              <div className="inline-flex items-center gap-1" title="已绑定设备 / 设备上限">
                <DevicePhoneMobileIcon className="size-3.5" />
                {editingLimit ? (
                  <CompactNumberEditor
                    value={limitValue}
                    pending={limit.isPending}
                    label="设备上限"
                    placeholder="默认"
                    min={0}
                    hint="留空 = 默认，0 = 不限"
                    onChange={setLimitValue}
                    onSubmit={saveLimit}
                    onCancel={() => setEditingLimit(false)}
                  />
                ) : (
                  <span className="tnum">设备 {cred.device_count}/{deviceLimit}</span>
                )}
              </div>
              <span className="inline-flex items-center gap-1" title="累计等价 API 费用">
                <WalletIcon className="size-3.5" />
                <span className="tnum">{formatUsd(cred.cost_total)}</span>
              </span>
              <span className="inline-flex items-center gap-1" title="最近一次转发使用">
                <ClockIcon className="size-3.5" />
                {cred.last_used != null ? relativeTime(cred.last_used) : '未使用'}
              </span>
            </div>
          </div>
        </TableCell>
      </TableRow>

    <TableRow
      className={cn('group/row hidden text-xs hover:bg-muted/25 lg:table-row', cred.disabled && 'bg-muted/10')}
      data-state={selected ? 'selected' : undefined}
    >
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
          <ListCellLine>
            <input
              type="checkbox"
              checked={selected}
              onChange={(e) => onSelectedChange?.(e.target.checked)}
              className="size-4 rounded border-border accent-primary"
              aria-label={`选择 ${cred.label}`}
            />
          </ListCellLine>
        )}
      </TableCell>

      {/* 账号列保持单行：头像承载状态，名称 / ID / 异常状态 / 套餐依次排列。 */}
      <TableCell className={cn(COL.account, 'py-2.5')}>
        <ListCellLine className="gap-3">
          <div className="relative shrink-0">
            <div
              className={cn(
                'grid size-8 place-items-center rounded-full border border-border text-xs font-semibold',
                cred.disabled
                  ? 'bg-muted/70 text-muted-foreground/70'
                  : 'bg-muted text-foreground',
              )}
              aria-hidden
            >
              {initial}
            </div>
            <span className="absolute -bottom-1 -right-1 grid size-3.5 place-items-center rounded-full bg-card">
              <ListCredentialStatus
                status={status}
                expiry={expiry}
                nearLimit={nearLimit}
                abnormal={isAbnormal(cred)}
                quotaMax={quotaMax}
                compact
              />
            </span>
          </div>
          <div className="flex min-w-0 flex-1 items-center gap-2">
            <div className="min-w-0 flex-1 [&>div]:w-full">
              <EditableCredentialName
                cred={cred}
                editing={editing}
                name={name}
                pending={rename.isPending}
                onNameChange={setName}
                onStart={() => setEditing(true)}
                onCancel={() => { setEditing(false); setName(cred.label) }}
                onSubmit={() => rename.mutate(name.trim())}
              />
            </div>
            {!editing && status.label !== '运行正常' && (
              <ListCredentialStatus
                status={status}
                expiry={expiry}
                nearLimit={nearLimit}
                abnormal={isAbnormal(cred)}
                quotaMax={quotaMax}
                compact
                showLabel
              />
            )}
            {!editing && cred.tier && (
              <Badge
                variant="outline"
                className={cn('h-5 shrink-0 px-1.5 py-0 text-2xs font-medium', tierBadgeClass(cred.tier))}
              >
                {cred.tier}
              </Badge>
            )}
          </div>
        </ListCellLine>
      </TableCell>

      {/* 调度把开关和优先级收在同一列，避免用户跨列理解两者关系。 */}
      <TableCell className={cn(COL.schedule, 'py-2.5')}>
        <ListCellLine className="gap-2">
          {editingPriority ? (
            <CompactNumberEditor
              value={priorityValue}
              pending={prio.isPending}
              label="优先级"
              compact
              onChange={setPriorityValue}
              onSubmit={savePriority}
              onCancel={() => setEditingPriority(false)}
            />
          ) : (
            <>
              <CredentialRowActions
                cred={cred}
                actions={actions}
                showMenu={false}
                onRename={() => setEditing(true)}
                onDeviceLimit={startLimitEdit}
                onTest={() => setTesting(true)}
                onRequestDelete={() => setConfirmDelete(true)}
              />
              <Button
                type="button"
                size="sm"
                variant="ghost"
                className="inline-flex h-7 items-center gap-1 rounded px-1.5 font-mono text-2xs text-muted-foreground transition-colors hover:bg-muted hover:text-foreground"
                onClick={startPriorityEdit}
                title="修改调度优先级（数值小者优先）"
              >
                P{cred.priority}
                <PencilIcon className="size-2.5 opacity-40" />
              </Button>
            </>
          )}
        </ListCellLine>
      </TableCell>

      {/* 5h / 7d 放入同一个额度列，保持窗口之间的横向比较。 */}
      <TableCell className={cn(COL.quota, 'py-2.5')}>
        <div className="grid h-8 grid-cols-2 items-center gap-4">
          <ListQuotaMeter label="5h" util={u5h} reset={cred.quota?.rl_5h_reset ?? null} />
          <ListQuotaMeter label="7d" util={u7d} reset={cred.quota?.rl_7d_reset ?? null} />
        </div>
      </TableCell>

      <TableCell className={cn(COL.devices, 'py-2.5 tnum text-muted-foreground')}>
        <ListCellLine>
          {editingLimit ? (
            <CompactNumberEditor
              value={limitValue}
              pending={limit.isPending}
              label="设备上限"
              placeholder="默认"
              min={0}
              compact
              hint="留空 = 默认，0 = 不限"
              onChange={setLimitValue}
              onSubmit={saveLimit}
              onCancel={() => setEditingLimit(false)}
            />
          ) : (
            <Button
              type="button"
              size="sm"
              variant="ghost"
              className="inline-flex h-8 items-center gap-1.5 rounded px-1.5 tnum transition-colors hover:bg-muted hover:text-foreground"
              onClick={startLimitEdit}
              title="已绑定设备 / 设备上限；点击修改上限"
            >
              <DevicePhoneMobileIcon className="size-3.5" />
              {cred.device_count}/{deviceLimit}
              <PencilIcon className="size-2.5 opacity-40" />
            </Button>
          )}
        </ListCellLine>
      </TableCell>

      <TableCell className={cn(COL.activity, 'py-2.5')}>
        <ListCellLine className="gap-1.5 whitespace-nowrap text-2xs text-muted-foreground">
          <ClockIcon className="size-3.5 shrink-0" />
          <span className="text-foreground/80" title="最近一次转发使用">
            {cred.last_used != null ? relativeTime(cred.last_used) : '未使用'}
          </span>
          <span aria-hidden>·</span>
          <span className="tnum" title="累计等价 API 费用">{formatUsd(cred.cost_total)}</span>
        </ListCellLine>
      </TableCell>

      <TableCell className={cn(COL.actions, 'py-2.5')}>
        <ListCellLine className="justify-end">
          <CredentialRowActions
            cred={cred}
            actions={actions}
            showSwitch={false}
            onRename={() => setEditing(true)}
            onDeviceLimit={startLimitEdit}
            onTest={() => setTesting(true)}
            onRequestDelete={() => setConfirmDelete(true)}
          />
          {/* 弹窗用 Portal 渲染到 body，挂在 <td> 里不会破坏表格结构。 */}
          <DeleteCredentialDialog
            cred={cred}
            actions={actions}
            open={confirmDelete}
            onOpenChange={setConfirmDelete}
          />
          <ConnectivityTestDialog cred={cred} open={testing} onOpenChange={setTesting} />
        </ListCellLine>
      </TableCell>
    </TableRow>
    </>
  )
}

function ListCellLine({
  children,
  className,
}: {
  children: ReactNode
  className?: string
}) {
  return (
    <div className={cn('flex h-8 min-w-0 items-center', className)}>
      {children}
    </div>
  )
}

function ListCredentialStatus({
  status, expiry, nearLimit, abnormal, quotaMax, compact = false, showLabel = false,
}: {
  status: ReturnType<typeof statusMeta>
  expiry: ReturnType<typeof expiryMeta>
  nearLimit: boolean
  abnormal: boolean
  quotaMax: number
  compact?: boolean
  showLabel?: boolean
}) {
  const label = nearLimit && !abnormal ? `额度 ${Math.round(quotaMax * 100)}%` : status.label
  const detail = expiry.title
  const tooltip = detail ? `${label} · ${detail}` : label

  return (
    <span
      className={cn(
        'group/status relative inline-flex items-center justify-center outline-none',
        showLabel && 'gap-1.5',
      )}
      tabIndex={0}
      aria-label={tooltip}
    >
      <span
        className={cn(compact ? 'size-1.5' : 'size-2', 'shrink-0 rounded-full', status.dot)}
        aria-hidden
      />
      {showLabel && (
        <span
          className={cn(
            'text-2xs',
            nearLimit && !abnormal ? 'font-medium text-warn' : expiry.className,
          )}
        >
          {label}
        </span>
      )}
      <span
        role="tooltip"
        className="pointer-events-none invisible absolute bottom-full left-0 z-50 mb-2 w-max max-w-64 rounded-md bg-primary px-2.5 py-1.5 text-2xs font-normal text-primary-foreground opacity-0 shadow-md transition-[opacity,visibility] group-hover/status:visible group-hover/status:opacity-100 group-focus-visible/status:visible group-focus-visible/status:opacity-100"
      >
        {tooltip}
      </span>
    </span>
  )
}

function EditableCredentialName({
  cred, editing, name, pending, mobile = false,
  onNameChange, onStart, onCancel, onSubmit,
}: {
  cred: Credential
  editing: boolean
  name: string
  pending: boolean
  mobile?: boolean
  onNameChange: (name: string) => void
  onStart: () => void
  onCancel: () => void
  onSubmit: () => void
}) {
  if (editing) {
    return (
      <form
        className="flex min-w-0 items-center gap-1"
        onSubmit={(event) => { event.preventDefault(); onSubmit() }}
      >
        <Input
          value={name}
          onChange={(event) => onNameChange(event.target.value)}
          autoFocus
          className={cn('h-7 min-w-0 text-xs', mobile ? 'flex-1' : 'w-44')}
          aria-label="账号名称"
        />
        <Button type="submit" size="icon" variant="ghost" className="size-7" disabled={pending} aria-label="保存账号名称">
          {pending ? <ArrowPathIcon className="size-3.5 animate-spin" /> : <CheckIcon className="size-3.5" />}
        </Button>
        <Button type="button" size="icon" variant="ghost" className="size-7" aria-label="取消重命名" onClick={onCancel}>
          <XMarkIcon className="size-3.5" />
        </Button>
      </form>
    )
  }

  return (
    <div className="flex min-w-0 items-center gap-1.5">
      <Button
        type="button"
        size="sm"
        variant="link"
        onClick={onStart}
        className={cn(
          'h-5 min-w-0 justify-start truncate p-0 text-left font-medium',
          mobile ? 'text-sm' : 'text-xs',
        )}
        title={`${cred.label} · #${cred.id} · 点击重命名`}
      >
        {cred.label}
      </Button>
      <span
        className="shrink-0 rounded bg-muted px-1.5 py-0.5 font-mono text-[0.625rem] leading-none text-muted-foreground"
        title={`账号 ID：${cred.id}`}
      >
        #{cred.id}
      </span>
    </div>
  )
}

function CompactNumberEditor({
  value, pending, label, placeholder, min, compact = false, hint, onChange, onSubmit, onCancel,
}: {
  value: string
  pending: boolean
  label: string
  placeholder?: string
  min?: number
  compact?: boolean
  hint?: string
  onChange: (value: string) => void
  onSubmit: () => void
  onCancel: () => void
}) {
  return (
    <div className="inline-flex flex-col items-end gap-1">
      <form
        className={cn('inline-flex items-center justify-end', compact ? 'gap-px' : 'gap-0.5')}
        onSubmit={(event) => { event.preventDefault(); onSubmit() }}
      >
        <Input
          type="number"
          min={min}
          value={value}
          onChange={(event) => onChange(event.target.value)}
          onKeyDown={(event) => { if (event.key === 'Escape') onCancel() }}
          autoFocus
          placeholder={placeholder}
          title={hint}
          className={cn(
            'h-7 text-center font-mono text-2xs',
            compact
              ? 'w-9 rounded-none border-border bg-transparent px-1 shadow-none [border-width:0_0_1px_0] focus-visible:ring-0'
              : 'w-12 px-1.5',
          )}
          aria-label={label}
        />
        <Button type="submit" size="icon" variant="ghost" className={compact ? 'size-6' : 'size-7'} disabled={pending} aria-label={`保存${label}`}>
          {pending ? <ArrowPathIcon className="size-3 animate-spin" /> : <CheckIcon className="size-3" />}
        </Button>
        <Button type="button" size="icon" variant="ghost" className={compact ? 'size-6' : 'size-7'} onClick={onCancel} aria-label={`取消修改${label}`}>
          <XMarkIcon className="size-3" />
        </Button>
      </form>
      {hint && !compact && (
        <span className="text-[0.625rem] leading-4 text-muted-foreground">{hint}</span>
      )}
    </div>
  )
}

function ListQuotaMeter({
  label, util, reset,
}: {
  label: string
  util: number | null
  reset: number | null
}) {
  const emptyText = reset != null ? '已重置' : '未知'
  const emptyTitle = reset != null
    ? `${label}窗口已重置，之后暂无新请求`
    : `${label}额度暂无数据`

  if (util == null) {
    return (
      <div
        className="grid grid-cols-[1.5rem_minmax(0,1fr)_auto] items-center gap-2 text-2xs text-muted-foreground"
        title={emptyTitle}
      >
        <span className="font-medium text-foreground/70">{label}</span>
        <span className="h-1.5 rounded-full bg-border/70" aria-hidden />
        <span className="min-w-8 text-right">{emptyText}</span>
      </div>
    )
  }

  const percent = Math.min(100, Math.max(0, Math.round(util * 100)))
  const color = util >= 0.9 ? 'bg-bad' : util >= 0.7 ? 'bg-warn' : 'bg-ok'

  return (
    <div
      className="grid grid-cols-[1.5rem_minmax(0,1fr)_2rem] items-center gap-2 text-2xs"
      title={`${label}额度使用率 ${percent}%`}
    >
      <span className="font-medium text-foreground/70">{label}</span>
      <div
        className="h-1.5 overflow-hidden rounded-full bg-border/80"
        role="progressbar"
        aria-label={`${label}额度`}
        aria-valuemin={0}
        aria-valuemax={100}
        aria-valuenow={percent}
      >
        <div className={cn('h-full rounded-full', color)} style={{ width: `${percent}%` }} />
      </div>
      <span className="tnum text-right text-muted-foreground">{percent}%</span>
    </div>
  )
}

function CredentialRowActions({
  cred, actions, showSwitch = true, showMenu = true,
  onRename, onDeviceLimit, onTest, onRequestDelete,
}: {
  cred: Credential
  actions: CredentialActions
  showSwitch?: boolean
  showMenu?: boolean
  onRename: () => void
  onDeviceLimit: () => void
  onTest: () => void
  onRequestDelete: () => void
}) {
  const { toggle } = actions

  return (
    <div className="flex h-8 shrink-0 items-center justify-end gap-1">
      {showSwitch && <span className="relative inline-flex shrink-0 items-center">
        <Switch
          variant="success"
          checked={!cred.disabled}
          onCheckedChange={(on) => toggle.mutate(!on)}
          disabled={toggle.isPending}
          title={switchTitle(cred)}
          aria-label={switchTitle(cred)}
          className={cn(
            toggle.isPending && 'opacity-0',
            isAbnormal(cred) && 'data-[state=checked]:bg-muted-foreground/50',
          )}
        />
        {toggle.isPending && (
          <ArrowPathIcon className="absolute left-1/2 top-1/2 size-3.5 -translate-x-1/2 -translate-y-1/2 animate-spin text-muted-foreground" />
        )}
      </span>}
      {/* 菜单项会继续打开 Dialog；让 Dialog 独占 modal 指针锁，关闭后页面才能正常恢复。 */}
      {showMenu && <DropdownMenu modal={false}>
        <DropdownMenuTrigger asChild>
          <Button size="icon" variant="ghost" className="size-8 shrink-0 text-muted-foreground" aria-label={`打开 ${cred.label} 菜单`}>
            <EllipsisHorizontalIcon className="size-4" />
          </Button>
        </DropdownMenuTrigger>
        <CredentialMenuContent
          cred={cred}
          actions={actions}
          onRename={onRename}
          onDeviceLimit={onDeviceLimit}
          onTest={onTest}
          onRequestDelete={onRequestDelete}
        />
      </DropdownMenu>}
    </div>
  )
}
