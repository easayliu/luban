import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { toast } from 'sonner'
import {
  ArrowPathIcon, DevicePhoneMobileIcon, DocumentDuplicateIcon, PencilIcon, XMarkIcon,
} from '@heroicons/react/24/outline'
import {
  listCredentialDevices, unbindCredentialDevice, type Credential, type DeviceBinding,
} from '@/api/credentials'
import {
  copyText, extractError, formatFullTime, formatUsd, relativeTime,
} from '@/lib/utils'
import {
  inputToLimit, limitToInput, type CredentialActions,
} from '@/components/credential-shared'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import {
  Dialog, DialogBody, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle,
} from '@/components/ui/dialog'

/**
 * 某账号的设备容量与活跃绑定明细。
 *
 * 设备查询只在弹窗打开时启用，卡片与列表可复用同一个入口而不会增加常驻请求。
 */
export function CredentialDevicesDialog({
  cred, open, onOpenChange, limit,
}: {
  cred: Credential
  open: boolean
  onOpenChange: (open: boolean) => void
  limit: CredentialActions['limit']
}) {
  const [editingLimit, setEditingLimit] = useState(false)
  const [limitVal, setLimitVal] = useState(limitToInput(cred.device_limit))
  const devices = useQuery({
    queryKey: ['credential-devices', cred.id],
    queryFn: () => listCredentialDevices(cred.id),
    enabled: open,
  })
  const currentDeviceCount = devices.data?.length ?? cred.device_count
  const effectiveLimit = cred.device_limit_effective > 0
    ? `${cred.device_limit_effective} 台`
    : '不限'
  const limitMode = cred.device_limit === 0
    ? '跟随默认'
    : cred.device_limit < 0
      ? '明确不限'
      : '独立设置'

  const handleOpenChange = (next: boolean) => {
    if (!next) {
      setEditingLimit(false)
      setLimitVal(limitToInput(cred.device_limit))
    }
    onOpenChange(next)
  }

  const startEditingLimit = () => {
    setLimitVal(limitToInput(cred.device_limit))
    setEditingLimit(true)
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogContent className="max-w-2xl">
        <DialogHeader className="border-b-0 pb-5 pr-14 sm:px-6 sm:pr-16 sm:pt-5">
          <div className="flex items-start gap-3">
            <span className="grid size-10 shrink-0 place-items-center rounded-lg bg-muted text-muted-foreground ring-1 ring-inset ring-border/70">
              <DevicePhoneMobileIcon className="size-5" aria-hidden />
            </span>
            <div className="min-w-0 flex-1">
              <DialogTitle className="text-lg">已绑定设备</DialogTitle>
              <DialogDescription className="mt-1 line-clamp-2 break-all text-sm leading-5" title={cred.label}>
                {cred.label}
              </DialogDescription>
              <div className="mt-2 flex flex-wrap items-center gap-2 text-xs text-muted-foreground">
                <Badge variant="outline" className="h-5 px-1.5 py-0 font-mono text-2xs">#{cred.id}</Badge>
                <span className="inline-flex items-center gap-1.5">
                  <span className="size-1.5 rounded-full bg-ok" aria-hidden />
                  <span className="tnum">{currentDeviceCount} 台活跃设备</span>
                </span>
              </div>
            </div>
          </div>
        </DialogHeader>
        <DialogBody className="p-0 sm:p-0">
          {editingLimit ? (
            <form
              className="border-y border-border/70 bg-muted/25 px-4 py-4 sm:px-6 sm:py-5"
              onSubmit={(event) => {
                event.preventDefault()
                limit.mutate(inputToLimit(limitVal), {
                  onSuccess: () => setEditingLimit(false),
                })
              }}
            >
              <div className="grid gap-4 sm:grid-cols-[minmax(0,1fr)_7rem_auto] sm:items-end">
                <label htmlFor={`dialog-device-limit-${cred.id}`}>
                  <span className="block text-sm font-medium">设备上限</span>
                  <span className="mt-1 block text-xs leading-5 text-muted-foreground">
                    留空跟随全局默认，输入 0 表示不限设备数。
                  </span>
                </label>
                <Input
                  id={`dialog-device-limit-${cred.id}`}
                  type="number"
                  min={0}
                  value={limitVal}
                  onChange={(event) => setLimitVal(event.target.value)}
                  autoFocus
                  placeholder="默认"
                  className="h-10 w-full px-3 text-sm sm:h-9"
                />
                <div className="grid grid-cols-2 gap-2 sm:flex">
                  <Button
                    type="button"
                    size="sm"
                    variant="outline"
                    className="h-10 sm:h-9"
                    disabled={limit.isPending}
                    onClick={() => {
                      setEditingLimit(false)
                      setLimitVal(limitToInput(cred.device_limit))
                    }}
                  >
                    取消
                  </Button>
                  <Button type="submit" size="sm" className="h-10 sm:h-9" disabled={limit.isPending}>
                    {limit.isPending && <ArrowPathIcon className="animate-spin" />}
                    保存
                  </Button>
                </div>
              </div>
            </form>
          ) : (
            <section className="border-y border-border/70 bg-muted/25" aria-labelledby={`device-capacity-${cred.id}`}>
              <div className="flex items-center justify-between gap-3 px-4 py-3 sm:px-6">
                <div>
                  <h3 id={`device-capacity-${cred.id}`} className="text-sm font-semibold">设备容量</h3>
                  <p className="mt-0.5 text-xs text-muted-foreground">控制此账号可同时绑定的设备数量</p>
                </div>
                <Button type="button" size="sm" variant="outline" className="shrink-0" onClick={startEditingLimit}>
                  <PencilIcon className="size-3.5" />
                  调整上限
                </Button>
              </div>
              <dl className="grid grid-cols-3 divide-x divide-border/70 border-t border-border/70 bg-card/60">
                <CapacityStat label="已绑定" value={`${currentDeviceCount} 台`} />
                <CapacityStat label="生效上限" value={effectiveLimit} />
                <CapacityStat label="上限策略" value={limitMode} />
              </dl>
            </section>
          )}
          <DeviceList
            credId={cred.id}
            data={devices.data}
            isPending={devices.isPending}
            isFetching={devices.isFetching}
            error={devices.error}
            onRetry={() => { void devices.refetch() }}
          />
        </DialogBody>
        <DialogFooter className="flex-col items-stretch bg-muted/30 sm:flex-row sm:items-center sm:justify-between sm:px-6">
          <p className="text-xs leading-5 text-muted-foreground">
            解绑只会释放当前名额；设备下次请求时仍可能重新绑定。
          </p>
          <Button type="button" variant="outline" className="w-full shrink-0 sm:w-auto" onClick={() => handleOpenChange(false)}>
            关闭
          </Button>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  )
}

