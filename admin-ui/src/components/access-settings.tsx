import { useEffect, useRef, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  CableIcon,
  CheckIcon,
  ClipboardIcon,
  EyeIcon,
  EyeOffIcon,
  GaugeIcon,
  KeyRoundIcon,
  LockKeyholeIcon,
  SaveIcon,
  Settings2Icon,
  ShieldCheckIcon,
  SmartphoneIcon,
  SparklesIcon,
  TerminalIcon,
  TimerIcon,
  Trash2Icon,
} from 'lucide-react'
import {
  getSettings,
  setApiKey,
  setBareRateLimit,
  setDefaultDeviceLimit,
  setDefaultRpmLimit,
  setDeviceRetention,
  setDeviceRpmLimit,
  setDeviceTtl,
  setLatestCcRelease,
  setMinClientVersion,
  setRequireDeviceId,
  setSessionConcurrencyLimit,
  setSessionRpmLimit,
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
import { SettingsGroup } from '@/components/settings-group'

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
            {t('客户端接入', 'Client access')}
          </DialogTitle>
          <DialogDescription>
            {t(
              '配置客户端接入地址、身份验证 Key 和 Claude Code 片段。',
              'Configure the client endpoint, authentication key, and Claude Code setup.',
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
        <SettingsGroup
          icon={CableIcon}
          title={t('连接与认证', 'Connection & authentication')}
          description={t(
            '复制客户端接入地址，并配置代理用于验证来访请求的 Key。',
            'Copy the client endpoint and configure the key used to authenticate incoming requests.',
          )}
        >
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
        </SettingsGroup>
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

export function DeviceSettingsContent() {
  const { t } = useI18n()
  const settingsQuery = useQuery({ queryKey: ['settings'], queryFn: getSettings })

  if (settingsQuery.isPending) {
    return (
      <div className="flex min-h-40 items-center justify-center gap-2 text-sm text-muted-foreground" role="status">
        <Spinner className="size-4" />
        {t('正在加载设备策略', 'Loading device policies')}
      </div>
    )
  }

  if (settingsQuery.isError) {
    return (
      <div className="flex min-h-40 flex-col items-center justify-center gap-3 text-center" role="alert">
        <p className="text-sm font-medium">
          {t('无法读取设备策略', 'Unable to load device policies')}
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
      <DevicePolicyOverview settings={settingsQuery.data} />

      <SettingsGroup
        icon={GaugeIcon}
        title={t('绑定与容量', 'Bindings & capacity')}
        description={t(
          '决定设备占用名额多久、多久内优先返回原账号，以及每个账号默认可容纳多少设备。',
          'Control how long devices hold slots, how long they prefer their original account, and the default capacity per account.',
        )}
      >
        <DeviceBindingTtl />
        <DeviceBindingRetention />
        <DefaultDeviceLimit />
      </SettingsGroup>

      <SettingsGroup
        icon={TimerIcon}
        title={t('转发速率', 'Request rate')}
        description={t(
          '限制单个账号、单台设备、单个会话每分钟最多转发多少条请求，以及单个会话的最大并发数。RPM 与账号列表里那列口径一致，并发上限防止 Claude Desktop 的 cache 预热脉冲打爆上游。',
          'Cap how many requests a single account, device, or session forwards per minute, and the max concurrency per session. RPM uses the same window as the account list; the concurrency cap tames Claude Desktop\'s cache-warming burst.',
        )}
      >
        <DefaultRpmLimit />
        <DeviceRpmLimit />
        <SessionRpmLimit />
        <SessionConcurrencyLimit />
      </SettingsGroup>

      <SettingsGroup
        icon={ShieldCheckIcon}
        title={t('身份与防滥用', 'Identity & abuse prevention')}
        description={t(
          '控制无有效设备身份的请求是直接拒绝，还是限速后放行。',
          'Choose whether requests without a valid device identity are rejected or allowed with rate limiting.',
        )}
      >
        <RequireDeviceIdToggle />
        <BareRateLimit />
      </SettingsGroup>

      <SettingsGroup
        icon={TerminalIcon}
        title={t('客户端版本', 'Client version')}
        description={t(
          '按 User-Agent 里自报的 claude-cli 版本：卡住过旧的 Claude Code，并把自称比官方最新版还新的客户端识别为非官方；其它客户端不受影响。',
          'Judge by the claude-cli version in the User-Agent: block outdated Claude Code builds, and treat clients claiming a version newer than the latest official release as unofficial; other clients are unaffected.',
        )}
      >
        <MinClientVersion />
        <LatestCcRelease />
      </SettingsGroup>
    </div>
  )
}

export function SecuritySettingsContent() {
  const { t } = useI18n()

  return (
    <div className="space-y-4">
      <SettingsGroup
        icon={LockKeyholeIcon}
        title={t('管理密码', 'Admin password')}
        description={t(
          '控制谁可以登录管理控制台；不会影响客户端通过代理发起请求。',
          'Control who can sign in to the admin console; this does not affect proxied client requests.',
        )}
      >
        <AdminPassword />
      </SettingsGroup>
    </div>
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
  // 三道 RPM 闸各写各的，都没配才是一句「不限」；只配一部分时只显示配了的那些。
  const rpmParts = [
    settings.default_rpm_limit > 0
      ? t(
          `账号 ${settings.default_rpm_limit.toLocaleString(locale)}`,
          `${settings.default_rpm_limit.toLocaleString(locale)}/account`,
        )
      : null,
    settings.device_rpm_limit > 0
      ? t(
          `设备 ${settings.device_rpm_limit.toLocaleString(locale)}`,
          `${settings.device_rpm_limit.toLocaleString(locale)}/device`,
        )
      : null,
    settings.session_rpm_limit > 0
      ? t(
          `会话 ${settings.session_rpm_limit.toLocaleString(locale)}`,
          `${settings.session_rpm_limit.toLocaleString(locale)}/session`,
        )
      : null,
    settings.session_concurrency_limit > 0
      ? t(
          `并发 ${settings.session_concurrency_limit}`,
          `${settings.session_concurrency_limit} concurrent`,
        )
      : null,
  ].filter(Boolean)
  // 不再缀「条 / 分钟」：RPM 这个词本身就是每分钟条数，标题已经写着，缀上只会把这格挤到换行。
  const rpmPolicy = rpmParts.length > 0 ? rpmParts.join(' · ') : t('不限', 'Unlimited')
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
      // 账号与设备两道 RPM 闸挤在同一格里：概览一行六格已经到头，再加一格窄屏会散架，
      // 而这两个值总是一起看的——「账号 30 · 设备 10」比分两格更省地方也更好读。
      label: t('RPM 上限', 'RPM limits'),
      value: rpmPolicy,
    },
    {
      label: t('无身份请求', 'Unidentified requests'),
      value: bareRequestPolicy,
    },
    {
      label: t('最低客户端版本', 'Minimum client version'),
      value: settings.min_client_version
        ? t(`${settings.min_client_version} 及以上`, `${settings.min_client_version}+`)
        : t('不限', 'Unlimited'),
    },
    {
      label: t('官方最新版本', 'Latest official release'),
      value: effectiveLatestRelease(settings),
    },
  ]

  return (
    <SettingsGroup
      icon={SmartphoneIcon}
      title={t('当前策略', 'Current policy')}
      description={t(
        '下面是当前生效的设备绑定与身份处理摘要。',
        'A summary of the device binding and identity rules currently in effect.',
      )}
    >
      {/* 窄屏两列、宽屏一行自适应列宽（auto-cols-auto）。边框类按这个布局写死的。 */}
      <dl
        aria-label={t('当前设备策略概览', 'Current device policy overview')}
        className="grid grid-cols-2 md:grid-flow-col md:auto-cols-auto"
      >
        {items.map((item, index) => (
          <div
            key={item.label}
            // 六格铺平那档把左右内边距收窄一点：省下来的宽度全给值，少一次换行。
            className={`min-w-0 px-5 py-4 md:px-4 ${index >= 2 ? 'border-t md:border-t-0' : ''} ${index % 2 === 1 ? 'border-l' : ''} ${index > 0 ? 'md:border-l' : ''}`}
          >
            <dt className="text-xs text-muted-foreground">{item.label}</dt>
            <dd className="mt-1 font-semibold text-sm leading-snug whitespace-nowrap">{item.value}</dd>
          </div>
        ))}
      </dl>
    </SettingsGroup>
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
    <Field className="grid gap-4 p-5 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-center sm:gap-x-6">
      <div className="min-w-0 space-y-1.5">
        <FieldLabel>{t('活跃名额有效期', 'Active slot lifetime')}</FieldLabel>
        <FieldDescription className="max-w-xl leading-5">
          {t(
            '设备在此时长内没有请求便释放占用的账号名额；与原账号的关联仍按保留期保存。',
            'A device releases its account slot after this much inactivity; its affinity with the original account is retained separately.',
          )}
        </FieldDescription>
        <Badge variant="secondary" size="sm">{hint}</Badge>
      </div>
      <div className="flex w-full items-center gap-2 sm:w-auto">
        <NumberField
          className="min-w-0 flex-1 sm:w-40 sm:flex-none"
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
      </div>
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
    <Field className="grid gap-4 p-5 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-center sm:gap-x-6">
      <div className="min-w-0 space-y-1.5">
        <FieldLabel>{t('原账号关联保留期', 'Account affinity retention')}</FieldLabel>
        <FieldDescription className="max-w-xl leading-5">
          {conflict
            ? t(
                '保留期短于有效期时按有效期处理，等于关闭软绑定。',
                'A retention shorter than the lifetime is treated as the lifetime, which effectively disables soft binding.',
              )
            : t(
                '名额释放后，设备在此期限内回来仍优先使用原账号，减少 thinking 签名跨账号导致的降级重试。',
                'After its slot is released, a returning device still prefers its original account, reducing retries caused by account-bound thinking signatures.',
              )}
        </FieldDescription>
        <Badge variant={conflict ? 'warning' : 'secondary'} size="sm">{hint}</Badge>
      </div>
      <div className="flex w-full items-center gap-2 sm:w-auto">
        <NumberField
          className="min-w-0 flex-1 sm:w-40 sm:flex-none"
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
      </div>
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
    <Field className="grid gap-4 p-5 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-center sm:gap-x-6">
      <div className="min-w-0 space-y-1.5">
        <FieldLabel>{t('默认设备上限', 'Default device limit')}</FieldLabel>
        <FieldDescription className="max-w-xl leading-5">
          {t(
            '未单独配置的账号使用此上限；账号独立设置优先。',
            'Accounts without an individual limit use this value; account-specific settings take priority.',
          )}
        </FieldDescription>
        <Badge variant="secondary" size="sm">
          {parsed > 0
            ? t(
                `每个账号最多 ${parsed} 台设备`,
                `Up to ${parsed} ${parsed === 1 ? 'device' : 'devices'} per account`,
              )
            : t('不限（不设默认上限）', 'Unlimited (no default limit)')}
        </Badge>
      </div>
      <div className="flex w-full items-center gap-2 sm:w-auto">
        <NumberField
          className="min-w-0 flex-1 sm:w-40 sm:flex-none"
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
      </div>
    </Field>
  )
}

/**
 * 全局默认账号 RPM 上限：账号未单独配置时套用。
 *
 * 窗口固定 60 秒，与账号列表那列 RPM 同一个口径（含失败的、含 count_tokens），
 * 所以「上限 30」和「当前 12」可以直接比。
 */
function DefaultRpmLimit() {
  const qc = useQueryClient()
  const { language, t } = useI18n()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const [draft, setDraft] = useState<number | null>(null)

  useEffect(() => {
    if (data) setDraft(data.default_rpm_limit)
  }, [data?.default_rpm_limit])

  const save = useMutation({
    mutationFn: (limit: number) => setDefaultRpmLimit(limit),
    onSuccess: (settings: Settings) => {
      toastManager.add({
        title: t('默认 RPM 上限已更新', 'Default RPM limit updated'),
        description: settings.default_rpm_limit > 0
          ? t(
              `每个账号每分钟最多转发 ${settings.default_rpm_limit} 条请求。`,
              `Each account forwards at most ${settings.default_rpm_limit} requests per minute.`,
            )
          : t('默认 RPM 上限已取消。', 'The default RPM limit has been removed.'),
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

  const current = data?.default_rpm_limit ?? 0
  const parsed = Math.max(0, Math.floor(draft ?? 0))

  return (
    <Field className="grid gap-4 p-5 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-center sm:gap-x-6">
      <div className="min-w-0 space-y-1.5">
        <FieldLabel>{t('默认 RPM 上限', 'Default RPM limit')}</FieldLabel>
        <FieldDescription className="max-w-xl leading-5">
          {t(
            '未单独配置的账号使用此上限；账号独立设置优先。打满后新请求分流到别的账号，已绑定的设备收到 429 与 retry-after。',
            'Accounts without an individual limit use this value; account-specific settings take priority. Once full, new requests spill to another account and already-bound devices get a 429 with retry-after.',
          )}
        </FieldDescription>
        <Badge variant="secondary" size="sm">
          {parsed > 0
            ? t(`每个账号每分钟最多 ${parsed} 条`, `Up to ${parsed} requests per minute per account`)
            : t('不限（不设默认上限）', 'Unlimited (no default limit)')}
        </Badge>
      </div>
      <div className="flex w-full items-center gap-2 sm:w-auto">
        <NumberField
          className="min-w-0 flex-1 sm:w-40 sm:flex-none"
          min={0}
          value={draft}
          onValueChange={setDraft}
        >
          <NumberFieldGroup>
            <NumberFieldDecrement aria-label={t('减少默认 RPM 上限', 'Decrease default RPM limit')} />
            <NumberFieldInput aria-label={t('默认 RPM 上限', 'Default RPM limit')} />
            <NumberFieldIncrement aria-label={t('增加默认 RPM 上限', 'Increase default RPM limit')} />
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
      </div>
    </Field>
  )
}

/**
 * 每设备 RPM 上限：单台设备最近 60 秒最多转发多少条，超了直接 429，不换号。
 *
 * 与账号 RPM 各管一头：账号那道防的是「一个号被打爆」，这道防的是「一台机器把同账号下
 * 其他设备的额度挤没」。两道都配了的话一条请求要先过设备、再过账号。
 */
function DeviceRpmLimit() {
  const qc = useQueryClient()
  const { language, t } = useI18n()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const [draft, setDraft] = useState<number | null>(null)

  useEffect(() => {
    if (data) setDraft(data.device_rpm_limit)
  }, [data?.device_rpm_limit])

  const save = useMutation({
    mutationFn: (limit: number) => setDeviceRpmLimit(limit),
    onSuccess: (settings: Settings) => {
      toastManager.add({
        title: t('设备 RPM 上限已更新', 'Per-device RPM limit updated'),
        description: settings.device_rpm_limit > 0
          ? t(
              `每台设备每分钟最多转发 ${settings.device_rpm_limit} 条请求。`,
              `Each device forwards at most ${settings.device_rpm_limit} requests per minute.`,
            )
          : t('设备 RPM 上限已取消。', 'The per-device RPM limit has been removed.'),
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

  const current = data?.device_rpm_limit ?? 0
  const parsed = Math.max(0, Math.floor(draft ?? 0))
  // 关掉设备身份校验后，裸请求没有 device_id，落不进设备的桶——这时只有裸请求速率上限管得着。
  const bareAllowed = data?.require_device_id === false

  return (
    <Field className="grid gap-4 p-5 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-center sm:gap-x-6">
      <div className="min-w-0 space-y-1.5">
        <FieldLabel>{t('设备 RPM 上限', 'Per-device RPM limit')}</FieldLabel>
        <FieldDescription className="max-w-xl leading-5">
          {t(
            '单台设备每分钟最多转发多少条，超了直接 429 并给出 retry-after。不换号：换哪个账号都是同一台机器在刷。0 表示不限。',
            'How many requests a single device may forward per minute; beyond that it gets a 429 with retry-after. No credential swap happens — it is the same machine either way. 0 means unlimited.',
          )}
        </FieldDescription>
        <div className="flex flex-wrap items-center gap-2">
          <Badge variant="secondary" size="sm">
            {parsed > 0
              ? t(`每台设备每分钟最多 ${parsed} 条`, `Up to ${parsed} requests per minute per device`)
              : t('不限', 'Unlimited')}
          </Badge>
          {parsed > 0 && bareAllowed && (
            <Badge variant="warning" size="sm">
              {t('无身份请求不受此闸管', 'Unidentified requests bypass this')}
            </Badge>
          )}
        </div>
      </div>
      <div className="flex w-full items-center gap-2 sm:w-auto">
        <NumberField
          className="min-w-0 flex-1 sm:w-40 sm:flex-none"
          min={0}
          value={draft}
          onValueChange={setDraft}
        >
          <NumberFieldGroup>
            <NumberFieldDecrement aria-label={t('减少设备 RPM 上限', 'Decrease per-device RPM limit')} />
            <NumberFieldInput aria-label={t('设备 RPM 上限', 'Per-device RPM limit')} />
            <NumberFieldIncrement aria-label={t('增加设备 RPM 上限', 'Increase per-device RPM limit')} />
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
      </div>
    </Field>
  )
}

/**
 * 每会话 RPM 上限：单个会话最近 60 秒最多转发多少条，超了直接 429，不换号。
 *
 * 与设备那道是同一件事的两个粒度：一台机器上开三个 CC 窗口，真实并发是三份对话的并发，
 * 按设备一刀切会让它们互相挤额度。但会话 id 轮换是免费的（/clear、新窗口、重启都换一个），
 * 所以它替代不了设备闸——两道一起配，会话给贴合单个对话节奏的值，设备给它的几倍兜总量。
 */
function SessionRpmLimit() {
  const qc = useQueryClient()
  const { language, t } = useI18n()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const [draft, setDraft] = useState<number | null>(null)

  useEffect(() => {
    if (data) setDraft(data.session_rpm_limit)
  }, [data?.session_rpm_limit])

  const save = useMutation({
    mutationFn: (limit: number) => setSessionRpmLimit(limit),
    onSuccess: (settings: Settings) => {
      toastManager.add({
        title: t('会话 RPM 上限已更新', 'Per-session RPM limit updated'),
        description: settings.session_rpm_limit > 0
          ? t(
              `每个会话每分钟最多转发 ${settings.session_rpm_limit} 条请求。`,
              `Each session forwards at most ${settings.session_rpm_limit} requests per minute.`,
            )
          : t('会话 RPM 上限已取消。', 'The per-session RPM limit has been removed.'),
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

  const current = data?.session_rpm_limit ?? 0
  const parsed = Math.max(0, Math.floor(draft ?? 0))
  const deviceLimit = data?.device_rpm_limit ?? 0
  // 两种配错法各提示一句，都不代为改数字：改与不改是运维的判断，替他改比让他看见更糟。
  // 1) 设备闸没配：客户端换个会话 id 就是满血的新桶，这道闸等于没有护栏。
  const noDeviceBackstop = parsed > 0 && deviceLimit === 0
  // 2) 设备上限不比会话大：设备的桶总是先满，会话这道永远轮不到判定，等于白配。
  const shadowedByDevice = parsed > 0 && deviceLimit > 0 && deviceLimit <= parsed

  return (
    <Field className="grid gap-4 p-5 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-center sm:gap-x-6">
      <div className="min-w-0 space-y-1.5">
        <FieldLabel>{t('会话 RPM 上限', 'Per-session RPM limit')}</FieldLabel>
        <FieldDescription className="max-w-xl leading-5">
          {t(
            '单个会话每分钟最多转发多少条，超了直接 429 并给出 retry-after。比设备那道细一层：同一台机器上的多个会话各有自己的额度，不再互相挤。0 表示不限。',
            'How many requests a single session may forward per minute; beyond that it gets a 429 with retry-after. One level finer than the per-device gate: concurrent sessions on the same machine no longer share one budget. 0 means unlimited.',
          )}
        </FieldDescription>
        <div className="flex flex-wrap items-center gap-2">
          <Badge variant="secondary" size="sm">
            {parsed > 0
              ? t(`每个会话每分钟最多 ${parsed} 条`, `Up to ${parsed} requests per minute per session`)
              : t('不限', 'Unlimited')}
          </Badge>
          {noDeviceBackstop && (
            <Badge variant="warning" size="sm">
              {t('换个会话 id 即绕开，建议同时配设备上限', 'Bypassed by a new session id — set a per-device limit too')}
            </Badge>
          )}
          {shadowedByDevice && (
            <Badge variant="warning" size="sm">
              {t(
                `设备上限 ${deviceLimit} 更先触发，这道闸不会生效`,
                `The per-device limit of ${deviceLimit} always trips first — this gate never fires`,
              )}
            </Badge>
          )}
        </div>
      </div>
      <div className="flex w-full items-center gap-2 sm:w-auto">
        <NumberField
          className="min-w-0 flex-1 sm:w-40 sm:flex-none"
          min={0}
          value={draft}
          onValueChange={setDraft}
        >
          <NumberFieldGroup>
            <NumberFieldDecrement aria-label={t('减少会话 RPM 上限', 'Decrease per-session RPM limit')} />
            <NumberFieldInput aria-label={t('会话 RPM 上限', 'Per-session RPM limit')} />
            <NumberFieldIncrement aria-label={t('增加会话 RPM 上限', 'Increase per-session RPM limit')} />
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
      </div>
    </Field>
  )
}

/**
 * 每会话并发在途上限：限制单个 session 同时在飞的请求数。
 *
 * Claude Desktop 启动时会并行发 20+ 条 max_tokens=1 的 cache 预热请求，瞬间打爆上游的
 * 组织级速率限制（裸 429），再经代理换号重试扩散到整个凭证池。给一个 3~5 的并发上限就能
 * 把脉冲拉平。
 */
function SessionConcurrencyLimit() {
  const qc = useQueryClient()
  const { language, t } = useI18n()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const [draft, setDraft] = useState<number | null>(null)

  useEffect(() => {
    if (data) setDraft(data.session_concurrency_limit)
  }, [data?.session_concurrency_limit])

  const save = useMutation({
    mutationFn: (limit: number) => setSessionConcurrencyLimit(limit),
    onSuccess: (settings: Settings) => {
      toastManager.add({
        title: t('会话并发上限已更新', 'Per-session concurrency limit updated'),
        description: settings.session_concurrency_limit > 0
          ? t(
              `每个会话最多同时 ${settings.session_concurrency_limit} 条请求在飞。`,
              `Each session may have at most ${settings.session_concurrency_limit} requests in flight.`,
            )
          : t('会话并发上限已取消。', 'The per-session concurrency limit has been removed.'),
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

  const current = data?.session_concurrency_limit ?? 0
  const parsed = Math.max(0, Math.floor(draft ?? 0))

  return (
    <Field className="grid gap-4 p-5 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-center sm:gap-x-6">
      <div className="min-w-0 space-y-1.5">
        <FieldLabel>{t('会话并发上限', 'Per-session concurrency limit')}</FieldLabel>
        <FieldDescription className="max-w-xl leading-5">
          {t(
            '单个会话同时在飞的最大请求数，超了直接 429 并给出 retry-after。用于遏制 Claude Desktop 启动时的 cache 预热脉冲（20+ 条并发），避免打爆上游速率限制。0 表示不限。',
            'Maximum concurrent in-flight requests per session; beyond that it gets a 429 with retry-after. Tames the cache-warming burst Claude Desktop fires on startup (20+ concurrent requests) to avoid tripping upstream rate limits. 0 means unlimited.',
          )}
        </FieldDescription>
        <div className="flex flex-wrap items-center gap-2">
          <Badge variant="secondary" size="sm">
            {parsed > 0
              ? t(`每个会话最多 ${parsed} 条并发`, `Up to ${parsed} concurrent per session`)
              : t('不限', 'Unlimited')}
          </Badge>
        </div>
      </div>
      <div className="flex w-full items-center gap-2 sm:w-auto">
        <NumberField
          className="min-w-0 flex-1 sm:w-40 sm:flex-none"
          min={0}
          value={draft}
          onValueChange={setDraft}
        >
          <NumberFieldGroup>
            <NumberFieldDecrement aria-label={t('减少会话并发上限', 'Decrease per-session concurrency limit')} />
            <NumberFieldInput aria-label={t('会话并发上限', 'Per-session concurrency limit')} />
            <NumberFieldIncrement aria-label={t('增加会话并发上限', 'Increase per-session concurrency limit')} />
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
      </div>
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

/** 版本号的可接受写法：`2`、`2.1`、`2.1.220`，可带 `-beta.1` 之类的后缀（按主版本算）。 */
const VERSION_RE = /^\d+(\.\d+)*([-+][0-9A-Za-z.]+)?$/

/** 严格三段版本：官方发布清单的形态，`latest_cc_release` 只收这个。 */
const RELEASE_RE = /^\d+\.\d+\.\d+$/

/** 比较两个 `主.次.修` 串；解析不出的按最小。 */
function compareRelease(a: string, b: string): number {
  const pa = a.split('.').map(Number)
  const pb = b.split('.').map(Number)
  for (let i = 0; i < 3; i++) {
    const d = (pa[i] ?? 0) - (pb[i] ?? 0)
    if (d !== 0) return d
  }
  return 0
}

/** 实际生效的版本上限：学到/手填的值与模拟基线取大——后端同一口径。 */
function effectiveLatestRelease(settings: Settings): string {
  const learned = settings.latest_cc_release
  if (!learned) return settings.cc_version_base
  return compareRelease(learned, settings.cc_version_base) >= 0 ? learned : settings.cc_version_base
}

/**
 * 官方最新 Claude Code 版本：来访 UA 自报高于它的不当官方客户端。
 *
 * 自动从 downloads.claude.ai 每 30 分钟学一次（只升不降）并落库；这里可以手动填（官方刚发新版、
 * 自动检查还没轮到时）或删掉（退回基线、等下次自动学）。
 */
function LatestCcRelease() {
  const qc = useQueryClient()
  const { language, t } = useI18n()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const [draft, setDraft] = useState('')

  useEffect(() => {
    if (data) setDraft(data.latest_cc_release)
  }, [data?.latest_cc_release])

  const save = useMutation({
    mutationFn: (version: string) => setLatestCcRelease(version),
    onSuccess: (settings: Settings, version: string) => {
      toastManager.add({
        title: version
          ? t('官方最新版本已更新', 'Latest official release updated')
          : t('官方最新版本已清除', 'Latest official release cleared'),
        description: version
          ? t(
              `自称高于 ${effectiveLatestRelease(settings)} 的客户端将按非官方处理；自动检查学到更新的版本会覆盖它。`,
              `Clients claiming a version newer than ${effectiveLatestRelease(settings)} are treated as unofficial; a newer version learned by the automatic check will replace it.`,
            )
          : t(
              `退回基线 ${settings.cc_version_base}，下次自动检查（最多 30 分钟）再学。`,
              `Back to the baseline ${settings.cc_version_base}; the next automatic check (within 30 minutes) will relearn it.`,
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

  const value = draft.trim()
  const current = data?.latest_cc_release ?? ''
  const base = data?.cc_version_base ?? ''
  const malformed = value !== '' && !RELEASE_RE.test(value)
  // 填一个不高于基线的值没有效果（上限取大），提示一下免得以为生效了。
  const belowBase = !malformed && value !== '' && base !== '' && compareRelease(value, base) < 0

  return (
    <Field className="grid gap-4 p-5 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-center sm:gap-x-6">
      <div className="min-w-0 space-y-1.5">
        <FieldLabel htmlFor="latest-cc-release">
          {t('官方最新 Claude Code 版本', 'Latest official Claude Code release')}
        </FieldLabel>
        <FieldDescription className="max-w-xl leading-5">
          {t(
            `User-Agent 里自称高于该版本的 claude-cli 不当官方客户端（走模拟路径）。每 30 分钟从 downloads.claude.ai 自动学一次，只升不降，重启保留。官方刚发新版、自动检查还没轮到时可在这里手动填；删掉则退回基线 ${base || '—'}，等下次自动学。`,
            `A claude-cli User-Agent claiming a version newer than this is not treated as an official client (it takes the simulated path). Learned automatically from downloads.claude.ai every 30 minutes, never downgraded, kept across restarts. Fill it in by hand when a new release just shipped and the check has not run yet; clearing it falls back to the baseline ${base || '—'} until the next check.`,
          )}
        </FieldDescription>
        <Badge variant={malformed || belowBase ? 'warning' : 'secondary'} size="sm">
          {malformed
            ? t('写法应形如 2.1.260', 'Expected something like 2.1.260')
            : belowBase
              ? t(`低于基线 ${base}，不会生效`, `Below the baseline ${base}; has no effect`)
              : current
                ? t(`当前上限 ${data ? effectiveLatestRelease(data) : current}`, `Current cap ${data ? effectiveLatestRelease(data) : current}`)
                : t(`尚未学到，按基线 ${base}`, `Not learned yet; using the baseline ${base}`)}
        </Badge>
      </div>
      <div className="flex w-full items-center gap-2 sm:w-auto">
        <Input
          id="latest-cc-release"
          className="min-w-0 flex-1 sm:w-40 sm:flex-none"
          placeholder={base || '2.1.260'}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
        />
        <Button
          size="sm"
          loading={save.isPending && save.variables !== ''}
          disabled={malformed || value === '' || value === current}
          onClick={() => save.mutate(value)}
        >
          <SaveIcon />
          {t('保存', 'Save')}
        </Button>
        <Button
          size="sm"
          variant="outline"
          loading={save.isPending && save.variables === ''}
          disabled={current === ''}
          onClick={() => save.mutate('')}
          aria-label={t('清除', 'Clear')}
        >
          <Trash2Icon />
          {t('清除', 'Clear')}
        </Button>
      </div>
    </Field>
  )
}

/**
 * 最低 Claude Code 版本：UA 自报 `claude-cli/<版本>` 且低于此值的请求直接 403。
 *
 * 只是引导升级用的闸，不是安全边界——UA 是客户端自报的，改一个头就能绕过。
 */
function MinClientVersion() {
  const qc = useQueryClient()
  const { language, t } = useI18n()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const [draft, setDraft] = useState('')

  useEffect(() => {
    if (data) setDraft(data.min_client_version)
  }, [data?.min_client_version])

  const save = useMutation({
    mutationFn: (version: string) => setMinClientVersion(version),
    onSuccess: (settings: Settings) => {
      toastManager.add({
        title: settings.min_client_version
          ? t('最低客户端版本已更新', 'Minimum client version updated')
          : t('最低客户端版本已取消', 'Minimum client version removed'),
        description: settings.min_client_version
          ? t(
              `低于 ${settings.min_client_version} 的 Claude Code 将被拒绝。`,
              `Claude Code older than ${settings.min_client_version} will be rejected.`,
            )
          : t('不再按版本拦截客户端。', 'Clients are no longer filtered by version.'),
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

  const value = draft.trim()
  const current = data?.min_client_version ?? ''
  // 空串是合法输入（= 取消限制），只有写错格式才拦下——后端同样会 400，这里先拦一道免得白跑。
  const malformed = value !== '' && !VERSION_RE.test(value)

  return (
    <Field className="grid gap-4 p-5 sm:grid-cols-[minmax(0,1fr)_auto] sm:items-center sm:gap-x-6">
      <div className="min-w-0 space-y-1.5">
        <FieldLabel htmlFor="min-client-version">
          {t('最低 Claude Code 版本', 'Minimum Claude Code version')}
        </FieldLabel>
        <FieldDescription className="max-w-xl leading-5">
          {t(
            '低于该版本的 Claude Code 会收到 403 与升级提示。留空表示不限。只认 User-Agent 里的 claude-cli 版本，SDK、浏览器等其它客户端一律放行；UA 可被客户端伪造，只用于引导升级。',
            'Older Claude Code builds get a 403 with an upgrade hint. Leave empty for no limit. Only the claude-cli version in the User-Agent is checked — SDKs, browsers and other clients always pass; a User-Agent can be forged, so treat this as an upgrade nudge, not a security boundary.',
          )}
        </FieldDescription>
        <Badge variant={malformed ? 'warning' : 'secondary'} size="sm">
          {malformed
            ? t('写法应形如 2.1.220', 'Expected something like 2.1.220')
            : value
              ? t(`要求 ${value} 及以上`, `Requires ${value} or newer`)
              : t('不限版本', 'Any version')}
        </Badge>
      </div>
      <div className="flex w-full items-center gap-2 sm:w-auto">
        <Input
          id="min-client-version"
          className="min-w-0 flex-1 sm:w-40 sm:flex-none"
          placeholder="2.1.220"
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
        />
        <Button
          size="sm"
          loading={save.isPending}
          disabled={malformed || value === current}
          onClick={() => save.mutate(value)}
        >
          <SaveIcon />
          {t('保存', 'Save')}
        </Button>
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
    <div className="p-5">
      <div className="grid gap-5 lg:grid-cols-[minmax(0,1fr)_minmax(22rem,0.9fr)] lg:items-start">
        <div className="min-w-0">
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
          <p className="mt-1 max-w-xl text-xs leading-5 text-muted-foreground">
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
        <div className="space-y-3">
          <div className="grid grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto] items-end gap-3">
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
            <Button
              className="max-sm:size-8 max-sm:px-0"
              size="sm"
              loading={save.isPending}
              disabled={inactive || unchanged}
              onClick={() => save.mutate({ limit, win })}
            >
              <SaveIcon />
              <span className="max-sm:sr-only">{t('保存', 'Save')}</span>
            </Button>
          </div>
        </div>
      </div>
      <p className="mt-4 border-t pt-4 text-xs leading-5 text-muted-foreground">
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
