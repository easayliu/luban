import { useEffect, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { FolderIcon, PencilIcon, PlusIcon, Trash2Icon, UsersIcon } from 'lucide-react'
import { createGroup, deleteGroup, setGroupGrants, updateGroup, type PoolGroup } from '@/api/groups'
import { listUsers, type ConsoleUser } from '@/api/users'
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
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import { Form } from '@/components/ui/form'
import { Input } from '@/components/ui/input'
import { Label } from '@/components/ui/label'
import { toastManager } from '@/components/ui/toast'
import { SettingsGroup } from '@/components/settings-group'
import { useGroups } from '@/components/group-picker'
import { ErrorState, LoadingState } from '@/components/state-placeholders'

/** 可被开放分组的成员：代理（下属用户自动继承），以及管理员直属的用户。 */
function grantable(users: ConsoleUser[]): ConsoleUser[] {
  return users.filter((u) => u.role === 'agent' || (u.role === 'user' && u.parent_username === 'admin'))
}

/**
 * 系统设置 →「号池分组」：建分组、改名、删分组、设开放名单。
 *
 * 默认分组对所有人开放、不能删。其余分组开放给代理（他名下的用户自动继承）或管理员直属的
 * 用户；代理和用户上号时只能选开放给自己的分组。删分组时，只在这一个分组里的号会挪进默认分组。
 */
export function GroupSettingsContent() {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  const groupsQuery = useGroups()
  const usersQuery = useQuery({ queryKey: ['users'], queryFn: listUsers })
  const users = usersQuery.data ?? []
  const nameOf = (id: number) => users.find((u) => u.id === id)?.username ?? `#${id}`
  const [editing, setEditing] = useState<PoolGroup | 'new' | null>(null)
  const [granting, setGranting] = useState<PoolGroup | null>(null)
  const [deleting, setDeleting] = useState<PoolGroup | null>(null)

  const refresh = () => {
    void qc.invalidateQueries({ queryKey: ['groups'] })
    void qc.invalidateQueries({ queryKey: ['credentials'] })
    void qc.invalidateQueries({ queryKey: ['api-keys'] })
  }
  const remove = useMutation({
    mutationFn: (g: PoolGroup) => deleteGroup(g.id),
    onSuccess: () => {
      refresh()
      setDeleting(null)
      toastManager.add({ title: t('分组已删除', 'Group deleted'), type: 'success' })
    },
    onError: (error) => {
      toastManager.add({ title: t('操作失败', 'Operation failed'), description: extractError(error, language), type: 'error' })
    },
  })

  if (groupsQuery.isPending) {
    return <LoadingState label={t('正在加载分组', 'Loading groups')} />
  }
  // 读失败时原先落到空列表，看着像「还没有分组」；明确报错并给重试。
  if (groupsQuery.isError) {
    return (
      <ErrorState
        error={groupsQuery.error}
        retrying={groupsQuery.isFetching}
        title={t('无法读取账号分组', 'Unable to load account groups')}
        onRetry={() => void groupsQuery.refetch()}
      />
    )
  }
  const groups = groupsQuery.data

  return (
    <div className="space-y-4">
      <SettingsGroup
        icon={FolderIcon}
        title={t('账号分组', 'Account groups')}
        description={t(
          '账号可同时属于多个分组；接入 Key 绑定分组后，只从这些分组中调度。',
          'An account can be in several groups; a key bound to groups only uses accounts from them.',
        )}
      >
        <div className="divide-y">
          {groups.map((g) => (
            <div className="flex flex-wrap items-center gap-3 px-4 py-3 sm:px-5" key={g.id}>
              <div className="min-w-0 flex-1">
                <div className="flex items-center gap-2 text-sm font-medium">
                  <span className="truncate">{g.name}</span>
                  {g.is_default && <Badge size="xs" variant="secondary">{t('默认', 'Default')}</Badge>}
                  <span className="text-xs font-normal text-muted-foreground tabular-nums">
                    {t(`${g.credential_count ?? 0} 个账号`, `${g.credential_count ?? 0} accounts`)}
                  </span>
                </div>
                {g.note && <p className="mt-0.5 text-xs text-muted-foreground">{g.note}</p>}
                <p className="mt-1 text-xs text-muted-foreground">
                  {g.is_default
                    ? t('对所有成员开放', 'Open to all members')
                    : g.grants && g.grants.length > 0
                      ? t(`开放给：${g.grants.map(nameOf).join('、')}`, `Open to: ${g.grants.map(nameOf).join(', ')}`)
                      : t('仅管理员可用', 'Admin only')}
                </p>
              </div>
              <div className="flex shrink-0 items-center gap-1.5">
                {!g.is_default && (
                  <Button size="sm" variant="outline" onClick={() => setGranting(g)}>
                    <UsersIcon />{t('开放名单', 'Access')}
                  </Button>
                )}
                <Button size="sm" variant="outline" onClick={() => setEditing(g)}>
                  <PencilIcon />{t('编辑', 'Edit')}
                </Button>
                {!g.is_default && (
                  <Button size="sm" variant="destructive-outline" onClick={() => setDeleting(g)}>
                    <Trash2Icon />{t('删除', 'Delete')}
                  </Button>
                )}
              </div>
            </div>
          ))}
        </div>
        <div className="border-t px-4 py-3 sm:px-5">
          <Button size="sm" onClick={() => setEditing('new')}>
            <PlusIcon />{t('新建分组', 'New group')}
          </Button>
        </div>
      </SettingsGroup>

      <GroupEditDialog group={editing} onClose={() => setEditing(null)} onSaved={refresh} />
      <GrantsDialog
        group={granting}
        users={grantable(users)}
        onClose={() => setGranting(null)}
        onSaved={refresh}
      />
      <AlertDialog open={!!deleting} onOpenChange={(next) => { if (!next && !remove.isPending) setDeleting(null) }}>
        <AlertDialogPopup>
          <AlertDialogHeader>
            <AlertDialogTitle>{t('删除分组', 'Delete group')}</AlertDialogTitle>
            <AlertDialogDescription>
              {deleting && t(
                `删除「${deleting.name}」后，仅属于该分组的账号将移入默认分组，接入 Key 对它的绑定同时解除。`,
                `After deleting “${deleting.name}”, accounts only in this group move to the default group, and access keys stop being bound to it.`,
              )}
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

/** 新建 / 编辑分组的名称与说明。 */
function GroupEditDialog({
  group,
  onClose,
  onSaved,
}: {
  group: PoolGroup | 'new' | null
  onClose: () => void
  onSaved: () => void
}) {
  const { t, language } = useI18n()
  const [name, setName] = useState('')
  const [note, setNote] = useState('')
  useEffect(() => {
    if (!group) return
    setName(group === 'new' ? '' : group.name)
    setNote(group === 'new' ? '' : group.note)
  }, [group])
  const save = useMutation({
    mutationFn: async ({ name, note }: { name: string; note: string }) => {
      if (group === 'new') await createGroup(name, note)
      else if (group) await updateGroup(group.id, name, note)
    },
    onSuccess: () => {
      onSaved()
      onClose()
      toastManager.add({ title: t('分组已保存', 'Group saved'), type: 'success' })
    },
    onError: (error) => {
      toastManager.add({ title: t('操作失败', 'Operation failed'), description: extractError(error, language), type: 'error' })
    },
  })
  const canSubmit = name.trim().length > 0 && name.trim().length <= 32

  return (
    <Dialog open={!!group} onOpenChange={(next) => { if (!next && !save.isPending) onClose() }}>
      <DialogPopup>
        <DialogHeader>
          <DialogTitle>{group === 'new' ? t('新建分组', 'New group') : t('编辑分组', 'Edit group')}</DialogTitle>
        </DialogHeader>
        <Form
          className="contents"
          onSubmit={(event) => {
            event.preventDefault()
            if (canSubmit && !save.isPending) save.mutate({ name: name.trim(), note: note.trim() })
          }}
        >
          <DialogPanel className="space-y-4">
            <Field>
              <FieldLabel>{t('名称', 'Name')}</FieldLabel>
              <Input autoFocus maxLength={32} onChange={(event) => setName(event.target.value)} value={name} />
              <FieldDescription>{t('1～32 个字符，不能与其他分组重名。', '1 to 32 characters, unique among groups.')}</FieldDescription>
            </Field>
            <Field>
              <FieldLabel>{t('说明（可选）', 'Note (optional)')}</FieldLabel>
              <Input onChange={(event) => setNote(event.target.value)} value={note} />
            </Field>
          </DialogPanel>
          <DialogFooter>
            <DialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</DialogClose>
            <Button disabled={!canSubmit} loading={save.isPending} type="submit">{t('保存', 'Save')}</Button>
          </DialogFooter>
        </Form>
      </DialogPopup>
    </Dialog>
  )
}

/** 设分组的开放名单：代理与管理员直属的用户，复选。 */
function GrantsDialog({
  group,
  users,
  onClose,
  onSaved,
}: {
  group: PoolGroup | null
  users: ConsoleUser[]
  onClose: () => void
  onSaved: () => void
}) {
  const { t, language } = useI18n()
  const [value, setValue] = useState<number[]>([])
  useEffect(() => {
    if (group) setValue(group.grants ?? [])
  }, [group])
  const save = useMutation({
    mutationFn: (userIds: number[]) => setGroupGrants(group!.id, userIds),
    onSuccess: () => {
      onSaved()
      onClose()
      toastManager.add({ title: t('开放名单已更新', 'Access updated'), type: 'success' })
    },
    onError: (error) => {
      toastManager.add({ title: t('操作失败', 'Operation failed'), description: extractError(error, language), type: 'error' })
    },
  })

  return (
    <Dialog open={!!group} onOpenChange={(next) => { if (!next && !save.isPending) onClose() }}>
      <DialogPopup>
        <DialogHeader>
          <DialogTitle>{t('开放名单', 'Access')}</DialogTitle>
          <DialogDescription>
            {group && t(
              `勾选的成员可将账号加入「${group.name}」。开放给代理时，其下属用户同样可用。`,
              `Checked members can add accounts to “${group.name}”. Opening it to an agent also opens it to the agent’s users.`,
            )}
          </DialogDescription>
        </DialogHeader>
        <DialogPanel>
          {users.length === 0 ? (
            <p className="text-sm text-muted-foreground">
              {t('暂无代理或管理员直属的用户，请先在「成员管理」中创建。', 'No agents or users directly under the admin yet; create them under Members first.')}
            </p>
          ) : (
            // CheckboxGroup 的值是字符串：用户 id 在这里进出时转换。
            <CheckboxGroup
              aria-label={t('开放名单', 'Access')}
              className="max-h-72 items-stretch gap-0 divide-y overflow-y-auto rounded-lg border"
              value={value.map(String)}
              onValueChange={(next) => setValue(next.map(Number))}
            >
              {users.map((u) => (
                <Label className="cursor-pointer gap-3 px-3 py-2.5 transition-colors hover:bg-accent/48" key={u.id}>
                  <Checkbox value={String(u.id)} />
                  <span className="min-w-0 flex-1 truncate">{u.username}</span>
                  <Badge size="xs" variant={u.role === 'agent' ? 'info' : 'secondary'}>
                    {u.role === 'agent' ? t('代理', 'Agent') : t('用户', 'User')}
                  </Badge>
                </Label>
              ))}
            </CheckboxGroup>
          )}
        </DialogPanel>
        <DialogFooter>
          <DialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</DialogClose>
          <Button loading={save.isPending} onClick={() => save.mutate(value)}>{t('保存', 'Save')}</Button>
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  )
}
