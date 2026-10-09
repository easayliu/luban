import { useEffect, useState } from 'react'
import { useMutation } from '@tanstack/react-query'
import { changePassword } from '@/api/auth'
import { useI18n } from '@/lib/i18n'
import { extractError } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import {
  Dialog,
  DialogClose,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogPanel,
  DialogPopup,
  DialogTitle,
} from '@/components/ui/dialog'
import { Field, FieldDescription, FieldError, FieldLabel } from '@/components/ui/field'
import { Form } from '@/components/ui/form'
import { toastManager } from '@/components/ui/toast'
import { MIN_PASSWORD_LENGTH, PasswordInput } from '@/components/password-input'

/**
 * 代理和用户改自己的密码。改成功后其它设备上的登录全部下线，当前这个保留。
 *
 * 管理员不走这里：它的密码在系统设置的「控制台安全」里改，那边还能清除；访客的密码由管理员设。
 */
export function ChangePasswordDialog({
  open,
  onOpenChange,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const { t, language } = useI18n()
  const [password, setPassword] = useState('')
  const [confirm, setConfirm] = useState('')

  // 每次打开都清空：上次输了一半的密码不该留到下次。
  useEffect(() => {
    if (!open) return
    setPassword('')
    setConfirm('')
  }, [open])

  const save = useMutation({
    mutationFn: () => changePassword(password.trim()),
    onSuccess: () => {
      onOpenChange(false)
      toastManager.add({
        title: t('密码已修改', 'Password changed'),
        description: t('其他设备上的登录已退出。', 'Sign-ins on other devices have been signed out.'),
        type: 'success',
      })
    },
  })

  const tooShort = password.trim().length > 0 && password.trim().length < MIN_PASSWORD_LENGTH
  const mismatch = confirm.length > 0 && confirm.trim() !== password.trim()
  const canSubmit = password.trim().length >= MIN_PASSWORD_LENGTH && confirm.trim() === password.trim()

  return (
    <Dialog open={open} onOpenChange={(next) => { if (!save.isPending) onOpenChange(next) }}>
      <DialogPopup>
        <DialogHeader>
          <DialogTitle>{t('修改密码', 'Change password')}</DialogTitle>
          <DialogDescription>
            {t('修改后，其他设备上的登录将退出。', 'Other devices are signed out after the change.')}
          </DialogDescription>
        </DialogHeader>
        <Form
          className="contents"
          onSubmit={(event) => {
            event.preventDefault()
            if (canSubmit) save.mutate()
          }}
        >
          <DialogPanel className="space-y-4">
            <Field invalid={tooShort}>
              <FieldLabel>{t('新密码', 'New password')}</FieldLabel>
              <PasswordInput autoFocus invalid={tooShort} onChange={setPassword} value={password} />
              <FieldDescription>
                {t(`至少 ${MIN_PASSWORD_LENGTH} 个字符。`, `At least ${MIN_PASSWORD_LENGTH} characters.`)}
              </FieldDescription>
            </Field>
            <Field invalid={mismatch || save.isError}>
              <FieldLabel>{t('确认新密码', 'Confirm new password')}</FieldLabel>
              <PasswordInput invalid={mismatch || save.isError} onChange={setConfirm} value={confirm} />
              {mismatch && <FieldError match>{t('两次输入的密码不一致', 'The passwords do not match')}</FieldError>}
              {save.isError && !mismatch && <FieldError match>{extractError(save.error, language)}</FieldError>}
            </Field>
          </DialogPanel>
          <DialogFooter>
            <DialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</DialogClose>
            <Button disabled={!canSubmit} loading={save.isPending} type="submit">
              {t('保存', 'Save')}
            </Button>
          </DialogFooter>
        </Form>
      </DialogPopup>
    </Dialog>
  )
}
