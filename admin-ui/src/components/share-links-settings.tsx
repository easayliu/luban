import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { Link2Icon, PlusIcon, Trash2Icon } from 'lucide-react'
import {
  createShareLink,
  deleteShareLink,
  listShareLinks,
  shareUrl,
  type ShareLink,
} from '@/api/auth'
import { useI18n } from '@/lib/i18n'
import { cn, extractError, formatFullTime } from '@/lib/utils'
import { CopyButton } from '@/components/copy-button'
import { ErrorState, LoadingState } from '@/components/state-placeholders'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import { Form } from '@/components/ui/form'
import { Input } from '@/components/ui/input'
import { Select, SelectItem, SelectPopup, SelectTrigger, SelectValue } from '@/components/ui/select'
import { toastManager } from '@/components/ui/toast'

const HOUR = 3600
const DAY = 86_400

/**
 * 分享链接：给多人只读查看用。每人（或每个场合）一条，各带有效期，到期自动失效，删除即撤销；
 * 持链接者的权限与访客相同。明文只在新建时显示一次。
 */
export function ShareLinksSettings() {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  const queryKey = ['share-links']
  const links = useQuery({ queryKey, queryFn: listShareLinks })
  const [label, setLabel] = useState('')
  const [ttl, setTtl] = useState(String(7 * DAY))
  const [created, setCreated] = useState<string | null>(null)
  const [deleting, setDeleting] = useState<number | null>(null)

  const ttlItems = [
    { value: String(HOUR), label: t('1 小时', '1 hour') },
    { value: String(DAY), label: t('1 天', '1 day') },
    { value: String(7 * DAY), label: t('7 天', '7 days') },
    { value: String(30 * DAY), label: t('30 天', '30 days') },
    { value: String(90 * DAY), label: t('90 天', '90 days') },
  ]

  const failed = (error: unknown) => {
    toastManager.add({ title: t('操作失败', 'Operation failed'), description: extractError(error, language), type: 'error' })
  }
  const refresh = () => void qc.invalidateQueries({ queryKey })
  const create = useMutation({
    mutationFn: () => createShareLink(label.trim(), Number(ttl)),
    onSuccess: (res) => {
      refresh()
      setLabel('')
      setCreated(shareUrl(res.key))
    },
    onError: failed,
  })
  const remove = useMutation({
    mutationFn: (link: ShareLink) => deleteShareLink(link.id),
    onSuccess: () => {
      refresh()
      setDeleting(null)
      toastManager.add({
        title: t('分享链接已删除', 'Share link deleted'),
        description: t('持该链接的访客将在下次请求时退出。', 'Anyone using it is signed out on their next request.'),
        type: 'success',
      })
    },
    onError: failed,
  })

  const now = Date.now() / 1000
  const list = links.data ?? []

  return (
    <Field className="p-4 sm:p-5">
      <FieldLabel>{t('分享链接（限时只读）', 'Share links (time-limited, read-only)')}</FieldLabel>
      {created ? (
        <div className="w-full space-y-2">
          <div className="flex items-center gap-2 rounded-lg border px-3 py-2">
            <code className="min-w-0 flex-1 truncate font-mono text-xs">{created}</code>
            <CopyButton label={t('复制链接', 'Copy link')} text={created} />
          </div>
          <FieldDescription className="text-warning-foreground">
            {t('链接仅显示这一次，请立即复制并发给对方。', 'The link is shown only once. Copy it now and send it.')}
          </FieldDescription>
          <Button size="sm" variant="outline" onClick={() => setCreated(null)}>
            {t('完成', 'Done')}
          </Button>
        </div>
      ) : (
        <Form
          className="grid w-full gap-2 sm:grid-cols-[minmax(0,1fr)_9rem_auto]"
          onSubmit={(event) => {
            event.preventDefault()
            if (!create.isPending) create.mutate()
          }}
        >
          <Input
            aria-label={t('名称', 'Name')}
            maxLength={64}
            onChange={(event) => setLabel(event.target.value)}
            placeholder={t('名称，例如：运营组', 'Name, e.g. ops team')}
            value={label}
          />
          <Select items={ttlItems} value={ttl} onValueChange={(value) => value && setTtl(value)}>
            <SelectTrigger aria-label={t('有效期', 'Valid for')}><SelectValue /></SelectTrigger>
            <SelectPopup alignItemWithTrigger={false}>
              {ttlItems.map((item) => (
                <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>
              ))}
            </SelectPopup>
          </Select>
          <Button loading={create.isPending} type="submit">
            <PlusIcon />{t('生成链接', 'Create link')}
          </Button>
        </Form>
      )}
      <FieldDescription>
        {t(
          '持链接者以访客身份只读查看，到期自动失效，删除即刻撤销。每人单独发一条，便于分别收回。',
          'Anyone with the link gets viewer access. It expires automatically and can be revoked at any time. Create one per person so each can be revoked separately.',
        )}
      </FieldDescription>

      <div className="mt-1 w-full divide-y rounded-lg border">
        {links.isPending ? (
          <LoadingState className="min-h-20" label={t('正在加载分享链接', 'Loading share links')} />
        ) : links.isError ? (
          <ErrorState
            error={links.error}
            title={t('无法读取分享链接', 'Unable to load share links')}
            onRetry={() => links.refetch()}
            retrying={links.isFetching}
          />
        ) : list.length === 0 ? (
          <div className="px-3 py-3 text-xs text-muted-foreground">{t('尚未生成分享链接。', 'No share links yet.')}</div>
        ) : list.map((link) => {
          const expired = link.expires_at <= now
          return (
            <div className={cn('flex items-center gap-3 px-3 py-2.5 text-sm', expired && 'opacity-64')} key={link.id}>
              <Link2Icon aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
              <div className="min-w-0 flex-1">
                <div className="flex flex-wrap items-center gap-1.5">
                  <span className="truncate font-medium">{link.label || t('未命名', 'Untitled')}</span>
                  <code className="font-mono text-xs text-muted-foreground">{link.prefix}…</code>
                  {expired && <Badge size="xs" variant="secondary">{t('已过期', 'Expired')}</Badge>}
                </div>
                <div className="mt-0.5 flex flex-wrap items-center gap-1 text-xs text-muted-foreground">
                  <span>
                    {expired
                      ? t(`过期于 ${formatFullTime(link.expires_at, language)}`, `Expired ${formatFullTime(link.expires_at, language)}`)
                      : t(`有效期至 ${formatFullTime(link.expires_at, language)}`, `Valid until ${formatFullTime(link.expires_at, language)}`)}
                  </span>
                  <span>·</span>
                  <span>
                    {link.last_used_at
                      ? t(`最近使用 ${formatFullTime(link.last_used_at, language)}`, `Last used ${formatFullTime(link.last_used_at, language)}`)
                      : t('从未使用', 'Never used')}
                  </span>
                </div>
              </div>
              {deleting === link.id ? (
                <div className="flex shrink-0 items-center gap-1">
                  <Button disabled={remove.isPending} size="sm" variant="ghost" onClick={() => setDeleting(null)}>
                    {t('取消', 'Cancel')}
                  </Button>
                  <Button loading={remove.isPending} size="sm" variant="destructive" onClick={() => remove.mutate(link)}>
                    {t('确认删除', 'Delete')}
                  </Button>
                </div>
              ) : (
                <Button
                  aria-label={t('删除分享链接', 'Delete share link')}
                  size="icon-sm"
                  variant="ghost"
                  onClick={() => setDeleting(link.id)}
                >
                  <Trash2Icon />
                </Button>
              )}
            </div>
          )
        })}
      </div>
    </Field>
  )
}
