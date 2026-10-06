import { useCallback, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  CheckIcon,
  GlobeIcon,
  PencilIcon,
  PlayIcon,
  FilterIcon,
  ListPlusIcon,
  PlusIcon,
  RotateCwIcon,
  Trash2Icon,
  UsersIcon,
  XIcon,
} from 'lucide-react'
import {
  addProxy,
  deleteProxies,
  deleteProxy,
  listProxies,
  type ProxyTestResult,
  type SavedProxy,
  testProxy,
  updateProxy,
} from '@/api/proxies'
import { useI18n } from '@/lib/i18n'
import { proxyMaskedUrl } from '@/components/credential-shared'
import { failedProxyTest, ProxyTestResultView } from '@/components/credential-proxy-dialog'
import { extractError } from '@/lib/utils'
import {
  AlertDialog,
  AlertDialogClose,
  AlertDialogDescription,
  AlertDialogFooter,
  AlertDialogHeader,
  AlertDialogPopup,
  AlertDialogTitle,
} from '@/components/ui/alert-dialog'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Form } from '@/components/ui/form'
import { Input } from '@/components/ui/input'
import { Spinner } from '@/components/ui/spinner'
import { toastManager } from '@/components/ui/toast'
import { Hint } from '@/components/ui/tooltip'
import { SettingsGroup } from '@/components/settings-group'
import { ProxyAccountsDialog } from '@/components/proxy-accounts-dialog'
import { locationLabel, ProxyBatchImportDialog, runConcurrently } from '@/components/proxy-batch-import-dialog'

