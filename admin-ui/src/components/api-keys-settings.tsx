import { useEffect, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  BanIcon,
  CheckIcon,
  CircleCheckIcon,
  CopyIcon,
  EllipsisIcon,
  EyeIcon,
  KeyRoundIcon,
  PencilIcon,
  PlusIcon,
  TerminalIcon,
  Trash2Icon,
} from 'lucide-react'
import {
  createApiKey,
  deleteApiKey,
  listApiKeys,
  revealApiKey,
  updateApiKey,
  type ApiKey,
} from '@/api/groups'
import { useI18n } from '@/lib/i18n'
import { cn, copyText, extractError, formatFullTime } from '@/lib/utils'
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
import { Form } from '@/components/ui/form'
import { Input } from '@/components/ui/input'
import { Menu, MenuItem, MenuPopup, MenuSeparator, MenuTrigger } from '@/components/ui/menu'
import { toastManager } from '@/components/ui/toast'
import { Hint } from '@/components/ui/tooltip'
import { OrderedGroupPicker, useGroups } from '@/components/group-picker'

/** Claude Code 的接入片段。 */
export function setupSnippet(key: string): string {
  return `export ANTHROPIC_BASE_URL=${window.location.origin}\nexport ANTHROPIC_AUTH_TOKEN=${key}`
}

function CopyInline({ text, label }: { text: string; label: string }) {
  const { t } = useI18n()
  const [copied, setCopied] = useState(false)
  return (
    <Hint label={copied ? t('已复制', 'Copied') : label}>
      <Button
        aria-label={label}
        size="icon-xs"
        type="button"
        variant="ghost"
        onClick={() => {
          void copyText(text).then((ok) => {
            if (!ok) return
            setCopied(true)
            setTimeout(() => setCopied(false), 2000)
          })
        }}
      >
        {copied ? <CheckIcon /> : <CopyIcon />}
      </Button>
    </Hint>
  )
}

/** 显示一把 Key 的明文与接入片段，各带复制按钮。新建成功与「查看」都用它。 */
function KeyReveal({ secret }: { secret: string }) {
  const { t } = useI18n()
  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2 rounded-lg border px-3 py-2">
        <code className="min-w-0 flex-1 truncate font-mono text-sm">{secret}</code>
        <CopyInline label={t('复制 Key', 'Copy key')} text={secret} />
      </div>
      <div className="relative">
        <pre className="max-w-full overflow-x-auto rounded-lg border bg-muted/72 p-3 pe-10 font-mono text-xs leading-5">
          {setupSnippet(secret)}
        </pre>
        <span className="absolute end-1.5 top-1.5">
          <CopyInline label={t('复制接入片段', 'Copy setup snippet')} text={setupSnippet(secret)} />
        </span>
      </div>
    </div>
  )
}

/**
 * 接入 Key 管理（系统设置 → 客户端接入）：给外部系统对接用，可以建多把，每把按顺序绑定若干
 * 号池分组（不绑 = 全部号；排在前面的分组优先，用不了才溢出到后面的）。
 *
 * `envKey`：`LUBAN_API_KEY` 设的那把（可用全部号），只读列在最上面。
 */
