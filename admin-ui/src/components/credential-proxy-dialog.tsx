import { useEffect, useState } from 'react'
import { useMutation, useQuery } from '@tanstack/react-query'
import { GlobeIcon, MapPinIcon, PlayIcon, XIcon } from 'lucide-react'
import { type Credential } from '@/api/credentials'
import { listProxies, testProxy, type ProxyTestResult, type SavedProxy } from '@/api/proxies'
import { useI18n } from '@/lib/i18n'
import { displayCredentialLabel, extractError } from '@/lib/utils'
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
              <ProxyPickerCombobox proxies={savedProxies} value={trimmed} onPick={setValue} />
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
}: {
  proxies: SavedProxy[]
  value: string
  onPick: (url: string) => void
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
      <ComboboxTrigger className="w-full min-w-0 flex-1">
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
            <ComboboxItem
              key={p.id}
              value={p.id}
              title={p.credential_labels.length > 0
                ? `${p.url}\n${t('使用账号', 'Used by')}: ${p.credential_labels.join(', ')}`
                : p.url}
            >
              <div className="min-w-0">
                <div className="flex items-baseline gap-2">
                  <span className="truncate font-medium">{p.label}</span>
                  {p.credential_count > 0 && (
                    <span className="shrink-0 text-xs text-muted-foreground">
                      {p.credential_count} {t('账号', 'acct')}
                    </span>
                  )}
                </div>
                <div className="truncate font-mono text-xs text-muted-foreground">
                  {proxyMaskedUrl(p.url)}
                </div>
              </div>
            </ComboboxItem>
          )
        }}
      </ComboboxPopup>
    </Combobox>
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
