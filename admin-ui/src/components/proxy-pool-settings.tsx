import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  CheckIcon,
  GlobeIcon,
  PencilIcon,
  PlusIcon,
  Trash2Icon,
  XIcon,
} from 'lucide-react'
import {
  addProxy,
  deleteProxy,
  listProxies,
  type SavedProxy,
  updateProxy,
} from '@/api/proxies'
import { useI18n } from '@/lib/i18n'
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
import { Separator } from '@/components/ui/separator'
import { Spinner } from '@/components/ui/spinner'
import { toastManager } from '@/components/ui/toast'
import { SettingsGroup } from '@/components/settings-group'

export function ProxyPoolSettingsContent() {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  const proxiesQuery = useQuery({ queryKey: ['proxies'], queryFn: listProxies })

  const [addLabel, setAddLabel] = useState('')
  const [addUrl, setAddUrl] = useState('')

  const invalidate = () => qc.invalidateQueries({ queryKey: ['proxies'] })
  const onError = (title: string, error: unknown) =>
    toastManager.add({ title, description: extractError(error, language), type: 'error' })

  const create = useMutation({
    mutationFn: () => addProxy(addLabel.trim(), addUrl.trim()),
    onSuccess: (p) => {
      toastManager.add({
        title: t('已添加代理', 'Proxy added'),
        description: p.label,
        type: 'success',
      })
      setAddLabel('')
      setAddUrl('')
      invalidate()
    },
    onError: (e) => onError(t('添加代理失败', 'Failed to add proxy'), e),
  })

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
        <Form
          className="flex flex-wrap items-end gap-2 p-4"
          onSubmit={(event) => {
            event.preventDefault()
            if (addLabel.trim() && addUrl.trim() && !create.isPending) create.mutate()
          }}
        >
          <div className="min-w-0 flex-1 space-y-1">
            <label className="text-xs font-medium" htmlFor="proxy-pool-add-label">
              {t('名称', 'Name')}
            </label>
            <Input
              id="proxy-pool-add-label"
              value={addLabel}
              onChange={(event) => setAddLabel(event.target.value)}
              placeholder={t('如：日本节点', 'e.g. Japan node')}
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
            type="submit"
            size="sm"
            disabled={!addLabel.trim() || !addUrl.trim()}
            loading={create.isPending}
          >
            <PlusIcon />
            {t('添加', 'Add')}
          </Button>
        </Form>

        {proxies.length > 0 && <Separator />}

        {proxies.length === 0 ? (
          <p className="p-4 text-center text-sm text-muted-foreground">
            {t('代理池为空，添加第一条代理地址吧。', 'The pool is empty. Add your first proxy address.')}
          </p>
        ) : (
          <ul className="divide-y" role="list">
            {proxies.map((proxy) => (
              <ProxyRow key={proxy.id} proxy={proxy} />
            ))}
          </ul>
        )}
      </SettingsGroup>
    </div>
  )
}

function ProxyRow({ proxy }: { proxy: SavedProxy }) {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  const [editing, setEditing] = useState(false)
  const [label, setLabel] = useState(proxy.label)
  const [url, setUrl] = useState(proxy.url)
  const [confirmDelete, setConfirmDelete] = useState(false)

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
      <li className="flex flex-wrap items-end gap-2 p-4">
        <div className="min-w-0 flex-1 space-y-1">
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
    <li className="flex items-center gap-3 p-4">
      <div className="min-w-0 flex-1">
        <div className="flex items-center gap-2">
          <span className="truncate text-sm font-medium">{proxy.label}</span>
          {proxy.credential_count > 0 && (
            <Badge variant="secondary" size="sm">
              {t(`${proxy.credential_count} 个账号`, `${proxy.credential_count} account${proxy.credential_count === 1 ? '' : 's'}`)}
            </Badge>
          )}
        </div>
        <p className="mt-0.5 truncate text-xs text-muted-foreground" title={proxy.url}>
          {proxy.url}
        </p>
      </div>
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

      <AlertDialog open={confirmDelete} onOpenChange={setConfirmDelete}>
        <AlertDialogPopup>
          <AlertDialogHeader>
            <AlertDialogTitle>
              {t(`删除代理「${proxy.label}」`, `Delete proxy "${proxy.label}"`)}
            </AlertDialogTitle>
            <AlertDialogDescription>
              {proxy.credential_count > 0
                ? t(
                    `当前有 ${proxy.credential_count} 个账号正在使用这条代理。删除后这些账号的代理设置不会改变，但它不再出现在代理池中。`,
                    `${proxy.credential_count} account${proxy.credential_count === 1 ? ' is' : 's are'} currently using this proxy. Deleting it won’t change those accounts’ proxy settings, but it will no longer appear in the pool.`,
                  )
                : t(
                    '确定从代理池中删除这条记录？',
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
