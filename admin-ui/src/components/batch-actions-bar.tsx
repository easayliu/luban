import { useState } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import { ChevronDownIcon, PauseIcon, PlayIcon, Trash2Icon, XIcon } from 'lucide-react'
import {
  deleteCredentials, setDeviceLimits, setDisabledMany, setPriorities,
  type Credential,
} from '@/api/credentials'
import { cn, extractError } from '@/lib/utils'
import {
  AlertDialog, AlertDialogClose, AlertDialogDescription, AlertDialogFooter,
  AlertDialogHeader, AlertDialogPopup, AlertDialogTitle,
} from '@/components/ui/alert-dialog'
import { Button } from '@/components/ui/button'
import {
  Card,
} from '@/components/ui/card'
import { Checkbox } from '@/components/ui/checkbox'
import {
  NumberField, NumberFieldDecrement, NumberFieldGroup, NumberFieldIncrement,
  NumberFieldInput,
} from '@/components/ui/number-field'
import { Select, SelectItem, SelectPopup, SelectTrigger, SelectValue } from '@/components/ui/select'
import { toastManager } from '@/components/ui/toast'
import { Toolbar } from '@/components/ui/toolbar'

const LIMIT_MODE_ITEMS = [
  { value: 'default', label: '跟随默认' },
  { value: 'unlimited', label: '不限设备' },
  { value: 'custom', label: '独立上限' },
] as const

/**
 * 批量操作条：全选/清空 + 优先级 / 设备上限 / 启停 / 删除。
 *
 * 所有操作都作用于**当前筛选结果里被勾选的账号**（跨页保留勾选）。写操作走各自的批量
 * 接口，后端在单事务内完成，不会出现「改了一半」的中间态。
 */
