import { useEffect, useState } from 'react'
import { useMutation, useQuery } from '@tanstack/react-query'
import { GlobeIcon, MapPinIcon, PlayIcon, XIcon } from 'lucide-react'
import { type Credential } from '@/api/credentials'
import { listProxies, testProxy, type ProxyTestResult, type SavedProxy } from '@/api/proxies'
import { useI18n } from '@/lib/i18n'
import { useReadOnly } from '@/lib/role'
import { displayCredentialLabel, extractError, formatMs } from '@/lib/utils'
import { ClampedDescription } from '@/components/settings-group'
import { proxyMaskedUrl, type CredentialActions } from '@/components/credential-shared'
import { Alert, AlertDescription } from '@/components/ui/alert'
import { Button } from '@/components/ui/button'
import {
  Combobox,
  ComboboxItem,
  ComboboxPopup,
  ComboboxTrigger,
  ComboboxValue,
} from '@/components/ui/combobox'
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
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import { Input } from '@/components/ui/input'
import { Hint } from '@/components/ui/tooltip'

export function CredentialProxyDialog({
  cred,
  open,
  onOpenChange,
  proxy,
}: {
  cred: Credential
  open: boolean
  onOpenChange: (open: boolean) => void
  proxy: CredentialActions['proxy']
}) {
  const { t, language } = useI18n()
  const readOnly = useReadOnly()
  const credentialLabel = displayCredentialLabel(cred.label, language)
  const [value, setValue] = useState(cred.proxy ?? '')

  useEffect(() => {
    if (open) setValue(cred.proxy ?? '')
  }, [open, cred.proxy])

  const proxiesQuery = useQuery({
    queryKey: ['proxies'],
    queryFn: listProxies,
    enabled: open,
  })
  const savedProxies = proxiesQuery.data ?? []

  const trimmed = value.trim()
  const current = cred.proxy ?? ''
  const dirty = trimmed !== current

  const save = () => {
    proxy.mutate(trimmed === '' ? null : trimmed, {
      onSuccess: () => onOpenChange(false),
    })
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogPopup>
        <DialogHeader>
          <DialogTitle>{t('出站代理', 'Outbound proxy')}</DialogTitle>
          <Hint label={credentialLabel}>
            <DialogDescription className="mt-1 truncate">
              {credentialLabel}
            </DialogDescription>
          </Hint>
        </DialogHeader>

        <DialogPanel className="space-y-4">
          {!readOnly && savedProxies.length > 0 && (
            <Field>
              <FieldLabel>{t('从代理池选择', 'Pick from proxy pool')}</FieldLabel>
              <ProxyPickerCombobox
                ariaLabel={t('从代理池选择', 'Pick from proxy pool')}
                proxies={savedProxies}
                value={trimmed}
                onPick={setValue}
              />
            </Field>
          )}

          <Field>
            <FieldLabel htmlFor="cred-proxy">{t('代理地址', 'Proxy URL')}</FieldLabel>
            <Input
              id="cred-proxy"
              value={value}
              readOnly={readOnly}
              onChange={(event) => setValue(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === 'Enter' && dirty && !proxy.isPending) save()
              }}
              placeholder="socks5://127.0.0.1:1080"
              spellCheck={false}
              autoComplete="off"
            />
            {/* 两段格式说明合成一段、默认收两行：开头那两行（支持哪些协议、可带账号密码、留空直连）
                是填写时要看的，socks5h 与 socks4 的来龙去脉按需展开。原来两段全摊开，手机上九行，
                把下面那条「不会回退直连」的警示挤到了屏幕外。 */}
            <FieldDescription className="leading-relaxed">
              {/* 长说明默认收两行、末尾「了解更多」，同设置页的 ClampedDescription：超过 140 字各宽度都收，60–140 字只在手机上收。 */}
              <ClampedDescription text={t(
                '支持 socks5://、socks5h://、http://、https://，可带 user:pass@（密码中的特殊字符需进行 percent-encode，如 # 写作 %23）。留空表示直连。填写 socks5:// 时，保存时会自动改为 socks5h://，由代理端而非本机解析域名。本机解析会将上游域名泄露给本地 DNS，解析结果也是距离本机最近的 IP；此外，不少住宅代理只接受域名形式的请求，收到 IP 形式的请求会直接断开连接。不再支持 socks4/socks4a：SOCKS4 协议无法携带用户名和密码，填写的认证信息会被静默丢弃。',
                'Supports socks5://, socks5h://, http://, https://, optionally with user:pass@ (percent-encode special characters in the password, e.g. # as %23). Leave empty for a direct connection. socks5:// is rewritten to socks5h:// on save, so DNS is resolved at the proxy rather than locally. Local resolution leaks the upstream hostname to your DNS, yields an IP close to you rather than the proxy, and many residential proxies reject address-form requests outright. socks4/socks4a are no longer supported: the SOCKS4 protocol cannot carry a username and password, so credentials would be silently dropped.',
              )} />
            </FieldDescription>
          </Field>

          {/* 测试会经这条代理出网，访客不给。 */}
          {!readOnly && <ProxyTestBlock url={trimmed} />}

          <Alert>
            <GlobeIcon />
            <AlertDescription>
              {t(
                '配置后，该账号的全部出站流量（转发、token 刷新、账号信息与连通性测试）均经由此代理。代理不可用时，该账号的请求将直接失败，而不会回退为直连，以免向上游暴露真实 IP。',
                "Once set, all of this account's outbound traffic goes through it: forwarding, token refresh, profile, and connectivity tests. If the proxy is unusable the account's requests fail outright rather than falling back to a direct connection, which would expose your real IP upstream.",
              )}
            </AlertDescription>
          </Alert>
        </DialogPanel>

        <DialogFooter>
          <DialogClose render={<Button variant="outline" />}>
            {readOnly ? t('关闭', 'Close') : t('取消', 'Cancel')}
          </DialogClose>
          {!readOnly && (
            <Button onClick={save} disabled={!dirty || proxy.isPending}>
              {trimmed === '' ? t('改回直连', 'Switch to direct') : t('保存', 'Save')}
            </Button>
          )}
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  )
}