/**
 * 某账号当前绑定的设备明细。上层查询只在弹窗打开时启用，故列表仍按需拉取。
 *
 * 口径与账号「设备 x/y」的 x 完全一致（后端按同一个绑定 TTL 过滤）。
 */
function DeviceList({
  credId, data, isPending, isFetching, error, onRetry,
}: {
  credId: number
  data: DeviceBinding[] | undefined
  isPending: boolean
  isFetching: boolean
  error: Error | null
  onRetry: () => void
}) {
  const qc = useQueryClient()
  const queryKey = ['credential-devices', credId] as const

  // 手动解绑：连带刷新账号列表，所有视图上的「设备 x/y」要立刻跟着掉一台。
  const unbind = useMutation({
    mutationFn: (deviceId: string) => unbindCredentialDevice(credId, deviceId),
    onSuccess: (_, deviceId) => {
      toast.success('已解绑')
      qc.setQueryData<DeviceBinding[]>(queryKey, (current) =>
        current?.filter((device) => device.device_id !== deviceId))
      qc.invalidateQueries({ queryKey })
      qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (e) => toast.error('解绑失败', { description: extractError(e) }),
  })

  return (
    <section className="px-4 py-5 sm:px-6" aria-labelledby={`active-devices-${credId}`}>
      <div className="mb-3 flex items-end justify-between gap-3">
        <div>
          <h3 id={`active-devices-${credId}`} className="text-sm font-semibold">活跃设备</h3>
          <p className="mt-0.5 text-xs text-muted-foreground">按最近活跃时间排序</p>
        </div>
        {!isPending && !error && (
          <span className="inline-flex items-center gap-1.5 text-xs text-muted-foreground">
            {isFetching && <ArrowPathIcon className="size-3.5 animate-spin" aria-hidden />}
            <span className="tnum">{data?.length ?? 0} 台</span>
          </span>
        )}
      </div>
      {isPending ? (
        <div className="space-y-2" role="status" aria-label="正在读取设备列表">
          {Array.from({ length: 2 }, (_, index) => (
            <div key={index} className="animate-pulse rounded-lg border border-border/70 p-4">
              <div className="flex items-center gap-3">
                <span className="size-10 rounded-lg bg-muted" />
                <span className="min-w-0 flex-1 space-y-2">
                  <span className="block h-3 w-3/5 rounded bg-muted" />
                  <span className="block h-2.5 w-2/5 rounded bg-muted" />
                </span>
              </div>
            </div>
          ))}
        </div>
      ) : error ? (
        <div className="rounded-lg bg-bad-soft p-4 text-bad ring-1 ring-inset ring-bad/10" role="alert">
          <p className="text-sm font-medium">设备列表读取失败</p>
          <p className="mt-1 break-words text-xs leading-5">{extractError(error)}</p>
          <Button type="button" size="sm" variant="outline" className="mt-3 border-bad/20 text-bad hover:bg-bad-soft hover:text-bad" onClick={onRetry}>
            <ArrowPathIcon className="size-3.5" />重试
          </Button>
        </div>
      ) : !data || data.length === 0 ? (
        <div className="rounded-lg border border-dashed border-border px-4 py-8 text-center">
          <span className="mx-auto grid size-10 place-items-center rounded-lg bg-muted text-muted-foreground">
            <DevicePhoneMobileIcon className="size-5" aria-hidden />
          </span>
          <p className="mt-3 text-sm font-medium">暂无活跃设备</p>
          <p className="mt-1 text-xs text-muted-foreground">设备完成一次请求后会出现在这里。</p>
        </div>
      ) : (
        <ul className="overflow-hidden rounded-lg border border-border/80 bg-card divide-y divide-border/80">
          {data.map((d) => (
            <li key={d.device_id} className="px-4 py-4">
              <div className="flex items-start gap-3">
                <span className="grid size-10 shrink-0 place-items-center rounded-lg bg-muted text-muted-foreground ring-1 ring-inset ring-border/70">
                  <DevicePhoneMobileIcon className="size-4" />
                </span>
                <div className="min-w-0 flex-1">
                  <Button
                    type="button"
                    size="sm"
                    variant="link"
                    className="group h-auto max-w-full justify-start gap-1.5 p-0 font-mono text-xs font-medium text-foreground hover:no-underline"
                    title={`${d.device_id}（点击复制）`}
                    aria-label={`复制设备 ID ${d.device_id}`}
                    onClick={async () => {
                      const ok = await copyText(d.device_id)
                      if (ok) toast.success('已复制 device_id')
                      else toast.error('复制失败', { description: d.device_id })
                    }}
                  >
                    <span className="truncate">{d.device_id}</span>
                    <DocumentDuplicateIcon className="size-3.5 shrink-0 text-muted-foreground group-hover:text-foreground" aria-hidden />
                  </Button>
                  <span
                    className="mt-1 block tnum text-2xs leading-4 text-muted-foreground"
                    title={`首次绑定 ${formatFullTime(d.created_at)} · 最近活跃 ${formatFullTime(d.last_seen_at)}`}
                  >
                    首次绑定 {relativeTime(d.created_at)} · 最近活跃 {relativeTime(d.last_seen_at)}
                  </span>
                </div>
                <Button
                  size="sm"
                  variant="ghost"
                  className="h-8 shrink-0 px-2 text-xs text-bad hover:bg-bad-soft hover:text-bad"
                  onClick={() => unbind.mutate(d.device_id)}
                  disabled={unbind.isPending}
                  title="解绑设备"
                  aria-label={`解绑设备 ${d.device_id}`}
                >
                  {unbind.isPending && unbind.variables === d.device_id
                    ? <ArrowPathIcon className="size-3.5 animate-spin" />
                    : <XMarkIcon className="size-3.5" />}
                  解绑
                </Button>
              </div>
              <dl className="mt-4 grid grid-cols-3 gap-3 border-t border-border/60 pt-3">
                <DeviceStat label="请求" value={`${d.request_count} 次`} />
                <DeviceStat label="本账号花费" value={formatUsd(d.cost_usd)} />
                <DeviceStat label="全部账号花费" value={formatUsd(d.cost_usd_all)} />
              </dl>
            </li>
          ))}
        </ul>
      )}
    </section>
  )
}

function CapacityStat({ label, value }: { label: string; value: string }) {
  return (
    <div className="min-w-0 px-3 py-3 sm:px-4">
      <dt className="text-2xs text-muted-foreground">{label}</dt>
      <dd className="mt-1 truncate text-sm font-semibold tnum text-foreground" title={value}>{value}</dd>
    </div>
  )
}

function DeviceStat({ label, value }: { label: string; value: string }) {
  return (
    <div className="min-w-0">
      <dt className="text-2xs leading-4 text-muted-foreground">{label}</dt>
      <dd className="mt-0.5 truncate text-xs font-medium tnum text-foreground" title={value}>{value}</dd>
    </div>
  )
}
