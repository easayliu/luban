import { useState } from 'react'
import { useMutation, useQuery, useQueryClient, type UseQueryResult } from '@tanstack/react-query'
import {
  CopyIcon,
  MessagesSquareIcon,
  PencilIcon,
  RefreshCwIcon,
  SmartphoneIcon,
  Trash2Icon,
  UnlinkIcon,
} from 'lucide-react'
import {
  clearCredentialSessions,
  listCredentialDevices,
  listCredentialSessions,
  unbindCredentialDevice,
  unbindCredentialSession,
  type Credential,
  type DeviceBinding,
  type SessionBinding,
} from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import {
  cn,
  copyText,
  displayCredentialLabel,
  extractError,
  formatFullTime,
  formatUsd,
  relativeTime,
} from '@/lib/utils'
import { type CredentialActions } from '@/components/credential-shared'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Avatar, AvatarFallback } from '@/components/ui/avatar'
import { Badge, type BadgeProps } from '@/components/ui/badge'
import { Button, buttonVariants } from '@/components/ui/button'
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
  Meter,
  MeterIndicator,
  MeterLabel,
  MeterTrack,
} from '@/components/ui/meter'
import {
  Select,
  SelectItem,
  SelectPopup,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { Skeleton } from '@/components/ui/skeleton'
import { Tooltip, TooltipPopup, TooltipTrigger } from '@/components/ui/tooltip'
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
  sessionLimit,
}: {
  cred: Credential
  open: boolean
  onOpenChange: (open: boolean) => void
  limit: CredentialActions['limit']
  sessionLimit: CredentialActions['sessionLimit']
}) {
  const { t, language, locale } = useI18n()
  const credentialLabel = displayCredentialLabel(cred.label, language)
  const [editingLimit, setEditingLimit] = useState(false)
  const [limitPolicy, setLimitPolicy] = useState<LimitPolicy>(() => policyFromLimit(cred.device_limit))
  const [customLimit, setCustomLimit] = useState(Math.max(1, cred.device_limit))
  const devices = useQuery({
    queryKey: ['credential-devices', cred.id],
    queryFn: () => listCredentialDevices(cred.id),
    enabled: open,
  })
  // 会话列表在对话框这一层拉：头部要并排显示两种名额的活跃数，下半段的容量卡与列表也用它。
  const sessions = useQuery({
    queryKey: ['credential-sessions', cred.id],
    queryFn: () => listCredentialSessions(cred.id),
    enabled: open,
  })
  const currentSessionCount = sessions.data?.length ?? cred.session_count
  const formattedSessionCount = currentSessionCount.toLocaleString(locale)
  const sessionStatus = sessions.isPending
    ? { label: t('会话读取中', 'Loading sessions'), variant: 'secondary' as const }
    : sessions.error
      ? { label: t('会话读取失败', 'Sessions failed to load'), variant: 'error' as const }
      : {
          label: t(
            `${formattedSessionCount} 条活跃会话`,
            `${formattedSessionCount} active ${currentSessionCount === 1 ? 'session' : 'sessions'}`,
          ),
          variant: 'success' as const,
        }

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
              {/* 标题写全两种名额：这个对话框上半段是设备、下半段是模拟会话，头部两枚徽章各报各的活跃数。 */}
              <DialogTitle>{t('名额：设备与模拟会话', 'Slots: devices & sessions')}</DialogTitle>
              <DialogDescription className="mt-1 truncate" title={credentialLabel}>{credentialLabel}</DialogDescription>
              <div className="mt-2 flex flex-wrap items-center gap-2">
                <Badge variant="outline">#{cred.id}</Badge>
                <Badge variant={deviceStatus.variant} aria-live="polite">{deviceStatus.label}</Badge>
                <Badge variant={sessionStatus.variant} aria-live="polite">{sessionStatus.label}</Badge>
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
                <CardPanel className="space-y-3">
                  {/* 「4 台 / 上限 10 台」这种关系，一条占用条比两个并排的数字直观得多；
                      不限设备时没有分母，画条永远填不满的进度反而误导，所以只在有上限时出现。 */}
                  {cred.device_limit_effective > 0 ? (
                    <Meter
                      value={Math.min(currentDeviceCount, cred.device_limit_effective)}
                      max={cred.device_limit_effective}
                      className="gap-1.5"
                    >
                      <div className="flex items-center justify-between gap-2">
                        <MeterLabel className="text-xs text-muted-foreground">
                          {t('名额占用', 'Slots used')}
                        </MeterLabel>
                        <span className="shrink-0 font-medium text-sm tabular-nums">
                          {formattedCurrentDeviceCount}
                          <span className="text-muted-foreground">/{cred.device_limit_effective.toLocaleString(locale)}</span>
                        </span>
                      </div>
                      <MeterTrack className="h-1.5">
                        <MeterIndicator
                          className={currentDeviceCount >= cred.device_limit_effective ? 'bg-warning' : 'bg-success'}
                        />
                      </MeterTrack>
                    </Meter>
                  ) : (
                    <div className="flex items-center justify-between gap-2">
                      <span className="text-xs text-muted-foreground">{t('名额占用', 'Slots used')}</span>
                      <span className="font-medium text-sm tabular-nums">
                        {formattedCurrentDeviceCount}
                        <span className="text-muted-foreground">/∞</span>
                      </span>
                    </div>
                  )}
                  <dl className="flex flex-wrap items-baseline gap-x-5 gap-y-2">
                    <CapacityStat label={t('生效上限', 'Effective limit')} value={effectiveLimit} />
                    <div className="min-w-0">
                      <dt className="text-xs text-muted-foreground">
                        {t('上限策略', 'Limit policy')}
                      </dt>
                      <dd className="mt-1"><Badge variant={currentPolicy.variant} size="sm">{currentPolicy.label}</Badge></dd>
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

            {/* 模拟会话是另一种名额：走模拟路径、没有设备身份的来访按会话键粘住账号。
                它们与设备名额互不相干（一条请求只占其一），故单独一张容量卡和一份列表。 */}
            <SessionCapacityCard cred={cred} sessionLimit={sessionLimit} sessions={sessions} />
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
        {/* 这里以前还挂一个条数。但它数的是列表条目（含模拟设备），跟头部徽章那个「真实绑定数」
            不是一个口径，两个 “N 台” 并排出现只会让人以为哪边算错了。名额多少看上面的占用条，
            模拟设备各自带徽章，这里只留刷新指示。 */}
        {!isPending && !error && isFetching && (
          <span className="inline-flex items-center gap-2 text-xs text-muted-foreground">
            <Spinner />
            {t('刷新中', 'Refreshing')}
          </span>
        )}
      </div>

      {isPending ? (
        <div
          className="space-y-2"
          role="status"
          aria-label={t('正在读取设备列表', 'Loading device list')}
        >
          {Array.from({ length: 3 }, (_, index) => (
            <div key={index} className="rounded-lg border bg-card px-3 py-2.5">
              <div className="flex items-center gap-2">
                <Skeleton className="size-4 shrink-0 rounded" />
                <Skeleton className="h-4 w-2/5" />
              </div>
              <div className="mt-1.5 flex items-center justify-between gap-4 pl-6">
                <Skeleton className="h-3 w-2/5" />
                <Skeleton className="h-3 w-24 shrink-0" />
              </div>
            </div>
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
        // 一台设备一张带头部和统计网格的卡片，绑满十几台时要滚很久才看得完；
        // 压成两行的紧凑条目后，同样的信息只占三分之一高度，一屏能对比多台设备。
        <ul className="space-y-1.5">
          {data.map((device) => {
            const formattedRequestCount = device.request_count.toLocaleString(locale)
            // 模拟客户端没有绑定行，也就没有这两个时刻（见 DeviceBinding.simulated）。
            const firstBoundFull = formatFullTime(device.created_at ?? 0, language)
            const lastSeenFull = formatFullTime(device.last_seen_at ?? 0, language)
            const firstBoundRelative = relativeTime(device.created_at ?? 0, undefined, language)
            const lastSeenRelative = relativeTime(device.last_seen_at ?? 0, undefined, language)
            const meta = device.simulated
              ? t(
                  '按账号派生的身份，不占设备名额',
                  'Account-derived identity; does not use a device slot',
                )
              : t(
                  `首次绑定 ${firstBoundRelative} · 最近活跃 ${lastSeenRelative}`,
                  `First bound ${firstBoundRelative} · Last active ${lastSeenRelative}`,
                )
            const metaDetail = device.simulated
              ? t(
                  '非 Claude Code 客户端，按账号派生的身份；不写绑定行，也不占设备名额',
                  'Third-party client using an account-derived identity; it creates no binding row and uses no device slot',
                )
              : t(
                  `首次绑定 ${firstBoundFull} · 最近活跃 ${lastSeenFull}`,
                  `First bound ${firstBoundFull} · Last active ${lastSeenFull}`,
                )
            return (
              <li key={device.device_id} className="rounded-lg border bg-card px-3 py-2.5">
                <div className="flex min-w-0 items-center gap-2">
                  <SmartphoneIcon className="size-4 shrink-0 text-muted-foreground" aria-hidden />
                  <Tooltip>
                    <TooltipTrigger
                      render={<span />}
                      className="min-w-0 flex-1 truncate font-mono text-xs"
                    >
                      {device.device_id}
                    </TooltipTrigger>
                    <TooltipPopup className="max-w-80 whitespace-normal break-all text-left">
                      {device.device_id}
                    </TooltipPopup>
                  </Tooltip>
                  {device.simulated && (
                    <Badge variant="secondary" size="sm">{t('模拟', 'Simulated')}</Badge>
                  )}
                  <Tooltip>
                    <TooltipTrigger
                      className={cn(buttonVariants({ size: 'icon-xs', variant: 'ghost' }), 'shrink-0')}
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
                    </TooltipTrigger>
                    <TooltipPopup>{t('复制设备 ID', 'Copy device ID')}</TooltipPopup>
                  </Tooltip>
                  {/* 模拟伪设备没有绑定行可删，故不给解绑按钮——点了也只会是一次空操作。 */}
                  {!device.simulated && (
                    <Button
                      size="xs"
                      variant="destructive-outline"
                      className="ml-1 shrink-0"
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
                  )}
                </div>
                <div className="mt-1 flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1 pl-6 text-muted-foreground text-xs">
                  <Tooltip>
                    <TooltipTrigger render={<span />} className="min-w-0 truncate">
                      {meta}
                    </TooltipTrigger>
                    <TooltipPopup className="max-w-80 whitespace-normal text-left leading-5">
                      {metaDetail}
                    </TooltipPopup>
                  </Tooltip>
                  <div className="flex shrink-0 items-baseline gap-3 tabular-nums">
                    <DeviceStat
                      label={t('请求', 'Requests')}
                      value={formattedRequestCount}
                    />
                    <DeviceStat
                      label={t('本账号', 'This account')}
                      value={formatUsd(device.cost_usd)}
                      hint={t('这台设备经本账号产生的等价 API 费用', 'Equivalent API cost this device incurred through this account')}
                    />
                    <DeviceStat
                      label={t('全部账号', 'All accounts')}
                      value={formatUsd(device.cost_usd_all)}
                      hint={t('这台设备在本网关所有账号上的累计花费', "This device's total cost across every account on this gateway")}
                    />
                  </div>
                </div>
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

/** 设备条目里的行内统计：`标签 值`，标签退到次要色，值用前景色顶住。 */
function DeviceStat({ label, value, hint }: { label: string; value: string; hint?: string }) {
  const stat = (
    <span className="whitespace-nowrap">
      {label} <span className="font-medium text-foreground">{value}</span>
    </span>
  )
  if (!hint) return stat
  return (
    <Tooltip>
      <TooltipTrigger render={stat} />
      <TooltipPopup className="max-w-72 whitespace-normal text-left leading-5">{hint}</TooltipPopup>
    </Tooltip>
  )
}

/**
 * 模拟会话容量：占用条 + 生效上限 + 策略徽章，以及自己的编辑态（与设备上限那张卡分开：两者
 * 各自一个 mutation、各自一套三态，混在一个表单里保存哪一个都说不清）。列表挂在卡片下面。
 */
function SessionCapacityCard({
  cred,
  sessionLimit,
  sessions,
}: {
  cred: Credential
  sessionLimit: CredentialActions['sessionLimit']
  sessions: UseQueryResult<SessionBinding[]>
}) {
  const { t, locale } = useI18n()
  const [editing, setEditing] = useState(false)
  const [policy, setPolicy] = useState<LimitPolicy>(() => policyFromLimit(cred.session_limit))
  const [custom, setCustom] = useState(Math.max(1, cred.session_limit))
  const count = sessions.data?.length ?? cred.session_count
  const formattedCount = count.toLocaleString(locale)
  const effective = cred.session_limit_effective
  const policyItems = [
    { value: 'default' as const, label: t('跟随全局默认', 'Use global default') },
    { value: 'unlimited' as const, label: t('不限会话数', 'Unlimited sessions') },
    { value: 'custom' as const, label: t('自定义上限', 'Custom limit') },
  ]
  const effectiveLabel = effective > 0
    ? t(`${effective.toLocaleString(locale)} 条`, `${effective.toLocaleString(locale)} ${effective === 1 ? 'session' : 'sessions'}`)
    : t('不限', 'Unlimited')
  const currentPolicy = {
    label: cred.session_limit === 0
      ? t('跟随默认', 'Use default')
      : cred.session_limit < 0
        ? t('不限', 'Unlimited')
        : t('自定义', 'Custom'),
    variant: policyVariant(cred.session_limit),
  }
  const reset = () => {
    setEditing(false)
    setPolicy(policyFromLimit(cred.session_limit))
    setCustom(Math.max(1, cred.session_limit))
  }
  const save = () => {
    const normalized = Number.isFinite(custom) ? Math.max(1, Math.floor(custom)) : 1
    const next = policy === 'default' ? 0 : policy === 'unlimited' ? -1 : normalized
    sessionLimit.mutate(next, { onSuccess: () => setEditing(false) })
  }

  return (
    <div className="space-y-5">
      <Card>
        <CardHeader>
          <CardTitle className="text-sm leading-snug">
            {editing ? t('模拟会话上限', 'Simulated session limit') : t('模拟会话容量', 'Simulated session capacity')}
          </CardTitle>
          <CardDescription className="text-xs">
            {t(
              '走模拟路径、没有设备身份的来访按对话（自带的会话 id，否则缓存前缀 + 首条用户消息）粘住账号并占一个槽位；出站会话 id 按槽位派生、释放后被下一个对话复用，上游看到的会话 id 数就是上限。与设备名额互不相干。',
              'Requests on the simulation path without a device identity bind to this account per conversation (their session id, else cache prefix + first user message) and take a slot; the outbound session id derives from the slot and is reused by the next conversation once freed, so upstream sees at most this many session ids. Independent of device slots.',
            )}
          </CardDescription>
          {!editing && (
            <CardAction>
              <Button
                type="button"
                size="sm"
                variant="outline"
                onClick={() => {
                  setPolicy(policyFromLimit(cred.session_limit))
                  setCustom(Math.max(1, cred.session_limit))
                  setEditing(true)
                }}
              >
                <PencilIcon />
                {t('调整上限', 'Adjust limit')}
              </Button>
            </CardAction>
          )}
        </CardHeader>
        {editing ? (
          <CardPanel className="space-y-4">
            <div className="grid gap-4 sm:grid-cols-2">
              <Field>
                <FieldLabel>{t('上限策略', 'Limit policy')}</FieldLabel>
                <Select
                  items={policyItems}
                  value={policy}
                  onValueChange={(value) => {
                    if (value) setPolicy(value as LimitPolicy)
                  }}
                >
                  <SelectTrigger aria-label={t('会话上限策略', 'Session limit policy')}>
                    <SelectValue />
                  </SelectTrigger>
                  <SelectPopup>
                    {policyItems.map((item) => (
                      <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>
                    ))}
                  </SelectPopup>
                </Select>
                <FieldDescription>
                  {t(
                    '“默认”会自动应用全局会话上限，不等于不限。',
                    '“Default” applies the global session limit; it does not mean unlimited.',
                  )}
                </FieldDescription>
              </Field>
              {policy === 'custom' && (
                <Field>
                  <FieldLabel>{t('最多活跃会话', 'Maximum active sessions')}</FieldLabel>
                  <NumberField
                    value={custom}
                    min={1}
                    step={1}
                    onValueChange={(value) => setCustom(value ?? 1)}
                  >
                    <NumberFieldGroup>
                      <NumberFieldDecrement />
                      <NumberFieldInput aria-label={t('自定义会话上限', 'Custom session limit')} />
                      <NumberFieldIncrement />
                    </NumberFieldGroup>
                  </NumberField>
                  <FieldDescription>
                    {t('该设置只影响当前账号。', 'This setting only affects the current account.')}
                  </FieldDescription>
                </Field>
              )}
            </div>
            <div className="flex justify-end gap-2">
              <Button type="button" variant="outline" disabled={sessionLimit.isPending} onClick={reset}>
                {t('取消', 'Cancel')}
              </Button>
              <Button type="button" loading={sessionLimit.isPending} onClick={save}>{t('保存', 'Save')}</Button>
            </div>
          </CardPanel>
        ) : (
          <CardPanel className="space-y-3">
            {effective > 0 ? (
              <Meter value={Math.min(count, effective)} max={effective} className="gap-1.5">
                <div className="flex items-center justify-between gap-2">
                  <MeterLabel className="text-xs text-muted-foreground">
                    {t('名额占用', 'Slots used')}
                  </MeterLabel>
                  <span className="shrink-0 font-medium text-sm tabular-nums">
                    {formattedCount}
                    <span className="text-muted-foreground">/{effective.toLocaleString(locale)}</span>
                  </span>
                </div>
                <MeterTrack className="h-1.5">
                  <MeterIndicator className={count >= effective ? 'bg-warning' : 'bg-success'} />
                </MeterTrack>
              </Meter>
            ) : (
              <div className="flex items-center justify-between gap-2">
                <span className="text-xs text-muted-foreground">{t('名额占用', 'Slots used')}</span>
                <span className="font-medium text-sm tabular-nums">
                  {formattedCount}
                  <span className="text-muted-foreground">/∞</span>
                </span>
              </div>
            )}
            <dl className="flex flex-wrap items-baseline gap-x-5 gap-y-2">
              <CapacityStat label={t('生效上限', 'Effective limit')} value={effectiveLabel} />
              <div className="min-w-0">
                <dt className="text-xs text-muted-foreground">{t('上限策略', 'Limit policy')}</dt>
                <dd className="mt-1"><Badge variant={currentPolicy.variant} size="sm">{currentPolicy.label}</Badge></dd>
              </div>
            </dl>
          </CardPanel>
        )}
      </Card>

      <SessionList
        credId={cred.id}
        data={sessions.data}
        isPending={sessions.isPending}
        isFetching={sessions.isFetching}
        error={sessions.error}
        onRetry={() => { void sessions.refetch() }}
      />
    </div>
  )
}

/**
 * 拆开会话键：后端写的是 `lb:v2:<来源>:<值>`（见 `session_binding_key`），来源 `sid` 是来访
 * 自带的会话 id、`pfx` 是「缓存前缀 + 对话起点」的指纹。
 *
 * 认不出前缀的只可能是旧口径的残留（开库时那条迁移会清掉），退回原来那套按长相猜的判法。
 */
function parseSessionKey(key: string): { source: 'sid' | 'pfx'; value: string } {
  const m = /^lb:v\d+:(sid|pfx):([\s\S]*)$/.exec(key)
  if (m) return { source: m[1] as 'sid' | 'pfx', value: m[2] }
  return { source: /^[0-9a-f]{32}$/.test(key) ? 'pfx' : 'sid', value: key }
}

function SessionList({
  credId,
  data,
  isPending,
  isFetching,
  error,
  onRetry,
}: {
  credId: number
  data: SessionBinding[] | undefined
  isPending: boolean
  isFetching: boolean
  error: Error | null
  onRetry: () => void
}) {
  const { t, language, locale } = useI18n()
  const qc = useQueryClient()
  const queryKey = ['credential-sessions', credId] as const
  const unbind = useMutation({
    mutationFn: (sessionKey: string) => unbindCredentialSession(credId, sessionKey),
    onSuccess: (_, sessionKey) => {
      toastManager.add({ title: t('已解绑', 'Session unbound'), type: 'success' })
      qc.setQueryData<SessionBinding[]>(queryKey, (current) =>
        current?.filter((session) => session.session_key !== sessionKey))
      qc.invalidateQueries({ queryKey })
      qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (error) => toastManager.add({
      title: t('解绑失败', 'Failed to unbind session'),
      description: extractError(error, language),
      type: 'error',
    }),
  })
  // 一键清空：会话是 luban 自己派生的键、数量比设备多得多，逐条点没意义。
  const clearAll = useMutation({
    mutationFn: () => clearCredentialSessions(credId),
    onSuccess: (removed) => {
      toastManager.add({
        title: t(`已清理 ${removed} 条会话`, `Cleared ${removed} ${removed === 1 ? 'session' : 'sessions'}`),
        type: 'success',
      })
      qc.setQueryData<SessionBinding[]>(queryKey, [])
      qc.invalidateQueries({ queryKey })
      qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (error) => toastManager.add({
      title: t('清理失败', 'Failed to clear sessions'),
      description: extractError(error, language),
      type: 'error',
    }),
  })

  return (
    <section className="space-y-3" aria-labelledby={`active-sessions-${credId}`}>
      <div className="flex items-end justify-between gap-3">
        <div>
          <h3 id={`active-sessions-${credId}`} className="font-semibold text-sm">
            {t('活跃模拟会话', 'Active simulated sessions')}
          </h3>
          <p className="text-xs text-muted-foreground">
            {t('按最近活跃时间排序', 'Sorted by most recent activity')}
          </p>
        </div>
        <div className="flex items-center gap-2">
          {!isPending && !error && isFetching && (
            <span className="inline-flex items-center gap-2 text-xs text-muted-foreground">
              <Spinner />
              {t('刷新中', 'Refreshing')}
            </span>
          )}
          {!isPending && !error && (data?.length ?? 0) > 0 && (
            <Button
              type="button"
              size="xs"
              variant="destructive-outline"
              loading={clearAll.isPending}
              disabled={unbind.isPending}
              onClick={() => clearAll.mutate()}
              title={t('清掉这个账号的全部模拟会话绑定（含休眠的）；下一条请求照常重新选号', 'Remove every simulated session binding on this account (dormant ones too); the next request selects an account as usual')}
            >
              <Trash2Icon />
              {t('全部清理', 'Clear all')}
            </Button>
          )}
        </div>
      </div>

      {isPending ? (
        <div className="space-y-2" role="status" aria-label={t('正在读取会话列表', 'Loading session list')}>
          {Array.from({ length: 2 }, (_, index) => (
            <div key={index} className="rounded-lg border bg-card px-3 py-2.5">
              <div className="flex items-center gap-2">
                <Skeleton className="size-4 shrink-0 rounded" />
                <Skeleton className="h-4 w-2/5" />
              </div>
              <div className="mt-1.5 flex items-center justify-between gap-4 pl-6">
                <Skeleton className="h-3 w-2/5" />
                <Skeleton className="h-3 w-16 shrink-0" />
              </div>
            </div>
          ))}
        </div>
      ) : error ? (
        <Alert variant="error">
          <AlertTitle>{t('会话列表读取失败', 'Failed to load session list')}</AlertTitle>
          <AlertDescription>
            <p className="break-words">{extractError(error, language)}</p>
            <Button type="button" size="sm" variant="destructive-outline" onClick={onRetry}>
              <RefreshCwIcon />
              {t('重试', 'Retry')}
            </Button>
          </AlertDescription>
        </Alert>
      ) : !data || data.length === 0 ? (
        <Empty className="py-8">
          <EmptyHeader>
            <EmptyMedia variant="icon"><MessagesSquareIcon /></EmptyMedia>
            <EmptyTitle className="text-base">{t('暂无活跃会话', 'No active sessions')}</EmptyTitle>
            <EmptyDescription>
              {t(
                '走模拟路径且没有设备身份的请求完成一次后会出现在这里。',
                'A session appears here after a simulated request without a device identity completes.',
              )}
            </EmptyDescription>
          </EmptyHeader>
        </Empty>
      ) : (
        <ul className="space-y-1.5">
          {data.map((session) => {
            const firstBoundRelative = relativeTime(session.created_at, undefined, language)
            const lastSeenRelative = relativeTime(session.last_seen_at, undefined, language)
            const firstBoundFull = formatFullTime(session.created_at, language)
            const lastSeenFull = formatFullTime(session.last_seen_at, language)
            // 主行是上游看到的会话 id（按槽位派生、对话之间复用），槽位号做徽章；对话自己的键
            // 退到悬浮提示里，来源直接读键上那一段（`lb:v2:sid:` / `lb:v2:pfx:`），不再靠
            // 「是不是 32 个 hex」猜——uuid 去掉横线也是 32 个 hex。
            const { source, value } = parseSessionKey(session.session_key)
            const derived = source === 'pfx'
            const keyKind = derived ? t('按前缀', 'by prefix') : t('自带 id', 'client id')
            // 最近一轮的模型：记在绑定行上、不参与键（换模型不另起会话），旧库为空就不占位。
            const modelLabel = session.last_model ? `${session.last_model} · ` : ''
            return (
              <li key={session.session_key} className="rounded-lg border bg-card px-3 py-2.5">
                <div className="flex min-w-0 items-center gap-2">
                  <MessagesSquareIcon className="size-4 shrink-0 text-muted-foreground" aria-hidden />
                  <Badge variant="outline" size="sm" className="shrink-0 tabular-nums" title={t('槽位：会话 id 由它派生，释放后被下一个对话复用', 'Slot: the session id derives from it and is reused by the next conversation once freed')}>
                    #{session.slot}
                  </Badge>
                  <Tooltip>
                    <TooltipTrigger render={<span />} className="min-w-0 flex-1 truncate font-mono text-xs">
                      {session.session_id}
                    </TooltipTrigger>
                    <TooltipPopup className="max-w-80 whitespace-normal break-all text-left leading-5">
                      {t(`上游看到的会话 id ${session.session_id}`, `Session id upstream sees: ${session.session_id}`)}
                      <br />
                      {t(`对话键（${keyKind}）${value}`, `Conversation key (${keyKind}): ${value}`)}
                    </TooltipPopup>
                  </Tooltip>
                  <Badge variant="secondary" size="sm">
                    {derived ? t('按前缀', 'By prefix') : t('自带 id', 'Client id')}
                  </Badge>
                  <Tooltip>
                    <TooltipTrigger
                      className={cn(buttonVariants({ size: 'icon-xs', variant: 'ghost' }), 'shrink-0')}
                      aria-label={t(`复制会话 id ${session.session_id}`, `Copy session id ${session.session_id}`)}
                      onClick={async () => {
                        const copied = await copyText(session.session_id)
                        toastManager.add(copied
                          ? { title: t('已复制会话 id', 'Copied session id'), type: 'success' }
                          : { title: t('复制失败', 'Copy failed'), description: session.session_id, type: 'error' })
                      }}
                    >
                      <CopyIcon />
                    </TooltipTrigger>
                    <TooltipPopup>{t('复制会话 id', 'Copy session id')}</TooltipPopup>
                  </Tooltip>
                  <Button
                    size="xs"
                    variant="destructive-outline"
                    className="ml-1 shrink-0"
                    loading={unbind.isPending && unbind.variables === session.session_key}
                    disabled={unbind.isPending && unbind.variables !== session.session_key}
                    onClick={() => unbind.mutate(session.session_key)}
                    aria-label={t(`解绑会话 ${session.session_key}`, `Unbind session ${session.session_key}`)}
                  >
                    <UnlinkIcon />
                    {t('解绑', 'Unbind')}
                  </Button>
                </div>
                <div className="mt-1 flex flex-wrap items-baseline justify-between gap-x-4 gap-y-1 pl-6 text-muted-foreground text-xs">
                  <Tooltip>
                    <TooltipTrigger render={<span />} className="min-w-0 truncate">
                      {t(
                        `${modelLabel}首次绑定 ${firstBoundRelative} · 最近活跃 ${lastSeenRelative}`,
                        `${modelLabel}First bound ${firstBoundRelative} · Last active ${lastSeenRelative}`,
                      )}
                    </TooltipTrigger>
                    <TooltipPopup className="max-w-80 whitespace-normal text-left leading-5">
                      {t(
                        `${modelLabel}首次绑定 ${firstBoundFull} · 最近活跃 ${lastSeenFull}`,
                        `${modelLabel}First bound ${firstBoundFull} · Last active ${lastSeenFull}`,
                      )}
                    </TooltipPopup>
                  </Tooltip>
                  <div className="flex shrink-0 items-baseline gap-3 tabular-nums">
                    <DeviceStat label={t('请求', 'Requests')} value={session.request_count.toLocaleString(locale)} />
                  </div>
                </div>
              </li>
            )
          })}
        </ul>
      )}
    </section>
  )
}
