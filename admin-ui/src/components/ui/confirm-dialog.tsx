import * as React from 'react'
import * as AlertDialogPrimitive from '@radix-ui/react-alert-dialog'
import { ArrowPathIcon, ExclamationTriangleIcon } from '@heroicons/react/24/outline'
import { Button } from '@/components/ui/button'
import { cn } from '@/lib/utils'

/**
 * 危险操作的二次确认框。
 *
 * 取代原生 `confirm()`：后者除了样式不可控，还有个真会咬人的坑——浏览器允许用户勾选
 * 「阻止此页面再弹出对话框」，勾上之后 `confirm()` 不弹窗直接返回 `false`，于是删除
 * 按钮变成点了没反应，用户只会以为按钮坏了。
 *
 * 用 AlertDialog 而非 Dialog：前者天然带 `role="alertdialog"`（读屏把整段后果说明当必读
 * 内容播报）、初始焦点落在 Cancel 上（回车不该把账号删掉）、且点遮罩不关闭——误触关掉
 * 一个确认框事小，但把「已确认」和「已取消」变得难以分辨事大。
 *
 * 确认按钮刻意不用 `AlertDialog.Action`：它点完立即关闭，提交中的转圈就没机会显示，
 * 也拦不住重复提交。这里用普通按钮，由调用方在成功后自己关。
 */
export function ConfirmDialog({
  open, onOpenChange, title, description, confirmText, pending = false, onConfirm,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  title: string
  description: React.ReactNode
  confirmText: string
  /** 提交中：按钮转圈并禁用，避免重复提交。 */
  pending?: boolean
  onConfirm: () => void
}) {
  return (
    <AlertDialogPrimitive.Root open={open} onOpenChange={onOpenChange}>
      <AlertDialogPrimitive.Portal>
        <AlertDialogPrimitive.Overlay
          className={cn(
            'fixed inset-0 z-50 bg-black/45 backdrop-blur-[2px]',
            'data-[state=open]:animate-in data-[state=closed]:animate-out data-[state=closed]:fade-out-0 data-[state=open]:fade-in-0',
          )}
        />
        <AlertDialogPrimitive.Content
          className={cn(
            'fixed inset-x-0 bottom-0 z-50 w-full max-w-md overflow-hidden rounded-t-xl border border-b-0 border-border bg-card text-card-foreground shadow-lg',
            'sm:bottom-auto sm:left-1/2 sm:right-auto sm:top-1/2 sm:w-[calc(100vw-2rem)] sm:-translate-x-1/2 sm:-translate-y-1/2 sm:rounded-lg sm:border-b',
            'data-[state=open]:animate-in data-[state=closed]:animate-out data-[state=closed]:fade-out-0 data-[state=open]:fade-in-0 data-[state=closed]:slide-out-to-bottom-4 data-[state=open]:slide-in-from-bottom-4 sm:data-[state=closed]:zoom-out-95 sm:data-[state=open]:zoom-in-95',
          )}
        >
          <div className="border-b border-border px-4 py-4 sm:px-5">
            <AlertDialogPrimitive.Title className="flex items-center gap-2 text-base font-semibold leading-none tracking-tight">
              <ExclamationTriangleIcon className="size-4 shrink-0 text-bad" />
              {title}
            </AlertDialogPrimitive.Title>
          </div>
          <div className="px-4 py-4 sm:px-5">
            <AlertDialogPrimitive.Description className="text-sm text-muted-foreground">
              {description}
            </AlertDialogPrimitive.Description>
            <div className="mt-5 grid grid-cols-2 gap-2 sm:flex sm:justify-end">
              <AlertDialogPrimitive.Cancel asChild>
                <Button variant="outline" size="sm" disabled={pending}>取消</Button>
              </AlertDialogPrimitive.Cancel>
              <Button variant="destructive" size="sm" onClick={onConfirm} disabled={pending}>
                {pending && <ArrowPathIcon className="size-3.5 animate-spin" />}
                {confirmText}
              </Button>
            </div>
          </div>
        </AlertDialogPrimitive.Content>
      </AlertDialogPrimitive.Portal>
    </AlertDialogPrimitive.Root>
  )
}
