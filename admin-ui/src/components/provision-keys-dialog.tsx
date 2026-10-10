import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { BanIcon, CircleCheckIcon, EllipsisIcon, KeyRoundIcon, PlusIcon, Trash2Icon } from 'lucide-react'
import {
  createProvisionKey,
  deleteProvisionKey,
  listProvisionKeys,
  updateProvisionKey,
  type ProvisionKey,
} from '@/api/groups'
import { useI18n } from '@/lib/i18n'
import { useIsAdmin } from '@/lib/role'
import { cn, extractError, formatFullTime } from '@/lib/utils'
import { CopyButton } from '@/components/copy-button'
import { ErrorState, LoadingState } from '@/components/state-placeholders'
import { Badge } from '@/components/ui/badge'
import { Button, buttonVariants } from '@/components/ui/button'
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
import { Form } from '@/components/ui/form'
import { Input } from '@/components/ui/input'
import { Menu, MenuItem, MenuPopup, MenuSeparator, MenuTrigger } from '@/components/ui/menu'
import { toastManager } from '@/components/ui/toast'

/** 脚本上号的调用示例：先取授权链接，登录后把回调页上的 `code#state` 交回来。 */
function scriptSnippet(key: string): string {
  const base = window.location.origin
  return [
    `KEY=${key}`,
    '# 1. Get an authorization URL and sign in with it',
    `curl -s -H "Authorization: Bearer $KEY" ${base}/api/authorize`,
    '# 2. Submit the code#state shown after sign-in.',
    '#    Proxy: omit both fields to auto-assign the least-used proxy that passes a test,',
    '#    or set "proxy_id" (GET /api/proxies) or "proxy" (a full URL).',
    `curl -s -H "Authorization: Bearer $KEY" -H 'Content-Type: application/json' \\`,
    `  -d '{"code":"<code#state>","group_ids":[]}' ${base}/api/exchange`,
  ].join('\n')
}

/**
 * 上号 Key（账号菜单 → 上号 Key）：给自动化脚本添加账号用。Key 只能调用取授权链接、交授权码
 * 与列出分组这几个接口，上的号落在建 Key 的人名下。明文只在新建时显示一次。
 * admin 能看到并管理所有人的 Key；代理和用户只看到自己的。
 */
