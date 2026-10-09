import { useCallback, useEffect, useRef, useState, type ReactNode } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { ArrowRightIcon, ExternalLinkIcon, KeyRoundIcon, RefreshCwIcon } from 'lucide-react'
import { getAuthorizeUrl, exchangeCode, reauthorizeCredential, type Credential } from '@/api/credentials'
import { GroupPicker, GroupPickerSkeleton, defaultGroupId, useGroups } from '@/components/group-picker'
import { listProxies } from '@/api/proxies'
import { useI18n } from '@/lib/i18n'
import { ReauthorizeContext } from '@/lib/reauthorize'
import { displayCredentialLabel, extractError } from '@/lib/utils'
import { ProxyPickerCombobox, ProxyTestBlock } from '@/components/credential-proxy-dialog'
import { CopyButton } from '@/components/copy-button'
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

/**
 * 添加账号弹窗：授权 → 粘贴 code#state → 可选备注 → 新增一条凭证。
 *
 * 传了 `reauth` 就是「重新授权」：同样两步，但换到的 token 覆盖这个号，不新增；备注与代理
 * 两栏不出——显示名、代理都沿用原号的，换码也走原号的代理。
 */
export function AddAccount({
  open, onOpenChange, reauth,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  reauth?: Credential | null
}) {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  const [authUrl, setAuthUrl] = useState<string | null>(null)
  const [code, setCode] = useState('')
  const [label, setLabel] = useState('')
  const [proxy, setProxy] = useState('')
  // 放进哪些号池分组：打开时预选默认分组（上号必选至少一个）。
  const groupsQuery = useGroups(open && !reauth)
  const [groupIds, setGroupIds] = useState<number[]>([])
  const authorizeSession = useRef(0)

  const reset = () => {
    setCode('')
    setLabel('')
    setProxy('')
    setGroupIds([])
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
  // 分组拉回来之后预选默认分组（只在还一个都没选时）。
  const defaultGroup = defaultGroupId(groupsQuery.data)
  useEffect(() => {
    if (open && defaultGroup != null) setGroupIds((ids) => (ids.length ? ids : [defaultGroup]))
  }, [open, defaultGroup])

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
    mutationFn: () => reauth
      ? reauthorizeCredential(reauth.id, code.trim())
      : exchangeCode(code.trim(), label.trim() || undefined, proxy.trim() || undefined, groupIds),
    onSuccess: (cred) => {
      toastManager.add({
        title: reauth ? t('已重新授权', 'Account reauthorized') : t('已添加账号', 'Account added'),
        description: displayCredentialLabel(cred.label, language),
        type: 'success',
      })
      qc.invalidateQueries({ queryKey: ['credentials'] })
      qc.invalidateQueries({ queryKey: ['proxies'] })
      handleOpenChange(false)
    },
    onError: (error) => toastManager.add({
      title: reauth ? t('重新授权失败', 'Reauthorization failed') : t('添加失败', 'Failed to add account'),
      description: extractError(error, language),
      type: 'error',
    }),
  })

  const proxiesQuery = useQuery({
    queryKey: ['proxies'],
    queryFn: listProxies,
    enabled: open && !reauth,
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
          <DialogTitle>{reauth ? t('重新授权账号', 'Reauthorize account') : t('添加 Claude 账号', 'Add Claude account')}</DialogTitle>
          {/* 不再挂一句「完成授权后粘贴授权结果」：下面的 1、2 两步就是这句话本身。说明留给读屏。
              重新授权例外：得说清会替换哪个号、哪些设置保留。 */}
          {reauth ? (
            <DialogDescription>
              {t(
                `授权完成后替换「${displayCredentialLabel(reauth.label, language)}」的 token，优先级、上限、出站代理等设置保持不变。`,
                `Replaces the token of “${displayCredentialLabel(reauth.label, language)}” once authorized; priority, limits, outbound proxy and other settings are kept.`,
              )}
            </DialogDescription>
          ) : (
            <DialogDescription className="sr-only">
              {t(
                '完成 Claude OAuth 授权后，粘贴授权结果以接入订阅账号。',
                'Complete Claude OAuth authorization, then paste the result to connect a subscription account.',
              )}
            </DialogDescription>
          )}
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
                {reauth
                  ? t(
                      '须使用该账号原本的 Claude 账号登录，登录其他账号将被拒绝。',
                      'Sign in with this account’s original Claude account; a different account is rejected.',
                    )
                  : t(
                      '使用要接入的 Claude 订阅账号完成授权。',
                      'Authorize with the Claude subscription account you want to connect.',
                    )}
              </FieldDescription>
              {/* 生成之后按钮原地换成「打开 / 复制」，不再另起一块「授权链接已生成」的提示：那块提示的标题、
                  说明、按钮（「打开授权页面」与本步标题一字不差）说的都是同一件事。 */}
              {authUrl ? (
                <div className="flex flex-wrap items-center gap-2">
                  <Button render={<a href={authUrl} rel="noreferrer" target="_blank" />} variant="outline">
                    <ExternalLinkIcon />
                    {t('打开授权页面', 'Open authorization page')}
                  </Button>
                  {/* 复制失败时把链接原文放进报错，方便手动复制。 */}
                  <CopyButton
                    errorDescription={authUrl}
                    label={t('复制后可在其他浏览器或设备上完成授权', 'Copy it to authorize in another browser or on another device')}
                    text={authUrl}
                  >
                    {t('复制链接', 'Copy link')}
                  </CopyButton>
                  {/* 链接有时效，授权页开久了会过期；关掉弹窗再开也行，这里给个就近的出口。 */}
                  <Button
                    type="button"
                    variant="ghost"
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
              {!reauth && (<>
              <Field name="groups">
                <FieldLabel>{t('账号分组', 'Account groups')}</FieldLabel>
                {groupsQuery.data ? (
                  <GroupPicker groups={groupsQuery.data} value={groupIds} onChange={setGroupIds} />
                ) : (
                  <GroupPickerSkeleton />
                )}
                <FieldDescription>
                  {t(
                    '至少选择一个。接入 Key 绑定分组后，仅调度这些分组中的账号。',
                    'Pick at least one. An access key bound to groups only uses the accounts in those groups.',
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
                  <div className="w-full space-y-2">
                    {/* 字段标签已是「出站代理」，这里两种填法只是小字提示，不再各挂一个 Label。 */}
                    <p className="text-xs text-muted-foreground">{t('从代理池选择', 'Pick from proxy pool')}</p>
                    <div className="flex w-full items-center gap-2">
                      <ProxyPickerCombobox
                        ariaLabel={t('从代理池选择', 'Pick from proxy pool')}
                        proxies={savedProxies}
                        value={proxy.trim()}
                        onPick={setProxy}
                      />
                      {proxy.trim() && (
                        <Button
                          type="button"
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
                      <p className="mt-1 text-xs text-muted-foreground">{t('或手动填写地址', 'Or enter an address')}</p>
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
                    '换取授权码与获取账号信息均经由此代理，添加后自动设为该账号的出站代理。留空则直连。',
                    'The token exchange and profile fetch go through this proxy; it becomes the account’s outbound proxy once added. Leave blank to connect directly.',
                  )}
                </FieldDescription>
                <ProxyTestBlock url={proxy.trim()} />
              </Field>
              </>)}
            </div>
          </DialogPanel>
          <DialogFooter>
            <DialogClose render={<Button variant="ghost" />} disabled={busy}>
              {t('取消', 'Cancel')}
            </DialogClose>
            <Button
              type="submit"
              loading={exchange.isPending}
              disabled={!code.trim() || (!reauth && groupIds.length === 0)}
            >
              {reauth ? <KeyRoundIcon /> : <ArrowRightIcon />}
              {reauth ? t('重新授权', 'Reauthorize') : t('添加账号', 'Add account')}
            </Button>
          </DialogFooter>
        </Form>
      </DialogPopup>
    </Dialog>
  )
}

/**
 * 在应用根部挂一个「重新授权」对话框，经 {@link ReauthorizeContext} 交给各处 ⋯ 菜单打开。
 * 关闭时只收起、不清掉账号：清掉的话关闭动画那几帧标题会跳回「添加 Claude 账号」。
 */
export function ReauthorizeProvider({ children }: { children: ReactNode }) {
  const [cred, setCred] = useState<Credential | null>(null)
  const [open, setOpen] = useState(false)
  const start = useCallback((next: Credential) => {
    setCred(next)
    setOpen(true)
  }, [])
  return (
    <ReauthorizeContext.Provider value={start}>
      {children}
      {cred && <AddAccount open={open} onOpenChange={setOpen} reauth={cred} />}
    </ReauthorizeContext.Provider>
  )
}
