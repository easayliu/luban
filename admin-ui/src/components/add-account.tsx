import { useState } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import {
  ArrowTopRightOnSquareIcon, ArrowRightIcon, ArrowPathIcon,
} from '@heroicons/react/24/outline'
import { toast } from 'sonner'
import { getAuthorizeUrl, exchangeCode } from '@/api/credentials'
import { extractError } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import {
  Dialog, DialogContent, DialogHeader, DialogBody, DialogTitle, DialogDescription,
} from '@/components/ui/dialog'
import { Textarea } from '@/components/ui/textarea'
import { Input } from '@/components/ui/input'

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

  const authorize = useMutation({
    mutationFn: getAuthorizeUrl,
    onSuccess: ({ url }) => { setAuthUrl(url); window.open(url, '_blank', 'noopener') },
    onError: (e) => toast.error('生成授权链接失败', { description: extractError(e) }),
  })

  const exchange = useMutation({
    mutationFn: () => exchangeCode(code.trim(), label.trim() || undefined),
    onSuccess: (cred) => {
      toast.success('已添加账号', { description: cred.label })
      qc.invalidateQueries({ queryKey: ['credentials'] })
      // 关闭并复位，下次打开是干净的表单（授权码是一次性的）。
      setCode('')
      setLabel('')
      setAuthUrl(null)
      onOpenChange(false)
    },
    onError: (e) => toast.error('添加失败', { description: extractError(e) }),
  })

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent>
        <DialogHeader>
          <DialogTitle>添加 Claude 账号</DialogTitle>
          <DialogDescription>完成 OAuth 授权后，账号会加入当前调度池。</DialogDescription>
        </DialogHeader>
        <DialogBody className="space-y-3">
          <Step n={1} title="打开授权页面">
            <p className="text-xs leading-5 text-muted-foreground">使用需要接入的 Claude 订阅账号完成授权。</p>
            <Button className="w-full sm:w-auto" onClick={() => authorize.mutate()} disabled={authorize.isPending}>
              {authorize.isPending ? <ArrowPathIcon className="animate-spin" /> : <ArrowTopRightOnSquareIcon />}
              打开 Claude 授权页
            </Button>
            {authUrl && (
              <div className="rounded-md border border-border bg-muted/40 px-3 py-2 text-xs text-muted-foreground">
                新标签页未打开时，可{' '}
                <a href={authUrl} target="_blank" rel="noopener" className="font-medium text-foreground underline underline-offset-2">
                  手动打开授权页面
                </a>
              </div>
            )}
          </Step>

          <Step n={2} title="提交授权结果">
            <label className="block space-y-1.5 text-sm font-medium" htmlFor="oauth-result">
              <span>授权结果</span>
              <Textarea
                id="oauth-result"
                value={code}
                onChange={(e) => setCode(e.target.value)}
                placeholder="粘贴完整的 code#state"
                className="min-h-24"
              />
            </label>
            <label className="block space-y-1.5 text-sm font-medium" htmlFor="account-label">
              <span>账号备注 <span className="font-normal text-muted-foreground">（可选）</span></span>
              <Input
                id="account-label"
                value={label}
                onChange={(e) => setLabel(e.target.value)}
                placeholder="留空时使用账号邮箱"
              />
            </label>
            <div className="flex justify-end">
              <Button className="w-full sm:w-auto" onClick={() => exchange.mutate()} disabled={exchange.isPending || !code.trim()}>
                {exchange.isPending ? <ArrowPathIcon className="animate-spin" /> : <ArrowRightIcon />}
                添加账号
              </Button>
            </div>
          </Step>
        </DialogBody>
      </DialogContent>
    </Dialog>
  )
}

function Step({ n, title, children }: { n: number; title: string; children: React.ReactNode }) {
  return (
    <div className="rounded-lg border border-border bg-card p-4">
      <div className="mb-3 flex items-center gap-2.5">
        <span className="flex size-6 items-center justify-center rounded-md bg-primary text-xs font-semibold text-primary-foreground">{n}</span>
        <span className="text-sm font-semibold">{title}</span>
      </div>
      <div className="space-y-3 pl-0 sm:pl-8">{children}</div>
    </div>
  )
}
