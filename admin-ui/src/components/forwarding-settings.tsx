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
import { useI18n } from '@/lib/i18n'
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
  const { t } = useI18n()

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogPopup className="sm:max-w-3xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <SlidersHorizontalIcon aria-hidden="true" />
            {t('转发策略', 'Forwarding policy')}
          </DialogTitle>
          <DialogDescription>
            {t(
              '配置兼容、缓存、限流和错误恢复策略。',
              'Configure compatibility, caching, rate limiting, and error recovery policies.',
            )}
          </DialogDescription>
        </DialogHeader>
        <DialogPanel>
          <ForwardingSettingsContent />
        </DialogPanel>
      </DialogPopup>
    </Dialog>
  )
}

export function ForwardingSettingsContent() {
  const { t } = useI18n()
  const settingsQuery = useQuery({ queryKey: ['settings'], queryFn: getSettings })

  if (settingsQuery.isPending) {
    return (
      <div className="flex min-h-40 items-center justify-center gap-2 text-sm text-muted-foreground" role="status">
        <Spinner className="size-4" />
        {t('正在加载设置', 'Loading settings')}
      </div>
    )
  }

  if (settingsQuery.isError) {
    return (
      <div className="flex min-h-40 flex-col items-center justify-center gap-3 text-center" role="alert">
        <p className="text-sm font-medium">
          {t('无法读取当前设置', 'Unable to load current settings')}
        </p>
        <Button
          size="sm"
          variant="outline"
          loading={settingsQuery.isFetching}
          onClick={() => settingsQuery.refetch()}
        >
          {t('重试', 'Retry')}
        </Button>
      </div>
    )
  }

  return (
    <div className="space-y-4">
      <Alert variant="info">
        <InfoIcon aria-hidden="true" />
        <AlertTitle>{t('必要请求头不受影响', 'Required headers are unaffected')}</AlertTitle>
        <AlertDescription>
          <p>
            {t('下列开关不影响必要的', 'The following toggles do not affect required')}{' '}
            <code className="font-mono text-foreground">Authorization</code>{' '}
            {t('注入。', 'injection.')}
          </p>
        </AlertDescription>
      </Alert>

      <SettingsGroup icon={BadgeCheckIcon} title={t('身份与订阅', 'Identity & subscription')}>
        <ForwardingToggle
          k="spoof_identity"
          label={t('身份一致性', 'Identity consistency')}
          summary={t(
            '让客户端身份与当前账号、设备保持一致；关闭后原样转发。',
            'Keep the client identity consistent with the current account and device; when disabled, forward it unchanged.',
          )}
        />
        {/* 紧跟「身份一致性」：它是本项的前置开关，挨着放才好一起改。 */}
        <ForwardingToggle
          k="fill_metadata"
          label={t('补齐设备身份', 'Fill missing device identity')}
          summary={t(
            '请求未携带设备身份时，按当前账号补一份；已携带的不改动。',
            'When a request lacks a device identity, add one for the current account; leave existing values unchanged.',
          )}
          requires={{ key: 'spoof_identity', label: t('身份一致性', 'Identity consistency') }}
          description={
            <>
              {t(
                '官方客户端每条请求都带设备身份，缺失本身就是一处差异。常见于模仿 Claude Code 的第三方客户端。补出的身份与「身份一致性」用同一套取值，会话标识优先沿用请求自带的。',
                'The official client includes a device identity with every request, so a missing identity is itself a discrepancy. This is common in third-party clients that imitate Claude Code. The generated identity uses the same values as “Identity consistency,” while a session identifier already present in the request takes precedence.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="billing_cch"
          label={t('订阅计费标识', 'Subscription billing identifier')}
          summary={t(
            '补齐订阅客户端所需的计费标识。',
            'Add the billing identifier required by subscription clients.',
          )}
        />
      </SettingsGroup>

      <SettingsGroup icon={ServerIcon} title={t('协议与请求头', 'Protocol & request headers')}>
        <ForwardingToggle
          k="merge_beta"
          label={t('Beta 标记', 'Beta flags')}
          summary={t(
            '合并客户端 Beta 标记并补齐订阅所需标记。',
            'Merge the client’s Beta flags and add the flags required for subscriptions.',
          )}
        />
        <ForwardingToggle
          k="fill_client_headers"
          label={t('客户端请求头', 'Client request headers')}
          summary={t(
            '补齐缺失的版本、编码和请求标识，不覆盖已有值。',
            'Fill in missing version, encoding, and request identifiers without overwriting existing values.',
          )}
        />
        <ForwardingToggle
          k="orig_header_case"
          label={t('请求头形态', 'Request header shape')}
          summary={t(
            '还原官方客户端的请求头拼写与顺序；仅在排查兼容问题时关闭。',
            'Restore the official client’s request header casing and order; disable only when troubleshooting compatibility issues.',
          )}
        />
      </SettingsGroup>

      <SettingsGroup icon={DatabaseIcon} title={t('系统提示词', 'System prompt')}>
        <ForwardingToggle
          k="system_shape"
          label={t('分块与缓存形态', 'Block shape & caching')}
          summary={t(
            '按官方客户端对齐系统提示词的分块与缓存断点；同时把块数封顶在 4 块。',
            'Align system prompt blocks and cache breakpoints with the official client, and cap the block count at 4.',
          )}
          description={
            <>
              {t(
                '只调整分块，不改变提示词文本、也不改缓存时长——缓存时长（ttl）一概沿用客户端自己传的，客户端没传就用上游默认，luban 不替它决定。它不只是缓存优化：官方客户端的系统提示词恒为 4 块，超出的会被上游判成第三方应用，改从超额额度（extra usage）扣费而不是订阅额度，所以多出来的块会被并回第 4 块。无法识别切点时原样转发。',
                'Only block boundaries are adjusted — the prompt text is unchanged, and so is cache duration: the ttl always comes from the client, or from the upstream default when the client sends none. luban never decides it for you. This is not merely a cache optimization: the official client always sends exactly 4 system blocks, and anything beyond that is treated upstream as a third-party app and billed to extra usage instead of your plan, so surplus blocks are merged back into the fourth. Requests are forwarded unchanged when no split point can be identified.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="cache_scope_global"
          label={t('基座缓存跨账号共享', 'Share base-prompt cache across accounts')}
          summary={t(
            '给官方基座那块标 scope:"global"，让所有账号共用同一份基座缓存。',
            'Mark the official base prompt block with scope:"global" so every account shares one cached copy.',
          )}
          requires={{
            key: 'merge_beta',
            label: t('协议与请求头 · Beta 标记', 'Protocol & request headers · Beta flags'),
          }}
          description={
            <>
              {t(
                '基座是按模型族固定的官方提示词，全网同一份，标记后跨账号命中同一份缓存，省下重复的写入。注意：官方客户端总是把 scope 和 ttl:1h 一起发，而 luban 不写 ttl，所以发出去的是官方不产生的组合；介意形态贴合度就关掉，代价是每个账号各写各的基座缓存。该标记需要上游的 prompt-caching-scope beta，故依赖「Beta 标记」开关。',
                'The base prompt is a fixed official block per model family — identical everywhere — so marking it lets all accounts hit one cached copy instead of each paying its own cache write. Note: the official client always sends scope together with ttl:1h, and luban does not write ttl, so the emitted combination is one the official client never produces. Turn this off if you care about exact shape fidelity; the cost is a separate base-prompt cache write per account. The marker requires the upstream prompt-caching-scope beta, hence the dependency on "Beta flags".',
              )}
            </>
          }
        />
      </SettingsGroup>

      <SettingsGroup icon={TerminalIcon} title={t('非官方客户端', 'Third-party clients')}>
        <ForwardingToggle
          k="simulate_cc"
          label={t('模拟 Claude Code', 'Emulate Claude Code')}
          summary={t(
            '让 SDK 和第三方客户端按 Claude Code 请求形态转发。',
            'Forward SDK and third-party client requests in the Claude Code request format.',
          )}
          requires={{
            key: 'merge_beta',
            label: t('协议与请求头 · Beta 标记', 'Protocol & request headers · Beta flags'),
          }}
          description={
            <>
              {t(
                '仅改写非 Claude Code 请求。开启后会增加系统提示词和客户端请求头，可能提高 Token 成本并改变输出风格。此类请求通常没有设备身份，需先关闭「设备身份校验」。',
                'Only non-Claude Code requests are rewritten. Enabling this adds a system prompt and client request headers, which may increase Token costs and change the output style. These requests usually have no device identity, so disable “Device identity checks” first.',
              )}
            </>
          }
        />
      </SettingsGroup>

      <SettingsGroup icon={RefreshCwIcon} title={t('限流与错误恢复', 'Rate limits & error recovery')}>
        <ForwardingToggle
          k="rate_limit_retry"
          label={t('429 自动换号', '429 automatic account switching')}
          summary={t(
            '遇到限流后，冷却受限账号或模型，并换用其他账号重试。',
            'After a rate limit, cool down the affected account or model and retry with another account.',
          )}
          description={
            <>
              {t(
                '账号额度耗尽时冷却整个账号；只有当前模型受限时仅冷却该模型。默认分别冷却 60 / 30 秒，并优先采用上游等待时间。换号会改绑有设备身份的请求，也可能降低缓存命中率；达到重试上限或没有其他账号时返回',
                'When an account’s quota is exhausted, the entire account is cooled down; when only the current model is limited, only that model is cooled down. The defaults are 60 / 30 seconds respectively, with the upstream wait time taking precedence. Switching accounts rebinds requests that carry a device identity and may also reduce the cache hit rate. When the retry limit is reached or no other account is available, return',
              )}{' '}
              <code className="font-mono tabular-nums">429</code>{t('。', '.')}
            </>
          }
        />
        <RetryMax />
        <ForwardingToggle
          k="thinking_signature_retry"
          label={t('thinking 签名兜底', 'thinking signature fallback')}
          summary={t(
            '账号切换导致历史 thinking 签名失效时，自动降级并重试一次。',
            'When switching accounts invalidates a historical thinking signature, automatically downgrade it and retry once.',
          )}
          description={
            <>
              {t('无法验证的历史', 'Unverifiable historical')}{' '}
              <code className="font-mono">thinking</code>{' '}
              {t(
                '会转为普通文本，并用同一账号重试一次，不会删除原内容。工具续跑仍可能失败；重试会增加一次请求成本，失败时返回原始 400。',
                'content is converted to plain text and retried once with the same account; the original content is not deleted. Tool continuation may still fail. The retry adds the cost of one request, and a failure returns the original 400 response.',
              )}
            </>
          }
        />
      </SettingsGroup>
    </div>
  )
}

/** 429 后追加尝试的账号数（不含首次请求；0 = 不重试；后端限制在 0~10）。 */
function RetryMax() {
  const { language, t } = useI18n()
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
        title: t('429 重试策略已更新', '429 retry policy updated'),
        description: settings.rate_limit_retry_max > 0
          ? t(
              `最多追加尝试 ${settings.rate_limit_retry_max} 个账号。`,
              `Try up to ${settings.rate_limit_retry_max} additional ${settings.rate_limit_retry_max === 1 ? 'account' : 'accounts'}.`,
            )
          : t(
              '429 将直接透传，不冷却、不换号。',
              '429 responses will pass through unchanged, without cooldown or account switching.',
            ),
        type: 'success',
      })
      qc.setQueryData(['settings'], settings)
    },
    onError: (error) => {
      toastManager.add({
        title: t('保存失败', 'Save failed'),
        description: extractError(error, language),
        type: 'error',
      })
    },
  })

  const count = Math.min(10, Math.max(0, Math.floor(draft ?? 0)))
  const enabled = data?.rate_limit_retry ?? true

  return (
    <Field className="p-5">
      <div className="flex w-full flex-wrap items-end justify-between gap-3">
        <div className="min-w-0 space-y-1">
          <FieldLabel>{t('追加重试账号数', 'Additional retry accounts')}</FieldLabel>
          <FieldDescription>
            {t(
              '填 2 时最多尝试 3 个账号（含首次）。',
              'Set this to 2 to try up to 3 accounts in total, including the first.',
            )}
          </FieldDescription>
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
              <NumberFieldDecrement
                aria-label={t('减少 429 追加重试账号数', 'Decrease additional accounts retried after 429')}
              />
              <NumberFieldInput
                aria-label={t('429 追加重试账号数', 'Additional accounts to retry after 429')}
              />
              <NumberFieldIncrement
                aria-label={t('增加 429 追加重试账号数', 'Increase additional accounts retried after 429')}
              />
            </NumberFieldGroup>
          </NumberField>
          <Button
            size="sm"
            loading={save.isPending}
            disabled={!enabled || count === (data?.rate_limit_retry_max ?? 2)}
            onClick={() => save.mutate(count)}
          >
            <SaveIcon />
            {t('保存', 'Save')}
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
  requires,
}: {
  k: ForwardingKey
  label: string
  summary: string
  description?: ReactNode
  /**
   * 依赖的前置开关：它关着时本项即便存着「开」也不会生效（后端同样这么判），
   * 故置灰并改写副标题，把这层依赖摆到界面上——否则就是个拨得动、却一动不动的开关。
   * 存储值不动，前置开关一开回来，本项还是原来那个状态。
   */
  requires?: { key: ForwardingKey; label: string }
}) {
  const { language, t } = useI18n()
  const id = useId()
  const qc = useQueryClient()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const enabled = data?.[k] ?? true
  const blocked = requires != null && data?.[requires.key] === false

  const save = useMutation({
    mutationFn: (next: boolean) => setForwarding(k, next),
    onSuccess: (settings: Settings) => {
      toastManager.add({
        title: settings[k]
          ? t(`${label}已开启`, `${label} enabled`)
          : t(`${label}已关闭`, `${label} disabled`),
        description: summary,
        type: 'success',
      })
      qc.setQueryData(['settings'], settings)
      qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (error) => {
      toastManager.add({
        title: t('保存失败', 'Save failed'),
        description: extractError(error, language),
        type: 'error',
      })
    },
  })

  return (
    <Field className="p-5" disabled={blocked}>
      <div className="flex w-full items-start justify-between gap-4">
        <div className="min-w-0 space-y-1">
          <FieldLabel htmlFor={id}>{label}</FieldLabel>
          <FieldDescription className="leading-5">
            {blocked
              ? t(`需先开启「${requires.label}」`, `Enable “${requires.label}” first`)
              : summary}
          </FieldDescription>
        </div>
        <Switch
          id={id}
          checked={enabled && !blocked}
          disabled={save.isPending || blocked}
          onCheckedChange={(next) => save.mutate(next)}
        />
      </div>
      {description && (
        <details className="group text-xs text-muted-foreground">
          <summary className="flex w-fit cursor-pointer list-none items-center gap-1.5 rounded-sm font-medium transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring [&::-webkit-details-marker]:hidden">
            {t('影响与限制', 'Impact & limitations')}
            <ChevronDownIcon
              aria-hidden="true"
              className="size-3 transition-transform group-open:rotate-180"
            />
          </summary>
          <div className="mt-2 border-l-2 border-border pl-3 leading-5 [&_code]:rounded-sm [&_code]:bg-muted [&_code]:px-1 [&_code]:py-0.5 [&_code]:text-foreground">
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