export function BatchActionsBar({
  all, selected, onSelectedChange, onClear,
}: {
  all: Credential[]
  selected: Set<number>
  onSelectedChange: (next: Set<number>) => void
  onClear: () => void
}) {
  const qc = useQueryClient()
  const [priority, setPriority] = useState(0)
  const [limitMode, setLimitMode] = useState<'default' | 'unlimited' | 'custom'>('default')
  const [customLimit, setCustomLimit] = useState(1)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [advancedOpen, setAdvancedOpen] = useState(false)

  const ids = [...selected]
  const n = selected.size
  const notify = (msg: string, clearSelection = false) => {
    toastManager.add({ title: msg, type: 'success' })
    qc.invalidateQueries({ queryKey: ['credentials'] })
    if (clearSelection) onSelectedChange(new Set())
  }
  const onError = (error: unknown) => toastManager.add({
    title: '批量操作失败',
    description: extractError(error),
    type: 'error',
  })

  const applyPriority = useMutation({
    mutationFn: (p: number) => setPriorities(ids, p),
    onSuccess: (_r, p) => notify(`已把 ${n} 个账号设为 P${p}`),
    onError,
  })
  const applyLimit = useMutation({
    mutationFn: (v: number) => setDeviceLimits(ids, v),
    onSuccess: (_r, v) =>
      notify(
        v > 0 ? `已把 ${n} 个账号的设备上限设为 ${v}`
          : v === 0 ? `已把 ${n} 个账号改为跟随全局默认上限`
          : `已把 ${n} 个账号设为不限设备数`,
      ),
    onError,
  })
  const applyDisabled = useMutation({
    mutationFn: (d: boolean) => setDisabledMany(ids, d),
    onSuccess: (_r, d) => notify(`已${d ? '停用' : '启用'} ${n} 个账号`),
    onError,
  })
  const applyDelete = useMutation({
    mutationFn: () => deleteCredentials(ids),
    // 账号已不存在，留着勾选没有意义，顺手清空。批量条不会随之卸载，确认框得自己关。
    onSuccess: () => { setConfirmDelete(false); notify(`已删除 ${n} 个账号`, true) },
    onError: (e) => { setConfirmDelete(false); onError(e) },
  })

  const busy =
    applyPriority.isPending || applyLimit.isPending ||
    applyDisabled.isPending || applyDelete.isPending
  const allSelected = all.length > 0 && all.every((item) => selected.has(item.id))
  const deviceLimit = limitMode === 'default' ? 0 : limitMode === 'unlimited' ? -1 : Math.max(1, Math.floor(customLimit))

  return (
    <Card render={<section aria-label="批量操作" />} className="rounded-xl">
        <div className="flex min-h-14 flex-wrap items-center gap-3 p-3">
          <label className="mr-auto flex cursor-pointer items-center gap-2 text-xs">
            <Checkbox
              checked={allSelected}
              indeterminate={n > 0 && !allSelected}
              onCheckedChange={(checked) => onSelectedChange(checked ? new Set(all.map((item) => item.id)) : new Set())}
            />
            <span aria-live="polite">
              已选 <span className="tnum font-semibold text-foreground">{n}</span>
              <span className="text-muted-foreground"> / {all.length}</span>
            </span>
          </label>

          <Toolbar className="gap-3 border-0 bg-transparent p-0 shadow-none">
            <Button size="sm" variant="outline" aria-label="启用所选账号" disabled={busy} loading={applyDisabled.isPending && applyDisabled.variables === false} onClick={() => applyDisabled.mutate(false)}>
              <PlayIcon /><span className="max-sm:sr-only">启用</span>
            </Button>
            <Button size="sm" variant="outline" aria-label="停用所选账号" disabled={busy} loading={applyDisabled.isPending && applyDisabled.variables === true} onClick={() => applyDisabled.mutate(true)}>
              <PauseIcon /><span className="max-sm:sr-only">停用</span>
            </Button>
            <Button size="sm" variant="destructive-outline" aria-label="删除所选账号" disabled={busy} onClick={() => setConfirmDelete(true)}>
              <Trash2Icon /><span className="max-sm:sr-only">删除</span>
            </Button>
          </Toolbar>

          <Button
            size="sm"
            variant={advancedOpen ? 'secondary' : 'outline'}
            aria-expanded={advancedOpen}
            aria-controls="batch-advanced-settings"
            onClick={() => setAdvancedOpen((open) => !open)}
          >
            更多设置
            <ChevronDownIcon className={cn('size-4 transition-transform', advancedOpen && 'rotate-180')} />
          </Button>

          <Button size="icon-sm" variant="ghost" onClick={onClear} title="清空选择" aria-label="清空选择">
            <XIcon />
          </Button>
        </div>

        {advancedOpen && (
          <div id="batch-advanced-settings" className="grid gap-3 border-t p-3 lg:grid-cols-2">
            <div className="flex flex-wrap items-center gap-2">
              <div className="mr-auto min-w-28">
                <div className="text-xs font-medium">调度优先级</div>
                <div className="text-xs text-muted-foreground">数值越小越优先</div>
              </div>
              <NumberField
                id="batch-priority"
                value={priority}
                min={0}
                step={1}
                size="sm"
                onValueChange={(value) => setPriority(Math.max(0, Math.floor(value ?? 0)))}
              >
                <NumberFieldGroup>
                  <NumberFieldDecrement />
                  <NumberFieldInput aria-label="批量设置优先级" />
                  <NumberFieldIncrement />
                </NumberFieldGroup>
              </NumberField>
              <Button size="sm" loading={applyPriority.isPending} disabled={busy} onClick={() => applyPriority.mutate(priority)}>
                应用
              </Button>
            </div>

            <div className="flex flex-wrap items-center gap-2 lg:border-l lg:pl-3">
              <div className="mr-auto min-w-28">
                <div className="text-xs font-medium">设备上限</div>
                <div className="text-xs text-muted-foreground">默认、不限或独立上限</div>
              </div>
              <Select items={LIMIT_MODE_ITEMS} value={limitMode} onValueChange={(value) => value && setLimitMode(value as typeof limitMode)}>
                <SelectTrigger aria-label="批量设置设备上限策略" size="sm" className="min-w-28"><SelectValue /></SelectTrigger>
                <SelectPopup>
                  {LIMIT_MODE_ITEMS.map((item) => (
                    <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>
                  ))}
                </SelectPopup>
              </Select>
              {limitMode === 'custom' && (
                <NumberField value={customLimit} min={1} step={1} size="sm" onValueChange={(value) => setCustomLimit(Math.max(1, Math.floor(value ?? 1)))}>
                  <NumberFieldGroup>
                    <NumberFieldDecrement />
                    <NumberFieldInput aria-label="批量设置独立设备上限" />
                    <NumberFieldIncrement />
                  </NumberFieldGroup>
                </NumberField>
              )}
              <Button size="sm" loading={applyLimit.isPending} disabled={busy} onClick={() => applyLimit.mutate(deviceLimit)}>
                应用
              </Button>
            </div>
          </div>
        )}

        <AlertDialog open={confirmDelete} onOpenChange={setConfirmDelete}>
          <AlertDialogPopup>
            <AlertDialogHeader>
              <AlertDialogTitle>删除 {n} 个账号</AlertDialogTitle>
              <AlertDialogDescription>
                确定删除选中的 {n} 个账号？历史用量记录与设备绑定将一并清除，且无法恢复。
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogClose render={<Button variant="outline" />}>取消</AlertDialogClose>
              <Button variant="destructive" loading={applyDelete.isPending} onClick={() => applyDelete.mutate()}>
                删除 {n} 个
              </Button>
            </AlertDialogFooter>
          </AlertDialogPopup>
        </AlertDialog>
    </Card>
  )
}
