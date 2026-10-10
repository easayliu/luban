import { useCallback, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  EllipsisIcon,
  FilterIcon,
  GlobeIcon,
  ListPlusIcon,
  PencilIcon,
  PlayIcon,
  PlusIcon,
  RotateCwIcon,
  Trash2Icon,
  UsersIcon,
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
import { useIsAdmin } from '@/lib/role'
import { useDocumentTitle } from '@/lib/use-document-title'
import { cn, extractError, formatMs } from '@/lib/utils'
import { AppFooter } from '@/components/app-footer'
import { AccountMenu, AppHeader, MainNav, type MainSection } from '@/components/app-header'
import { proxyMaskedUrl } from '@/components/credential-shared'
import { failedProxyTest, ProxyTestResultView } from '@/components/credential-proxy-dialog'
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
import { Button, buttonVariants } from '@/components/ui/button'
import { Card } from '@/components/ui/card'
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
import { Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from '@/components/ui/empty'
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import { Form } from '@/components/ui/form'
import { Input } from '@/components/ui/input'
import { Menu, MenuItem, MenuPopup, MenuSeparator, MenuTrigger } from '@/components/ui/menu'
import { Spinner } from '@/components/ui/spinner'
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from '@/components/ui/table'
import { toastManager } from '@/components/ui/toast'
import { Toggle } from '@/components/ui/toggle'
import { Hint } from '@/components/ui/tooltip'
import { ErrorState, LoadingState } from '@/components/state-placeholders'
import { ProxyAccountsDialog } from '@/components/proxy-accounts-dialog'
import { locationLabel, ProxyBatchImportDialog, runConcurrently } from '@/components/proxy-batch-import-dialog'

/**
 * 列宽预算（`table-fixed`），口径同费用页：每格内边距 p-2.5，可用内容宽 = 列宽 − 20px。
 * 地址只在 md 起单独成列，窄屏收到名称底下一行。手机 358px 上：操作 88 + 使用 72 + 连通 96，
 * 名称还剩 100 出头。
 */
const COL = {
  name: 'w-auto md:w-[28%]',
  url: 'hidden w-auto md:table-cell',
  used: 'w-18',
  status: 'w-24 sm:w-52',
  actions: 'w-22',
} as const

/** 「使用账号」悬浮提示里最多列几个名字，余下的折成「等 N 个」。 */
const USED_BY_PREVIEW = 5

/**
 * 代理池：与账号池、费用、成员管理平级的一级页面。原来放在系统设置里，可系统设置只对管理员开放，
 * 代理和用户名下的号要配代理却没处维护自己的池（后端 `/proxies` 早已按归属收窄）。管理员看到的是
 * 全部成员的代理，代理和用户只看到自己添加的。访客不进这一页。
 */
export function ProxiesPage({
  onNavigate,
  onSignOut,
}: {
  onNavigate: (section: MainSection) => void
  onSignOut: () => void
}) {
  const { t } = useI18n()
  useDocumentTitle(`${t('代理池', 'Proxy pool')} · Luban`)

  return (
    <div className="app-shell flex min-h-dvh flex-col text-foreground">
      <AppHeader
        actions={<AccountMenu onNavigate={onNavigate} onSignOut={onSignOut} />}
        nav={<MainNav current="proxies" onNavigate={onNavigate} />}
        onNavigateHome={() => onNavigate('pool')}
      />
      <main className="page-frame relative flex-1 py-4 pb-8 sm:py-5 sm:pb-10">
        <ProxyPoolContent />
      </main>
      <AppFooter />
    </div>
  )
}

/** 弹框里正在处理哪一条代理、做什么。 */
type Pending =
  | { kind: 'add' }
  | { kind: 'edit'; proxy: SavedProxy }
  | { kind: 'accounts'; proxy: SavedProxy }
  | { kind: 'delete'; proxy: SavedProxy }

function ProxyPoolContent() {
  const { t, language } = useI18n()
  const isAdmin = useIsAdmin()
  const qc = useQueryClient()
  const proxiesQuery = useQuery({ queryKey: ['proxies'], queryFn: listProxies })

  // 测试结果按地址记，提在页面这一层：添加框、每一行、「全部测试」三处共用一份，
  // 添加前测过的结果随新行一起带下去，不用再测一遍。
  const [results, setResults] = useState<Record<string, ProxyTestResult>>({})
  const [testing, setTesting] = useState<Set<string>>(() => new Set())
  // 记下是哪个批量按钮在跑：两个按钮互斥，但转圈只转被点的那个。
  const [batchRunning, setBatchRunning] = useState<'all' | 'failed' | null>(null)
  const [onlyFailed, setOnlyFailed] = useState(false)
  const [confirmDeleteFailed, setConfirmDeleteFailed] = useState(false)
  const [importOpen, setImportOpen] = useState(false)
  const [pending, setPending] = useState<Pending | null>(null)

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

  const remove = useMutation({
    mutationFn: (proxy: SavedProxy) => deleteProxy(proxy.id),
    onSuccess: (_r, proxy) => {
      toastManager.add({ title: t('已删除代理', 'Proxy deleted'), type: 'success' })
      dismissResult(proxy.url)
      setPending(null)
      invalidate()
      qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (e) => {
      setPending(null)
      onError(t('删除代理失败', 'Failed to delete proxy'), e)
    },
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
    <div className="space-y-3 sm:space-y-4">
      {/* 页头卡片：与成员管理、费用同构——标题与计数在左，主动作在右。 */}
      <section aria-labelledby="proxies-page-title" className="overflow-hidden rounded-2xl border bg-card shadow-xs/5">
        <div className="flex flex-wrap items-center justify-between gap-3 px-4 py-4 sm:px-5">
          <div className="flex min-w-0 flex-wrap items-center gap-2.5">
            <h1 className="min-w-0 text-lg font-semibold tracking-tight" id="proxies-page-title">
              {t('代理池', 'Proxy pool')}
            </h1>
            {proxiesQuery.isSuccess && (
              <Hint
                label={isAdmin
                  ? t(
                      '可复用的出站代理，可在账号的代理设置中选用，也可在此批量分配。此处列出全部成员添加的代理。',
                      'Reusable outbound proxies. Pick one in an account’s proxy settings, or assign in bulk here. Proxies added by all members are listed.',
                    )
                  : t(
                      '可复用的出站代理，可在账号的代理设置中选用，也可在此批量分配。代理池仅本人可见。',
                      'Reusable outbound proxies. Pick one in an account’s proxy settings, or assign in bulk here. Visible only to you.',
                    )}
              >
                <Badge className="tabular-nums" variant="secondary">
                  {testedCount > 0
                    ? t(
                        `共 ${proxies.length} 条 · ${okCount}/${testedCount} 可用`,
                        `${proxies.length} total · ${okCount}/${testedCount} working`,
                      )
                    : t(`共 ${proxies.length} 条`, `${proxies.length} total`)}
                </Badge>
              </Hint>
            )}
          </div>
          <div className="flex items-center gap-2">
            {proxies.length > 0 && (
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
            )}
            <Button size="sm" variant="outline" disabled={!proxiesQuery.isSuccess} onClick={() => setImportOpen(true)}>
              <ListPlusIcon />
              {t('批量导入', 'Import')}
            </Button>
            <Button size="sm" disabled={!proxiesQuery.isSuccess} onClick={() => setPending({ kind: 'add' })}>
              <PlusIcon />
              {t('添加代理', 'Add proxy')}
            </Button>
          </div>
        </div>
      </section>

      {/* 失败项单独一条：筛选、重测、删除都只针对这批，和页头的全池操作分开，免得误删。 */}
      {failedProxies.length > 0 && (
        <div className="flex flex-wrap items-center justify-between gap-x-3 gap-y-2 rounded-xl border border-destructive/30 bg-destructive/5 px-4 py-2.5 sm:px-5">
          <p className="text-sm font-medium text-destructive-foreground tabular-nums">
            {t(`${failedProxies.length} 条测试失败`, `${failedProxies.length} failed`)}
          </p>
          <div className="flex flex-wrap items-center gap-2">
            <Toggle size="sm" variant="outline" pressed={filtering} onPressedChange={setOnlyFailed}>
              <FilterIcon />
              {filtering ? t('显示全部', 'Show all') : t('仅显示失败项', 'Only failed')}
            </Toggle>
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

      {proxiesQuery.isPending ? (
        <Card>
          <LoadingState label={t('正在加载代理池', 'Loading the proxy pool')} />
        </Card>
      ) : proxiesQuery.isError ? (
        <Card>
          <ErrorState
            error={proxiesQuery.error}
            title={t('无法读取代理池', 'Unable to load the proxy pool')}
            onRetry={() => proxiesQuery.refetch()}
            retrying={proxiesQuery.isFetching}
          />
        </Card>
      ) : proxies.length === 0 ? (
        <Card>
          <Empty>
            <EmptyHeader>
              <EmptyMedia variant="icon"><GlobeIcon /></EmptyMedia>
              <EmptyTitle>{t('代理池中暂无代理', 'The proxy pool is empty')}</EmptyTitle>
              <EmptyDescription>
                {t('添加后，即可在账号的代理设置中选用，或在此批量分配。', 'Once added, a proxy can be picked in an account’s settings or assigned in bulk here.')}
              </EmptyDescription>
            </EmptyHeader>
          </Empty>
        </Card>
      ) : (
        <Table className="table-fixed" variant="card">
          <TableHeader>
            <TableRow>
              <TableHead className={COL.name}>{t('名称', 'Name')}</TableHead>
              <TableHead className={COL.url}>{t('地址', 'URL')}</TableHead>
              <TableHead className={COL.used}>{t('使用账号', 'In use')}</TableHead>
              <TableHead className={COL.status}>{t('连通性', 'Status')}</TableHead>
              <TableHead className={COL.actions}><span className="sr-only">{t('操作', 'Actions')}</span></TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {visibleProxies.map((proxy) => (
              <ProxyRow
                key={proxy.id}
                proxy={proxy}
                result={results[proxy.url]}
                testing={testing.has(proxy.url)}
                onTest={() => runTest(proxy.url)}
                onPending={(kind) => setPending({ kind, proxy })}
              />
            ))}
          </TableBody>
        </Table>
      )}

      <ProxyFormDialog
        proxy={pending?.kind === 'edit' ? pending.proxy : null}
        open={pending?.kind === 'add' || pending?.kind === 'edit'}
        pool={proxies}
        results={results}
        testing={testing}
        runTest={runTest}
        onDismissResult={dismissResult}
        onSaved={(from, to) => {
          // 后端可能把地址归一化（如 socks5:// 升成 socks5h://），测试结果改挂到入库后的地址上。
          setResults((prev) => {
            const result = prev[from]
            if (!result || from === to) return prev
            const next = { ...prev, [to]: result }
            delete next[from]
            return next
          })
          setPending(null)
          invalidate()
        }}
        onClose={() => setPending(null)}
      />

      {/* 只关自己这一次：账号分配弹框保存中也能关掉，旧请求回来时 onSuccess 仍会调 onOpenChange(false)。
          那时 pending 可能已换成新开的添加、编辑或另一条代理的分配框，无条件清空会把它一并关掉、
          丢掉未保存的输入。所以按打开时的那份 pending 比对，不是它就不动。 */}
      {pending?.kind === 'accounts' && (() => {
        const session = pending
        return (
          <ProxyAccountsDialog
            key={session.proxy.id}
            proxy={session.proxy}
            pool={proxies}
            open
            onOpenChange={(next) => {
              if (!next) setPending((cur) => (cur === session ? null : cur))
            }}
          />
        )
      })()}

      <ProxyBatchImportDialog
        open={importOpen}
        onOpenChange={setImportOpen}
        poolLabels={proxies.map((p) => p.label)}
        results={results}
        testing={testing}
        runTest={runTest}
      />

      <AlertDialog
        open={pending?.kind === 'delete'}
        onOpenChange={(next) => { if (!next && !remove.isPending) setPending(null) }}
      >
        {pending?.kind === 'delete' && (
          <AlertDialogPopup>
            <AlertDialogHeader>
              <AlertDialogTitle>
                {t(`删除代理「${pending.proxy.label}」`, `Delete proxy "${pending.proxy.label}"`)}
              </AlertDialogTitle>
              <AlertDialogDescription>
                {pending.proxy.credential_count > 0
                  ? t(
                      `当前有 ${pending.proxy.credential_count} 个账号正在使用此代理。删除后，这些账号的代理设置保持不变，该代理仅从代理池中移除。`,
                      `${pending.proxy.credential_count} account${pending.proxy.credential_count === 1 ? ' is' : 's are'} currently using this proxy. Deleting it won’t change those accounts’ proxy settings, but it will no longer appear in the pool.`,
                    )
                  : t('确定从代理池中删除此代理？', 'Remove this entry from the proxy pool?')}
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogClose render={<Button variant="outline" />}>
                {t('取消', 'Cancel')}
              </AlertDialogClose>
              <Button variant="destructive" loading={remove.isPending} onClick={() => remove.mutate(pending.proxy)}>
                {t('删除', 'Delete')}
              </Button>
            </AlertDialogFooter>
          </AlertDialogPopup>
        )}
      </AlertDialog>

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
                    `其中 ${failedInUse.length} 条仍被 ${failedInUseAccounts} 个账号使用。删除仅会将其从代理池移除，这些账号的代理设置保持不变，仍将使用这些不可用的代理；如需更换，请先在「调整使用账号」中调整。`,
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
    </div>
  )
}

/**
 * 代理池里的一行。原来每行把使用账号的名字一个个铺成徽章、测试结果再展开成一个框，号一多一行就占
 * 半屏；现在一行一条：使用账号只给个数（悬浮看前几个名字，点开调整），测试结果收成一格读数。
 */
function ProxyRow({
  proxy,
  result,
  testing,
  onTest,
  onPending,
}: {
  proxy: SavedProxy
  result: ProxyTestResult | undefined
  testing: boolean
  onTest: () => void
  onPending: (kind: 'edit' | 'accounts' | 'delete') => void
}) {
  const { t } = useI18n()
  const masked = proxyMaskedUrl(proxy.url)
  // 窄屏名称底下只有 100 来 px，整串地址截到最后只剩「socks5h://…」；去掉协议与打码的凭据，留 host:port。
  const host = masked.replace(/^[a-z0-9+.-]+:\/\/(?:[^@/]*@)?/i, '')
  const names = proxy.credential_labels
  const usedHint = names.length === 0
    ? t('暂无账号使用，点击分配', 'Not in use. Click to assign.')
    : names.slice(0, USED_BY_PREVIEW).join('、') +
      (names.length > USED_BY_PREVIEW
        ? t(` 等 ${names.length} 个账号`, ` and ${names.length - USED_BY_PREVIEW} more`)
        : '')

  return (
    <TableRow>
      <TableCell className={COL.name}>
        <p className="truncate font-medium">{proxy.label}</p>
        {/* 窄屏没有地址列，地址收到名称底下。 */}
        <p className="mt-1 truncate text-xs text-muted-foreground md:hidden">{host}</p>
      </TableCell>
      <TableCell className={COL.url}>
        <Hint label={masked}>
          <p className="truncate text-muted-foreground">{masked}</p>
        </Hint>
      </TableCell>
      <TableCell className={COL.used}>
        <Hint label={usedHint}>
          <Button
            aria-label={t(`调整使用账号（${proxy.credential_count}）`, `Manage accounts (${proxy.credential_count})`)}
            className={cn('tabular-nums', proxy.credential_count === 0 && 'text-muted-foreground')}
            size="sm"
            variant="ghost"
            onClick={() => onPending('accounts')}
          >
            <UsersIcon />
            {proxy.credential_count}
          </Button>
        </Hint>
      </TableCell>
      <TableCell className={COL.status}>
        <ProxyStatus result={result} testing={testing} />
      </TableCell>
      <TableCell className={cn(COL.actions, 'text-right')}>
        <Hint label={t('测试', 'Test')}>
          <Button size="icon-sm" variant="ghost" loading={testing} onClick={onTest} aria-label={t('测试', 'Test')}>
            <PlayIcon />
          </Button>
        </Hint>
        <Menu>
          <MenuTrigger
            aria-label={t(`${proxy.label} 的操作`, `Actions for ${proxy.label}`)}
            className={buttonVariants({ size: 'icon-sm', variant: 'ghost' })}
          >
            <EllipsisIcon />
          </MenuTrigger>
          <MenuPopup align="end" className="w-44">
            <MenuItem onClick={() => onPending('accounts')}>
              <UsersIcon />{t('调整使用账号', 'Manage accounts')}
            </MenuItem>
            <MenuItem onClick={() => onPending('edit')}>
              <PencilIcon />{t('编辑', 'Edit')}
            </MenuItem>
            <MenuSeparator />
            <MenuItem variant="destructive" onClick={() => onPending('delete')}>
              <Trash2Icon />{t('删除', 'Delete')}
            </MenuItem>
          </MenuPopup>
        </Menu>
      </TableCell>
    </TableRow>
  )
}

/** 连通性一格：没测过是「未测试」，通了给地区与延迟（悬浮看出口 IP），不通给「不可用」（悬浮看原因）。 */
function ProxyStatus({ result, testing }: { result: ProxyTestResult | undefined; testing: boolean }) {
  const { t } = useI18n()
  if (testing) {
    return (
      <span className="inline-flex items-center gap-1.5 text-muted-foreground">
        <Spinner className="size-3.5" />
        {t('测试中', 'Testing')}
      </span>
    )
  }
  if (!result) return <span className="text-muted-foreground">{t('未测试', 'Not tested')}</span>
  if (!result.ok) {
    return (
      <Hint label={result.error ?? t('不可用', 'Unreachable')}>
        <span className="inline-flex max-w-full items-center gap-1.5 text-destructive-foreground">
          <span aria-hidden="true" className="size-1.5 shrink-0 rounded-full bg-destructive" />
          <span className="truncate">{t('不可用', 'Unreachable')}</span>
        </span>
      </Hint>
    )
  }
  const place = [result.city, result.country].filter(Boolean).join(', ')
  const detail = [result.ip, [result.city, result.region, result.country].filter(Boolean).join(', '), result.org]
    .filter(Boolean)
    .join(' · ')
  return (
    <Hint label={detail}>
      <span className="inline-flex max-w-full items-center gap-1.5">
        <span aria-hidden="true" className="size-1.5 shrink-0 rounded-full bg-success" />
        {/* 窄屏这一格只有 76px，只放得下延迟；地区留给悬浮提示。 */}
        {place && <span className="truncate max-sm:hidden">{place}</span>}
        <span className="shrink-0 text-muted-foreground tabular-nums">{formatMs(result.latency_ms)}</span>
      </span>
    </Hint>
  )
}

/** 添加 / 编辑代理的弹框。`proxy` 为 null 是添加。测试结果与页面共用一份（按地址记）。 */
function ProxyFormDialog({
  proxy,
  open,
  pool,
  results,
  testing,
  runTest,
  onDismissResult,
  onSaved,
  onClose,
}: {
  proxy: SavedProxy | null
  open: boolean
  pool: SavedProxy[]
  results: Record<string, ProxyTestResult>
  testing: Set<string>
  runTest: (url: string) => Promise<ProxyTestResult>
  onDismissResult: (url: string) => void
  /** 保存成功：`from` 是框里填的地址，`to` 是入库后（可能归一化过）的地址。 */
  onSaved: (from: string, to: string) => void
  onClose: () => void
}) {
  const { t, language } = useI18n()
  const editing = proxy != null
  const [label, setLabel] = useState('')
  const [url, setUrl] = useState('')
  // 每次打开按目标重新填一遍：上一次没保存的输入不该带到下一条上。
  const [openedFor, setOpenedFor] = useState<number | 'add' | null>(null)
  const target = open ? (proxy?.id ?? 'add') : null
  if (target !== openedFor) {
    setOpenedFor(target)
    if (target !== null) {
      setLabel(proxy?.label ?? '')
      setUrl(proxy?.url ?? '')
    }
  }

  const trimmedUrl = url.trim()
  const result = trimmedUrl ? results[trimmedUrl] : undefined

  const save = useMutation({
    mutationFn: () => {
      if (proxy) return updateProxy(proxy.id, label.trim(), trimmedUrl)
      const name = label.trim() || locationLabel(result, pool.map((p) => p.label))
      return addProxy(name, trimmedUrl)
    },
    onSuccess: (saved) => {
      toastManager.add({
        title: editing ? t('已更新代理', 'Proxy updated') : t('已添加代理', 'Proxy added'),
        description: saved.label,
        type: 'success',
      })
      onSaved(trimmedUrl, saved.url)
    },
    onError: (e) =>
      toastManager.add({
        title: editing ? t('更新代理失败', 'Failed to update proxy') : t('添加代理失败', 'Failed to add proxy'),
        description: extractError(e, language),
        type: 'error',
      }),
  })
  const canSubmit = !!trimmedUrl && (!editing || !!label.trim())

  return (
    <Dialog open={open} onOpenChange={(next) => { if (!next && !save.isPending) onClose() }}>
      <DialogPopup>
        <DialogHeader>
          <DialogTitle>{editing ? t('编辑代理', 'Edit proxy') : t('添加代理', 'Add proxy')}</DialogTitle>
          <DialogDescription>
            {t(
              '支持 http、https、socks5、socks5h 协议，可附带用户名与密码。建议保存前先测试连通性。',
              'Supports http, https, socks5 and socks5h, with optional username and password. Testing before saving is recommended.',
            )}
          </DialogDescription>
        </DialogHeader>
        <Form
          className="contents"
          onSubmit={(event) => {
            event.preventDefault()
            if (canSubmit && !save.isPending) save.mutate()
          }}
        >
          <DialogPanel className="space-y-5">
            <Field>
              <FieldLabel>{t('代理地址', 'Proxy URL')}</FieldLabel>
              <Input
                autoComplete="off"
                autoFocus
                placeholder="socks5://user:pass@127.0.0.1:1080"
                spellCheck={false}
                value={url}
                onChange={(event) => setUrl(event.target.value)}
              />
            </Field>
            <Field>
              <FieldLabel>{t('名称', 'Name')}</FieldLabel>
              <Input
                placeholder={editing ? undefined : t('留空则自动命名', 'Auto-named if empty')}
                value={label}
                onChange={(event) => setLabel(event.target.value)}
              />
              {!editing && (
                <FieldDescription>
                  {t('留空则自动命名：测试通过的取出口地区，否则取 host:port。', 'Leave empty to name it automatically: the exit location if tested, otherwise host:port.')}
                </FieldDescription>
              )}
            </Field>
            <div className="space-y-2">
              <Button
                type="button"
                size="sm"
                variant="outline"
                disabled={!trimmedUrl}
                loading={testing.has(trimmedUrl)}
                onClick={() => runTest(trimmedUrl)}
              >
                <PlayIcon />
                {t('测试连通性', 'Test connectivity')}
              </Button>
              {result && <ProxyTestResultView result={result} onDismiss={() => onDismissResult(trimmedUrl)} />}
            </div>
          </DialogPanel>
          <DialogFooter>
            <DialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</DialogClose>
            <Button type="submit" disabled={!canSubmit} loading={save.isPending}>
              {editing ? t('保存', 'Save') : t('添加', 'Add')}
            </Button>
          </DialogFooter>
        </Form>
      </DialogPopup>
    </Dialog>
  )
}