/**
 * 代理池选择器。**添加账号页与本弹窗共用**——此前两处各有一份相同实现，
 * 迁移到 Combobox 时只改了一处（见 ef7639b），故收成一个组件。
 *
 * 三个要点：
 * 1. `items` 必须传：Base UI 的 `Combobox.Empty` 与内置筛选都依赖它
 *    （类型注释原文 "Requires the `items` prop on the root component"）。
 *    不传的话「无匹配结果」会和整份列表一起显示，而且搜索框根本不过滤。
 * 2. URL 走 [proxyMaskedUrl] 脱敏：代理地址常带 user:pass@，原样铺在下拉里
 *    等于把密码摊开给截图和录屏——代理池设置页早就是脱敏显示的，这里对齐。
 * 3. 每行两行封顶，使用者列表退到 title：代理一多，三行一条会让弹层铺满整屏。
 */
export function ProxyPickerCombobox({
  proxies,
  value,
  onPick,
  ariaLabel,
}: {
  proxies: SavedProxy[]
  value: string
  onPick: (url: string) => void
  /** 触发按钮的读屏名称：外面的 FieldLabel 关联不到这个按钮，得单独给。 */
  ariaLabel?: string
}) {
  const { t } = useI18n()
  const byId = (id: number) => proxies.find((p) => p.id === id)
  const ids = proxies.map((p) => p.id)

  return (
    <Combobox
      items={ids}
      value={proxies.find((p) => p.url === value)?.id ?? null}
      onValueChange={(id) => {
        const found = byId(id as number)
        if (found) onPick(found.url)
      }}
      itemToStringLabel={(id) => byId(id as number)?.label ?? ''}
      // 必须自定义 filter：Base UI 默认只拿 itemToStringLabel 去匹配，而这里的 label
      // 常是「1」「2」这种用户随手起的名字。把地址与使用账号一并纳入匹配面，
      // 占位符承诺的「搜索标签、地址或使用账号」才真的成立。
      // 匹配用**原始** URL 而非脱敏后的：用户是照着自己配的地址找的，*** 搜不到。
      filter={(id, query) => {
        const q = query.trim().toLowerCase()
        if (!q) return true
        const p = byId(id as number)
        if (!p) return false
        return `${p.label} ${p.url} ${p.credential_labels.join(' ')}`.toLowerCase().includes(q)
      }}
    >
      <ComboboxTrigger aria-label={ariaLabel} className="w-full min-w-0 flex-1">
        <ComboboxValue placeholder={t('选择代理…', 'Select a proxy…')} />
      </ComboboxTrigger>
      <ComboboxPopup
        className="max-h-80"
        inputPlaceholder={t('搜索标签、地址或使用账号…', 'Search label, URL, or account…')}
        emptyText={t('无匹配结果', 'No matches')}
      >
        {(id: number) => {
          const p = byId(id)
          if (!p) return null
          return (
            <ComboboxItem key={p.id} value={p.id}>
              <Hint
                label={p.credential_labels.length > 0
                  ? `${p.url}\n${t('使用账号', 'Used by')}: ${p.credential_labels.join(', ')}`
                  : p.url}
              >
                <div className="min-w-0">
                  <div className="flex items-baseline gap-2">
                    <span className="truncate font-medium">{p.label}</span>
                    {p.credential_count > 0 && (
                      <span className="shrink-0 text-xs text-muted-foreground">
                        {p.credential_count} {t('个账号', 'acct')}
                      </span>
                    )}
                  </div>
                  <div className="truncate font-mono text-xs text-muted-foreground">
                    {proxyMaskedUrl(p.url)}
                  </div>
                </div>
              </Hint>
            </ComboboxItem>
          )
        }}
      </ComboboxPopup>
    </Combobox>
  )
}

