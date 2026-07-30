import { useEffect, useRef, useState } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import { ArrowRightIcon, ExternalLinkIcon } from 'lucide-react'
import { getAuthorizeUrl, exchangeCode } from '@/api/credentials'
import { extractError } from '@/lib/utils'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Button } from '@/components/ui/button'
import {
  Dialog, DialogClose, DialogDescription, DialogFooter, DialogHeader,
  DialogPanel, DialogPopup, DialogTitle,
} from '@/components/ui/dialog'
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import { Form } from '@/components/ui/form'
import { Input } from '@/components/ui/input'
import { Textarea } from '@/components/ui/textarea'
import { toastManager } from '@/components/ui/toast'

interface AuthorizeRequest {
  session: number
  popup: Window | null
}

/** 添加账号弹窗：授权 → 粘贴 code#state → 可选备注 → 新增一条凭证。 */
export function AddAccount({
  open, onOpenChange,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const qc = useQueryClient()
  const [authUrl, setAuthUrl] = useState<string | null>(null)
  const [code, setCode] = useState('')
  const [label, setLabel] = useState('')
  const authorizeSession = useRef(0)

  const reset = () => {
    setCode('')
    setLabel('')
    setAuthUrl(null)
  }
  const handleOpenChange = (next: boolean) => {
    if (!next) reset()
    onOpenChange(next)
  }

  useEffect(() => {
    authorizeSession.current += 1
    if (open) reset()
  }, [open])

  const authorize = useMutation({
    mutationFn: (_request: AuthorizeRequest) => getAuthorizeUrl(),
    onSuccess: ({ url }, request) => {
      if (request.session !== authorizeSession.current) {
        request.popup?.close()
        return
      }
      setAuthUrl(url)
      if (request.popup && !request.popup.closed) {
        try {
          request.popup.location.replace(url)
        } catch {
          // 跨窗口导航失败时，弹窗内仍会显示可手动点击的授权链接。
        }
      }
    },
    onError: (error, request) => {
      request.popup?.close()
      if (request.session !== authorizeSession.current) return
      toastManager.add({
        title: '生成授权链接失败',
        description: extractError(error),
        type: 'error',
      })
    },
  })

  const exchange = useMutation({
    mutationFn: () => exchangeCode(code.trim(), label.trim() || undefined),
    onSuccess: (cred) => {
      toastManager.add({ title: '已添加账号', description: cred.label, type: 'success' })
      qc.invalidateQueries({ queryKey: ['credentials'] })
      handleOpenChange(false)
    },
    onError: (error) => toastManager.add({
      title: '添加失败',
      description: extractError(error),
      type: 'error',
    }),
  })

  const busy = authorize.isPending || exchange.isPending

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        if (!next && busy) return
        handleOpenChange(next)
      }}
    >
      <DialogPopup closeProps={{ disabled: busy }}>
        <DialogHeader>
          <DialogTitle>添加 Claude 账号</DialogTitle>
          <DialogDescription>
            完成 Claude OAuth 授权后，粘贴授权结果以接入订阅账号。
          </DialogDescription>
        </DialogHeader>
        <Form
          className="contents"
          onSubmit={(event) => {
            event.preventDefault()
            if (!exchange.isPending && code.trim()) exchange.mutate()
          }}
        >
          <DialogPanel className="space-y-6">
            <Field>
              <FieldLabel>1. 打开授权页面</FieldLabel>
              <FieldDescription>使用要接入的 Claude 订阅账号完成授权。</FieldDescription>
              <Button
                type="button"
                variant="outline"
                loading={authorize.isPending}
                onClick={() => {
                  const popup = window.open('about:blank', '_blank')
                  if (popup) popup.opener = null
                  authorize.mutate({ session: authorizeSession.current, popup })
                }}
              >
                <ExternalLinkIcon />
                打开 Claude 授权页
              </Button>
            </Field>

            {authUrl && (
              <Alert variant="info">
                <ExternalLinkIcon aria-hidden />
                <AlertTitle>授权页已在新标签打开</AlertTitle>
                <AlertDescription>
                  <p>
                    如果浏览器拦截了新标签页，可{' '}
                    <a href={authUrl} target="_blank" rel="noopener">
                      手动打开授权页面
                    </a>
                    。
                  </p>
                </AlertDescription>
              </Alert>
            )}

            <div className="space-y-4">
              <div className="font-medium text-sm">2. 提交授权结果</div>
              <Field name="code">
                <FieldLabel htmlFor="oauth-result">授权结果</FieldLabel>
                <Textarea
                  id="oauth-result"
                  name="code"
                  value={code}
                  onChange={(event) => setCode(event.target.value)}
                  placeholder="粘贴完整的 code#state"
                  className="min-h-24 font-mono"
                  required
                />
                <FieldDescription>请粘贴 Claude 授权完成后返回的完整内容。</FieldDescription>
              </Field>
              <Field name="label">
                <FieldLabel htmlFor="account-label">账号备注（可选）</FieldLabel>
                <Input
                  id="account-label"
                  name="label"
                  value={label}
                  onChange={(event) => setLabel(event.target.value)}
                  placeholder="留空时使用账号邮箱"
                />
              </Field>
            </div>
          </DialogPanel>
          <DialogFooter>
            <DialogClose render={<Button variant="ghost" />} disabled={busy}>
              取消
            </DialogClose>
            <Button
              type="submit"
              loading={exchange.isPending}
              disabled={!code.trim()}
            >
              <ArrowRightIcon />
              添加账号
            </Button>
          </DialogFooter>
        </Form>
      </DialogPopup>
    </Dialog>
  )
}