export function ProxyPoolSettingsContent() {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  const proxiesQuery = useQuery({ queryKey: ['proxies'], queryFn: listProxies })

  const [addLabel, setAddLabel] = useState('')
  const [addUrl, setAddUrl] = useState('')

  // 测试结果按地址记，提在页面这一层：添加框、每一行、「全部测试」三处共用一份，
  // 添加前测过的结果随新行一起带下去，不用再测一遍。
  const [results, setResults] = useState<Record<string, ProxyTestResult>>({})
  const [testing, setTesting] = useState<Set<string>>(() => new Set())
  // 记下是哪个批量按钮在跑：两个按钮互斥，但转圈只转被点的那个。
  const [batchRunning, setBatchRunning] = useState<'all' | 'failed' | null>(null)
  const [onlyFailed, setOnlyFailed] = useState(false)
  const [confirmDeleteFailed, setConfirmDeleteFailed] = useState(false)
  const [importOpen, setImportOpen] = useState(false)

  const runTest = useCallback(
    async (url: string): Promise<ProxyTestResult> => {
      setTesting((prev) => new Set(prev).add(url))
      let result: ProxyTestResult
      try {
        result = await testProxy(url)
      } catch (e) {
        result = failedProxyTest(extractError(e, language))
      }
      setResults((prev) => ({ ...prev, [url]: result }))
      setTesting((prev) => {
        const next = new Set(prev)
        next.delete(url)
        return next
      })
      return result
    },
    [language],
  )
  const dismissResult = (url: string) =>
    setResults((prev) => {
      const next = { ...prev }
      delete next[url]
      return next
    })

  const invalidate = () => qc.invalidateQueries({ queryKey: ['proxies'] })
  const onError = (title: string, error: unknown) =>
    toastManager.add({ title, description: extractError(error, language), type: 'error' })

  const trimmedAddUrl = addUrl.trim()
  const addResult = results[trimmedAddUrl]

  const create = useMutation({
    mutationFn: () => {
      const label =
        addLabel.trim() ||
        locationLabel(addResult, (proxiesQuery.data ?? []).map((p) => p.label))
      return addProxy(label, trimmedAddUrl)
    },
    onSuccess: (p) => {
      toastManager.add({
        title: t('已添加代理', 'Proxy added'),
        description: p.label,
        type: 'success',
      })
      // 后端可能把地址归一化（如 socks5:// 升成 socks5h://），测试结果改挂到入库后的地址上。
      if (addResult) {
        setResults((prev) => {
          const next = { ...prev, [p.url]: addResult }
          if (p.url !== trimmedAddUrl) delete next[trimmedAddUrl]
          return next
        })
      }
      setAddLabel('')
      setAddUrl('')
      invalidate()
    },
    onError: (e) => onError(t('添加代理失败', 'Failed to add proxy'), e),
  })

  const removeFailed = useMutation({
    mutationFn: (targets: SavedProxy[]) => deleteProxies(targets.map((p) => p.id)),
    onSuccess: (deleted, targets) => {
      setResults((prev) => {
        const next = { ...prev }
        for (const p of targets) delete next[p.url]
        return next
      })
      setConfirmDeleteFailed(false)
      // 筛选跟着这批失败项一起结束：不复位的话，下次测出失败会一下子只剩失败项，像是代理被删了。
      setOnlyFailed(false)
      invalidate()
      qc.invalidateQueries({ queryKey: ['credentials'] })
      toastManager.add({
        title: t(`已删除 ${deleted} 条代理`, `Deleted ${deleted} prox${deleted === 1 ? 'y' : 'ies'}`),
        type: 'success',
      })
    },
    onError: (e) => {
      setConfirmDeleteFailed(false)
      onError(t('删除代理失败', 'Failed to delete proxies'), e)
    },
  })

  const testAll = async (urls: string[], kind: 'all' | 'failed') => {
    setBatchRunning(kind)
    const { ok, failed } = await runConcurrently(urls, runTest)
    setBatchRunning(null)
    toastManager.add({
      title: t('测试完成', 'Test finished'),
      description: t(`${ok} 条可用，${failed} 条不可用`, `${ok} working, ${failed} failed`),
      type: failed === 0 ? 'success' : 'warning',
    })
  }

  if (proxiesQuery.isPending) {
    return (
      <div
        className="flex min-h-40 items-center justify-center gap-2 text-sm text-muted-foreground"
        role="status"
      >
        <Spinner className="size-4" />
        {t('正在加载', 'Loading')}
      </div>
    )
  }

  if (proxiesQuery.isError) {
    return (
      <div
        className="flex min-h-40 flex-col items-center justify-center gap-3 text-center"
        role="alert"
      >
        <p className="text-sm font-medium">
          {t('无法读取代理池', 'Unable to load the proxy pool')}
        </p>
        <Button
          size="sm"
          variant="outline"
          loading={proxiesQuery.isFetching}
          onClick={() => proxiesQuery.refetch()}
        >
          {t('重试', 'Retry')}
        </Button>
      </div>
    )
  }

  const proxies = proxiesQuery.data ?? []
  const testedCount = proxies.filter((p) => results[p.url]).length
  const okCount = proxies.filter((p) => results[p.url]?.ok).length
  const failedProxies = proxies.filter((p) => results[p.url]?.ok === false)
  const failedUrls = failedProxies.map((p) => p.url)
  // 失败项清空后（重测都通了 / 删光了）自动回到全部列表，不留一个空的筛选结果。
  const filtering = onlyFailed && failedProxies.length > 0
  const visibleProxies = filtering ? failedProxies : proxies
  const failedInUse = failedProxies.filter((p) => p.credential_count > 0)
  const failedInUseAccounts = failedInUse.reduce((n, p) => n + p.credential_count, 0)

  return (
    <div className="space-y-4">
      <SettingsGroup
        icon={GlobeIcon}
        title={t('代理池', 'Proxy pool')}
        description={t(
          '集中管理可复用的出站代理地址。添加后可在各账号的代理设置中快速选取，也可通过批量操作一次性分配给多个账号。',
          'Manage reusable outbound proxy addresses. Once added, they can be quickly selected in each account’s proxy settings or assigned to multiple accounts via batch actions.',
        )}
      >
        {/* 手机上名称独占一行、地址与「添加」同一行：三样硬挤一行时两个输入框各只剩 90 来 px，
            占位提示都被截断（「如：日本节」「socks5://127.0.0.1:108」）。 */}
        <Form
          className="flex flex-wrap items-end gap-2 px-4 py-4 sm:px-5"
          onSubmit={(event) => {
            event.preventDefault()
            if (trimmedAddUrl && !create.isPending) create.mutate()
          }}
        >
          <div className="min-w-0 flex-1 space-y-1 max-sm:basis-full">
            <label className="text-xs font-medium" htmlFor="proxy-pool-add-label">
              {t('名称', 'Name')}
            </label>
            <Input
              id="proxy-pool-add-label"
              value={addLabel}
              onChange={(event) => setAddLabel(event.target.value)}
              placeholder={t('留空则自动命名', 'Auto-named if empty')}
              size="sm"
            />
          </div>
          <div className="min-w-0 flex-[2] space-y-1">
            <label className="text-xs font-medium" htmlFor="proxy-pool-add-url">
              {t('代理地址', 'Proxy URL')}
            </label>
            <Input
              id="proxy-pool-add-url"
              value={addUrl}
              onChange={(event) => setAddUrl(event.target.value)}
              placeholder="socks5://127.0.0.1:1080"
              spellCheck={false}
              autoComplete="off"
              size="sm"
            />
          </div>
          <Button
            type="button"
            size="sm"
            variant="outline"
            disabled={!trimmedAddUrl}
            loading={testing.has(trimmedAddUrl)}
            onClick={() => runTest(trimmedAddUrl)}
          >
            <PlayIcon />
            {t('测试', 'Test')}
          </Button>
          <Button
            type="submit"
            size="sm"
            disabled={!trimmedAddUrl}
            loading={create.isPending}
          >
            <PlusIcon />
            {t('添加', 'Add')}
          </Button>
          <Button type="button" size="sm" variant="outline" onClick={() => setImportOpen(true)}>
            <ListPlusIcon />
            {t('批量导入', 'Import')}
          </Button>
          {addResult && (
            <div className="basis-full">
              <ProxyTestResultView result={addResult} onDismiss={() => dismissResult(trimmedAddUrl)} />
            </div>
          )}
        </Form>

        {proxies.length > 0 && (
          <div className="flex flex-wrap items-center justify-between gap-x-3 gap-y-2 px-4 py-2.5 sm:px-5">
            <p className="text-xs text-muted-foreground tabular-nums">
              {testedCount > 0
                ? t(
                    `共 ${proxies.length} 条 · 已测试 ${testedCount} 条，其中 ${okCount} 条可用`,
                    `${proxies.length} total · ${testedCount} tested, ${okCount} working`,
                  )
                : t(`共 ${proxies.length} 条`, `${proxies.length} total`)}
            </p>
            <div className="flex items-center gap-2">
              <Button
                size="sm"
                variant="outline"
                loading={batchRunning === 'all'}
                disabled={batchRunning !== null}
                onClick={() => testAll(proxies.map((p) => p.url), 'all')}
              >
                <PlayIcon />
                {t('全部测试', 'Test all')}
              </Button>
            </div>
          </div>
        )}

        {/* 失败项单独一行：筛选、重测、删除都只针对这批，和上面的全池操作分开，免得误删。 */}
        {failedProxies.length > 0 && (
          <div className="flex flex-wrap items-center justify-between gap-x-3 gap-y-2 bg-destructive/5 px-4 py-2.5 sm:px-5">
            <p className="text-xs font-medium text-destructive-foreground tabular-nums">
              {t(`${failedProxies.length} 条测试失败`, `${failedProxies.length} failed`)}
            </p>
            <div className="flex flex-wrap items-center gap-2">
              <Button
                size="sm"
                variant={filtering ? 'secondary' : 'ghost'}
                aria-pressed={filtering}
                onClick={() => setOnlyFailed((v) => !v)}
              >
                <FilterIcon />
                {filtering ? t('显示全部', 'Show all') : t('仅显示失败项', 'Only failed')}
              </Button>
              <Button
                size="sm"
                variant="outline"
                loading={batchRunning === 'failed'}
                disabled={batchRunning !== null}
                onClick={() => testAll(failedUrls, 'failed')}
              >
                <RotateCwIcon />
                {t('重测', 'Retest')}
              </Button>
              <Button
                size="sm"
                variant="destructive-outline"
                disabled={batchRunning !== null}
                onClick={() => setConfirmDeleteFailed(true)}
              >
                <Trash2Icon />
                {t('删除', 'Delete')}
              </Button>
            </div>
          </div>
        )}

        <ProxyBatchImportDialog
          open={importOpen}
          onOpenChange={setImportOpen}
          poolLabels={proxies.map((p) => p.label)}
          results={results}
          testing={testing}
          runTest={runTest}
        />

        <AlertDialog open={confirmDeleteFailed} onOpenChange={setConfirmDeleteFailed}>
          <AlertDialogPopup>
            <AlertDialogHeader>
              <AlertDialogTitle>
                {t(
                  `删除 ${failedProxies.length} 条测试失败的代理`,
                  `Delete ${failedProxies.length} failed prox${failedProxies.length === 1 ? 'y' : 'ies'}`,
                )}
              </AlertDialogTitle>
              <AlertDialogDescription>
                {failedInUse.length > 0
                  ? t(
                      `其中 ${failedInUse.length} 条仍被 ${failedInUseAccounts} 个账号使用。删除仅会将其从代理池移除，这些账号的代理设置保持不变，仍将使用这些不可用的代理；如需更换，请先在「使用账号」中调整。`,
                      `${failedInUse.length} of them ${failedInUse.length === 1 ? 'is' : 'are'} still used by ${failedInUseAccounts} account${failedInUseAccounts === 1 ? '' : 's'}. Deleting only removes them from the pool; those accounts keep using the unreachable proxy. Reassign them under “Manage accounts” first if needed.`,
                    )
                  : t('这些代理未被任何账号使用，删除后将从代理池移除。', 'No accounts use these proxies. They will be removed from the pool.')}
              </AlertDialogDescription>
            </AlertDialogHeader>
            <ul className="max-h-48 space-y-1 overflow-y-auto px-4 pb-4 text-sm sm:px-6" role="list">
              {failedProxies.map((p) => (
                <li key={p.id} className="flex items-baseline gap-2">
                  <span className="truncate">{p.label}</span>
                  {p.credential_count > 0 && (
                    <span className="shrink-0 text-xs text-muted-foreground tabular-nums">
                      {t(`${p.credential_count} 个账号使用中`, `${p.credential_count} in use`)}
                    </span>
                  )}
                </li>
              ))}
            </ul>
            <AlertDialogFooter>
              <AlertDialogClose render={<Button variant="outline" />}>
                {t('取消', 'Cancel')}
              </AlertDialogClose>
              <Button
                variant="destructive"
                loading={removeFailed.isPending}
                // 弹窗开着时单条重测可能把失败项清空，空列表发出去后端会回 400。
                disabled={failedProxies.length === 0}
                onClick={() => removeFailed.mutate(failedProxies)}
              >
                {t('删除', 'Delete')}
              </Button>
            </AlertDialogFooter>
          </AlertDialogPopup>
        </AlertDialog>

        {proxies.length === 0 ? (
          <p className="px-4 py-4 text-center sm:px-5 text-sm text-muted-foreground">
            {t('代理池中暂无代理。在上方填写地址即可添加第一条，名称可留空。', 'The proxy pool is empty. Enter a URL above to add the first proxy; the name is optional.')}
          </p>
        ) : (
          <ul className="divide-y" role="list">
            {visibleProxies.map((proxy) => (
              <ProxyRow
                key={proxy.id}
                proxy={proxy}
                pool={proxies}
                result={results[proxy.url]}
                testing={testing.has(proxy.url)}
                onTest={() => runTest(proxy.url)}
                onDismissResult={() => dismissResult(proxy.url)}
              />
            ))}
          </ul>
        )}
      </SettingsGroup>
    </div>
  )
}

