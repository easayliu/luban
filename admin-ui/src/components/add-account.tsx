import { useEffect, useRef, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { ArrowRightIcon, CopyIcon, ExternalLinkIcon } from 'lucide-react'
import { getAuthorizeUrl, exchangeCode } from '@/api/credentials'
import { listProxies } from '@/api/proxies'
import { useI18n } from '@/lib/i18n'
import { copyText, displayCredentialLabel, extractError } from '@/lib/utils'
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
}

/** 添加账号弹窗：授权 → 粘贴 code#state → 可选备注 → 新增一条凭证。 */
export function AddAccount({
  open, onOpenChange,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  const [authUrl, setAuthUrl] = useState<string | null>(null)
  const [code, setCode] = useState('')
  const [label, setLabel] = useState('')
  const [proxy, setProxy] = useState('')
  const authorizeSession = useRef(0)

  const reset = () => {
    setCode('')
    setLabel('')
    setProxy('')
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
      if (request.session !== authorizeSession.current) return
      setAuthUrl(url)
    },
    onError: (error, request) => {
      if (request.session !== authorizeSession.current) return
      toastManager.add({
        title: t('生成授权链接失败', 'Failed to create authorization link'),
        description: extractError(error, language),
        type: 'error',
      })
    },
  })

  const exchange = useMutation({
    mutationFn: () => exchangeCode(code.trim(), label.trim() || undefined, proxy.trim() || undefined),
    onSuccess: (cred) => {
      toastManager.add({
        title: t('已添加账号', 'Account added'),
        description: displayCredentialLabel(cred.label, language),
        type: 'success',
      })
      qc.invalidateQueries({ queryKey: ['credentials'] })
      handleOpenChange(false)
    },
    onError: (error) => toastManager.add({
      title: t('添加失败', 'Failed to add account'),
      description: extractError(error, language),
      type: 'error',
    }),
  })

  const proxiesQuery = useQuery({
    queryKey: ['proxies'],
    queryFn: listProxies,
    enabled: open,
  })
  const savedProxies = proxiesQuery.data ?? []

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
          <DialogTitle>{t('添加 Claude 账号', 'Add Claude account')}</DialogTitle>
          <DialogDescription>
            {t(
              '完成 Claude OAuth 授权后，粘贴授权结果以接入订阅账号。',
              'Complete Claude OAuth authorization, then paste the result to connect a subscription account.',
            )}
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
              <FieldLabel>{t('1. 打开授权页面', '1. Open the authorization page')}</FieldLabel>
              <FieldDescription>
                {t(
                  '使用要接入的 Claude 订阅账号完成授权。',
                  'Authorize with the Claude subscription account you want to connect.',
                )}
              </FieldDescription>
              <Button
                type="button"
                variant="outline"
                loading={authorize.isPending}
                onClick={() => {
                  authorize.mutate({ session: authorizeSession.current })
                }}
              >
                <ExternalLinkIcon />
                {t('生成授权链接', 'Generate authorization link')}
              </Button>
            </Field>

            {authUrl && (
              <Alert variant="info">
                <ExternalLinkIcon aria-hidden />
                <AlertTitle>{t('授权链接已生成', 'Authorization link ready')}</AlertTitle>
                <AlertDescription>
                  <p>
                    {t(
                      '点击下方链接打开授权页面，或复制链接到其它浏览器/设备上完成授权。',
                      'Click the link below to open the authorization page, or copy it to another browser or device.',
                    )}
                  </p>
                  <div className="mt-2 flex items-center gap-2">
                    <a href={authUrl} target="_blank" rel="noopener">
                      <Button type="button" size="sm" variant="outline">
                        <ExternalLinkIcon />
                        {t('打开授权页面', 'Open authorization page')}
                      </Button>
                    </a>
                    <Button
                      type="button"
                      size="sm"
                      variant="outline"
                      onClick={async () => {
                        const copied = await copyText(authUrl)
                        toastManager.add(copied
                          ? { title: t('已复制授权链接', 'Authorization link copied'), type: 'success' }
                          : {
                              title: t('复制失败，请手动复制', 'Copy failed; copy the link manually'),
                              description: authUrl,
                              type: 'error',
                            })
                      }}
                    >
                      <CopyIcon />
                      {t('复制链接', 'Copy link')}
                    </Button>
                  </div>
                </AlertDescription>
              </Alert>
            )}

            <div className="space-y-4">
              <div className="font-medium text-sm">
                {t('2. 提交授权结果', '2. Submit the authorization result')}
              </div>
              <Field name="code">
                <FieldLabel htmlFor="oauth-result">{t('授权结果', 'Authorization result')}</FieldLabel>
                <Textarea
                  id="oauth-result"
                  name="code"
                  value={code}
                  onChange={(event) => setCode(event.target.value)}
                  placeholder={t('粘贴完整的 code#state', 'Paste the complete code#state')}
                  className="min-h-24"
                  required
                />
                <FieldDescription>
                  {t(
                    '请粘贴 Claude 授权完成后返回的完整内容。',
                    'Paste the complete value returned after Claude authorization.',
                  )}
                </FieldDescription>
              </Field>
              <Field name="label">
                <FieldLabel htmlFor="account-label">
                  {t('账号备注（可选）', 'Account label (optional)')}
                </FieldLabel>
                <Input
                  id="account-label"
                  name="label"
                  value={label}
                  onChange={(event) => setLabel(event.target.value)}
                  placeholder={t('留空时使用账号邮箱', 'Leave blank to use the account email')}
                />
              </Field>
              <Field name="proxy">
                <FieldLabel htmlFor="account-proxy">
                  {t('出站代理（可选）', 'Outbound proxy (optional)')}
                </FieldLabel>
                {savedProxies.length > 0 && (
                  <div className="flex flex-wrap gap-1.5">
                    {savedProxies.map((p) => (
                      <Button
                        key={p.id}
                        type="button"
                        size="sm"
                        variant={proxy.trim() === p.url ? 'secondary' : 'outline'}
                        onClick={() => setProxy(p.url)}
                        title={p.url}
                      >
                        {p.label}
                      </Button>
                    ))}
                    {proxy.trim() && (
                      <Button
                        type="button"
                        size="sm"
                        variant="ghost"
                        onClick={() => setProxy('')}
                      >
                        {t('清除', 'Clear')}
                      </Button>
                    )}
                  </div>
                )}
                <Input
                  id="account-proxy"
                  name="proxy"
                  value={proxy}
                  onChange={(event) => setProxy(event.target.value)}
                  placeholder="socks5://127.0.0.1:1080"
                  spellCheck={false}
                  autoComplete="off"
                />
                <FieldDescription>
                  {t(
                    '登录换码和拉取账号信息会走此代理，入库后自动存为该账号的逐账号代理。支持 socks5://、http:// 等，留空表示直连。',
                    'Token exchange and profile fetch will go through this proxy. It is automatically saved as the per-account proxy after login. Supports socks5://, http://, etc. Leave blank for a direct connection.',
                  )}
                </FieldDescription>
              </Field>
            </div>
          </DialogPanel>
          <DialogFooter>
            <DialogClose render={<Button variant="ghost" />} disabled={busy}>
              {t('取消', 'Cancel')}
            </DialogClose>
            <Button
              type="submit"
              loading={exchange.isPending}
              disabled={!code.trim()}
            >
              <ArrowRightIcon />
              {t('添加账号', 'Add account')}
            </Button>
          </DialogFooter>
        </Form>
      </DialogPopup>
    </Dialog>
  )
}