/** 请求本身失败（网络错误、400 地址不合法）时，也折成一条失败结果，和代理不通走同一处展示。 */
export function failedProxyTest(error: string): ProxyTestResult {
  return {
    ok: false, ip: null, country: null, city: null, region: null, org: null,
    latency_ms: 0, error,
  }
}

/** 一条测试结果的展示框：成功给出口 IP 与地区，失败给错误原因。代理池页与本组件共用。 */
export function ProxyTestResultView({
  result,
  onDismiss,
}: {
  result: ProxyTestResult
  onDismiss: () => void
}) {
  return (
    <div className={`flex items-start gap-2 rounded-md border px-3 py-2 text-xs ${result.ok ? 'border-success/30 bg-success/5' : 'border-destructive/30 bg-destructive/5'}`}>
      <MapPinIcon className="mt-0.5 size-3.5 shrink-0" />
      {result.ok ? (
        <div className="min-w-0 space-y-0.5">
          <p className="font-medium">{result.ip}</p>
          <p className="text-muted-foreground">
            {[result.city, result.region, result.country].filter(Boolean).join(', ')}
            {result.org && ` · ${result.org}`}
            {` · ${formatMs(result.latency_ms)}`}
          </p>
        </div>
      ) : (
        <p className="min-w-0 break-all text-destructive-foreground">
          {result.error}{result.latency_ms > 0 && ` · ${formatMs(result.latency_ms)}`}
        </p>
      )}
      <Button size="icon-sm" variant="ghost" className="-my-0.5 ml-auto shrink-0" onClick={onDismiss}>
        <XIcon className="size-3" />
      </Button>
    </div>
  )
}

/** 代理测试按钮 + 结果展示，可复用于代理对话框和添加账号页面。 */
export function ProxyTestBlock({ url }: { url: string }) {
  const { t, language } = useI18n()
  // 结果记下测的是哪个地址：改了输入框后，上一个地址的「✓」不能挂在新地址下面。
  const [tested, setTested] = useState<{ url: string; result: ProxyTestResult } | null>(null)
  const test = useMutation({
    mutationFn: (target: string) => testProxy(target),
    onSuccess: (result, target) => setTested({ url: target, result }),
    onError: (e, target) => setTested({ url: target, result: failedProxyTest(extractError(e, language)) }),
  })
  const result = tested?.url === url ? tested.result : null

  if (!url) return null

  return (
    <div className="space-y-2">
      <Button
        type="button"
        size="sm"
        variant="outline"
        loading={test.isPending && test.variables === url}
        onClick={() => test.mutate(url)}
      >
        <PlayIcon />
        {t('测试代理', 'Test proxy')}
      </Button>
      {result && <ProxyTestResultView result={result} onDismiss={() => setTested(null)} />}
    </div>
  )
}
