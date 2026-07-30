import { useState } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import { ListChecksIcon, PauseIcon, PlayIcon, Trash2Icon, XIcon } from 'lucide-react'
import {
  deleteCredentials, setDeviceLimits, setDisabledMany, setPriorities,
  type Credential,
} from '@/api/credentials'
import { extractError } from '@/lib/utils'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import {
  AlertDialog, AlertDialogClose, AlertDialogDescription, AlertDialogFooter,
  AlertDialogHeader, AlertDialogPopup, AlertDialogTitle,
} from '@/components/ui/alert-dialog'
import { Button } from '@/components/ui/button'
import {
  Card, CardAction, CardDescription, CardHeader, CardPanel, CardTitle,
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
  all, selected, onSelectedChange, onClose,
}: {
  all: Credential[]
  selected: Set<number>
  onSelectedChange: (next: Set<number>) => void
  onClose: () => void
}) {
  const qc = useQueryClient()
  const [priority, setPriority] = useState(0)
  const [limitMode, setLimitMode] = useState<'default' | 'unlimited' | 'custom'>('default')
  const [customLimit, setCustomLimit] = useState(1)
  const [confirmDelete, setConfirmDelete] = useState(false)

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
  const none = n === 0
  const allSelected = all.length > 0 && n === all.length
  const deviceLimit = limitMode === 'default' ? 0 : limitMode === 'unlimited' ? -1 : Math.max(1, Math.floor(customLimit))

  return (
    <Card>
      <CardHeader>
        <CardTitle className="text-base">批量操作</CardTitle>
        <CardDescription aria-live="polite">
          已选 <span className="tnum font-medium text-foreground">{n}</span> / {all.length} 个账号
        </CardDescription>
        <CardAction>
          <Button size="icon-sm" variant="ghost" onClick={onClose} title="退出批量模式" aria-label="退出批量模式">
            <XIcon />
          </Button>
        </CardAction>
      </CardHeader>
      <CardPanel className="space-y-5">
        <div className="flex flex-wrap items-center justify-between gap-3">
          <label className="flex cursor-pointer items-center gap-2 text-sm font-medium">
            <Checkbox
              checked={allSelected}
              indeterminate={n > 0 && !allSelected}
              onCheckedChange={(checked) => onSelectedChange(checked ? new Set(all.map((item) => item.id)) : new Set())}
            />
            选择当前筛选结果
          </label>
          <Toolbar className="p-1">
            <Button size="sm" variant="outline" disabled={none || busy} loading={applyDisabled.isPending && applyDisabled.variables === false} onClick={() => applyDisabled.mutate(false)}>
              <PlayIcon />启用
            </Button>
            <Button size="sm" variant="outline" disabled={none || busy} loading={applyDisabled.isPending && applyDisabled.variables === true} onClick={() => applyDisabled.mutate(true)}>
              <PauseIcon />停用
            </Button>
            <Button size="sm" variant="destructive-outline" disabled={none || busy} onClick={() => setConfirmDelete(true)}>
              <Trash2Icon />删除
            </Button>
          </Toolbar>
        </div>

        {none && (
          <Alert>
            <ListChecksIcon />
            <AlertTitle>尚未选择账号</AlertTitle>
            <AlertDescription>先在当前筛选结果中勾选需要处理的账号。</AlertDescription>
          </Alert>
        )}

        <div className="grid gap-4 lg:grid-cols-2">
          <div className="space-y-2">
            <div>
              <div className="text-sm font-medium">调度优先级</div>
              <div className="text-xs text-muted-foreground">数值越小越优先</div>
            </div>
            <div className="flex items-end gap-2">
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
              <Button size="sm" loading={applyPriority.isPending} disabled={none || busy} onClick={() => applyPriority.mutate(priority)}>
                应用
              </Button>
            </div>
          </div>

          <div className="space-y-2">
            <div>
              <div className="text-sm font-medium">设备上限</div>
              <div className="text-xs text-muted-foreground">明确选择默认、不限或独立上限</div>
            </div>
            <div className="flex items-end gap-2">
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
              <Button size="sm" loading={applyLimit.isPending} disabled={none || busy} onClick={() => applyLimit.mutate(deviceLimit)}>
                应用
              </Button>
            </div>
          </div>
        </div>
      </CardPanel>

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
