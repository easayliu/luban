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
import { useI18n } from '@/lib/i18n'
import {
  cn,
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

const LIMIT_POLICY_ITEMS: {
  value: LimitPolicy
  chinese: string
  english: string
}[] = [
  { value: 'default', chinese: '跟随全局默认', english: 'Use global default' },
  { value: 'unlimited', chinese: '不限设备数', english: 'Unlimited devices' },
  { value: 'custom', chinese: '自定义上限', english: 'Custom limit' },
]

function policyFromLimit(limit: number): LimitPolicy {
  if (limit === 0) return 'default'
  if (limit < 0) return 'unlimited'
  return 'custom'
}

function policyVariant(deviceLimit: number): BadgeProps['variant'] {
  if (deviceLimit === 0) return 'secondary'
  if (deviceLimit < 0) return 'outline'
  return 'info'
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
  const { t, locale } = useI18n()
  const [editingLimit, setEditingLimit] = useState(false)
  const [limitPolicy, setLimitPolicy] = useState<LimitPolicy>(() => policyFromLimit(cred.device_limit))
  const [customLimit, setCustomLimit] = useState(Math.max(1, cred.device_limit))
  const devices = useQuery({
    queryKey: ['credential-devices', cred.id],
    queryFn: () => listCredentialDevices(cred.id),
    enabled: open,
  })

  // 只数真实绑定：模拟客户端的伪设备也在这个列表里，但它们不写绑定、不占设备名额，
  // 后端的 device_count（卡片上那个数）同样数不到它们。把它们算进来，就会得到
  // 「卡片显示 11 台、展开却是 12 台」这种对不上的展示。
  const currentDeviceCount =
    devices.data?.filter((device) => !device.simulated).length ?? cred.device_count
  const formattedCurrentDeviceCount = currentDeviceCount.toLocaleString(locale)
  const currentDeviceNoun = currentDeviceCount === 1 ? 'device' : 'devices'
  const limitPolicyItems = LIMIT_POLICY_ITEMS.map((item) => ({
    value: item.value,
    label: t(item.chinese, item.english),
  }))
  const effectiveLimit = cred.device_limit_effective > 0
    ? t(
      `${cred.device_limit_effective.toLocaleString(locale)} 台`,
      `${cred.device_limit_effective.toLocaleString(locale)} ${cred.device_limit_effective === 1 ? 'device' : 'devices'}`,
    )
    : t('不限', 'Unlimited')
  const currentPolicy = {
    label: cred.device_limit === 0
      ? t('跟随默认', 'Use default')
      : cred.device_limit < 0
        ? t('不限', 'Unlimited')
        : t('自定义', 'Custom'),
    variant: policyVariant(cred.device_limit),
  }
  const deviceStatus = devices.isPending
    ? { label: t('正在读取', 'Loading'), variant: 'secondary' as const }
    : devices.error
      ? { label: t('读取失败', 'Failed to load'), variant: 'error' as const }
      : devices.isFetching
        ? {
            label: t(
              `刷新中 · ${formattedCurrentDeviceCount} 台`,
              `Refreshing · ${formattedCurrentDeviceCount} ${currentDeviceNoun}`,
            ),
            variant: 'info' as const,
          }
        : {
            label: t(
              `${formattedCurrentDeviceCount} 台活跃设备`,
              `${formattedCurrentDeviceCount} active ${currentDeviceNoun}`,
            ),
            variant: 'success' as const,
          }

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
              <DialogTitle>{t('已绑定设备', 'Bound devices')}</DialogTitle>
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
                  <CardTitle className="text-sm leading-snug">
                    {t('设备上限', 'Device limit')}
                  </CardTitle>
                  <CardDescription className="text-xs">
                    {t(
                      '明确选择账号是跟随全局设置、不限设备，还是使用独立上限。',
                      'Choose whether this account uses the global default, allows unlimited devices, or has a custom limit.',
                    )}
                  </CardDescription>
                </CardHeader>
                <CardPanel className="grid gap-4 sm:grid-cols-2">
                  <Field>
                    <FieldLabel>{t('上限策略', 'Limit policy')}</FieldLabel>
                    <Select
                      items={limitPolicyItems}
                      value={limitPolicy}
                      onValueChange={(value) => {
                        if (value) setLimitPolicy(value as LimitPolicy)
                      }}
                    >
                      <SelectTrigger aria-label={t('上限策略', 'Limit policy')}>
                        <SelectValue />
                      </SelectTrigger>
                      <SelectPopup>
                        {limitPolicyItems.map((item) => (
                          <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>
                        ))}
                      </SelectPopup>
                    </Select>
                    <FieldDescription>
                      {t(
                        '“默认”会自动应用全局设备上限，不等于不限。',
                        '“Default” applies the global device limit; it does not mean unlimited.',
                      )}
                    </FieldDescription>
                  </Field>

                  {limitPolicy === 'custom' && (
                    <Field>
                      <FieldLabel>{t('最多绑定设备', 'Maximum bound devices')}</FieldLabel>
                      <NumberField
                        value={customLimit}
                        min={1}
                        step={1}
                        onValueChange={(value) => setCustomLimit(value ?? 1)}
                      >
                        <NumberFieldGroup>
                          <NumberFieldDecrement />
                          <NumberFieldInput aria-label={t('自定义设备上限', 'Custom device limit')} />
                          <NumberFieldIncrement />
                        </NumberFieldGroup>
                      </NumberField>
                      <FieldDescription>
                        {t('该设置只影响当前账号。', 'This setting only affects the current account.')}
                      </FieldDescription>
                    </Field>
                  )}
                </CardPanel>
              </Card>
            ) : (
              <Card>
                <CardHeader>
                  <CardTitle className="text-sm leading-snug">
                    {t('设备容量', 'Device capacity')}
                  </CardTitle>
                  <CardDescription className="text-xs">
                    {t(
                      '控制此账号可同时保持活跃绑定的设备数量。',
                      'Controls how many active device bindings this account can keep at once.',
                    )}
                  </CardDescription>
                  <CardAction>
                    <Button type="button" size="sm" variant="outline" onClick={startEditingLimit}>
                      <PencilIcon />
                      {t('调整上限', 'Adjust limit')}
                    </Button>
                  </CardAction>
                </CardHeader>
                <CardPanel>
                  <dl className="grid grid-cols-2 gap-x-4 gap-y-3 sm:grid-cols-3">
                    <CapacityStat
                      label={t('已绑定', 'Bound')}
                      value={t(
                        `${formattedCurrentDeviceCount} 台`,
                        `${formattedCurrentDeviceCount} ${currentDeviceNoun}`,
                      )}
                    />
                    <CapacityStat label={t('生效上限', 'Effective limit')} value={effectiveLimit} />
                    <div className="col-span-2 min-w-0 sm:col-span-1">
                      <dt className="text-xs text-muted-foreground">
                        {t('上限策略', 'Limit policy')}
                      </dt>
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
                  {t('取消', 'Cancel')}
                </Button>
                <Button type="submit" loading={limit.isPending}>{t('保存', 'Save')}</Button>
              </>
            ) : (
              <>
                <p className="mr-auto self-center text-xs text-muted-foreground">
                  {t(
                    '解绑只会释放当前名额；设备下次请求时仍可能重新绑定。',
                    'Unbinding only frees the current slot; the device may bind again on its next request.',
                  )}
                </p>
                <DialogClose render={<Button variant="outline" />}>{t('关闭', 'Close')}</DialogClose>
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
  const { t, language, locale } = useI18n()
  const qc = useQueryClient()
  const queryKey = ['credential-devices', credId] as const
  const deviceCountText = (count: number) => {
    const formatted = count.toLocaleString(locale)
    return t(`${formatted} 台`, `${formatted} ${count === 1 ? 'device' : 'devices'}`)
  }
  const unbind = useMutation({
    mutationFn: (deviceId: string) => unbindCredentialDevice(credId, deviceId),
    onSuccess: (_, deviceId) => {
      toastManager.add({ title: t('已解绑', 'Device unbound'), type: 'success' })
      qc.setQueryData<DeviceBinding[]>(queryKey, (current) =>
        current?.filter((device) => device.device_id !== deviceId))
      qc.invalidateQueries({ queryKey })
      qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (error) => toastManager.add({
      title: t('解绑失败', 'Failed to unbind device'),
      description: extractError(error, language),
      type: 'error',
    }),
  })

  return (
    <section className="space-y-3" aria-labelledby={`active-devices-${credId}`}>
      <div className="flex items-end justify-between gap-3">
        <div>
          <h3 id={`active-devices-${credId}`} className="font-semibold text-sm">
            {t('活跃设备', 'Active devices')}
          </h3>
          <p className="text-xs text-muted-foreground">
            {t('按最近活跃时间排序', 'Sorted by most recent activity')}
          </p>
        </div>
        {!isPending && !error && (
          <span className="inline-flex items-center gap-2 text-xs text-muted-foreground">
            {isFetching && <Spinner />}
            {deviceCountText(data?.length ?? 0)}
          </span>
        )}
      </div>

      {isPending ? (
        <div
          className="space-y-2"
          role="status"
          aria-label={t('正在读取设备列表', 'Loading device list')}
        >
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
          <AlertTitle>{t('设备列表读取失败', 'Failed to load device list')}</AlertTitle>
          <AlertDescription>
            <p className="break-words">{extractError(error, language)}</p>
            <Button type="button" size="sm" variant="destructive-outline" onClick={onRetry}>
              <RefreshCwIcon />
              {t('重试', 'Retry')}
            </Button>
          </AlertDescription>
        </Alert>
      ) : !data || data.length === 0 ? (
        <Empty className="py-10">
          <EmptyHeader>
            <EmptyMedia variant="icon"><SmartphoneIcon /></EmptyMedia>
            <EmptyTitle className="text-base">{t('暂无活跃设备', 'No active devices')}</EmptyTitle>
            <EmptyDescription>
              {t(
                '设备完成一次请求后会出现在这里。',
                'A device will appear here after it completes a request.',
              )}
            </EmptyDescription>
          </EmptyHeader>
        </Empty>
      ) : (
        <ul className="space-y-2">
          {data.map((device) => {
            const formattedRequestCount = device.request_count.toLocaleString(locale)
            // 模拟客户端没有绑定行，也就没有这两个时刻（见 DeviceBinding.simulated）。
            const firstBoundFull = formatFullTime(device.created_at ?? 0, language)
            const lastSeenFull = formatFullTime(device.last_seen_at ?? 0, language)
            const firstBoundRelative = relativeTime(device.created_at ?? 0, undefined, language)
            const lastSeenRelative = relativeTime(device.last_seen_at ?? 0, undefined, language)
            const meta = device.simulated
              ? t(
                  '非 Claude Code 客户端，按账号派生的身份；不占设备名额',
                  'Third-party client using an account-derived identity; does not use a device slot',
                )
              : t(
                  `首次绑定 ${firstBoundRelative} · 最近活跃 ${lastSeenRelative}`,
                  `First bound ${firstBoundRelative} · Last active ${lastSeenRelative}`,
                )
            const metaTitle = device.simulated
              ? undefined
              : t(
                  `首次绑定 ${firstBoundFull} · 最近活跃 ${lastSeenFull}`,
                  `First bound ${firstBoundFull} · Last active ${lastSeenFull}`,
                )
            return (
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
                        title={t(
                          `${device.device_id}（点击复制）`,
                          `${device.device_id} (click to copy)`,
                        )}
                        aria-label={t(
                          `复制设备 ID ${device.device_id}`,
                          `Copy device ID ${device.device_id}`,
                        )}
                        onClick={async () => {
                          const copied = await copyText(device.device_id)
                          toastManager.add(copied
                            ? {
                                title: t('已复制 device_id', 'Copied device_id'),
                                type: 'success',
                              }
                            : {
                                title: t('复制失败', 'Copy failed'),
                                description: device.device_id,
                                type: 'error',
                              })
                        }}
                      >
                        <CopyIcon />
                      </Button>
                    </CardTitle>
                    <CardDescription className="text-xs" title={metaTitle}>
                      {meta}
                    </CardDescription>
                    {/* 模拟伪设备没有绑定行可删，故不给解绑按钮——点了也只会是一次空操作。 */}
                    {!device.simulated && (
                      <CardAction>
                        <Button
                          size="sm"
                          variant="destructive-outline"
                          loading={unbind.isPending && unbind.variables === device.device_id}
                          disabled={unbind.isPending && unbind.variables !== device.device_id}
                          onClick={() => unbind.mutate(device.device_id)}
                          aria-label={t(
                            `解绑设备 ${device.device_id}`,
                            `Unbind device ${device.device_id}`,
                          )}
                        >
                          <UnlinkIcon />
                          {t('解绑', 'Unbind')}
                        </Button>
                      </CardAction>
                    )}
                  </CardHeader>
                  <CardPanel>
                    <dl className="grid grid-cols-2 gap-x-4 gap-y-3 sm:grid-cols-3">
                      <DeviceStat
                        label={t('请求', 'Requests')}
                        value={t(
                          `${formattedRequestCount} 次`,
                          `${formattedRequestCount} ${device.request_count === 1 ? 'request' : 'requests'}`,
                        )}
                      />
                      <DeviceStat
                        label={t('本账号花费', 'This account cost')}
                        value={formatUsd(device.cost_usd)}
                      />
                      <DeviceStat
                        className="col-span-2 sm:col-span-1"
                        label={t('全部账号花费', 'All accounts cost')}
                        value={formatUsd(device.cost_usd_all)}
                      />
                    </dl>
                  </CardPanel>
                </Card>
              </li>
            )
          })}
        </ul>
      )}
    </section>
  )
}

function CapacityStat({ label, value }: { label: string; value: string }) {
  return (
    <div className="min-w-0">
      <dt className="text-xs text-muted-foreground">{label}</dt>
      <dd className="mt-1 whitespace-nowrap font-semibold text-sm tabular-nums" title={value}>{value}</dd>
    </div>
  )
}

function DeviceStat({ label, value, className }: { label: string; value: string; className?: string }) {
  return (
    <div className={cn('min-w-0', className)}>
      <dt className="text-xs text-muted-foreground">{label}</dt>
      <dd className="mt-1 whitespace-nowrap font-medium text-sm tabular-nums" title={value}>{value}</dd>
    </div>
  )
}
