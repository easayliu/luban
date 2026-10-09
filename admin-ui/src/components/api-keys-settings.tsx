import { useEffect, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  BanIcon,
  CircleCheckIcon,
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
import { cn, extractError, formatFullTime } from '@/lib/utils'
import { CopyButton } from '@/components/copy-button'
import { ErrorState, LoadingState } from '@/components/state-placeholders'
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
import { Switch } from '@/components/ui/switch'
import { toastManager } from '@/components/ui/toast'
import { GroupPickerSkeleton, OrderedGroupPicker, useGroups } from '@/components/group-picker'

/** Claude Code 的接入片段。 */
export function setupSnippet(key: string): string {
  return `export ANTHROPIC_BASE_URL=${window.location.origin}\nexport ANTHROPIC_AUTH_TOKEN=${key}`
}

/**
 * 接入片段的代码块：这里的「查看与复制」和客户端接入页底部的占位片段共用一副样子。
 * 给了 `copyLabel` 就在右上角挂一枚复制按钮。
 */
export function SnippetBlock({ text, copyLabel }: { text: string; copyLabel?: string }) {
  return (
    <div className="relative">
      <pre className={cn('max-w-full overflow-x-auto rounded-lg border bg-muted/72 p-3 font-mono text-xs leading-5', copyLabel && 'pe-10')}>
        {text}
      </pre>
      {copyLabel && (
        <span className="absolute end-1.5 top-1.5">
          <CopyButton label={copyLabel} text={text} />
        </span>
      )}
    </div>
  )
}

/** 显示一把 Key 的明文与接入片段，各带复制按钮。新建成功与「查看」都用它。 */
function KeyReveal({ secret }: { secret: string }) {
  const { t } = useI18n()
  return (
    <div className="space-y-3">
      <div className="flex items-center gap-2 rounded-lg border px-3 py-2">
        <code className="min-w-0 flex-1 truncate font-mono text-sm">{secret}</code>
        <CopyButton label={t('复制 Key', 'Copy key')} text={secret} />
      </div>
      <SnippetBlock copyLabel={t('复制接入片段', 'Copy setup snippet')} text={setupSnippet(secret)} />
    </div>
  )
}

/**
 * 接入 Key 管理（系统设置 → 客户端接入）：给外部系统对接用，可以建多把，每把按顺序绑定若干
 * 号池分组（不绑 = 全部号；排在前面的分组优先，用不了才溢出到后面的）。
 *
 * `envKey`：`LUBAN_API_KEY` 设的那把（可用全部号），只读列在最上面。
 */
export function ApiKeysSettings({ envKey, required }: { envKey: string | null; required: boolean }) {
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
  // 「是否要求接入 Key」（settings 的 api_keys_required）在首次建 Key 时由服务端翻转，Key 有增删改
  // 都一并刷新 settings，否则删光 Key 后页面还停在建 Key 之前的状态，提示「任何人都能使用」。
  const refresh = () => {
    void qc.invalidateQueries({ queryKey: ['api-keys'] })
    void qc.invalidateQueries({ queryKey: ['settings'] })
  }
  const reveal = useMutation({
    mutationFn: (k: ApiKey) => revealApiKey(k.id).then((secret) => ({ k, secret })),
    onSuccess: ({ k, secret }) => setRevealed({ title: k.label || k.prefix, secret }),
    onError: failed,
  })
  const toggle = useMutation({
    // 只改启停，不碰范围（不传 all_groups）：绑定的分组被删光的 Key 停用再启用也不会变成全部号。
    mutationFn: (k: ApiKey) => updateApiKey(k.id, { label: k.label, disabled: !k.disabled }),
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
              '供外部系统对接使用。可绑定账号分组：仅从所绑定分组的账号中调度，靠前的分组优先；未绑定时可使用全部账号。',
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
              <div className="text-xs text-muted-foreground">{t('可使用全部账号，此处只读。', 'Uses every account; read-only here.')}</div>
            </div>
            <CopyButton label={t('复制接入片段', 'Copy setup snippet')} text={setupSnippet(envKey)} />
          </div>
        )}
        {/* 列表没拉到之前别落到下面「尚未配置接入 Key」的提示：那句话在加载中与读取失败时都是错的。 */}
        {keysQuery.isPending ? (
          <LoadingState className="min-h-24" label={t('正在加载接入 Key', 'Loading access keys')} />
        ) : keysQuery.isError ? (
          <ErrorState
            error={keysQuery.error}
            title={t('无法读取接入 Key', 'Unable to load access keys')}
            onRetry={() => keysQuery.refetch()}
            retrying={keysQuery.isFetching}
          />
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
                {k.all_groups
                  ? t('全部账号', 'All accounts')
                  : k.groups.length === 0
                    ? (
                        <Badge size="xs" variant="warning">
                          {t('绑定的分组已删除，暂不可用', 'Bound groups were deleted; unusable')}
                        </Badge>
                      )
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
        {keysQuery.isSuccess && !envKey && keys.length === 0 && (
          <div className="px-3 py-3 text-xs text-warning-foreground">
            {required
              ? t(
                  '当前没有可用的接入 Key：所有转发请求都将被拒绝。新建 Key 后即可恢复。',
                  'There is no access key: every forwarded request is rejected until you create one.',
                )
              : t(
                  '尚未配置接入 Key：转发不校验来访身份，任何客户端均可使用全部账号。',
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
              {t('请将 Key 或接入片段配置到对接系统，切勿截图外传。', 'Configure the key or the snippet in the calling system. Do not share screenshots of it.')}
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
              {t('删除后，使用该 Key 的系统将立即收到 401。', 'Systems using this key get 401 immediately after it is deleted.')}
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
  // 范围是显式的：「不限分组」开着才是全部号；关着就只限所选分组，一个都不选就谁也用不了。
  const [allGroups, setAllGroups] = useState(true)
  useEffect(() => {
    if (!apiKey) return
    setLabel(apiKey === 'new' ? '' : apiKey.label)
    setGroupIds(apiKey === 'new' ? [] : apiKey.groups)
    setAllGroups(apiKey === 'new' ? true : apiKey.all_groups)
  }, [apiKey])
  const save = useMutation({
    mutationFn: async (input: { label: string; groupIds: number[]; allGroups: boolean }) => {
      if (apiKey === 'new') {
        const created = await createApiKey(input.label, input.groupIds, input.allGroups)
        return { secret: created.key, label: input.label }
      }
      if (apiKey) {
        await updateApiKey(apiKey.id, {
          label: input.label,
          disabled: apiKey.disabled,
          all_groups: input.allGroups,
          group_ids: input.allGroups ? [] : input.groupIds,
        })
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

  // 新建时不许建出谁也用不了的 Key；编辑时允许保存（比如只改名字），上面已给出警告。
  const canSubmit = apiKey !== 'new' || allGroups || groupIds.length > 0

  return (
    <Dialog open={!!apiKey} onOpenChange={(next) => { if (!next && !save.isPending) onClose() }}>
      <DialogPopup>
        <DialogHeader>
          <DialogTitle>{apiKey === 'new' ? t('新建接入 Key', 'New access key') : t('编辑接入 Key', 'Edit access key')}</DialogTitle>
          <DialogDescription>
            {apiKey === 'new'
              ? t('Key 由系统生成，创建后可随时在「查看与复制」中取用。', 'The key is generated for you and can be viewed any time under “View & copy”.')
              : t('修改绑定的分组立即生效。', 'Changes to the bound groups take effect immediately.')}
          </DialogDescription>
        </DialogHeader>
        <Form
          className="contents"
          onSubmit={(event) => {
            event.preventDefault()
            if (!save.isPending && canSubmit) save.mutate({ label: label.trim(), groupIds, allGroups })
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
              <FieldLabel className="flex w-full items-center justify-between gap-3">
                <span>{t('不限分组（可使用全部账号）', 'Any group (all accounts)')}</span>
                <Switch checked={allGroups} onCheckedChange={setAllGroups} />
              </FieldLabel>
              <FieldDescription>
                {allGroups
                  ? t('该 Key 可使用账号池中的全部账号。', 'This key can use every account in the pool.')
                  : t('仅从下方所选分组的账号中调度。', 'Only accounts in the groups below are used.')}
              </FieldDescription>
            </Field>
            {!allGroups && (
              <Field>
                <FieldLabel>{t('绑定的账号分组', 'Bound account groups')}</FieldLabel>
                {groups ? (
                  <OrderedGroupPicker groups={groups} value={groupIds} onChange={setGroupIds} emptyHint={null} />
                ) : (
                  <GroupPickerSkeleton rows={2} />
                )}
                <FieldDescription>
                  {groupIds.length === 0
                    ? (
                        <span className="text-warning-foreground">
                          {t('尚未选择分组：该 Key 暂时无法使用任何账号。', 'No group selected: this key cannot use any account yet.')}
                        </span>
                      )
                    : t('顺序即优先级：靠前分组的账号均不可用时，才会使用后续分组的账号。', 'Order is priority: later groups are used only when every account in earlier groups is unavailable.')}
                </FieldDescription>
              </Field>
            )}
          </DialogPanel>
          <DialogFooter>
            <DialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</DialogClose>
            <Button disabled={!canSubmit} loading={save.isPending} type="submit">
              {apiKey === 'new' ? t('新建', 'Create') : t('保存', 'Save')}
            </Button>
          </DialogFooter>
        </Form>
      </DialogPopup>
    </Dialog>
  )
}
