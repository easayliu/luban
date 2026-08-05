import { useEffect, useRef, useState, type ReactNode } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  CheckIcon,
  ClipboardIcon,
  EyeIcon,
  EyeOffIcon,
  KeyRoundIcon,
  SaveIcon,
  Settings2Icon,
  SparklesIcon,
  Trash2Icon,
} from 'lucide-react'
import {
  getSettings,
  setApiKey,
  setBareRateLimit,
  setDefaultDeviceLimit,
  setDeviceRetention,
  setDeviceTtl,
  setRequireDeviceId,
  type Settings,
} from '@/api/settings'
import { changePassword, getAuthState, setup as setupPassword } from '@/api/auth'
import { clearPw, setPw } from '@/api/client'
import { useI18n } from '@/lib/i18n'
import { copyText, extractError, formatDuration } from '@/lib/utils'
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
import { Button, type ButtonProps } from '@/components/ui/button'
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
import { Input } from '@/components/ui/input'
import {
  InputGroup,
  InputGroupAddon,
  InputGroupInput,
} from '@/components/ui/input-group'
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

export function AccessSettings({
  open,
  onOpenChange,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const { t } = useI18n()

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogPopup className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Settings2Icon aria-hidden="true" />
            {t('接入与安全', 'Access & security')}
          </DialogTitle>
          <DialogDescription>
            {t(
              '配置客户端接入、设备策略和控制台安全。',
              'Configure client access, device policies, and console security.',
            )}
          </DialogDescription>
        </DialogHeader>
        <DialogPanel>
          <AccessSettingsContent />
        </DialogPanel>
      </DialogPopup>
    </Dialog>
  )
}

