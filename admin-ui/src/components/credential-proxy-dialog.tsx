import { useEffect, useState } from 'react'
import { useMutation, useQuery } from '@tanstack/react-query'
import { GlobeIcon, MapPinIcon, PlayIcon, XIcon } from 'lucide-react'
import { type Credential } from '@/api/credentials'
import { listProxies, testProxy, type ProxyTestResult } from '@/api/proxies'
import { useI18n } from '@/lib/i18n'
import { displayCredentialLabel, extractError } from '@/lib/utils'
import { type CredentialActions } from '@/components/credential-shared'
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
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'

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
      <DialogPopup className="max-w-lg">
        <DialogHeader>
          <DialogTitle>{t('出站代理', 'Outbound proxy')}</DialogTitle>
          <DialogDescription className="mt-1 truncate" title={credentialLabel}>
            {credentialLabel}
          </DialogDescription>
        </DialogHeader>

        <DialogPanel className="space-y-4">
          {savedProxies.length > 0 && (
            <div className="space-y-2">
              <Label>{t('从代理池选择', 'Pick from proxy pool')}</Label>
              <Combobox
                value={savedProxies.find((p) => p.url === trimmed)?.id ?? null}
                onValueChange={(id) => {
                  const found = savedProxies.find((p) => p.id === id)
                  if (found) setValue(found.url)
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
                <ComboboxTrigger className="w-full">
                  <ComboboxValue
                    placeholder={t('选择代理…', 'Select a proxy…')}
                  />
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
                            <span className="text-muted-foreground shrink-0 text-xs">
                              {p.credential_count} {t('账号', 'acct')}
                            </span>
                          )}
                        </div>
                        <div className="text-muted-foreground truncate text-xs">{p.url}</div>
                        {p.credential_labels.length > 0 && (
                          <div className="text-muted-foreground mt-0.5 truncate text-xs">
                            {p.credential_labels.join(', ')}
                          </div>
                        )}
                      </div>
                    </ComboboxItem>
                  ))}
                </ComboboxPopup>
              </Combobox>
            </div>
          )}

          <div className="space-y-2">
            <Label htmlFor="cred-proxy">{t('代理地址', 'Proxy URL')}</Label>
            <Input
              id="cred-proxy"
              value={value}
              onChange={(event) => setValue(event.target.value)}
              onKeyDown={(event) => {
                if (event.key === 'Enter' && dirty && !proxy.isPending) save()
              }}
              placeholder="socks5://127.0.0.1:1080"
              spellCheck={false}
              autoComplete="off"
            />
            <p className="text-muted-foreground text-xs leading-relaxed">
              {t(
                '支持 socks5://、socks5h://、http://、https://，可带 user:pass@（密码里的特殊字符要 percent-encode，如 # 写成 %23）。留空表示直连。',
                'Supports socks5://, socks5h://, http://, https://, optionally with user:pass@ (percent-encode special characters in the password, e.g. # as %23). Leave empty for a direct connection.',
              )}
            </p>
            <p className="text-muted-foreground text-xs leading-relaxed">
              {t(
                '填 socks5:// 会在保存时自动改成 socks5h://——让代理端解析域名，而不是在本机解析。本机解析会把上游域名泄露给本地 DNS，解析出的也是离你就近的 IP，而且不少住宅代理只接受域名形式、直接断连。socks4/socks4a 不再支持：SOCKS4 协议带不了账号密码，填了会被静默丢掉。',
                'socks5:// is rewritten to socks5h:// on save, so DNS is resolved at the proxy rather than locally. Local resolution leaks the upstream hostname to your DNS, yields an IP close to you rather than the proxy, and many residential proxies reject address-form requests outright. socks4/socks4a are no longer supported: the SOCKS4 protocol cannot carry a username and password, so credentials would be silently dropped.',
              )}
            </p>
          </div>

          <ProxyTestBlock url={trimmed} />

          <Alert>
            <GlobeIcon />
            <AlertDescription>
              {t(
                '配好之后，这个账号的全部出站流量都走它：转发、token 刷新、账号信息、连通性测试。代理不可用时该账号的请求会直接失败，不会退回直连——那样会把真实 IP 暴露给上游。',
                "Once set, all of this account's outbound traffic goes through it: forwarding, token refresh, profile, and connectivity tests. If the proxy is unusable the account's requests fail outright rather than falling back to a direct connection, which would expose your real IP upstream.",
              )}
            </AlertDescription>
          </Alert>
        </DialogPanel>

        <DialogFooter>
          <DialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</DialogClose>
          <Button onClick={save} disabled={!dirty || proxy.isPending}>
            {trimmed === '' ? t('改回直连', 'Use direct') : t('保存', 'Save')}
          </Button>
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  )
}

/** 代理测试按钮 + 结果展示，可复用于代理对话框和添加账号页面。 */
export function ProxyTestBlock({ url }: { url: string }) {
  const { t, language } = useI18n()
  const [result, setResult] = useState<ProxyTestResult | null>(null)
  const test = useMutation({
    mutationFn: () => testProxy(url),
    onSuccess: setResult,
    onError: (e) => setResult({
      ok: false, ip: null, country: null, city: null, region: null, org: null,
      latency_ms: 0, error: extractError(e, language),
    }),
  })

  if (!url) return null

  return (
    <div className="space-y-2">
      <Button
        type="button"
        size="sm"
        variant="outline"
        loading={test.isPending}
        onClick={() => test.mutate()}
      >
        <PlayIcon />
        {t('测试代理', 'Test proxy')}
      </Button>
      {result && (
        <div className={`flex items-start gap-2 rounded-md border px-3 py-2 text-xs ${result.ok ? 'border-success/30 bg-success/5' : 'border-destructive/30 bg-destructive/5'}`}>
          <MapPinIcon className="mt-0.5 size-3.5 shrink-0" />
          {result.ok ? (
            <div className="min-w-0 space-y-0.5">
              <p className="font-medium">{result.ip}</p>
              <p className="text-muted-foreground">
                {[result.city, result.region, result.country].filter(Boolean).join(', ')}
                {result.org && ` · ${result.org}`}
                {` · ${result.latency_ms}ms`}
              </p>
            </div>
          ) : (
            <p className="min-w-0 break-all text-destructive-foreground">
              {result.error}{result.latency_ms > 0 && ` · ${result.latency_ms}ms`}
            </p>
          )}
          <Button size="icon-sm" variant="ghost" className="-my-0.5 ml-auto shrink-0" onClick={() => setResult(null)}>
            <XIcon className="size-3" />
          </Button>
        </div>
      )}
    </div>
  )
}
