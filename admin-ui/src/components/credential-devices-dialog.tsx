import { useEffect, useState, type MouseEvent } from 'react'
import { useMutation, useQuery, useQueryClient, type UseQueryResult } from '@tanstack/react-query'
import {
  ChevronDownIcon,
  CopyIcon,
  MessagesSquareIcon,
  PencilIcon,
  RefreshCwIcon,
  ScrollTextIcon,
  SmartphoneIcon,
  Trash2Icon,
  UnlinkIcon,
} from 'lucide-react'
import {
  clearCredentialSessions,
  listCredentialDevices,
  listCredentialSessions,
  listSessionEvents,
  listSlotEvents,
  unbindCredentialDevice,
  unbindCredentialSession,
  type Credential,
  type DeviceBinding,
  type SessionBinding,
  type SessionEvent,
} from '@/api/credentials'
import { useI18n, type Language } from '@/lib/i18n'
import { useReadOnly } from '@/lib/role'
import {
  cn,
  copyText,
  displayCredentialLabel,
  extractError,
  formatDuration,
  formatFullTime,
  formatUsd,
  parseSessionKey,
  relativeTime,
} from '@/lib/utils'
import { ClampedDescription } from '@/components/settings-group'
import { deviceUsageMeta, METER_FILL, type CredentialActions } from '@/components/credential-shared'
import { RequestLookupDialog, type UsageDrillFilter } from '@/components/request-lookup-dialog'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Avatar, AvatarFallback } from '@/components/ui/avatar'
import { Badge, type BadgeProps } from '@/components/ui/badge'
import { Button, buttonVariants } from '@/components/ui/button'
import { Collapsible, CollapsiblePanel, CollapsibleTrigger } from '@/components/ui/collapsible'
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
  const readOnly = useReadOnly()
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
      ? { label: t('会话读取失败', 'Failed to load sessions'), variant: 'error' as const }
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
      <DialogPopup size="md">
        <DialogHeader>
          <div className="flex items-start gap-3 pr-8">
            <Avatar>
              <AvatarFallback>{cred.device_limit_applies ? <SmartphoneIcon /> : <MessagesSquareIcon />}</AvatarFallback>
            </Avatar>
            <div className="min-w-0 flex-1">
              {/* 标题写全两种名额：这个对话框上半段是设备、下半段是会话。 */}
              <DialogTitle>
                {cred.device_limit_applies
                  ? t('名额：设备与会话', 'Slots: devices and sessions')
                  : t('会话名额', 'Session slots')}
              </DialogTitle>
              <DialogDescription className="mt-1 truncate" title={credentialLabel}>{credentialLabel}</DialogDescription>
              <div className="mt-2 flex flex-wrap items-center gap-2">
                <Badge variant="outline">#{cred.id}</Badge>
                {/* 数量徽章只在读取中 / 读取失败时出现：读到了之后，下面两张容量卡的「名额占用 2/3」
                    「2/10」就是同一组数，头部再报一遍「2 台活跃设备」是重复。 */}
                {cred.device_limit_applies && (devices.isPending || devices.error) && (
                  <Badge variant={deviceStatus.variant} aria-live="polite">{deviceStatus.label}</Badge>
                )}
                {(sessions.isPending || sessions.error) && (
                  <Badge variant={sessionStatus.variant} aria-live="polite">{sessionStatus.label}</Badge>
                )}
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
            {/* 设备按会话占名额时设备上限不生效：设备那半（容量卡与设备列表）整个不出现，只剩会话。 */}
            {cred.device_limit_applies && (<>
            {editingLimit ? (
              <Card>
                <CardHeader>
                  <CardTitle className="text-sm leading-snug">
                    {t('设备上限', 'Device limit')}
                  </CardTitle>
                  <CardDescription className="text-xs">
                    {t(
                      '选择此账号跟随全局默认、不限设备数，或使用独立上限。',
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
                        '「跟随全局默认」会自动应用全局设备上限，不等于不限设备数。',
                        '“Use global default” applies the global device limit; it does not mean unlimited.',
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
                  {!readOnly && (
                    <CardAction>
                      <Button type="button" size="sm" variant="outline" onClick={startEditingLimit}>
                        <PencilIcon />
                        {t('调整上限', 'Adjust limit')}
                      </Button>
                    </CardAction>
                  )}
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
                        {/* 档位走 [deviceUsageMeta]，与卡片页脚的名额读数、列表里的名额条同一套判定：
                            0 灰、<70% 绿、≥70% 或只剩一个名额 黄、占满 红。原来这里是二分的
                            （占满才黄、永远不红），同一个 4/5 的号在卡片上是黄的、点进来却是绿的。 */}
                        <MeterIndicator
                          className={METER_FILL[deviceUsageMeta(currentDeviceCount, cred.device_limit_effective).level]}
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
            </>)}

            {/* 会话是另一种名额：设备上限不生效时的真实客户端、模拟路径上没有设备身份的来访按会话键
                粘住账号。它们与设备名额互不相干（一条请求只占其一），故单独一张容量卡和一份列表。 */}
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
                    '解绑仅释放当前名额；该设备下次请求时仍可能重新绑定。',
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

export function DeviceList({
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
  const readOnly = useReadOnly()
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
                '设备完成一次请求后将显示在此处。',
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
                  '非 Claude Code 客户端，使用按账号派生的身份；不写入绑定记录，也不占设备名额',
                  'Third-party client using an account-derived identity; it creates no binding record and uses no device slot',
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
                              title: t('已复制设备 ID', 'Device ID copied'),
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
                  {!device.simulated && !readOnly && (
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
                      valueClass="w-14"
                      hint={t('该设备经本账号产生的等价 API 费用', 'Equivalent API cost this device incurred through this account')}
                    />
                    <DeviceStat
                      label={t('全部账号', 'All accounts')}
                      value={formatUsd(device.cost_usd_all)}
                      valueClass="w-14"
                      hint={t('该设备在本网关所有账号上的累计费用', "This device's total cost across every account on this gateway")}
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

/**
 * 设备行 / 会话行右下角那组统计里的一项：`标签 值`，标签退到次要色，值用前景色顶住，并放在
 * 定宽的行内块里、左对齐，短值右边留白不补。
 *
 * 值若按内容伸缩，整组是右贴的，`$0.00` 与 `$123.45` 一差就把前面两项往左推——同一列的
 * 「请求」「本账号」「全部账号」在每一行都落在不同的位置，跨行读不成列。定宽之后三项在所有行
 * 上对齐。宽度按值的字形定：计数走默认的 2.75rem（够 `12,345`），金额传 `w-14`（够 `$0.0042`
 * 与 `$123.45`）；再长的值把这一格撑开，只影响那一行。
 */
/**
 * 会话行展开后的明细：完整会话键直接摆出来（悬浮提示里看不全、也没法选中复制），连同来访与上游
 * 两侧的会话 ID、槽位、模型和完整时间，下面是这条会话的历史事件。列表只含 TTL 内的绑定（后端
 * `list_sessions` 与名额计数同一口径），所以状态恒为活跃。面板收起时不挂载，事件在展开时才拉。
 */
function SessionDetails({
  credId,
  session,
  source,
  value,
  passthrough,
}: {
  credId: number
  session: SessionBinding
  source: 'sid' | 'pfx'
  value: string
  passthrough: boolean
}) {
  const { t, language } = useI18n()
  const [showSlot, setShowSlot] = useState(false)
  const events = useQuery({
    queryKey: ['session-events', credId, session.session_key],
    queryFn: () => listSessionEvents(credId, session.session_key),
  })
  const slotEvents = useQuery({
    queryKey: ['slot-events', credId, session.slot],
    queryFn: () => listSlotEvents(credId, session.slot),
    enabled: showSlot && !passthrough,
  })
  const rows: [string, string][] = [
    [t('状态', 'Status'), t('活跃', 'Active')],
    [t('来源', 'Source'), source === 'pfx' ? t('按缓存前缀识别', 'By cache prefix') : t('客户端自带会话 ID', 'Client session ID')],
    [t('来访会话 ID', 'Inbound session ID'), source === 'sid' ? value : '—'],
    [t('上游会话 ID', 'Upstream session ID'), session.session_id || '—'],
    [t('槽位', 'Slot'), passthrough ? t('无（沿用来访 ID，仍占名额）', 'None (keeps the inbound ID; still counts toward the limit)') : `#${session.slot}`],
    [t('来访设备', 'Device ID'), session.device_id ?? '—'],
    [t('最近模型', 'Last model'), session.last_model ?? '—'],
    [t('首次绑定', 'First bound'), formatFullTime(session.created_at, language)],
    [t('最近活跃', 'Last active'), formatFullTime(session.last_seen_at, language)],
  ]
  return (
    <div className="mt-2 space-y-2 border-t pt-2 pl-6 text-xs">
      <pre className="whitespace-pre-wrap break-all rounded-md bg-muted px-2.5 py-1.5 font-mono text-muted-foreground select-all">
        {session.session_key}
      </pre>
      <dl className="grid grid-cols-[auto_1fr] gap-x-4 gap-y-1">
        {rows.map(([label, text]) => (
          <div key={label} className="contents">
            <dt className="text-muted-foreground">{label}</dt>
            <dd className="min-w-0 break-all font-mono">{text}</dd>
          </div>
        ))}
      </dl>
      <div className="space-y-1 border-t pt-2">
        <div className="flex items-center justify-between gap-2">
          <span className="text-muted-foreground">{t('历史事件', 'History')}</span>
          {!passthrough && (
            <Button size="xs" variant="ghost" onClick={() => setShowSlot((v) => !v)} aria-expanded={showSlot}>
              {showSlot ? t('收起槽位历史', 'Hide slot history') : t(`槽位 #${session.slot} 历史`, `Slot #${session.slot} history`)}
            </Button>
          )}
        </div>
        <EventList query={events} language={language} />
      </div>
      {showSlot && !passthrough && (
        <div className="space-y-1 border-t pt-2">
          <span className="text-muted-foreground">
            {t(
              `槽位 #${session.slot}（上游会话 ID ${session.session_id.slice(0, 8)}）先后被哪些会话使用`,
              `Sessions that used slot #${session.slot} (upstream session ID ${session.session_id.slice(0, 8)})`,
            )}
          </span>
          <EventList query={slotEvents} language={language} showKey />
        </div>
      )}
    </div>
  )
}

/** 事件列表：时间、事件名、明细一行；`showKey` 时每行再带是哪条会话（槽位历史用）。 */
function EventList({
  query,
  language,
  showKey = false,
}: {
  query: UseQueryResult<SessionEvent[]>
  language: Language
  showKey?: boolean
}) {
  const { t } = useI18n()
  if (query.isPending) return <Skeleton className="h-4 w-3/5" />
  if (query.error) {
    return <p className="text-destructive-foreground">{extractError(query.error, language)}</p>
  }
  if (!query.data || query.data.length === 0) {
    return <p className="text-muted-foreground">{t('7 天内没有记录', 'Nothing recorded in the last 7 days')}</p>
  }
  return (
    <ul className="space-y-0.5">
      {query.data.map((e) => (
        <li key={e.id} className="grid grid-cols-[5.5rem_6rem_1fr] gap-x-3">
          <span className="text-muted-foreground" title={formatFullTime(e.ts, language)}>
            {relativeTime(e.ts, undefined, language)}
          </span>
          <span>{eventLabel(e.event, t)}</span>
          <span className="min-w-0 break-all text-muted-foreground">
            {showKey && <span className="font-mono text-foreground">{shortKey(e.session_key, t)} · </span>}
            {eventDetail(e, t, language)}
          </span>
        </li>
      ))}
    </ul>
  )
}

type T = (zh: string, en: string) => string

function eventLabel(event: string, t: T): string {
  switch (event) {
    case 'bound': return t('新建绑定', 'Bound')
    case 'slot_taken': return t('接手槽位', 'Took slot')
    case 'evicted': return t('槽位被接手', 'Slot taken over')
    case 'resumed': return t('休眠后恢复', 'Resumed')
    case 'reslotted': return t('换槽位', 'Changed slot')
    case 'rebound': return t('改绑', 'Rebound')
    case 'unbound': return t('解绑', 'Unbound')
    case 'expired': return t('过期清理', 'Expired')
    default: return event
  }
}

function reasonLabel(reason: string, t: T): string {
  switch (reason) {
    case 'disabled': return t('账号停用或已删除', 'account disabled or removed')
    case 'model_denied': return t('套餐不含该模型', 'model not in plan')
    case 'retried': return t('上游失败换号', 'switched after upstream failure')
    case 'cooling': return t('账号冷却中', 'account cooling down')
    case 'full': return t('名额已满', 'session limit reached')
    case 'bare_limit': return t('裸请求速率已满', 'bare request rate limit reached')
    case 'manual': return t('手动解绑', 'unbound manually')
    case 'clear': return t('一键清空', 'cleared all')
    case 'account_disabled': return t('账号停用', 'account disabled')
    case 'account_paused': return t('账号自动暂停', 'account paused automatically')
    case 'account_banned': return t('账号自动封停', 'account banned automatically')
    case 'account_removed': return t('账号删除', 'account removed')
    default: return reason
  }
}

/** 会话键的短写：来源加值的前 8 位，与流水那一格同一种写法。 */
function shortKey(key: string, t: T): string {
  const { source, value } = parseSessionKey(key)
  return `${source === 'pfx' ? t('前缀', 'prefix') : t('自带', 'client')} ${value.slice(0, 8)}`
}

/**
 * 点会话行的标题区（按钮之外）也能展开 / 收起，等同于点右侧的箭头：找到这一行的箭头替它点一下，
 * 开合状态仍由折叠组件自己管。拖选文字（复制 ID）时不算点击；键盘操作走箭头按钮本身。
 */
function toggleSessionRow(e: MouseEvent<HTMLElement>) {
  if ((e.target as HTMLElement).closest('button, a, input, [role="button"]')) return
  if (window.getSelection()?.toString()) return
  e.currentTarget
    .closest('li')
    ?.querySelector<HTMLElement>('[data-slot="collapsible-trigger"]')
    ?.click()
}

function eventDetail(e: SessionEvent, t: T, language: Language): string {
  const account = (id: number | null, label: string | null) =>
    id == null ? '—' : label ? displayCredentialLabel(label, language) : t(`账号 #${id}（已删除）`, `Account #${id} (removed)`)
  const slot = (n: number | null) => (n == null ? '—' : n < 0 ? t('沿用来访 ID', 'inbound ID') : `#${n}`)
  const idle = e.idle_secs != null ? t(`闲置 ${formatDuration(e.idle_secs, language)}`, `idle ${formatDuration(e.idle_secs, language)}`) : null
  const here = `${account(e.cred_id, e.cred_label)} · ${slot(e.slot)}`
  const parts: (string | null)[] = (() => {
    switch (e.event) {
      case 'slot_taken':
        return [here, e.other_key ? t(`前任 ${shortKey(e.other_key, t)}`, `previous ${shortKey(e.other_key, t)}`) : null, idle]
      case 'evicted':
        return [here, e.other_key ? t(`接手者 ${shortKey(e.other_key, t)}`, `taken by ${shortKey(e.other_key, t)}`) : null, idle]
      case 'resumed':
      case 'reslotted':
        return [e.prev_slot != null && e.prev_slot !== e.slot
          ? `${account(e.cred_id, e.cred_label)} · ${slot(e.prev_slot)} → ${slot(e.slot)}`
          : here]
      case 'rebound':
        return [
          `${account(e.prev_cred_id, e.prev_cred_label)} ${slot(e.prev_slot)} → ${account(e.cred_id, e.cred_label)} ${slot(e.slot)}`,
          e.reason ? reasonLabel(e.reason, t) : null,
        ]
      case 'unbound':
        return [here, e.reason ? reasonLabel(e.reason, t) : null, idle]
      case 'expired':
        return [here, idle]
      default:
        return [here]
    }
  })()
  return parts.filter(Boolean).join(' · ')
}

function DeviceStat({
  label,
  value,
  hint,
  valueClass = 'w-11',
}: { label: string; value: string; hint?: string; valueClass?: string }) {
  const stat = (
    <span className="whitespace-nowrap">
      {label}{' '}
      <span className={cn('inline-block text-left font-medium text-foreground', valueClass)}>
        {value}
      </span>
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
 * 会话容量：占用条 + 生效上限 + 策略徽章，以及自己的编辑态（与设备上限那张卡分开：两者
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
  const readOnly = useReadOnly()
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
            {editing ? t('会话上限', 'Session limit') : t('会话容量', 'Session capacity')}
          </CardTitle>
          <CardDescription className="text-xs">
            {/* 长说明默认收两行、末尾「了解更多」，同设置页的 ClampedDescription：超过 140 字各宽度都收，60–140 字只在手机上收。 */}
            <ClampedDescription text={t(
              '客户端请求按对话固定到账号，每个对话占用一个名额。真实客户端以其自带的会话 ID 识别对话（设备上限不生效时）；经模拟路径、无设备身份的请求自带会话 ID 时以其为准，否则按缓存前缀 + 首条用户消息识别，其出站会话 ID 由槽位派生，槽位释放后由下一个对话复用。该上限与设备名额相互独立。',
              'Client requests stick to this account per conversation, each conversation taking one slot. Real clients are identified by their own session ID (when the device limit does not apply); requests on the simulation path without a device identity use their own session ID if present, otherwise cache prefix + first user message, and their outbound session ID derives from a slot that is reused by the next conversation once freed. Independent of device slots.',
            )} />
          </CardDescription>
          {!editing && !readOnly && (
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
                    '「跟随全局默认」会自动应用全局会话上限，不等于不限会话数。',
                    '“Use global default” applies the global session limit; it does not mean unlimited.',
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
                  {/* 与上面的设备容量条同一套判定，见那边的注。 */}
                  <MeterIndicator className={METER_FILL[deviceUsageMeta(count, effective).level]} />
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

export function SessionList({
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
  const readOnly = useReadOnly()
  const qc = useQueryClient()
  const queryKey = ['credential-sessions', credId] as const
  // 展开着的会话详情里的历史事件跟着会话列表走：列表每拉完一次（刷新、解绑、窗口回到前台），
  // 这个号的会话事件与槽位历史一并失效重拉，详情开着也能看到新事件。放在列表这一层而不是
  // 外面的对话框：账号详情页也直接用这个列表，对话框在那里根本没挂载。
  useEffect(() => {
    if (isFetching) return
    qc.invalidateQueries({ queryKey: ['session-events', credId] })
    qc.invalidateQueries({ queryKey: ['slot-events', credId] })
  }, [isFetching, credId, qc])
  // 点「看请求」时带着这条会话的键去查流水；关掉就置空，对话框不常驻。
  const [drill, setDrill] = useState<UsageDrillFilter | null>(null)
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
            {t('活跃会话', 'Active sessions')}
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
          {!readOnly && !isPending && !error && (data?.length ?? 0) > 0 && (
            <Button
              type="button"
              size="xs"
              variant="destructive-outline"
              loading={clearAll.isPending}
              disabled={unbind.isPending}
              onClick={() => clearAll.mutate()}
              title={t('清除此账号的全部会话绑定（含休眠会话）；后续请求将照常重新选择账号','Remove every session binding on this account (dormant ones too); the next request selects an account as usual')}
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
                '有请求按会话占用名额后，会话将显示在此处。',
                'A session appears here once a request takes a session slot on this account.',
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
            // 主行是上游看到的会话 id（模拟会话按槽位派生、对话之间复用，真实客户端的是它自带 id
            // 按账号钉住后的值），槽位号做徽章（真实客户端不占槽位，记 -1）；对话自己的键
            // 退到悬浮提示里，来源直接读键上那一段（`lb:v2:sid:` / `lb:v2:pfx:`），不再靠
            // 「是不是 32 个 hex」猜——uuid 去掉横线也是 32 个 hex。
            const { source, value } = parseSessionKey(session.session_key)
            const derived = source === 'pfx'
            const passthrough = session.slot < 0
            const keyKind = derived ? t('按前缀', 'by prefix') : t('自带 ID', 'client ID')
            // 最近一轮的模型：记在绑定行上、不参与键（换模型不另起会话），旧库为空就不占位。
            const modelLabel = session.last_model ? `${session.last_model} · ` : ''
            return (
              <li key={session.session_key} className="rounded-lg border bg-card px-3 py-2.5">
                <Collapsible>
                  <div className="flex min-w-0 cursor-pointer items-center gap-2" onClick={toggleSessionRow}>
                    <MessagesSquareIcon className="size-4 shrink-0 text-muted-foreground" aria-hidden />
                    {passthrough ? (
                      <Badge variant="outline" size="sm" className="shrink-0" title={t('真实客户端的会话：沿用自带的会话 ID（按账号转换后发往上游），占名额、不分配槽位', 'A real client session: keeps its own session ID (converted per account before going upstream); counts toward the limit but gets no slot')}>
                        {t('真实', 'Real')}
                      </Badge>
                    ) : (
                      <Badge variant="outline" size="sm" className="shrink-0 tabular-nums" title={t('槽位：会话 ID 由槽位派生，槽位释放后由下一个对话复用', 'Slot: the session ID derives from it and is reused by the next conversation once freed')}>
                        #{session.slot}
                      </Badge>
                    )}
                    <Tooltip>
                      <TooltipTrigger render={<span />} className="min-w-0 flex-1 truncate font-mono text-xs">
                        {session.session_id || '—'}
                      </TooltipTrigger>
                      <TooltipPopup className="max-w-80 whitespace-normal break-all text-left leading-5">
                        {/* 真实客户端没带会话 ID（按前缀分会话）时出站也没有，后端给空串。 */}
                        {session.session_id
                          ? t(`上游可见的会话 ID ${session.session_id}`, `Session ID upstream sees: ${session.session_id}`)
                          : t('客户端未携带会话 ID，按缓存前缀与首条用户消息识别对话', 'The client sent no session ID; the conversation is identified by cache prefix and first user message')}
                        <br />
                        {t(`对话键（${keyKind}）${value}`, `Conversation key (${keyKind}): ${value}`)}
                      </TooltipPopup>
                    </Tooltip>
                    <Badge variant="secondary" size="sm">
                      {derived ? t('按前缀', 'By prefix') : t('自带 ID', 'Client ID')}
                    </Badge>
                    {session.session_id && (
                    <Tooltip>
                      <TooltipTrigger
                        className={cn(buttonVariants({ size: 'icon-xs', variant: 'ghost' }), 'shrink-0')}
                        aria-label={t(`复制会话 ID ${session.session_id}`, `Copy session ID ${session.session_id}`)}
                        onClick={async () => {
                          const copied = await copyText(session.session_id)
                          toastManager.add(copied
                            ? { title: t('已复制会话 ID', 'Session ID copied'), type: 'success' }
                            : { title: t('复制失败', 'Copy failed'), description: session.session_id, type: 'error' })
                        }}
                      >
                        <CopyIcon />
                      </TooltipTrigger>
                      <TooltipPopup>{t('复制会话 ID', 'Copy session ID')}</TooltipPopup>
                    </Tooltip>
                    )}
                    <Tooltip>
                      <TooltipTrigger
                        className={cn(buttonVariants({ size: 'icon-xs', variant: 'ghost' }), 'shrink-0')}
                        aria-label={t(`查看该会话的请求 ${session.session_id}`, `View requests for session ${session.session_id}`)}
                        // 按**对话键**筛而不是按上游那个 session_id：后者按槽位派生、对话之间
                        // 复用，按它筛会把先后占过同一槽位的几个对话混成一条。
                        onClick={() => setDrill({
                          credId,
                          sessionKey: session.session_key,
                          label: passthrough
                            ? t(`会话 ${(session.session_id || value).slice(0, 8)}`, `Session ${(session.session_id || value).slice(0, 8)}`)
                            : t(`会话 #${session.slot}`, `Session #${session.slot}`),
                          hours: 24,
                        })}
                      >
                        <ScrollTextIcon />
                      </TooltipTrigger>
                      <TooltipPopup>{t('查看该会话的请求','View this session’s requests')}</TooltipPopup>
                    </Tooltip>
                    {!readOnly && (
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
                    )}
                  </div>
                  <div
                    className="mt-1 flex cursor-pointer flex-wrap items-baseline justify-between gap-x-4 gap-y-1 pl-6 text-muted-foreground text-xs"
                    onClick={toggleSessionRow}
                  >
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
                    <div className="flex shrink-0 items-center gap-3 tabular-nums">
                      <DeviceStat label={t('请求', 'Requests')} value={session.request_count.toLocaleString(locale)} />
                      <CollapsibleTrigger
                        className={cn(buttonVariants({ size: 'icon-xs', variant: 'ghost' }), 'group shrink-0')}
                        aria-label={t('展开会话详情', 'Expand session details')}
                      >
                        <ChevronDownIcon className="transition-transform group-data-panel-open:rotate-180" />
                      </CollapsibleTrigger>
                    </div>
                  </div>
                  <CollapsiblePanel>
                    <SessionDetails credId={credId} session={session} source={source} value={value} passthrough={passthrough} />
                  </CollapsiblePanel>
                </Collapsible>
              </li>
            )
          })}
        </ul>
      )}
      {/* 「看请求」点开的流水：带这条会话的键，只列它自己的请求。没点过就不挂。 */}
      {drill && (
        <RequestLookupDialog
          open
          onOpenChange={(open) => { if (!open) setDrill(null) }}
          filter={drill}
        />
      )}
    </section>
  )
}