export function ApiKeysSettings({ envKey }: { envKey: string | null }) {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  const keysQuery = useQuery({ queryKey: ['api-keys'], queryFn: listApiKeys })
  const { data: groups } = useGroups()
  const keys = keysQuery.data ?? []
  const groupName = (id: number) => groups?.find((g) => g.id === id)?.name ?? `#${id}`
  const [editing, setEditing] = useState<ApiKey | 'new' | null>(null)
  const [revealed, setRevealed] = useState<{ title: string; secret: string } | null>(null)
  const [deleting, setDeleting] = useState<ApiKey | null>(null)

  const failed = (error: unknown) => {
    toastManager.add({ title: t('操作失败', 'Operation failed'), description: extractError(error, language), type: 'error' })
  }
  const refresh = () => void qc.invalidateQueries({ queryKey: ['api-keys'] })
  const reveal = useMutation({
    mutationFn: (k: ApiKey) => revealApiKey(k.id).then((secret) => ({ k, secret })),
    onSuccess: ({ k, secret }) => setRevealed({ title: k.label || k.prefix, secret }),
    onError: failed,
  })
  const toggle = useMutation({
    mutationFn: (k: ApiKey) => updateApiKey(k.id, { label: k.label, disabled: !k.disabled, group_ids: k.groups }),
    onSuccess: (_r, k) => {
      refresh()
      toastManager.add({ title: k.disabled ? t('Key 已启用', 'Key enabled') : t('Key 已停用', 'Key disabled'), type: 'success' })
    },
    onError: failed,
  })
  const remove = useMutation({
    mutationFn: (k: ApiKey) => deleteApiKey(k.id),
    onSuccess: () => {
      refresh()
      setDeleting(null)
      toastManager.add({ title: t('Key 已删除', 'Key deleted'), type: 'success' })
    },
    onError: failed,
  })

  return (
    <div className="p-4 sm:p-5">
      <div className="flex flex-wrap items-center justify-between gap-2">
        <div>
          <div className="text-sm font-medium">
            {t('接入 Key', 'Access keys')}{' '}
            <code className="font-mono text-xs font-normal text-muted-foreground">ANTHROPIC_AUTH_TOKEN</code>
          </div>
          <p className="mt-1 text-xs text-muted-foreground">
            {t(
              '给外部系统对接用。可绑定号池分组：只在这些分组的号里选号，排在前面的分组优先；不绑定则可用全部号。',
              'For external systems. Bind pool groups to restrict which accounts a key uses (earlier groups first); unbound keys can use every account.',
            )}
          </p>
        </div>
        <Button size="sm" onClick={() => setEditing('new')}>
          <PlusIcon />{t('新建 Key', 'New key')}
        </Button>
      </div>

      <div className="mt-3 divide-y rounded-lg border">
        {envKey && (
          <div className="flex items-center gap-3 px-3 py-2.5 text-sm">
            <KeyRoundIcon aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
            <div className="min-w-0 flex-1">
              <div className="font-medium">
                {t('环境变量', 'Environment variable')} <code className="font-mono text-xs">LUBAN_API_KEY</code>
              </div>
              <div className="text-xs text-muted-foreground">{t('可用全部号，此处只读。', 'Uses every account; read-only here.')}</div>
            </div>
            <CopyInline label={t('复制接入片段', 'Copy setup snippet')} text={setupSnippet(envKey)} />
          </div>
        )}
        {keys.map((k) => (
          <div className={cn('flex items-center gap-3 px-3 py-2.5 text-sm', k.disabled && 'opacity-64')} key={k.id}>
            <KeyRoundIcon aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
            <div className="min-w-0 flex-1">
              <div className="flex flex-wrap items-center gap-1.5">
                <span className="truncate font-medium">{k.label || t('未命名', 'Untitled')}</span>
                <code className="font-mono text-xs text-muted-foreground">{k.prefix}…</code>
                {k.disabled && <Badge size="xs" variant="error">{t('已停用', 'Disabled')}</Badge>}
              </div>
              <div className="mt-0.5 flex flex-wrap items-center gap-1 text-xs text-muted-foreground">
                {k.groups.length === 0
                  ? t('全部号', 'All accounts')
                  : k.groups.map((id, i) => (
                      <Badge key={id} size="xs" variant="outline">{i + 1}. {groupName(id)}</Badge>
                    ))}
                <span>· {formatFullTime(k.created_at, language)}</span>
              </div>
            </div>
            <Menu>
              <MenuTrigger
                aria-label={t('Key 操作', 'Key actions')}
                className={buttonVariants({ size: 'icon-sm', variant: 'ghost' })}
              >
                <EllipsisIcon />
              </MenuTrigger>
              <MenuPopup align="end" className="w-44">
                <MenuItem onClick={() => reveal.mutate(k)}>
                  <EyeIcon />{t('查看与复制', 'View & copy')}
                </MenuItem>
                <MenuItem onClick={() => setEditing(k)}>
                  <PencilIcon />{t('编辑', 'Edit')}
                </MenuItem>
                <MenuItem onClick={() => toggle.mutate(k)}>
                  {k.disabled ? <CircleCheckIcon /> : <BanIcon />}
                  {k.disabled ? t('启用', 'Enable') : t('停用', 'Disable')}
                </MenuItem>
                <MenuSeparator />
                <MenuItem variant="destructive" onClick={() => setDeleting(k)}>
                  <Trash2Icon />{t('删除', 'Delete')}
                </MenuItem>
              </MenuPopup>
            </Menu>
          </div>
        ))}
        {!envKey && keys.length === 0 && (
          <div className="px-3 py-3 text-xs text-warning-foreground">
            {t(
              '尚未配置接入 Key：转发不校验来访身份，任何人都能使用全部号。',
              'No access key is configured: forwarding does not authenticate callers, and anyone can use every account.',
            )}
          </div>
        )}
      </div>

      <KeyEditDialog
        apiKey={editing}
        onClose={() => setEditing(null)}
        onCreated={(secret, label) => {
          refresh()
          setEditing(null)
          setRevealed({ title: label, secret })
        }}
        onSaved={() => { refresh(); setEditing(null) }}
      />
      <Dialog open={!!revealed} onOpenChange={(next) => { if (!next) setRevealed(null) }}>
        <DialogPopup>
          <DialogHeader>
            <DialogTitle className="flex items-center gap-2">
              <TerminalIcon aria-hidden="true" className="size-4" />
              {revealed?.title || t('接入 Key', 'Access key')}
            </DialogTitle>
            <DialogDescription>
              {t('把 Key 或接入片段配置到对接系统。请勿截图外传。', 'Configure the key or the snippet in the calling system. Do not share screenshots of it.')}
            </DialogDescription>
          </DialogHeader>
          <DialogPanel>{revealed && <KeyReveal secret={revealed.secret} />}</DialogPanel>
          <DialogFooter>
            <DialogClose render={<Button />}>{t('完成', 'Done')}</DialogClose>
          </DialogFooter>
        </DialogPopup>
      </Dialog>
      <AlertDialog open={!!deleting} onOpenChange={(next) => { if (!next && !remove.isPending) setDeleting(null) }}>
        <AlertDialogPopup>
          <AlertDialogHeader>
            <AlertDialogTitle>{t('删除接入 Key', 'Delete access key')}</AlertDialogTitle>
            <AlertDialogDescription>
              {t('删除后，使用这把 Key 的系统会立即收到 401。', 'Systems using this key get 401 immediately after it is deleted.')}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogClose render={<Button disabled={remove.isPending} variant="ghost" />}>{t('取消', 'Cancel')}</AlertDialogClose>
            <Button loading={remove.isPending} variant="destructive" onClick={() => { if (deleting) remove.mutate(deleting) }}>
              {t('删除', 'Delete')}
            </Button>
          </AlertDialogFooter>
        </AlertDialogPopup>
      </AlertDialog>
    </div>
  )
}