export function AccessSettingsContent() {
  const qc = useQueryClient()
  const { language, t } = useI18n()
  const settingsQuery = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const { data } = settingsQuery

  const [draft, setDraft] = useState('')
  const [show, setShow] = useState(false)
  const [revealedSnippetKey, setRevealedSnippetKey] = useState<string | null>(null)
  const [clearKeyOpen, setClearKeyOpen] = useState(false)

  useEffect(() => {
    setDraft(data?.api_key ?? '')
    setShow(false)
    setRevealedSnippetKey(null)
  }, [data?.api_key])

  const save = useMutation({
    mutationFn: (key: string) => setApiKey(key),
    onSuccess: (settings: Settings) => {
      setClearKeyOpen(false)
      toastManager.add({
        title: settings.api_key
          ? t('接入 Key 已保存', 'Access key saved')
          : t('接入 Key 已清除', 'Access key cleared'),
        description: settings.api_key
          ? t('新的客户端接入 Key 已生效。', 'The new client access key is now active.')
          : t('代理将不再校验来访客户端。', 'The proxy will no longer authenticate incoming clients.'),
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

  const baseUrl = window.location.origin
  const envManaged = data?.env_managed ?? false
  const currentKey = data?.api_key ?? ''
  const showSnippetKey = currentKey !== '' && revealedSnippetKey === currentKey

  const generate = () => {
    const bytes = new Uint8Array(24)
    crypto.getRandomValues(bytes)
    const hex = Array.from(bytes).map((byte) => byte.toString(16).padStart(2, '0')).join('')
    setDraft(`luban-${hex}`)
    setShow(true)
  }

  const snippet =
    `export ANTHROPIC_BASE_URL=${baseUrl}\n` +
    (currentKey
      ? `export ANTHROPIC_AUTH_TOKEN=${currentKey}`
      : t(
          '# 未设置 Key，无需 ANTHROPIC_AUTH_TOKEN',
          '# No key configured; ANTHROPIC_AUTH_TOKEN is not required',
        ))
  const visibleSnippet = currentKey && !showSnippetKey
    ? `export ANTHROPIC_BASE_URL=${baseUrl}\nexport ANTHROPIC_AUTH_TOKEN=${t('[已隐藏]', '[hidden]')}`
    : snippet
  const snippetCopyLabel = currentKey
    ? t('复制完整接入片段（含 Key）', 'Copy the full setup snippet (includes the key)')
    : t('复制接入片段', 'Copy setup snippet')
  const snippetCopyErrorDescription = currentKey && !showSnippetKey
    ? t(
        '复制失败；请先显示 Key，再手动选择完整片段。',
        'Copy failed; reveal the key before selecting the full snippet manually.',
      )
    : t(
        '复制失败；请手动选择并复制接入片段。',
        'Copy failed; select and copy the setup snippet manually.',
      )

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
          {t('无法读取当前设置', 'Unable to load the current settings')}
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
    <>
      <div className="space-y-4">
        <SettingsSection title={t('客户端接入', 'Client access')}>
          <Field className="p-5">
            <FieldLabel>
              {t('接入地址', 'Access URL')}
              <code className="font-mono text-xs font-normal text-muted-foreground">ANTHROPIC_BASE_URL</code>
            </FieldLabel>
            <InputGroup>
              <InputGroupInput
                aria-label={t('接入地址', 'Access URL')}
                readOnly
                value={baseUrl}
              />
              <InputGroupAddon align="inline-end">
                <CopyButton
                  text={baseUrl}
                  label={t('复制接入地址', 'Copy access URL')}
                  copiedLabel={t('已复制接入地址', 'Access URL copied')}
                />
              </InputGroupAddon>
            </InputGroup>
          </Field>

          <Field className="p-5">
            <FieldLabel>
              {t('接入 Key', 'Access key')}
              <code className="font-mono text-xs font-normal text-muted-foreground">ANTHROPIC_AUTH_TOKEN</code>
            </FieldLabel>
            <InputGroup>
              <InputGroupInput
                aria-label={t('接入 Key', 'Access key')}
                onChange={(event) => setDraft(event.target.value)}
                placeholder={envManaged ? '' : t('留空则不校验来访', 'Leave blank to disable client authentication')}
                readOnly={envManaged}
                type={show ? 'text' : 'password'}
                value={draft}
              />
              <InputGroupAddon className="gap-4" align="inline-end">
                <Button
                  aria-label={show
                    ? t('隐藏接入 Key', 'Hide access key')
                    : t('显示接入 Key', 'Show access key')}
                  size="icon-sm"
                  title={show ? t('隐藏', 'Hide') : t('显示', 'Show')}
                  variant="ghost"
                  onClick={() => setShow((visible) => !visible)}
                >
                  {show ? <EyeOffIcon /> : <EyeIcon />}
                </Button>
                <CopyButton
                  text={draft}
                  label={t('复制接入 Key', 'Copy access key')}
                  copiedLabel={t('已复制接入 Key', 'Access key copied')}
                  size="icon-sm"
                />
              </InputGroupAddon>
            </InputGroup>
            {!envManaged && (
              <div className="flex flex-wrap items-center gap-2">
                <Button size="sm" variant="outline" onClick={generate}>
                  <SparklesIcon />
                  {t('生成', 'Generate')}
                </Button>
                <Button
                  size="sm"
                  loading={save.isPending}
                  disabled={draft === currentKey}
                  onClick={() => save.mutate(draft.trim())}
                >
                  <SaveIcon />
                  {t('保存', 'Save')}
                </Button>
                {currentKey && (
                  <Button
                    size="sm"
                    variant="destructive-outline"
                    onClick={() => setClearKeyOpen(true)}
                  >
                    <Trash2Icon />
                    {t('清空', 'Clear')}
                  </Button>
                )}
              </div>
            )}
            {envManaged && (
              <FieldDescription>
                {t('由环境变量', 'Managed by environment variable')}{' '}
                <code className="font-mono">LUBAN_API_KEY</code>
                {t(' 接管，网页只读。', '; this page is read-only.')}
              </FieldDescription>
            )}
          </Field>

          <Field className="p-5">
            <div className="flex w-full min-w-0 items-center justify-between gap-2">
              <FieldLabel>{t('Claude Code 接入片段', 'Claude Code setup snippet')}</FieldLabel>
              <div className="flex shrink-0 items-center gap-3">
                {currentKey && (
                  <Button
                    type="button"
                    aria-label={showSnippetKey
                      ? t('隐藏接入片段中的 Key', 'Hide the key in the setup snippet')
                      : t('显示接入片段中的 Key', 'Show the key in the setup snippet')}
                    size="icon"
                    title={showSnippetKey
                      ? t('隐藏 Key', 'Hide key')
                      : t('显示 Key', 'Show key')}
                    variant="ghost"
                    onClick={() => setRevealedSnippetKey((revealed) => (
                      revealed === currentKey ? null : currentKey
                    ))}
                  >
                    {showSnippetKey ? <EyeOffIcon /> : <EyeIcon />}
                  </Button>
                )}
                <CopyButton
                  text={snippet}
                  label={snippetCopyLabel}
                  copiedLabel={t('已复制接入片段', 'Setup snippet copied')}
                  copyErrorDescription={snippetCopyErrorDescription}
                  size="icon"
                />
              </div>
            </div>
            <pre className="max-w-full overflow-x-auto rounded-lg border bg-muted/72 p-3 font-mono text-xs leading-5">
              {visibleSnippet}
            </pre>
            {currentKey && (
              <FieldDescription>
                {t(
                  '为避免截图或录屏泄露，Key 默认隐藏；显示或复制都需要主动操作。',
                  'The key stays hidden by default to prevent screenshot or screen-recording leaks; revealing or copying it requires an explicit action.',
                )}
              </FieldDescription>
            )}
          </Field>
        </SettingsSection>

        <SettingsSection title={t('设备策略', 'Device policies')}>
          {data && <DevicePolicyOverview settings={data} />}
          <DevicePolicyGroup
            title={t('绑定与容量', 'Bindings & capacity')}
            description={t(
              '决定设备占用名额多久、多久内优先返回原账号，以及每个账号默认可容纳多少设备。',
              'Control how long devices hold slots, how long they prefer their original account, and the default capacity per account.',
            )}
          />
          <DeviceBindingTtl />
          <DeviceBindingRetention />
          <DefaultDeviceLimit />
          <DevicePolicyGroup
            title={t('身份与防滥用', 'Identity & abuse prevention')}
            description={t(
              '控制无有效设备身份的请求是直接拒绝，还是限速后放行。',
              'Choose whether requests without a valid device identity are rejected or allowed with rate limiting.',
            )}
          />
          <RequireDeviceIdToggle />
          <BareRateLimit />
        </SettingsSection>

        <SettingsSection title={t('控制台安全', 'Console security')}>
          <AdminPassword />
        </SettingsSection>
      </div>

      <AlertDialog
        open={clearKeyOpen}
        onOpenChange={(nextOpen) => {
          if (!save.isPending) setClearKeyOpen(nextOpen)
        }}
      >
        <AlertDialogPopup className="sm:max-w-md">
          <AlertDialogHeader>
            <AlertDialogTitle>{t('清除接入 Key', 'Clear access key')}</AlertDialogTitle>
            <AlertDialogDescription>
              {t(
                '清除后，代理将不再校验客户端身份。',
                'After clearing it, the proxy will no longer authenticate clients.',
              )}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogClose render={<Button disabled={save.isPending} variant="ghost" />}>
              {t('取消', 'Cancel')}
            </AlertDialogClose>
            <Button
              loading={save.isPending}
              variant="destructive"
              onClick={() => save.mutate('')}
            >
              {t('确认清除', 'Clear key')}
            </Button>
          </AlertDialogFooter>
        </AlertDialogPopup>
      </AlertDialog>
    </>
  )
}

function DevicePolicyOverview({ settings }: { settings: Settings }) {
  const { language, locale, t } = useI18n()
  const bareRequestPolicy = settings.require_device_id
    ? t('直接拒绝', 'Rejected')
    : settings.bare_rate_limit > 0
      ? t(
          `${settings.bare_rate_limit.toLocaleString(locale)} 条 / ${formatDuration(settings.bare_rate_window_secs, language)}`,
          `${settings.bare_rate_limit.toLocaleString(locale)} / ${formatDuration(settings.bare_rate_window_secs, language)}`,
        )
      : t('允许 · 不限速', 'Allowed · unlimited')
  const items = [
    {
      label: t('名额有效期', 'Slot lifetime'),
      value: settings.device_binding_ttl_secs > 0
        ? formatDuration(settings.device_binding_ttl_secs, language)
        : t('不自动释放', 'Never released'),
    },
    {
      label: t('原账号关联', 'Account affinity'),
      value: settings.device_binding_retention_secs > 0
        ? formatDuration(settings.device_binding_retention_secs, language)
        : t('永久保留', 'Kept forever'),
    },
    {
      label: t('默认容量', 'Default capacity'),
      value: settings.default_device_limit > 0
        ? t(
            `${settings.default_device_limit.toLocaleString(locale)} 台 / 账号`,
            `${settings.default_device_limit.toLocaleString(locale)} / account`,
          )
        : t('不限', 'Unlimited'),
    },
    {
      label: t('无身份请求', 'Unidentified requests'),
      value: bareRequestPolicy,
    },
  ]

  return (
    <section
      aria-label={t('当前设备策略概览', 'Current device policy overview')}
      className="grid grid-cols-2 gap-3 bg-muted/24 p-5 sm:grid-cols-4"
    >
      {items.map((item) => (
        <div key={item.label} className="min-w-0 rounded-lg border bg-background px-3 py-2.5 shadow-xs/5">
          <p className="text-xs text-muted-foreground">{item.label}</p>
          <p className="mt-1 truncate font-semibold text-sm" title={item.value}>{item.value}</p>
        </div>
      ))}
    </section>
  )
}

function DevicePolicyGroup({ title, description }: { title: string; description: string }) {
  return (
    <div className="bg-muted/32 px-5 py-3.5">
      <h3 className="font-semibold text-sm">{title}</h3>
      <p className="mt-1 max-w-2xl text-xs leading-5 text-muted-foreground">{description}</p>
    </div>
  )
}

const SECS_PER_HOUR = 3600

/** 秒 → 小时，保留足以原样还原秒数的最短小数位。 */
function toHours(secs: number): number {
  const exact = secs / SECS_PER_HOUR
  for (const digits of [0, 1, 2, 3]) {
    const rounded = Number(exact.toFixed(digits))
    if (Math.round(rounded * SECS_PER_HOUR) === secs) return rounded
  }
  return exact
}

/** 小时 → 秒；空值与负数按 0 = 不自动释放。 */
function hoursToSecs(hours: number | null): number {
  return Math.max(0, Math.round((hours ?? 0) * SECS_PER_HOUR))
}

/** 设备绑定有效期：设备超过该时长无请求即释放名额（绑定本身按保留期留着）。0 = 永不过期。 */
function DeviceBindingTtl() {
  const qc = useQueryClient()
  const { language, t } = useI18n()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const [draft, setDraft] = useState<number | null>(null)

  useEffect(() => {
    if (data) setDraft(toHours(data.device_binding_ttl_secs))
  }, [data?.device_binding_ttl_secs])

  const save = useMutation({
    mutationFn: (seconds: number) => setDeviceTtl(seconds),
    onSuccess: (settings: Settings) => {
      toastManager.add({
        title: t('设备策略已更新', 'Device policy updated'),
        description: t('设备绑定有效期已保存。', 'The device binding lifetime has been saved.'),
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

  const current = data?.device_binding_ttl_secs ?? 0
  const parsed = hoursToSecs(draft)
  const hint = parsed > 0
    ? t(
        `闲置${formatDuration(parsed, language)}后释放名额`,
        `Releases the slot after ${formatDuration(parsed, language)} idle`,
      )
    : t('名额不自动释放', 'Slots are not released automatically')

  return (
    <Field className="p-5">
      <FieldLabel>{t('活跃名额有效期（小时）', 'Active slot lifetime (hours)')}</FieldLabel>
      <div className="flex w-full flex-wrap items-center gap-2">
        <NumberField
          className="w-full sm:w-44"
          min={0}
          step={1}
          smallStep={0.5}
          value={draft}
          onValueChange={setDraft}
        >
          <NumberFieldGroup>
            <NumberFieldDecrement
              aria-label={t('减少活跃名额有效期', 'Decrease active slot lifetime')}
            />
            <NumberFieldInput
              aria-label={t('活跃名额有效期（小时）', 'Active slot lifetime in hours')}
            />
            <NumberFieldIncrement
              aria-label={t('增加活跃名额有效期', 'Increase active slot lifetime')}
            />
          </NumberFieldGroup>
        </NumberField>
        <Button
          size="sm"
          loading={save.isPending}
          disabled={parsed === current}
          onClick={() => save.mutate(parsed)}
        >
          <SaveIcon />
          {t('保存', 'Save')}
        </Button>
        <span className="text-xs text-muted-foreground">{hint}</span>
      </div>
      <FieldDescription>
        {t(
          '设备在此时长内没有请求便释放占用的账号名额；与原账号的关联仍按下方保留期保存。',
          'A device releases its account slot after this much inactivity; its affinity with the original account is retained for the period below.',
        )}
      </FieldDescription>
    </Field>
  )
}

const SECS_PER_DAY = 86400

/**
 * 秒 → 天，取能**原样还原**的最短小数位。
 *
 * 604800 给 7、43200 给 0.5；而 3600 这种不是整天数的（只可能来自接口直接改的库）
 * 任何短写法都还原不回去，就照实给完整小数——宁可难看，也不能让用户一按保存就把
 * 一个自己没动过的值悄悄改掉。
 */
function toDays(secs: number): number {
  const exact = secs / SECS_PER_DAY
  for (const digits of [0, 1, 2, 3]) {
    const rounded = Number(exact.toFixed(digits))
    if (Math.round(rounded * SECS_PER_DAY) === secs) return rounded
  }
  return exact
}

/** 天 → 秒（接口收的单位）；空值与负数一律按 0 = 永久保留。 */
function toSecs(days: number | null): number {
  return Math.max(0, Math.round((days ?? 0) * SECS_PER_DAY))
}

/**
 * 软绑定保留期：绑定超过有效期后不再占名额，但在这段时间内设备再来仍优先回原账号
 * （原账号还得有空位）。0 = 永久保留。
 */
function DeviceBindingRetention() {
  const qc = useQueryClient()
  const { language, t } = useI18n()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  // 按天填：保留期是「几天几周」这个量级的，写成 604800 秒没人一眼看得出来。
  // 接口仍收秒，天数只是这一格的输入单位（允许小数，0.5 天 = 12 小时）。
  const [draft, setDraft] = useState<number | null>(null)

  useEffect(() => {
    if (data) setDraft(toDays(data.device_binding_retention_secs))
  }, [data?.device_binding_retention_secs])

  const save = useMutation({
    mutationFn: (seconds: number) => setDeviceRetention(seconds),
    onSuccess: (settings: Settings) => {
      toastManager.add({
        title: t('设备策略已更新', 'Device policy updated'),
        description: t('原账号关联保留期已保存。', 'The account affinity retention has been saved.'),
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

  const current = data?.device_binding_retention_secs ?? 0
  const ttl = data?.device_binding_ttl_secs ?? 0
  const parsed = toSecs(draft)
  const hint = parsed > 0
    ? t(
        `优先返回原账号：${formatDuration(parsed, language)}`,
        `Prefer the original account for ${formatDuration(parsed, language)}`,
      )
    : t('永久优先返回原账号', 'Always prefer the original account')
  // 保留期短于有效期是自相矛盾的配置，后端会按有效期兜底（等于关掉软绑定），这里先提示一句。
  const conflict = parsed > 0 && ttl > 0 && parsed < ttl

  return (
    <Field className="p-5">
      <FieldLabel>{t('原账号关联保留期（天）', 'Account affinity retention (days)')}</FieldLabel>
      <div className="flex w-full flex-wrap items-center gap-2">
        <NumberField
          className="w-full sm:w-44"
          min={0}
          step={1}
          smallStep={0.5}
          value={draft}
          onValueChange={setDraft}
        >
          <NumberFieldGroup>
            <NumberFieldDecrement
              aria-label={t('减少原账号关联保留期', 'Decrease account affinity retention')}
            />
            <NumberFieldInput
              aria-label={t('原账号关联保留期（天）', 'Account affinity retention in days')}
            />
            <NumberFieldIncrement
              aria-label={t('增加原账号关联保留期', 'Increase account affinity retention')}
            />
          </NumberFieldGroup>
        </NumberField>
        <Button
          size="sm"
          loading={save.isPending}
          disabled={parsed === current}
          onClick={() => save.mutate(parsed)}
        >
          <SaveIcon />
          {t('保存', 'Save')}
        </Button>
        <span className="text-xs text-muted-foreground">{hint}</span>
      </div>
      <FieldDescription>
        {conflict
          ? t(
              '保留期短于有效期时按有效期处理，等于关闭软绑定。',
              'A retention shorter than the lifetime is treated as the lifetime, which effectively disables soft binding.',
            )
          : t(
              '名额释放后，设备在此期限内回来仍优先使用原账号，减少 thinking 签名跨账号导致的降级重试。',
              'After its slot is released, a returning device still prefers its original account during this period, reducing fallback retries caused by account-bound thinking signatures.',
            )}
      </FieldDescription>
    </Field>
  )
}

/** 全局默认设备上限：账号未单独配置时套用。 */
function DefaultDeviceLimit() {
  const qc = useQueryClient()
  const { language, t } = useI18n()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const [draft, setDraft] = useState<number | null>(null)

  useEffect(() => {
    if (data) setDraft(data.default_device_limit)
  }, [data?.default_device_limit])

  const save = useMutation({
    mutationFn: (limit: number) => setDefaultDeviceLimit(limit),
    onSuccess: (settings: Settings) => {
      toastManager.add({
        title: t('默认设备上限已更新', 'Default device limit updated'),
        description: settings.default_device_limit > 0
          ? t(
              `每个账号最多绑定 ${settings.default_device_limit} 台设备。`,
              `Each account can bind up to ${settings.default_device_limit} ${settings.default_device_limit === 1 ? 'device' : 'devices'}.`,
            )
          : t('默认设备上限已取消。', 'The default device limit has been removed.'),
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

  const current = data?.default_device_limit ?? 0
  const parsed = Math.max(0, Math.floor(draft ?? 0))

  return (
    <Field className="p-5">
      <FieldLabel>{t('默认设备上限（每个账号）', 'Default device limit (per account)')}</FieldLabel>
      <div className="flex w-full flex-wrap items-center gap-2">
        <NumberField
          className="w-full sm:w-44"
          min={0}
          value={draft}
          onValueChange={setDraft}
        >
          <NumberFieldGroup>
            <NumberFieldDecrement
              aria-label={t('减少默认设备上限', 'Decrease default device limit')}
            />
            <NumberFieldInput aria-label={t('默认设备上限', 'Default device limit')} />
            <NumberFieldIncrement
              aria-label={t('增加默认设备上限', 'Increase default device limit')}
            />
          </NumberFieldGroup>
        </NumberField>
        <Button
          size="sm"
          loading={save.isPending}
          disabled={parsed === current}
          onClick={() => save.mutate(parsed)}
        >
          <SaveIcon />
          {t('保存', 'Save')}
        </Button>
        <span className="text-xs text-muted-foreground">
          {parsed > 0
            ? t(
                `每个账号最多 ${parsed} 台设备`,
                `Up to ${parsed} ${parsed === 1 ? 'device' : 'devices'} per account`,
              )
            : t('不限（不设默认上限）', 'Unlimited (no default limit)')}
        </span>
      </div>
      <FieldDescription>
        {t(
          '未单独配置的账号使用此上限；账号独立设置优先。',
          'Accounts without an individual limit use this value; account-specific settings take priority.',
        )}
      </FieldDescription>
    </Field>
  )
}

/** 设备身份校验开关：关掉后放行无 metadata.user_id 的裸请求。 */
function RequireDeviceIdToggle() {
  const qc = useQueryClient()
  const { language, t } = useI18n()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const required = data?.require_device_id ?? true

  const save = useMutation({
    mutationFn: (next: boolean) => setRequireDeviceId(next),
    onSuccess: (settings: Settings) => {
      toastManager.add({
        title: settings.require_device_id
          ? t('设备身份校验已开启', 'Device identity checks enabled')
          : t('设备身份校验已关闭', 'Device identity checks disabled'),
        description: settings.require_device_id
          ? t('缺少设备身份的请求会被拒绝。', 'Requests without a device identity will be rejected.')
          : t('无设备身份的请求将被放行。', 'Requests without a device identity will be allowed.'),
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

  return (
    <Field className="p-5">
      <div className="flex w-full items-start justify-between gap-4">
        <div className="min-w-0 space-y-1">
          <div className="flex flex-wrap items-center gap-2">
            <FieldLabel htmlFor="require-device-id">
              {t('设备身份校验', 'Device identity checks')}
            </FieldLabel>
            <Badge variant={required ? 'success' : 'warning'} size="sm" aria-live="polite">
              {required ? t('严格模式', 'Strict') : t('兼容模式', 'Compatible')}
            </Badge>
          </div>
          <FieldDescription>
            {required
              ? t(
                  '缺少设备身份的请求会被拒绝。',
                  'Requests without a device identity will be rejected.',
                )
              : t(
                  '无设备身份的请求将被放行，且不受设备上限限制。',
                  'Requests without a device identity will be allowed and will not count toward device limits.',
                )}
          </FieldDescription>
        </div>
        <Switch
          id="require-device-id"
          checked={required}
          disabled={save.isPending}
          onCheckedChange={(next) => save.mutate(next)}
        />
      </div>
    </Field>
  )
}

/** 裸请求速率上限：单个账号在窗口内最多接收的无设备身份请求。 */
function BareRateLimit() {
  const qc = useQueryClient()
  const { language, t } = useI18n()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const [draft, setDraft] = useState<number | null>(null)
  const [windowDraft, setWindowDraft] = useState<number | null>(null)

  useEffect(() => {
    if (data) {
      setDraft(data.bare_rate_limit)
      setWindowDraft(data.bare_rate_window_secs)
    }
  }, [data?.bare_rate_limit, data?.bare_rate_window_secs])

  const save = useMutation({
    mutationFn: ({ limit, win }: { limit: number; win: number }) => setBareRateLimit(limit, win),
    onSuccess: (settings: Settings) => {
      toastManager.add({
        title: t(
          '无设备身份请求策略已更新',
          'Request policy for missing device identities updated',
        ),
        description: settings.bare_rate_limit > 0
          ? t(
              `每个账号 ${settings.bare_rate_limit} 条 / ${formatDuration(settings.bare_rate_window_secs, language)}。`,
              `${settings.bare_rate_limit} ${settings.bare_rate_limit === 1 ? 'request' : 'requests'} per account / ${formatDuration(settings.bare_rate_window_secs, language)}.`,
            )
          : t(
              '裸请求速率限制已取消。',
              'The rate limit for requests without a device identity has been removed.',
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

  const limit = Math.max(0, Math.floor(draft ?? 0))
  const win = Math.max(1, Math.floor(windowDraft ?? 60))
  const inactive = data?.require_device_id ?? true
  const unchanged = limit === (data?.bare_rate_limit ?? 0)
    && win === (data?.bare_rate_window_secs ?? 60)

  return (
    <div className="space-y-3 p-5">
      <div>
        <div className="flex flex-wrap items-center gap-2">
          <p className="font-medium text-sm">
            {t(
              '无设备身份请求上限（每个账号）',
              'Request limit for requests without a device identity (per account)',
            )}
          </p>
          <Badge variant={inactive ? 'secondary' : 'info'} size="sm">
            {inactive ? t('当前不生效', 'Inactive') : t('正在生效', 'Active')}
          </Badge>
        </div>
        <p className="mt-1 text-xs text-muted-foreground">
          {inactive
            ? t(
                '设备身份校验已开启，无身份请求会先被拒绝；切换到兼容模式后此限制才会生效。',
                'Identity checks are enabled, so unidentified requests are rejected first; this limit takes effect in compatible mode.',
              )
            : t(
                '限制兼容模式下放行的无身份请求，0 表示不限速。',
                'Limits unidentified requests allowed in compatible mode; 0 means unlimited.',
              )}
        </p>
      </div>
      <div className="grid gap-3 sm:grid-cols-2">
        <Field>
          <FieldLabel>{t('请求数', 'Request count')}</FieldLabel>
          <NumberField disabled={inactive} min={0} value={draft} onValueChange={setDraft}>
            <NumberFieldGroup>
              <NumberFieldDecrement
                aria-label={t(
                  '减少无设备身份请求上限',
                  'Decrease the request limit for requests without a device identity',
                )}
              />
              <NumberFieldInput
                aria-label={t(
                  '无设备身份请求上限（条）',
                  'Request limit for requests without a device identity',
                )}
              />
              <NumberFieldIncrement
                aria-label={t(
                  '增加无设备身份请求上限',
                  'Increase the request limit for requests without a device identity',
                )}
              />
            </NumberFieldGroup>
          </NumberField>
        </Field>
        <Field>
          <FieldLabel>{t('时间窗口（秒）', 'Time window (seconds)')}</FieldLabel>
          <NumberField disabled={inactive} min={1} value={windowDraft} onValueChange={setWindowDraft}>
            <NumberFieldGroup>
              <NumberFieldDecrement
                aria-label={t(
                  '减少无设备身份请求时间窗口',
                  'Decrease the time window for requests without a device identity',
                )}
              />
              <NumberFieldInput
                aria-label={t(
                  '无设备身份请求窗口（秒）',
                  'Time window for requests without a device identity in seconds',
                )}
              />
              <NumberFieldIncrement
                aria-label={t(
                  '增加无设备身份请求时间窗口',
                  'Increase the time window for requests without a device identity',
                )}
              />
            </NumberFieldGroup>
          </NumberField>
        </Field>
      </div>
      <Button
        size="sm"
        loading={save.isPending}
        disabled={inactive || unchanged}
        onClick={() => save.mutate({ limit, win })}
      >
        <SaveIcon />
        {t('保存', 'Save')}
      </Button>
      <p className="text-xs leading-5 text-muted-foreground">
        {inactive
          ? t(
              '当前配置会保留，关闭设备身份校验后自动恢复使用。',
              'The current configuration is preserved and becomes active automatically when identity checks are disabled.',
            )
          : t(
              '仅统计无设备身份的消息请求，Token 计数接口不计入；单个账号达到上限后自动换号，全部达到上限才拒绝。服务重启后重新计数。',
              'Only message requests without a device identity are counted; token-counting requests are excluded. The proxy switches accounts when one reaches its limit and rejects only when every account is capped. Counters reset after a service restart.',
            )}
      </p>
    </div>
  )
}

/** 管理密码：未设置→设置；已设置→修改/清除（环境接管时只读）。 */
function AdminPassword() {
  const { language, t } = useI18n()
  const authQuery = useQuery({ queryKey: ['auth-state'], queryFn: getAuthState })
  const { data } = authQuery
  const [password, setPassword] = useState('')
  const [clearOpen, setClearOpen] = useState(false)

  const save = useMutation({
    mutationFn: async (nextPassword: string) => {
      if (data?.configured) await changePassword(nextPassword)
      else await setupPassword(nextPassword)
    },
    onSuccess: (_result, nextPassword) => {
      setClearOpen(false)
      if (nextPassword) {
        setPw(nextPassword)
        toastManager.add({
          title: t('管理密码已设置', 'Admin password set'),
          description: t('新的管理密码已生效。', 'The new admin password is now active.'),
          type: 'success',
        })
      } else {
        clearPw()
        toastManager.add({
          title: t('管理密码已清除', 'Admin password cleared'),
          description: t('控制台将不再要求登录。', 'The console will no longer require sign-in.'),
          type: 'success',
        })
      }
      window.location.reload()
    },
    onError: (error) => {
      toastManager.add({
        title: t('操作失败', 'Operation failed'),
        description: extractError(error, language),
        type: 'error',
      })
    },
  })

  const envManaged = data?.env_managed ?? false
  const configured = data?.configured ?? false

  if (authQuery.isPending) {
    return (
      <Field className="p-5">
        <FieldLabel>{t('管理密码（登录网页所需）', 'Admin password (required for sign-in)')}</FieldLabel>
        <span className="inline-flex items-center gap-1.5 text-xs text-muted-foreground" role="status">
          <Spinner className="size-3" />
          {t('正在加载', 'Loading')}
        </span>
      </Field>
    )
  }

  if (authQuery.isError) {
    return (
      <Field className="p-5">
        <FieldLabel>{t('管理密码（登录网页所需）', 'Admin password (required for sign-in)')}</FieldLabel>
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-xs text-destructive-foreground">
            {t('无法读取登录状态', 'Unable to load sign-in status')}
          </span>
          <Button
            size="sm"
            variant="outline"
            loading={authQuery.isFetching}
            onClick={() => authQuery.refetch()}
          >
            {t('重试', 'Retry')}
          </Button>
        </div>
      </Field>
    )
  }

  return (
    <>
      <Field className="p-5">
        <FieldLabel>{t('管理密码（登录网页所需）', 'Admin password (required for sign-in)')}</FieldLabel>
        {envManaged ? (
          <FieldDescription>
            {t('由环境变量', 'Managed by environment variable')}{' '}
            <code className="font-mono">LUBAN_ADMIN_PASSWORD</code>
            {t(' 接管，网页只读。', '; this page is read-only.')}
          </FieldDescription>
        ) : (
          <>
            <div className="grid w-full gap-2 sm:grid-cols-[minmax(0,1fr)_auto_auto]">
              <Input
                aria-label={configured
                  ? t('新管理密码', 'New admin password')
                  : t('管理密码', 'Admin password')}
                onChange={(event) => setPassword(event.target.value)}
                placeholder={configured
                  ? t('输入新密码', 'Enter a new password')
                  : t('至少 4 位', 'At least 4 characters')}
                type="password"
                value={password}
              />
              <Button
                size="sm"
                loading={save.isPending}
                disabled={password.trim().length < 4}
                onClick={() => save.mutate(password.trim())}
              >
                <KeyRoundIcon />
                {configured ? t('修改', 'Change') : t('设置', 'Set')}
              </Button>
              {configured && (
                <Button
                  size="sm"
                  variant="destructive-outline"
                  disabled={save.isPending}
                  onClick={() => setClearOpen(true)}
                >
                  <Trash2Icon />
                  {t('清除', 'Clear')}
                </Button>
              )}
            </div>
            {!configured && (
              <FieldDescription>
                {t(
                  '未设置密码时，任何能访问控制台的设备都无需登录；对外开放时建议设置。',
                  'Without a password, any device that can reach the console can access it without signing in. Set one if the console is publicly accessible.',
                )}
              </FieldDescription>
            )}
          </>
        )}
      </Field>

      <AlertDialog
        open={clearOpen}
        onOpenChange={(nextOpen) => {
          if (!save.isPending) setClearOpen(nextOpen)
        }}
      >
        <AlertDialogPopup className="sm:max-w-md">
          <AlertDialogHeader>
            <AlertDialogTitle>{t('清除管理密码', 'Clear admin password')}</AlertDialogTitle>
            <AlertDialogDescription>
              {t(
                '清除后，控制台将不再要求登录。',
                'After clearing it, the console will no longer require sign-in.',
              )}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogClose render={<Button disabled={save.isPending} variant="ghost" />}>
              {t('取消', 'Cancel')}
            </AlertDialogClose>
            <Button
              loading={save.isPending}
              variant="destructive"
              onClick={() => save.mutate('')}
            >
              {t('确认清除', 'Clear password')}
            </Button>
          </AlertDialogFooter>
        </AlertDialogPopup>
      </AlertDialog>
    </>
  )
}

function SettingsSection({ title, children }: { title: string; children: ReactNode }) {
  return (
    <Frame>
      <FrameHeader>
        <FrameTitle>{title}</FrameTitle>
      </FrameHeader>
      <FramePanel className="divide-y p-0">{children}</FramePanel>
    </Frame>
  )
}

function CopyButton({
  text,
  label,
  copiedLabel,
  copyErrorDescription,
  size = 'icon-xs',
}: {
  text: string
  label?: string
  copiedLabel?: string
  copyErrorDescription?: string
  size?: ButtonProps['size']
}) {
  const { t } = useI18n()
  const [copied, setCopied] = useState(false)
  const copyAttemptRef = useRef(0)
  const resetTimerRef = useRef<number | null>(null)
  const idleLabel = label ?? t('复制', 'Copy')
  const successLabel = copiedLabel ?? t('已复制', 'Copied')

  useEffect(() => {
    copyAttemptRef.current += 1
    if (resetTimerRef.current !== null) {
      window.clearTimeout(resetTimerRef.current)
      resetTimerRef.current = null
    }
    setCopied(false)

    return () => {
      copyAttemptRef.current += 1
      if (resetTimerRef.current !== null) {
        window.clearTimeout(resetTimerRef.current)
      }
    }
  }, [text])

  return (
    <>
      <Button
        type="button"
        aria-label={copied ? successLabel : idleLabel}
        className={copied ? 'text-success' : undefined}
        size={size}
        title={copied ? successLabel : idleLabel}
        variant="ghost"
        onClick={async () => {
          if (!text) return
          const attempt = ++copyAttemptRef.current
          const copiedSuccessfully = await copyText(text)
          if (attempt !== copyAttemptRef.current) return

          if (copiedSuccessfully) {
            if (resetTimerRef.current !== null) {
              window.clearTimeout(resetTimerRef.current)
            }
            setCopied(true)
            resetTimerRef.current = window.setTimeout(() => {
              setCopied(false)
              resetTimerRef.current = null
            }, 1200)
            return
          }
          toastManager.add({
            title: t('复制失败', 'Copy failed'),
            description: copyErrorDescription
              ?? t('请手动选择并复制内容。', 'Select the content and copy it manually.'),
            type: 'error',
          })
        }}
      >
        {copied ? <CheckIcon /> : <ClipboardIcon />}
      </Button>
      <span className="sr-only" role="status" aria-live="polite">
        {copied ? successLabel : ''}
      </span>
    </>
  )
}
