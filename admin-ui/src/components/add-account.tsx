import { useEffect, useRef, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { ArrowRightIcon, CopyIcon, ExternalLinkIcon } from 'lucide-react'
import { getAuthorizeUrl, exchangeCode } from '@/api/credentials'
import { listProxies } from '@/api/proxies'
import { useI18n } from '@/lib/i18n'
import { copyText, displayCredentialLabel, extractError } from '@/lib/utils'
import { ProxyTestBlock } from '@/components/credential-proxy-dialog'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import {
  Combobox,
  ComboboxItem,
  ComboboxPopup,
  ComboboxTrigger,
  ComboboxValue,
} from '@/components/ui/combobox'
import { Button } from '@/components/ui/button'
import {
  Dialog, DialogClose, DialogDescription, DialogFooter, DialogHeader,
  DialogPanel, DialogPopup, DialogTitle,
} from '@/components/ui/dialog'
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import { Label } from '@/components/ui/label'
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
      qc.invalidateQueries({ queryKey: ['proxies'] })
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
                {/* 代理池选择与 [CredentialProxyDialog] 同一套 Combobox：代理池一多，
                    平铺的 chip 会把整个表单挤长，且标签常是「1」这种看不出所以然的名字——
                    下拉里能同时给出标签、URL 与使用者，还能搜。 */}
                {savedProxies.length > 0 && (
                  <div className="w-full space-y-2">
                    <Label>{t('从代理池选择', 'Pick from proxy pool')}</Label>
                    <div className="flex w-full items-center gap-2">
                      <Combobox
                        value={savedProxies.find((p) => p.url === proxy.trim())?.id ?? null}
                        onValueChange={(id) => {
                          const found = savedProxies.find((p) => p.id === id)
                          if (found) setProxy(found.url)
                        }}
                        itemToStringLabel={(id) => {
                          const p = savedProxies.find((px) => px.id === id)
                          return p ? p.label : ''
                        }}
                        itemToStringValue={(id) => {
                          const p = savedProxies.find((px) => px.id === id)
                          return p ? `${p.label} ${p.url} ${p.credential_labels.join(' ')}` : ''
                        }}
                      >
                        <ComboboxTrigger className="w-full min-w-0 flex-1">
                          <ComboboxValue placeholder={t('选择代理…', 'Select a proxy…')} />
                        </ComboboxTrigger>
                        <ComboboxPopup
                          inputPlaceholder={t('搜索标签、地址或使用账号…', 'Search label, URL, or account…')}
                          emptyText={t('无匹配结果', 'No matches')}
                        >
                          {savedProxies.map((p) => (
                            <ComboboxItem key={p.id} value={p.id}>
                              <div className="min-w-0">
                                <div className="flex items-baseline gap-2">
                                  <span className="truncate font-medium">{p.label}</span>
                                  {p.credential_count > 0 && (
                                    <span className="shrink-0 text-xs text-muted-foreground">
                                      {p.credential_count} {t('账号', 'acct')}
                                    </span>
                                  )}
                                </div>
                                <div className="truncate text-xs text-muted-foreground">{p.url}</div>
                                {p.credential_labels.length > 0 && (
                                  <div className="mt-0.5 truncate text-xs text-muted-foreground">
                                    {p.credential_labels.join(', ')}
                                  </div>
                                )}
                              </div>
                            </ComboboxItem>
                          ))}
                        </ComboboxPopup>
                      </Combobox>
                      {proxy.trim() && (
                        <Button
                          type="button"
                          size="sm"
                          variant="ghost"
                          className="shrink-0"
                          onClick={() => setProxy('')}
                        >
                          {t('清除', 'Clear')}
                        </Button>
                      )}
                    </div>
                  </div>
                )}
                <Label htmlFor="account-proxy" className="mt-1">
                  {t('或手动填写地址', 'Or enter an address')}
                </Label>
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
                <ProxyTestBlock url={proxy.trim()} />
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