function ProxyRow({
  proxy,
  pool,
  result,
  testing,
  onTest,
  onDismissResult,
}: {
  proxy: SavedProxy
  pool: SavedProxy[]
  result: ProxyTestResult | undefined
  testing: boolean
  onTest: () => void
  onDismissResult: () => void
}) {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  const [editing, setEditing] = useState(false)
  const [label, setLabel] = useState(proxy.label)
  const [url, setUrl] = useState(proxy.url)
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [accountsOpen, setAccountsOpen] = useState(false)

  const invalidate = () => qc.invalidateQueries({ queryKey: ['proxies'] })
  const onError = (title: string, error: unknown) =>
    toastManager.add({ title, description: extractError(error, language), type: 'error' })

  const edit = useMutation({
    mutationFn: () => updateProxy(proxy.id, label.trim(), url.trim()),
    onSuccess: () => {
      toastManager.add({ title: t('已更新代理', 'Proxy updated'), type: 'success' })
      setEditing(false)
      invalidate()
    },
    onError: (e) => onError(t('更新代理失败', 'Failed to update proxy'), e),
  })

  const remove = useMutation({
    mutationFn: () => deleteProxy(proxy.id),
    onSuccess: () => {
      toastManager.add({ title: t('已删除代理', 'Proxy deleted'), type: 'success' })
      setConfirmDelete(false)
      invalidate()
      qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (e) => {
      setConfirmDelete(false)
      onError(t('删除代理失败', 'Failed to delete proxy'), e)
    },
  })

  if (editing) {
    return (
      <li className="flex flex-wrap items-end gap-2 px-4 py-4 sm:px-5">
        <div className="min-w-0 flex-1 space-y-1 max-sm:basis-full">
          <label className="text-xs font-medium" htmlFor={`proxy-edit-label-${proxy.id}`}>
            {t('名称', 'Name')}
          </label>
          <Input
            id={`proxy-edit-label-${proxy.id}`}
            value={label}
            onChange={(event) => setLabel(event.target.value)}
            size="sm"
            autoFocus
          />
        </div>
        <div className="min-w-0 flex-[2] space-y-1">
          <label className="text-xs font-medium" htmlFor={`proxy-edit-url-${proxy.id}`}>
            {t('代理地址', 'Proxy URL')}
          </label>
          <Input
            id={`proxy-edit-url-${proxy.id}`}
            value={url}
            onChange={(event) => setUrl(event.target.value)}
            spellCheck={false}
            autoComplete="off"
            size="sm"
          />
        </div>
        <Button
          size="icon-sm"
          variant="outline"
          loading={edit.isPending}
          disabled={!label.trim() || !url.trim()}
          onClick={() => edit.mutate()}
          aria-label={t('保存', 'Save')}
        >
          <CheckIcon />
        </Button>
        <Button
          size="icon-sm"
          variant="ghost"
          onClick={() => {
            setEditing(false)
            setLabel(proxy.label)
            setUrl(proxy.url)
          }}
          aria-label={t('取消', 'Cancel')}
        >
          <XIcon />
        </Button>
      </li>
    )
  }

  return (
    <li className="space-y-2 px-4 py-4 sm:px-5">
      <div className="flex items-center gap-3">
        <div className="min-w-0 flex-1">
          <div className="flex items-center gap-2">
            <span className="truncate text-sm font-medium">{proxy.label}</span>
          </div>
          <Hint label={proxyMaskedUrl(proxy.url)}>
            <p className="mt-0.5 truncate text-xs text-muted-foreground">
              {proxyMaskedUrl(proxy.url)}
            </p>
          </Hint>
        </div>
        <Hint label={t('测试', 'Test')}>
          <Button
            size="icon-sm"
            variant="ghost"
            loading={testing}
            onClick={onTest}
            aria-label={t('测试', 'Test')}
          >
            <PlayIcon />
          </Button>
        </Hint>
        <Hint label={t('调整使用账号', 'Manage accounts')}>
          <Button
            size="icon-sm"
            variant="ghost"
            onClick={() => setAccountsOpen(true)}
            aria-label={t('调整使用账号', 'Manage accounts')}
          >
            <UsersIcon />
          </Button>
        </Hint>
        <Button
          size="icon-sm"
          variant="ghost"
          onClick={() => {
            setLabel(proxy.label)
            setUrl(proxy.url)
            setEditing(true)
          }}
          aria-label={t('编辑', 'Edit')}
        >
          <PencilIcon />
        </Button>
        <Button
          size="icon-sm"
          variant="ghost"
          onClick={() => setConfirmDelete(true)}
          aria-label={t('删除', 'Delete')}
        >
          <Trash2Icon />
        </Button>
      </div>
      {proxy.credential_labels.length > 0 && (
        <p className="flex flex-wrap items-center gap-1 text-xs text-muted-foreground">
          {/* 数量并进这一行的标签：原来名称旁还挂一枚「2 个账号」徽章，紧接着这里又把两个账号列出来。 */}
          <span className="tabular-nums">
            {t(`使用账号（${proxy.credential_count}）：`, `Used by (${proxy.credential_count}): `)}
          </span>
          {proxy.credential_labels.map((name, i) => (
            <Badge key={i} variant="outline" size="sm">{name}</Badge>
          ))}
        </p>
      )}
      {result && <ProxyTestResultView result={result} onDismiss={onDismissResult} />}

      <ProxyAccountsDialog
        proxy={proxy}
        pool={pool}
        open={accountsOpen}
        onOpenChange={setAccountsOpen}
      />

      <AlertDialog open={confirmDelete} onOpenChange={setConfirmDelete}>
        <AlertDialogPopup>
          <AlertDialogHeader>
            <AlertDialogTitle>
              {t(`删除代理「${proxy.label}」`, `Delete proxy "${proxy.label}"`)}
            </AlertDialogTitle>
            <AlertDialogDescription>
              {proxy.credential_count > 0
                ? t(
                    `当前有 ${proxy.credential_count} 个账号正在使用此代理。删除后，这些账号的代理设置保持不变，该代理仅从代理池中移除。`,
                    `${proxy.credential_count} account${proxy.credential_count === 1 ? ' is' : 's are'} currently using this proxy. Deleting it won’t change those accounts’ proxy settings, but it will no longer appear in the pool.`,
                  )
                : t(
                    '确定从代理池中删除此代理？',
                    'Remove this entry from the proxy pool?',
                  )}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogClose render={<Button variant="outline" />}>
              {t('取消', 'Cancel')}
            </AlertDialogClose>
            <Button variant="destructive" loading={remove.isPending} onClick={() => remove.mutate()}>
              {t('删除', 'Delete')}
            </Button>
          </AlertDialogFooter>
        </AlertDialogPopup>
      </AlertDialog>
    </li>
  )
}
