import { useState } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import {
  ArrowTopRightOnSquareIcon, ArrowRightIcon, ArrowPathIcon, XMarkIcon,
} from '@heroicons/react/24/outline'
import { toast } from 'sonner'
import { getAuthorizeUrl, exchangeCode } from '@/api/credentials'
import { extractError } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Card, CardContent, CardHeader, CardTitle } from '@/components/ui/card'
import { Textarea } from '@/components/ui/textarea'
import { Input } from '@/components/ui/input'

/** 添加账号面板：授权 → 粘贴 code#state → 可选备注 → 新增一条凭证。 */
export function AddAccount({ onClose }: { onClose: () => void }) {
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
      onClose()
    },
    onError: (e) => toast.error('添加失败', { description: extractError(e) }),
  })

  return (
    <Card>
      <CardHeader className="flex flex-row items-center justify-between space-y-0">
        <CardTitle>添加 Claude 账号</CardTitle>
        <Button size="icon" variant="ghost" className="h-8 w-8" onClick={onClose} title="收起">
          <XMarkIcon />
        </Button>
      </CardHeader>
      <CardContent className="space-y-5">
        <div className="space-y-2.5">
          <Step n={1} text="打开 Claude 授权页" />
          <Button onClick={() => authorize.mutate()} disabled={authorize.isPending}>
            {authorize.isPending ? <ArrowPathIcon className="animate-spin" /> : <ArrowTopRightOnSquareIcon />}
            生成授权链接并打开
          </Button>
          {authUrl && (
            <p className="text-xs text-muted-foreground">
              没弹出新标签页？手动点击：{' '}
              <a href={authUrl} target="_blank" rel="noopener" className="break-all text-primary underline-offset-2 hover:underline">
                {authUrl}
              </a>
            </p>
          )}
        </div>

        <div className="space-y-2.5">
          <Step n={2} text="粘贴授权结果" />
          <p className="text-xs text-muted-foreground">
            授权后页面会显示形如 <code className="font-mono">code#state</code> 的文本，整段粘贴到下面。
          </p>
          <Textarea value={code} onChange={(e) => setCode(e.target.value)} placeholder="在此粘贴 code#state" />
          <Input value={label} onChange={(e) => setLabel(e.target.value)} placeholder="账号备注（可选，留空则用账号邮箱自动命名）" />
          <Button onClick={() => exchange.mutate()} disabled={exchange.isPending || !code.trim()}>
            {exchange.isPending ? <ArrowPathIcon className="animate-spin" /> : <ArrowRightIcon />}
            完成添加
          </Button>
        </div>
      </CardContent>
    </Card>
  )
}

function Step({ n, text }: { n: number; text: string }) {
  return (
    <div className="flex items-center gap-2">
      <span className="flex h-6 w-6 items-center justify-center rounded-full bg-muted text-xs font-semibold text-foreground">
        {n}
      </span>
      <span className="text-sm font-medium">{text}</span>
    </div>
  )
}
