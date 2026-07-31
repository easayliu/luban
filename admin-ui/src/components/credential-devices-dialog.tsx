import { useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  CopyIcon,
  PencilIcon,
  RefreshCwIcon,
  SmartphoneIcon,
  UnlinkIcon,
} from 'lucide-react'
import {
  listCredentialDevices,
  unbindCredentialDevice,
  type Credential,
  type DeviceBinding,
} from '@/api/credentials'
import {
  copyText,
  extractError,
  formatFullTime,
  formatUsd,
  relativeTime,
} from '@/lib/utils'
import { type CredentialActions } from '@/components/credential-shared'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Avatar, AvatarFallback } from '@/components/ui/avatar'
import { Badge, type BadgeProps } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Card,
  CardAction,
  CardDescription,
  CardHeader,
  CardPanel,
  CardTitle,
} from '@/components/ui/card'
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
import {
  Empty,
  EmptyDescription,
  EmptyHeader,
  EmptyMedia,
  EmptyTitle,
} from '@/components/ui/empty'
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import { Form } from '@/components/ui/form'
import {
  NumberField,
  NumberFieldDecrement,
  NumberFieldGroup,
  NumberFieldIncrement,
  NumberFieldInput,
} from '@/components/ui/number-field'
import {
  Select,
  SelectItem,
  SelectPopup,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { Skeleton } from '@/components/ui/skeleton'
import { Spinner } from '@/components/ui/spinner'
import { toastManager } from '@/components/ui/toast'

type LimitPolicy = 'default' | 'unlimited' | 'custom'

const LIMIT_POLICY_ITEMS: { value: LimitPolicy; label: string }[] = [
  { value: 'default', label: '跟随全局默认' },
  { value: 'unlimited', label: '不限设备数' },
  { value: 'custom', label: '自定义上限' },
]

function policyFromLimit(limit: number): LimitPolicy {
  if (limit === 0) return 'default'
  if (limit < 0) return 'unlimited'
  return 'custom'
}

function policyMeta(deviceLimit: number): { label: string; variant: BadgeProps['variant'] } {
  if (deviceLimit === 0) return { label: '跟随默认', variant: 'secondary' }
  if (deviceLimit < 0) return { label: '不限', variant: 'outline' }
  return { label: '自定义', variant: 'info' }
}

export function CredentialDevicesDialog({
  cred,
  open,
  onOpenChange,
  limit,
}: {
  cred: Credential
  open: boolean
  onOpenChange: (open: boolean) => void
  limit: CredentialActions['limit']
}) {
  const [editingLimit, setEditingLimit] = useState(false)
  const [limitPolicy, setLimitPolicy] = useState<LimitPolicy>(() => policyFromLimit(cred.device_limit))
  const [customLimit, setCustomLimit] = useState(Math.max(1, cred.device_limit))
  const devices = useQuery({
    queryKey: ['credential-devices', cred.id],
    queryFn: () => listCredentialDevices(cred.id),
    enabled: open,
  })

  const currentDeviceCount = devices.data?.length ?? cred.device_count
  const effectiveLimit = cred.device_limit_effective > 0
    ? `${cred.device_limit_effective} 台`
    : '不限'
  const currentPolicy = policyMeta(cred.device_limit)
  const deviceStatus = devices.isPending
    ? { label: '正在读取', variant: 'secondary' as const }
    : devices.error
      ? { label: '读取失败', variant: 'error' as const }
      : devices.isFetching
        ? { label: `刷新中 · ${currentDeviceCount} 台`, variant: 'info' as const }
        : { label: `${currentDeviceCount} 台活跃设备`, variant: 'success' as const }

  const resetEditor = () => {
    setEditingLimit(false)
    setLimitPolicy(policyFromLimit(cred.device_limit))
    setCustomLimit(Math.max(1, cred.device_limit))
  }

  const handleOpenChange = (next: boolean) => {
    if (!next) resetEditor()
    onOpenChange(next)
  }

  const startEditingLimit = () => {
    setLimitPolicy(policyFromLimit(cred.device_limit))
    setCustomLimit(Math.max(1, cred.device_limit))
    setEditingLimit(true)
  }

  const saveLimit = () => {
    const normalizedCustomLimit = Number.isFinite(customLimit)
      ? Math.max(1, Math.floor(customLimit))
      : 1
    const nextLimit = limitPolicy === 'default'
      ? 0
      : limitPolicy === 'unlimited'
        ? -1
        : normalizedCustomLimit
    limit.mutate(nextLimit, { onSuccess: () => setEditingLimit(false) })
  }

  return (
    <Dialog open={open} onOpenChange={handleOpenChange}>
      <DialogPopup className="max-w-2xl">
        <DialogHeader>
          <div className="flex items-start gap-3 pr-8">
            <Avatar>
              <AvatarFallback><SmartphoneIcon /></AvatarFallback>
            </Avatar>
            <div className="min-w-0 flex-1">
              <DialogTitle>已绑定设备</DialogTitle>
              <DialogDescription className="mt-1 truncate" title={cred.label}>{cred.label}</DialogDescription>
              <div className="mt-2 flex flex-wrap items-center gap-2">
                <Badge variant="outline">#{cred.id}</Badge>
                <Badge variant={deviceStatus.variant} aria-live="polite">{deviceStatus.label}</Badge>
              </div>
            </div>
          </div>
        </DialogHeader>

        <Form
          className="contents"
          onSubmit={(event) => {
            event.preventDefault()
            if (editingLimit) saveLimit()
          }}
        >
          <DialogPanel className="space-y-5">
            {editingLimit ? (
              <Card>
                <CardHeader>
                  <CardTitle className="text-sm leading-snug">设备上限</CardTitle>
                  <CardDescription className="text-xs">明确选择账号是跟随全局设置、不限设备，还是使用独立上限。</CardDescription>
                </CardHeader>
                <CardPanel className="grid gap-4 sm:grid-cols-2">
                  <Field>
                    <FieldLabel>上限策略</FieldLabel>
                    <Select
                      items={LIMIT_POLICY_ITEMS}
                      value={limitPolicy}
                      onValueChange={(value) => {
                        if (value) setLimitPolicy(value as LimitPolicy)
                      }}
                    >
                      <SelectTrigger aria-label="上限策略">
                        <SelectValue />
                      </SelectTrigger>
                      <SelectPopup>
                        {LIMIT_POLICY_ITEMS.map((item) => (
                          <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>
                        ))}
                      </SelectPopup>
                    </Select>
                    <FieldDescription>
                      “默认”会自动应用全局设备上限，不等于不限。
                    </FieldDescription>
                  </Field>

                  {limitPolicy === 'custom' && (
                    <Field>
                      <FieldLabel>最多绑定设备</FieldLabel>
                      <NumberField
                        value={customLimit}
                        min={1}
                        step={1}
                        onValueChange={(value) => setCustomLimit(value ?? 1)}
                      >
                        <NumberFieldGroup>
                          <NumberFieldDecrement />
                          <NumberFieldInput aria-label="自定义设备上限" />
                          <NumberFieldIncrement />
                        </NumberFieldGroup>
                      </NumberField>
                      <FieldDescription>该设置只影响当前账号。</FieldDescription>
                    </Field>
                  )}
                </CardPanel>
              </Card>
            ) : (
              <Card>
                <CardHeader>
                  <CardTitle className="text-sm leading-snug">设备容量</CardTitle>
                  <CardDescription className="text-xs">控制此账号可同时保持活跃绑定的设备数量。</CardDescription>
                  <CardAction>
                    <Button type="button" size="sm" variant="outline" onClick={startEditingLimit}>
                      <PencilIcon />
                      调整上限
                    </Button>
                  </CardAction>
                </CardHeader>
                <CardPanel>
                  <dl className="grid grid-cols-3 gap-4">
                    <CapacityStat label="已绑定" value={`${currentDeviceCount} 台`} />
                    <CapacityStat label="生效上限" value={effectiveLimit} />
                    <div className="min-w-0">
                      <dt className="text-xs text-muted-foreground">上限策略</dt>
                      <dd className="mt-1"><Badge variant={currentPolicy.variant}>{currentPolicy.label}</Badge></dd>
                    </div>
                  </dl>
                </CardPanel>
              </Card>
            )}

            <DeviceList
              credId={cred.id}
              data={devices.data}
              isPending={devices.isPending}
              isFetching={devices.isFetching}
              error={devices.error}
              onRetry={() => { void devices.refetch() }}
            />
          </DialogPanel>

          <DialogFooter>
            {editingLimit ? (
              <>
                <Button type="button" variant="outline" disabled={limit.isPending} onClick={resetEditor}>
                  取消
                </Button>
                <Button type="submit" loading={limit.isPending}>保存</Button>
              </>
            ) : (
              <>
                <p className="mr-auto self-center text-xs text-muted-foreground">
                  解绑只会释放当前名额；设备下次请求时仍可能重新绑定。
                </p>
                <DialogClose render={<Button variant="outline" />}>关闭</DialogClose>
              </>
            )}
          </DialogFooter>
        </Form>
      </DialogPopup>
    </Dialog>
  )
}

function DeviceList({
  credId,
  data,
  isPending,
  isFetching,
  error,
  onRetry,
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
  const unbind = useMutation({
    mutationFn: (deviceId: string) => unbindCredentialDevice(credId, deviceId),
    onSuccess: (_, deviceId) => {
      toastManager.add({ title: '已解绑', type: 'success' })
      qc.setQueryData<DeviceBinding[]>(queryKey, (current) =>
        current?.filter((device) => device.device_id !== deviceId))
      qc.invalidateQueries({ queryKey })
      qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (error) => toastManager.add({
      title: '解绑失败',
      description: extractError(error),
      type: 'error',
    }),
  })

  return (
    <section className="space-y-3" aria-labelledby={`active-devices-${credId}`}>
      <div className="flex items-end justify-between gap-3">
        <div>
          <h3 id={`active-devices-${credId}`} className="font-semibold text-sm">活跃设备</h3>
          <p className="text-xs text-muted-foreground">按最近活跃时间排序</p>
        </div>
        {!isPending && !error && (
          <span className="inline-flex items-center gap-2 text-xs text-muted-foreground">
            {isFetching && <Spinner />}
            {data?.length ?? 0} 台
          </span>
        )}
      </div>

      {isPending ? (
        <div className="space-y-2" role="status" aria-label="正在读取设备列表">
          {Array.from({ length: 2 }, (_, index) => (
            <Card key={index}>
              <CardPanel className="flex items-center gap-3">
                <Skeleton className="size-8 rounded-full" />
                <div className="flex-1 space-y-2">
                  <Skeleton className="h-4 w-3/5" />
                  <Skeleton className="h-3 w-2/5" />
                </div>
              </CardPanel>
            </Card>
          ))}
        </div>
      ) : error ? (
        <Alert variant="error">
          <AlertTitle>设备列表读取失败</AlertTitle>
          <AlertDescription>
            <p className="break-words">{extractError(error)}</p>
            <Button type="button" size="sm" variant="destructive-outline" onClick={onRetry}>
              <RefreshCwIcon />
              重试
            </Button>
          </AlertDescription>
        </Alert>
      ) : !data || data.length === 0 ? (
        <Empty className="py-10">
          <EmptyHeader>
            <EmptyMedia variant="icon"><SmartphoneIcon /></EmptyMedia>
            <EmptyTitle className="text-base">暂无活跃设备</EmptyTitle>
            <EmptyDescription>设备完成一次请求后会出现在这里。</EmptyDescription>
          </EmptyHeader>
        </Empty>
      ) : (
        <ul className="space-y-2">
          {data.map((device) => (
            <li key={device.device_id}>
              <Card>
                <CardHeader>
                  <CardTitle className="flex min-w-0 items-center gap-2 text-sm leading-snug">
                    <SmartphoneIcon className="size-4 shrink-0" />
                    <span className="min-w-0 flex-1 truncate text-sm" title={device.device_id}>
                      {device.device_id}
                    </span>
                    <Button
                      type="button"
                      size="icon-sm"
                      variant="ghost"
                      title={`${device.device_id}（点击复制）`}
                      aria-label={`复制设备 ID ${device.device_id}`}
                      onClick={async () => {
                        const copied = await copyText(device.device_id)
                        toastManager.add(copied
                          ? { title: '已复制 device_id', type: 'success' }
                          : { title: '复制失败', description: device.device_id, type: 'error' })
                      }}
                    >
                      <CopyIcon />
                    </Button>
                  </CardTitle>
                  <CardDescription className="text-xs" title={`首次绑定 ${formatFullTime(device.created_at)} · 最近活跃 ${formatFullTime(device.last_seen_at)}`}>
                    首次绑定 {relativeTime(device.created_at)} · 最近活跃 {relativeTime(device.last_seen_at)}
                  </CardDescription>
                  <CardAction>
                    <Button
                      size="sm"
                      variant="destructive-outline"
                      loading={unbind.isPending && unbind.variables === device.device_id}
                      disabled={unbind.isPending && unbind.variables !== device.device_id}
                      onClick={() => unbind.mutate(device.device_id)}
                      aria-label={`解绑设备 ${device.device_id}`}
                    >
                      <UnlinkIcon />
                      解绑
                    </Button>
                  </CardAction>
                </CardHeader>
                <CardPanel>
                  <dl className="grid grid-cols-3 gap-4">
                    <DeviceStat label="请求" value={`${device.request_count} 次`} />
                    <DeviceStat label="本账号花费" value={formatUsd(device.cost_usd)} />
                    <DeviceStat label="全部账号花费" value={formatUsd(device.cost_usd_all)} />
                  </dl>
                </CardPanel>
              </Card>
            </li>
          ))}
        </ul>
      )}
    </section>
  )
}

function CapacityStat({ label, value }: { label: string; value: string }) {
  return (
    <div className="min-w-0">
      <dt className="text-xs text-muted-foreground">{label}</dt>
      <dd className="mt-1 truncate font-semibold text-sm tabular-nums" title={value}>{value}</dd>
    </div>
  )
}

function DeviceStat({ label, value }: { label: string; value: string }) {
  return (
    <div className="min-w-0">
      <dt className="text-xs text-muted-foreground">{label}</dt>
      <dd className="mt-1 truncate font-medium text-sm tabular-nums" title={value}>{value}</dd>
    </div>
  )
}