/** 新建 / 编辑接入 Key：名称与绑定的分组（有先后）。 */
function KeyEditDialog({
  apiKey,
  onClose,
  onCreated,
  onSaved,
}: {
  apiKey: ApiKey | 'new' | null
  onClose: () => void
  onCreated: (secret: string, label: string) => void
  onSaved: () => void
}) {
  const { t, language } = useI18n()
  const { data: groups } = useGroups(!!apiKey)
  const [label, setLabel] = useState('')
  const [groupIds, setGroupIds] = useState<number[]>([])
  useEffect(() => {
    if (!apiKey) return
    setLabel(apiKey === 'new' ? '' : apiKey.label)
    setGroupIds(apiKey === 'new' ? [] : apiKey.groups)
  }, [apiKey])
  const save = useMutation({
    mutationFn: async (input: { label: string; groupIds: number[] }) => {
      if (apiKey === 'new') {
        const created = await createApiKey(input.label, input.groupIds)
        return { secret: created.key, label: input.label }
      }
      if (apiKey) {
        await updateApiKey(apiKey.id, { label: input.label, disabled: apiKey.disabled, group_ids: input.groupIds })
      }
      return null
    },
    onSuccess: (created) => {
      if (created) onCreated(created.secret, created.label)
      else {
        onSaved()
        toastManager.add({ title: t('Key 已保存', 'Key saved'), type: 'success' })
      }
    },
    onError: (error) => {
      toastManager.add({ title: t('操作失败', 'Operation failed'), description: extractError(error, language), type: 'error' })
    },
  })

  return (
    <Dialog open={!!apiKey} onOpenChange={(next) => { if (!next && !save.isPending) onClose() }}>
      <DialogPopup>
        <DialogHeader>
          <DialogTitle>{apiKey === 'new' ? t('新建接入 Key', 'New access key') : t('编辑接入 Key', 'Edit access key')}</DialogTitle>
          <DialogDescription>
            {apiKey === 'new'
              ? t('Key 由系统生成，建好后可随时在「查看与复制」里取用。', 'The key is generated for you and can be viewed any time under “View & copy”.')
              : t('修改绑定的分组立即生效。', 'Changes to the bound groups take effect immediately.')}
          </DialogDescription>
        </DialogHeader>
        <Form
          className="contents"
          onSubmit={(event) => {
            event.preventDefault()
            if (!save.isPending) save.mutate({ label: label.trim(), groupIds })
          }}
        >
          <DialogPanel className="space-y-4">
            <Field>
              <FieldLabel>{t('名称', 'Name')}</FieldLabel>
              <Input
                autoFocus
                onChange={(event) => setLabel(event.target.value)}
                placeholder={t('例如：分发平台 A', 'e.g. Platform A')}
                value={label}
              />
            </Field>
            <Field>
              <FieldLabel>{t('绑定的号池分组', 'Bound pool groups')}</FieldLabel>
              {groups ? (
                <OrderedGroupPicker groups={groups} value={groupIds} onChange={setGroupIds} />
              ) : (
                <p className="text-sm text-muted-foreground">{t('正在加载分组', 'Loading groups')}</p>
              )}
              <FieldDescription>
                {t('顺序即优先级：前面分组的号都不可用时，才会用到后面分组的号。', 'Order is priority: later groups are used only when every account in earlier groups is unavailable.')}
              </FieldDescription>
            </Field>
          </DialogPanel>
          <DialogFooter>
            <DialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</DialogClose>
            <Button loading={save.isPending} type="submit">
              {apiKey === 'new' ? t('新建', 'Create') : t('保存', 'Save')}
            </Button>
          </DialogFooter>
        </Form>
      </DialogPopup>
    </Dialog>
  )
}
