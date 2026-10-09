import { useEffect, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { ArrowDownIcon, ArrowUpIcon, FolderIcon, PlusIcon, XIcon } from 'lucide-react'
import { setCredentialGroups, setCredentialsGroups, type Credential } from '@/api/credentials'
import { listGroups, type PoolGroup } from '@/api/groups'
import { useI18n } from '@/lib/i18n'
import { cn, extractError } from '@/lib/utils'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'
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
import { toastManager } from '@/components/ui/toast'
import { Hint } from '@/components/ui/tooltip'

/** 当前身份能看到的分组（管理员与访客是全部）。各处共用一份缓存。 */
export function useGroups(enabled = true) {
  return useQuery({ queryKey: ['groups'], queryFn: listGroups, staleTime: 30_000, enabled })
}

/** 默认分组的 id；分组还没拉回来时为 undefined。 */
export function defaultGroupId(groups: PoolGroup[] | undefined): number | undefined {
  return groups?.find((g) => g.is_default)?.id
}

/**
 * 分组复选列表：上号、改号的分组时用（不分先后）。至少选一个由调用方判。
 */
export function GroupPicker({
  groups,
  value,
  onChange,
  disabled,
}: {
  groups: PoolGroup[]
  value: number[]
  onChange: (value: number[]) => void
  disabled?: boolean
}) {
  const { t } = useI18n()
  const toggle = (id: number, on: boolean) =>
    onChange(on ? [...value, id] : value.filter((v) => v !== id))
  return (
    <div className="max-h-64 divide-y overflow-y-auto rounded-lg border">
      {groups.map((g) => {
        const checked = value.includes(g.id)
        return (
          <label
            className={cn(
              'flex cursor-pointer items-start gap-3 px-3 py-2.5 text-sm transition-colors hover:bg-accent/48',
              disabled && 'pointer-events-none opacity-64',
            )}
            key={g.id}
          >
            <Checkbox
              checked={checked}
              className="mt-0.5"
              disabled={disabled}
              onCheckedChange={(next) => toggle(g.id, next === true)}
            />
            <span className="min-w-0 flex-1">
              <span className="flex items-center gap-1.5 font-medium">
                <span className="truncate">{g.name}</span>
                {g.is_default && <Badge size="xs" variant="secondary">{t('默认', 'Default')}</Badge>}
              </span>
              {g.note && <span className="mt-0.5 block text-xs text-muted-foreground">{g.note}</span>}
            </span>
          </label>
        )
      })}
    </div>
  )
}

/**
 * 有先后的分组选择：接入 Key 绑定分组用。上面是已选的（顺序即优先级，可上下调、可移除），
 * 下面是还没选的（点一下加到末尾）。一个都不选 = 用全部号。
 */
export function OrderedGroupPicker({
  groups,
  value,
  onChange,
}: {
  groups: PoolGroup[]
  value: number[]
  onChange: (value: number[]) => void
}) {
  const { t } = useI18n()
  const byId = new Map(groups.map((g) => [g.id, g]))
  const rest = groups.filter((g) => !value.includes(g.id))
  const move = (index: number, delta: number) => {
    const next = [...value]
    const target = index + delta
    if (target < 0 || target >= next.length) return
    ;[next[index], next[target]] = [next[target], next[index]]
    onChange(next)
  }
  return (
    <div className="space-y-2">
      {value.length === 0 ? (
        <p className="rounded-lg border border-dashed px-3 py-2.5 text-xs text-muted-foreground">
          {t('未绑定分组：这把 Key 可用全部号。', 'No groups bound: this key can use every account.')}
        </p>
      ) : (
        <ol className="divide-y rounded-lg border">
          {value.map((id, index) => (
            <li className="flex items-center gap-2 px-3 py-2 text-sm" key={id}>
              <span className="w-5 shrink-0 text-xs text-muted-foreground tabular-nums">{index + 1}</span>
              <span className="min-w-0 flex-1 truncate font-medium">
                {byId.get(id)?.name ?? t(`分组 #${id}`, `Group #${id}`)}
              </span>
              <Hint label={t('上移', 'Move up')}>
                <Button aria-label={t('上移', 'Move up')} disabled={index === 0} size="icon-xs" type="button" variant="ghost" onClick={() => move(index, -1)}>
                  <ArrowUpIcon />
                </Button>
              </Hint>
              <Hint label={t('下移', 'Move down')}>
                <Button aria-label={t('下移', 'Move down')} disabled={index === value.length - 1} size="icon-xs" type="button" variant="ghost" onClick={() => move(index, 1)}>
                  <ArrowDownIcon />
                </Button>
              </Hint>
              <Hint label={t('移除', 'Remove')}>
                <Button aria-label={t('移除', 'Remove')} size="icon-xs" type="button" variant="ghost" onClick={() => onChange(value.filter((v) => v !== id))}>
                  <XIcon />
                </Button>
              </Hint>
            </li>
          ))}
        </ol>
      )}
      {rest.length > 0 && (
        <div className="flex flex-wrap gap-1.5">
          {rest.map((g) => (
            <Button key={g.id} size="xs" type="button" variant="outline" onClick={() => onChange([...value, g.id])}>
              <PlusIcon />
              {g.name}
            </Button>
          ))}
        </div>
      )}
    </div>
  )
}

/** 号所在的分组，一排小徽章。看不到名称的分组（管理员放进去的、没开放给自己的）合成一枚「其他」。 */
export function GroupBadges({ ids, className }: { ids: number[]; className?: string }) {
  const { t } = useI18n()
  const { data: groups } = useGroups()
  if (!groups || ids.length === 0) return null
  const byId = new Map(groups.map((g) => [g.id, g]))
  const known = ids.map((id) => byId.get(id)).filter((g): g is PoolGroup => !!g)
  const hidden = ids.length - known.length
  return (
    <span className={cn('inline-flex min-w-0 flex-wrap items-center gap-1', className)}>
      {known.map((g) => (
        <Badge className="max-w-32 truncate" key={g.id} size="xs" variant="outline">
          <FolderIcon aria-hidden="true" />
          {g.name}
        </Badge>
      ))}
      {hidden > 0 && (
        <Badge size="xs" variant="outline">{t(`其他 ${hidden} 个`, `${hidden} other`)}</Badge>
      )}
    </span>
  )
}

/**
 * 设置号所在的分组：单个号（`cred`）或一批号（`ids`）。整体替换，至少选一个；代理和用户只列
 * 开放给自己的分组。
 */
export function SetGroupsDialog({
  open,
  onOpenChange,
  cred,
  ids,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  cred?: Credential
  ids?: number[]
}) {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  const { data: groups, isPending } = useGroups(open)
  const [value, setValue] = useState<number[]>([])
  useEffect(() => {
    if (!open) return
    setValue(cred ? cred.groups.filter((id) => groups?.some((g) => g.id === id)) : [])
  }, [open, cred, groups])

  const save = useMutation({
    mutationFn: async (groupIds: number[]): Promise<void> => {
      if (cred) await setCredentialGroups(cred.id, groupIds)
      else await setCredentialsGroups(ids ?? [], groupIds)
    },
    onSuccess: () => {
      void qc.invalidateQueries({ queryKey: ['credentials'] })
      void qc.invalidateQueries({ queryKey: ['groups'] })
      onOpenChange(false)
      toastManager.add({ title: t('分组已更新', 'Groups updated'), type: 'success' })
    },
    onError: (error) => {
      toastManager.add({ title: t('操作失败', 'Operation failed'), description: extractError(error, language), type: 'error' })
    },
  })
  const count = cred ? 1 : (ids?.length ?? 0)

  return (
    <Dialog open={open} onOpenChange={(next) => { if (!save.isPending) onOpenChange(next) }}>
      <DialogPopup>
        <DialogHeader>
          <DialogTitle>{t('设置号池分组', 'Set pool groups')}</DialogTitle>
          <DialogDescription>
            {cred
              ? t('选中的分组会整体替换这个号现在所在的分组。', 'The selected groups replace the groups this account is in.')
              : t(`选中的分组会整体替换这 ${count} 个号现在所在的分组。`, `The selected groups replace the groups of these ${count} accounts.`)}
          </DialogDescription>
        </DialogHeader>
        <DialogPanel>
          {isPending || !groups ? (
            <p className="text-sm text-muted-foreground">{t('正在加载分组', 'Loading groups')}</p>
          ) : (
            <GroupPicker groups={groups} value={value} onChange={setValue} />
          )}
        </DialogPanel>
        <DialogFooter>
          <DialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</DialogClose>
          <Button disabled={value.length === 0} loading={save.isPending} onClick={() => save.mutate(value)}>
            {t('保存', 'Save')}
          </Button>
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  )
}