export function ProvisionKeysDialog({ open, onOpenChange }: { open: boolean; onOpenChange: (open: boolean) => void }) {
  const { t, language } = useI18n()
  const isAdmin = useIsAdmin()
  const qc = useQueryClient()
  const keysQuery = useQuery({ queryKey: ['provision-keys'], queryFn: listProvisionKeys, enabled: open })
  const keys = keysQuery.data ?? []
  const [label, setLabel] = useState('')
  const [created, setCreated] = useState<string | null>(null)
  const [deleting, setDeleting] = useState<number | null>(null)

  const failed = (error: unknown) => {
    toastManager.add({ title: t('操作失败', 'Operation failed'), description: extractError(error, language), type: 'error' })
  }
  const refresh = () => void qc.invalidateQueries({ queryKey: ['provision-keys'] })
  const create = useMutation({
    mutationFn: (name: string) => createProvisionKey(name),
    onSuccess: (res) => {
      refresh()
      setLabel('')
      setCreated(res.key)
    },
    onError: failed,
  })
  const toggle = useMutation({
    mutationFn: (k: ProvisionKey) => updateProvisionKey(k.id, { label: k.label, disabled: !k.disabled }),
    onSuccess: (_r, k) => {
      refresh()
      toastManager.add({ title: k.disabled ? t('Key 已启用', 'Key enabled') : t('Key 已停用', 'Key disabled'), type: 'success' })
    },
    onError: failed,
  })
  const remove = useMutation({
    mutationFn: (k: ProvisionKey) => deleteProvisionKey(k.id),
    onSuccess: () => {
      refresh()
      setDeleting(null)
      toastManager.add({ title: t('Key 已删除', 'Key deleted'), type: 'success' })
    },
    onError: failed,
  })

  const close = (next: boolean) => {
    if (next) return onOpenChange(true)
    setCreated(null)
    setDeleting(null)
    onOpenChange(false)
  }

  return (
    <Dialog open={open} onOpenChange={close}>
      <DialogPopup className="sm:max-w-xl">
        <DialogHeader>
          <DialogTitle>{t('上号 Key', 'Provision keys')}</DialogTitle>
          <DialogDescription>
            {t(
              '供自动化脚本添加账号使用。Key 仅能调用获取授权链接、提交授权码、查询分组与代理池的接口，添加的账号归属于 Key 的创建者。未指定代理时，按挂载账号从少到多测试创建者代理池中的代理，使用第一条测试通过的；代理池为空时直连，均不通时报错。修改密码后，名下的上号 Key 全部失效。',
              'For scripts that add accounts. A key can only get authorization links, submit authorization codes, and list groups and proxies; accounts it adds belong to the key’s creator. Without a specified proxy, proxies in the creator’s pool are tested from least to most used and the first that passes is used; an empty pool connects directly, and the request fails if none pass. Changing your password revokes all your provision keys.',
            )}
          </DialogDescription>
        </DialogHeader>
        <DialogPanel className="space-y-4">
          {created ? (
            <div className="space-y-3">
              <p className="text-xs text-warning-foreground">
                {t('Key 明文仅显示这一次，请立即复制保存。', 'The key is shown only once. Copy it now.')}
              </p>
              <div className="flex items-center gap-2 rounded-lg border px-3 py-2">
                <code className="min-w-0 flex-1 truncate font-mono text-sm">{created}</code>
                <CopyButton label={t('复制 Key', 'Copy key')} text={created} />
              </div>
              <div className="relative">
                <pre className="max-w-full overflow-x-auto rounded-lg border bg-muted/72 p-3 pe-10 font-mono text-xs leading-5">
                  {scriptSnippet(created)}
                </pre>
                <span className="absolute end-1.5 top-1.5">
                  <CopyButton label={t('复制调用示例', 'Copy example')} text={scriptSnippet(created)} />
                </span>
              </div>
              <Button size="sm" variant="outline" onClick={() => setCreated(null)}>
                {t('返回列表', 'Back to list')}
              </Button>
            </div>
          ) : (
            <>
              <Form
                className="flex gap-2"
                onSubmit={(event) => {
                  event.preventDefault()
                  if (!create.isPending) create.mutate(label.trim())
                }}
              >
                <Input
                  aria-label={t('名称', 'Name')}
                  className="flex-1"
                  maxLength={64}
                  onChange={(event) => setLabel(event.target.value)}
                  placeholder={t('名称，例如：批量上号脚本', 'Name, e.g. bulk add script')}
                  value={label}
                />
                <Button loading={create.isPending} type="submit">
                  <PlusIcon />{t('新建', 'Create')}
                </Button>
              </Form>
              <div className="divide-y rounded-lg border">
                {keysQuery.isPending ? (
                  <LoadingState className="min-h-24" label={t('正在加载上号 Key', 'Loading provision keys')} />
                ) : keysQuery.isError ? (
                  <ErrorState
                    error={keysQuery.error}
                    title={t('无法读取上号 Key', 'Unable to load provision keys')}
                    onRetry={() => keysQuery.refetch()}
                    retrying={keysQuery.isFetching}
                  />
                ) : keys.length === 0 ? (
                  <div className="px-3 py-3 text-xs text-muted-foreground">{t('尚未创建上号 Key。', 'No provision keys yet.')}</div>
                ) : keys.map((k) => (
                  <div className={cn('flex items-center gap-3 px-3 py-2.5 text-sm', k.disabled && 'opacity-64')} key={k.id}>
                    <KeyRoundIcon aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
                    <div className="min-w-0 flex-1">
                      <div className="flex flex-wrap items-center gap-1.5">
                        <span className="truncate font-medium">{k.label || t('未命名', 'Untitled')}</span>
                        <code className="font-mono text-xs text-muted-foreground">{k.prefix}…</code>
                        {k.disabled && <Badge size="xs" variant="error">{t('已停用', 'Disabled')}</Badge>}
                      </div>
                      <div className="mt-0.5 flex flex-wrap items-center gap-1 text-xs text-muted-foreground">
                        {isAdmin && <span>{k.username} ·</span>}
                        <span>
                          {k.last_used_at
                            ? t(`最近使用 ${formatFullTime(k.last_used_at, language)}`, `Last used ${formatFullTime(k.last_used_at, language)}`)
                            : t('从未使用', 'Never used')}
                        </span>
                      </div>
                    </div>
                    {deleting === k.id ? (
                      <div className="flex shrink-0 items-center gap-1">
                        <Button disabled={remove.isPending} size="sm" variant="ghost" onClick={() => setDeleting(null)}>
                          {t('取消', 'Cancel')}
                        </Button>
                        <Button loading={remove.isPending} size="sm" variant="destructive" onClick={() => remove.mutate(k)}>
                          {t('确认删除', 'Delete')}
                        </Button>
                      </div>
                    ) : (
                      <Menu>
                        <MenuTrigger
                          aria-label={t('Key 操作', 'Key actions')}
                          className={buttonVariants({ size: 'icon-sm', variant: 'ghost' })}
                        >
                          <EllipsisIcon />
                        </MenuTrigger>
                        <MenuPopup align="end" className="w-40">
                          <MenuItem onClick={() => toggle.mutate(k)}>
                            {k.disabled ? <CircleCheckIcon /> : <BanIcon />}
                            {k.disabled ? t('启用', 'Enable') : t('停用', 'Disable')}
                          </MenuItem>
                          <MenuSeparator />
                          <MenuItem variant="destructive" onClick={() => setDeleting(k.id)}>
                            <Trash2Icon />{t('删除', 'Delete')}
                          </MenuItem>
                        </MenuPopup>
                      </Menu>
                    )}
                  </div>
                ))}
              </div>
            </>
          )}
        </DialogPanel>
        <DialogFooter>
          <DialogClose render={<Button variant="outline" />}>{t('关闭', 'Close')}</DialogClose>
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  )
}
