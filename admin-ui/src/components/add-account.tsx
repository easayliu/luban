import { useEffect, useRef, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { ArrowRightIcon, CopyIcon, ExternalLinkIcon, RefreshCwIcon } from 'lucide-react'
import { getAuthorizeUrl, exchangeCode } from '@/api/credentials'
import { listProxies } from '@/api/proxies'
import { useI18n } from '@/lib/i18n'
import { copyText, displayCredentialLabel, extractError } from '@/lib/utils'
import { ProxyPickerCombobox, ProxyTestBlock } from '@/components/credential-proxy-dialog'
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
  // 选中的是代理池里的一条时，下拉框已经显示着它，手动输入框再摆一份同样的地址就是重复。
  const pickedFromPool = savedProxies.some((item) => item.url === proxy.trim())

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
          {/* 不再挂一句「完成授权后粘贴授权结果」：下面的 1、2 两步就是这句话本身。说明留给读屏。 */}
          <DialogDescription className="sr-only">
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
              {/* 生成之后按钮原地换成「打开 / 复制」，不再另起一块「授权链接已生成」的提示：那块提示的标题、
                  说明、按钮（「打开授权页面」与本步标题一字不差）说的都是同一件事。 */}
              {authUrl ? (
                <div className="flex flex-wrap items-center gap-2">
                  <a href={authUrl} target="_blank" rel="noopener">
                    <Button type="button" variant="outline">
                      <ExternalLinkIcon />
                      {t('打开授权页面', 'Open authorization page')}
                    </Button>
                  </a>
                  <Button
                    type="button"
                    variant="outline"
                    title={t('复制后可在其他浏览器或设备上完成授权', 'Copy it to authorize in another browser or on another device')}
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
                  {/* 链接有时效，授权页开久了会过期；关掉弹窗再开也行，这里给个就近的出口。 */}
                  <Button
                    type="button"
                    variant="ghost"
                    size="sm"
                    loading={authorize.isPending}
                    onClick={() => authorize.mutate({ session: authorizeSession.current })}
                  >
                    <RefreshCwIcon />
                    {t('重新生成', 'Regenerate')}
                  </Button>
                </div>
              ) : (
                <Button
                  type="button"
                  variant="outline"
                  className="w-fit"
                  loading={authorize.isPending}
                  onClick={() => {
                    authorize.mutate({ session: authorizeSession.current })
                  }}
                >
                  <ExternalLinkIcon />
                  {t('生成授权链接', 'Generate authorization link')}
                </Button>
              )}
            </Field>

            <div className="space-y-4">
              {/* 步骤标题直接当输入框的标签：原来是「2. 提交授权结果」标题 + 「授权结果」标签 + 「请粘贴…
                  完整内容」说明 + 「粘贴完整的 code#state」占位，一件事说四遍。 */}
              <Field name="code">
                <FieldLabel htmlFor="oauth-result">{t('2. 粘贴授权结果', '2. Paste the authorization result')}</FieldLabel>
                <Textarea
                  id="oauth-result"
                  name="code"
                  value={code}
                  onChange={(event) => setCode(event.target.value)}
                  placeholder={t('授权完成后页面上显示的 code#state', 'The code#state shown after authorization')}
                  className="min-h-24"
                  required
                />
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
                  <div className="w-full space-y-2">
                    <Label>{t('从代理池选择', 'Pick from proxy pool')}</Label>
                    <div className="flex w-full items-center gap-2">
                      <ProxyPickerCombobox
                        proxies={savedProxies}
                        value={proxy.trim()}
                        onPick={setProxy}
                      />
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
                {!pickedFromPool && (
                  <>
                    {savedProxies.length > 0 && (
                      <Label htmlFor="account-proxy" className="mt-1">
                        {t('或手动填写地址', 'Or enter an address')}
                      </Label>
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
                  </>
                )}
                <FieldDescription>
                  {t(
                    '换码与拉取账号信息都经由此代理，添加后自动设为该账号的出站代理。留空为直连。',
                    'The token exchange and profile fetch go through this proxy; it becomes the account’s outbound proxy once added. Leave blank to connect directly.',
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
