import { useMemo, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { SearchIcon } from 'lucide-react'
import { listCredentials, setProxies } from '@/api/credentials'
import type { SavedProxy } from '@/api/proxies'
import { useI18n } from '@/lib/i18n'
import { displayCredentialLabel, extractError } from '@/lib/utils'
import { proxyMaskedUrl } from '@/components/credential-shared'
import { Alert, AlertDescription } from '@/components/ui/alert'
import { Button } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'
import { CheckboxGroup } from '@/components/ui/checkbox-group'
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
import { Spinner } from '@/components/ui/spinner'
import { toastManager } from '@/components/ui/toast'
import { Hint } from '@/components/ui/tooltip'

/**
 * 在代理池里直接调整「哪些账号走这条代理」。
 *
 * 勾上 = 该账号改用这条代理（原来走别的代理也一并改过来）；取消勾选 = 改回直连。
 * 保存时只提交有变化的账号，拆成「加入」「移出」两次批量调用，复用 `/credentials/proxy`。
 */
export function ProxyAccountsDialog({
  proxy,
  pool,
  open,
  onOpenChange,
}: {
  proxy: SavedProxy
  /** 整个代理池，用来把其它账号当前的代理地址翻成名称。 */
  pool: SavedProxy[]
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  // 与主页面共用 ['credentials'] 缓存，打开弹窗时通常已有数据。
  const credsQuery = useQuery({ queryKey: ['credentials'], queryFn: listCredentials, enabled: open })
  const creds = credsQuery.data ?? []

  const [query, setQuery] = useState('')
  // 只记用户改动过的勾选状态；没改过的按账号当前配置显示，数据刷新后不会被旧快照盖掉。
  const [overrides, setOverrides] = useState<Map<number, boolean>>(() => new Map())

  const usesThis = (proxyUrl: string | null) => proxyUrl === proxy.url
  const isChecked = (id: number, proxyUrl: string | null) => overrides.get(id) ?? usesThis(proxyUrl)
  // CheckboxGroup 只认字符串数组：勾选状态由上面的覆盖表推出，变化时只把真正翻转的账号记进覆盖表
  //（被搜索藏起来的账号不会变）。
  const checkedValues = creds.filter((c) => isChecked(c.id, c.proxy)).map((c) => String(c.id))
  const onCheckedValuesChange = (next: string[]) => {
    const nextSet = new Set(next)
    setOverrides((prev) => {
      const map = new Map(prev)
      for (const c of creds) {
        const on = nextSet.has(String(c.id))
        if (on !== isChecked(c.id, c.proxy)) map.set(c.id, on)
      }
      return map
    })
  }

  // 正在用这条代理的排在前面，其余按原顺序；排序只看当前配置，勾选时行不跳动。
  const sorted = useMemo(
    () => [...creds].sort((a, b) => Number(usesThis(b.proxy)) - Number(usesThis(a.proxy))),
    // eslint-disable-next-line react-hooks/exhaustive-deps
    [creds, proxy.url],
  )
  const q = query.trim().toLowerCase()
  const visible = q
    ? sorted.filter((c) =>
        `${c.label} ${displayCredentialLabel(c.label, language)}`.toLowerCase().includes(q),
      )
    : sorted

  const toAdd = creds.filter((c) => overrides.get(c.id) === true && !usesThis(c.proxy))
  const toRemove = creds.filter((c) => overrides.get(c.id) === false && usesThis(c.proxy))
  const dirty = toAdd.length > 0 || toRemove.length > 0

  const reset = () => {
    setOverrides(new Map())
    setQuery('')
  }

  const save = useMutation({
    mutationFn: async () => {
      if (toAdd.length > 0) await setProxies(toAdd.map((c) => c.id), proxy.url)
      if (toRemove.length > 0) await setProxies(toRemove.map((c) => c.id), null)
    },
    onSuccess: () => {
      toastManager.add({ title: t('已更新使用账号', 'Accounts updated'), type: 'success' })
      qc.invalidateQueries({ queryKey: ['credentials'] })
      qc.invalidateQueries({ queryKey: ['proxies'] })
      onOpenChange(false)
      reset()
    },
    onError: (e) => {
      // 可能「加入」已成功、「移出」失败，刷新一次让界面与实际配置对齐。
      qc.invalidateQueries({ queryKey: ['credentials'] })
      qc.invalidateQueries({ queryKey: ['proxies'] })
      toastManager.add({
        title: t('更新使用账号失败', 'Failed to update accounts'),
        description: extractError(e, language),
        type: 'error',
      })
    },
  })

  const currentProxyText = (url: string | null) => {
    if (!url) return t('当前为直连', 'Currently direct')
    const name = pool.find((p) => p.url === url)?.label ?? proxyMaskedUrl(url)
    return t(`当前：${name}`, `Currently: ${name}`)
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        onOpenChange(next)
        if (!next) reset()
      }}
    >
      <DialogPopup>
        <DialogHeader>
          <DialogTitle>{t('使用账号', 'Accounts using this proxy')}</DialogTitle>
          <Hint label={proxy.label}>
            <DialogDescription className="mt-1 truncate">
              {proxy.label}
            </DialogDescription>
          </Hint>
        </DialogHeader>

        <DialogPanel className="space-y-3">
          {credsQuery.isPending ? (
            <div className="flex min-h-24 items-center justify-center gap-2 text-sm text-muted-foreground">
              <Spinner className="size-4" />
              {t('正在加载', 'Loading')}
            </div>
          ) : creds.length === 0 ? (
            <p className="py-6 text-center text-sm text-muted-foreground">
              {t('暂无账号。', 'No accounts yet.')}
            </p>
          ) : (
            <>
              {creds.length > 8 && (
                <div className="relative">
                  <SearchIcon className="pointer-events-none absolute top-1/2 left-2.5 size-3.5 -translate-y-1/2 text-muted-foreground" />
                  <Input
                    value={query}
                    onChange={(event) => setQuery(event.target.value)}
                    placeholder={t('搜索账号…', 'Search accounts…')}
                    className="pl-7"
                    size="sm"
                  />
                </div>
              )}
              <CheckboxGroup
                aria-label={t('使用账号', 'Accounts using this proxy')}
                className="max-h-80 items-stretch gap-0 divide-y overflow-y-auto rounded-md border"
                value={checkedValues}
                onValueChange={onCheckedValuesChange}
              >
                {visible.map((c) => (
                  <Label className="cursor-pointer gap-3 px-3 py-2 font-normal transition-colors hover:bg-accent/50" key={c.id}>
                    <Checkbox value={String(c.id)} />
                    <div className="min-w-0 flex-1">
                      <p className="truncate text-sm">{displayCredentialLabel(c.label, language)}</p>
                      {!usesThis(c.proxy) && (
                        <p className="truncate text-xs text-muted-foreground">
                          {currentProxyText(c.proxy)}
                        </p>
                      )}
                    </div>
                    {c.disabled && (
                      <span className="shrink-0 text-xs text-muted-foreground">
                        {t('已停用', 'Disabled')}
                      </span>
                    )}
                  </Label>
                ))}
                {visible.length === 0 && (
                  <p className="px-3 py-4 text-center text-sm text-muted-foreground">
                    {t('无匹配结果', 'No matches')}
                  </p>
                )}
              </CheckboxGroup>
              {toRemove.length > 0 && (
                <Alert>
                  <AlertDescription>
                    {t(
                      `取消勾选的 ${toRemove.length} 个账号将恢复直连，出站流量将使用本机真实 IP。`,
                      `${toRemove.length} unchecked account${toRemove.length === 1 ? '' : 's'} will switch to a direct connection and use this server’s real IP.`,
                    )}
                  </AlertDescription>
                </Alert>
              )}
            </>
          )}
        </DialogPanel>

        <DialogFooter>
          {dirty && (
            <p className="mr-auto self-center text-xs text-muted-foreground tabular-nums">
              {t(
                `加入 ${toAdd.length} 个，移出 ${toRemove.length} 个`,
                `${toAdd.length} to add, ${toRemove.length} to remove`,
              )}
            </p>
          )}
          <DialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</DialogClose>
          <Button onClick={() => save.mutate()} disabled={!dirty} loading={save.isPending}>
            {t('保存', 'Save')}
          </Button>
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  )
}
