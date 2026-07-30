import { useEffect, useId, useState, type ReactNode } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  BadgeCheckIcon,
  ChevronDownIcon,
  DatabaseIcon,
  InfoIcon,
  RefreshCwIcon,
  SaveIcon,
  ServerIcon,
  SlidersHorizontalIcon,
  TerminalIcon,
  type LucideIcon,
} from 'lucide-react'
import {
  getSettings,
  setForwarding,
  setRateLimitRetryMax,
  type ForwardingKey,
  type Settings,
} from '@/api/settings'
import { extractError } from '@/lib/utils'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import { Button } from '@/components/ui/button'
import {
  Dialog,
  DialogDescription,
  DialogHeader,
  DialogPanel,
  DialogPopup,
  DialogTitle,
} from '@/components/ui/dialog'
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import { Frame, FrameHeader, FramePanel, FrameTitle } from '@/components/ui/frame'
import {
  NumberField,
  NumberFieldDecrement,
  NumberFieldGroup,
  NumberFieldIncrement,
  NumberFieldInput,
} from '@/components/ui/number-field'
import { Spinner } from '@/components/ui/spinner'
import { Switch } from '@/components/ui/switch'
import { toastManager } from '@/components/ui/toast'

/**
 * 转发形态开关。
 *
 * 这些改动都不是「能不能用」的必需项。每一项都可以单独关闭，用于排查上游兼容性。
 */
export function ForwardingSettings({
  open,
  onOpenChange,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogPopup className="sm:max-w-3xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <SlidersHorizontalIcon aria-hidden="true" />
            转发形态
          </DialogTitle>
          <DialogDescription>配置兼容、缓存、限流和错误恢复策略。</DialogDescription>
        </DialogHeader>
        <DialogPanel>
          <ForwardingSettingsContent />
        </DialogPanel>
      </DialogPopup>
    </Dialog>
  )
}

export function ForwardingSettingsContent() {
  const settingsQuery = useQuery({ queryKey: ['settings'], queryFn: getSettings })

  if (settingsQuery.isPending) {
    return (
      <div className="flex min-h-40 items-center justify-center gap-2 text-sm text-muted-foreground" role="status">
        <Spinner className="size-4" />
        正在加载设置
      </div>
    )
  }

  if (settingsQuery.isError) {
    return (
      <div className="flex min-h-40 flex-col items-center justify-center gap-3 text-center" role="alert">
        <p className="text-sm font-medium">无法读取当前设置</p>
        <Button
          size="sm"
          variant="outline"
          loading={settingsQuery.isFetching}
          onClick={() => settingsQuery.refetch()}
        >
          重试
        </Button>
      </div>
    )
  }

  return (
    <div className="space-y-4">
      <Alert variant="info">
        <InfoIcon aria-hidden="true" />
        <AlertTitle>必要请求头不受影响</AlertTitle>
        <AlertDescription>
          下列开关不影响必要的 <code className="font-mono text-foreground">Authorization</code> 注入。
        </AlertDescription>
      </Alert>

      <SettingsGroup icon={BadgeCheckIcon} title="身份与订阅">
        <ForwardingToggle
          k="spoof_identity"
          label="身份一致性"
          summary="让客户端身份与当前账号、设备保持一致；关闭后原样转发。"
        />
        <ForwardingToggle
          k="billing_cch"
          label="订阅计费标识"
          summary="补齐订阅客户端所需的计费标识。"
        />
      </SettingsGroup>

      <SettingsGroup icon={ServerIcon} title="协议与请求头">
        <ForwardingToggle
          k="merge_beta"
          label="Beta 标记"
          summary="合并客户端 Beta 标记并补齐订阅所需标记。"
        />
        <ForwardingToggle
          k="fill_client_headers"
          label="客户端请求头"
          summary="补齐缺失的版本、编码和请求标识，不覆盖已有值。"
        />
        <ForwardingToggle
          k="orig_header_case"
          label="请求头形态"
          summary="还原官方客户端的请求头拼写与顺序；仅在排查兼容问题时关闭。"
        />
      </SettingsGroup>

      <SettingsGroup icon={DatabaseIcon} title="缓存优化">
        <ForwardingToggle
          k="system_shape"
          label="系统提示词缓存"
          summary="调整系统提示词分块，提高跨会话缓存复用。"
          description={
            <>
              只调整分块与缓存时间，不改变提示词文本。1 小时缓存写入单价是 5 分钟的 2 倍，
              但能延长复用时间；无法识别切点时原样转发。
            </>
          }
        />
      </SettingsGroup>

      <SettingsGroup icon={TerminalIcon} title="非官方客户端">
        <ForwardingToggle
          k="simulate_cc"
          label="模拟 Claude Code"
          summary="让 SDK 和第三方客户端按 Claude Code 请求形态转发。"
          description={
            <>
              仅改写非 Claude Code 请求。开启后会增加系统提示词和客户端请求头，
              可能提高 Token 成本并改变输出风格。此类请求通常没有设备身份，
              需先关闭「设备身份校验」。
            </>
          }
        />
      </SettingsGroup>

      <SettingsGroup icon={RefreshCwIcon} title="限流与错误恢复">
        <ForwardingToggle
          k="rate_limit_retry"
          label="429 自动换号"
          summary="遇到限流后，冷却受限账号或模型，并换用其他账号重试。"
          description={
            <>
              账号额度耗尽时冷却整个账号；只有当前模型受限时仅冷却该模型。默认分别冷却
              60 / 30 秒，并优先采用上游等待时间。换号会改绑有设备身份的请求，也可能降低缓存命中率；
              达到重试上限或没有其他账号时返回 <code>429</code>。
            </>
          }
        />
        <RetryMax />
        <ForwardingToggle
          k="thinking_signature_retry"
          label="thinking 签名兜底"
          summary="账号切换导致历史 thinking 签名失效时，自动降级并重试一次。"
          description={
            <>
              无法验证的历史 <code>thinking</code> 会转为普通文本，并用同一账号重试一次，
              不会删除原内容。工具续跑仍可能失败；重试会增加一次请求成本，失败时返回原始 400。
            </>
          }
        />
      </SettingsGroup>
    </div>
  )
}

/** 429 后追加尝试的账号数（不含首次请求；0 = 不重试；后端限制在 0~10）。 */
function RetryMax() {
  const qc = useQueryClient()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const [draft, setDraft] = useState<number | null>(null)

  useEffect(() => {
    if (data) setDraft(data.rate_limit_retry_max)
  }, [data?.rate_limit_retry_max])

  const save = useMutation({
    mutationFn: (count: number) => setRateLimitRetryMax(count),
    onSuccess: (settings: Settings) => {
      toastManager.add({
        title: '429 重试策略已更新',
        description: settings.rate_limit_retry_max > 0
          ? `最多追加尝试 ${settings.rate_limit_retry_max} 个账号。`
          : '429 将直接透传，不冷却、不换号。',
        type: 'success',
      })
      qc.setQueryData(['settings'], settings)
    },
    onError: (error) => {
      toastManager.add({ title: '保存失败', description: extractError(error), type: 'error' })
    },
  })

  const count = Math.min(10, Math.max(0, Math.floor(draft ?? 0)))
  const enabled = data?.rate_limit_retry ?? true

  return (
    <Field className="p-5">
      <div className="flex w-full flex-wrap items-end justify-between gap-3">
        <div className="min-w-0 space-y-1">
          <FieldLabel>追加重试账号数</FieldLabel>
          <FieldDescription>填 2 时最多尝试 3 个账号（含首次）。</FieldDescription>
        </div>
        <div className="flex items-center gap-2">
          <NumberField
            className="w-32"
            disabled={!enabled}
            max={10}
            min={0}
            value={draft}
            onValueChange={setDraft}
          >
            <NumberFieldGroup>
              <NumberFieldDecrement />
              <NumberFieldInput aria-label="429 追加重试账号数" />
              <NumberFieldIncrement />
            </NumberFieldGroup>
          </NumberField>
          <Button
            size="sm"
            loading={save.isPending}
            disabled={!enabled || count === (data?.rate_limit_retry_max ?? 2)}
            onClick={() => save.mutate(count)}
          >
            <SaveIcon />
            保存
          </Button>
        </div>
      </div>
    </Field>
  )
}

/** 单个开关：读写都走 ['settings']，改完让账号列表也失效。 */
function ForwardingToggle({
  k,
  label,
  summary,
  description,
}: {
  k: ForwardingKey
  label: string
  summary: string
  description?: ReactNode
}) {
  const id = useId()
  const qc = useQueryClient()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const enabled = data?.[k] ?? true

  const save = useMutation({
    mutationFn: (next: boolean) => setForwarding(k, next),
    onSuccess: (settings: Settings) => {
      toastManager.add({
        title: `${label}${settings[k] ? '已开启' : '已关闭'}`,
        description: summary,
        type: 'success',
      })
      qc.setQueryData(['settings'], settings)
      qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (error) => {
      toastManager.add({ title: '保存失败', description: extractError(error), type: 'error' })
    },
  })

  return (
    <Field className="p-5">
      <div className="flex w-full items-start justify-between gap-4">
        <div className="min-w-0 space-y-1">
          <FieldLabel htmlFor={id}>{label}</FieldLabel>
          <FieldDescription className="leading-5">{summary}</FieldDescription>
        </div>
        <Switch
          id={id}
          checked={enabled}
          disabled={save.isPending}
          onCheckedChange={(next) => save.mutate(next)}
        />
      </div>
      {description && (
        <details className="group text-xs text-muted-foreground">
          <summary className="flex w-fit cursor-pointer list-none items-center gap-1.5 rounded-sm font-medium transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring [&::-webkit-details-marker]:hidden">
            影响与限制
            <ChevronDownIcon className="size-3 transition-transform group-open:rotate-180" />
          </summary>
          <div className="mt-2 border-l-2 border-border pl-3 leading-5 [&_code]:rounded-sm [&_code]:bg-muted [&_code]:px-1 [&_code]:py-0.5 [&_code]:font-mono [&_code]:text-foreground">
            {description}
          </div>
        </details>
      )}
    </Field>
  )
}

function SettingsGroup({
  icon: Icon,
  title,
  children,
}: {
  icon: LucideIcon
  title: string
  children: ReactNode
}) {
  return (
    <Frame>
      <FrameHeader className="flex-row items-center gap-2">
        <Icon aria-hidden="true" className="size-4 text-muted-foreground" />
        <FrameTitle>{title}</FrameTitle>
      </FrameHeader>
      <FramePanel className="divide-y p-0">{children}</FramePanel>
    </Frame>
  )
}
