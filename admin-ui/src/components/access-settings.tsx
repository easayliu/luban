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
  MessagesSquareIcon,
  SaveIcon,
  Settings2Icon,
  ShieldCheckIcon,
  SparklesIcon,
  TerminalIcon,
  TimerIcon,
  Trash2Icon,
} from 'lucide-react'
import {
  setApiKey,
  setBareRateLimit,
  setDefaultDeviceLimit,
  setDefaultRpmLimit,
  setDefaultSessionLimit,
  setDeviceRetention,
  setDeviceRpmLimit,
  setDeviceTtl,
  setLatestCcRelease,
  setMinClientVersion,
  setRequireDeviceId,
  setSessionConcurrencyLimit,
  setSessionRetention,
  setSessionRpmLimit,
  setSessionTtl,
  type Settings,
} from '@/api/settings'
import { changePassword, getAuthState, setViewerPassword } from '@/api/auth'
import { clearToken } from '@/api/client'
import { useI18n } from '@/lib/i18n'
import { useMe } from '@/lib/role'
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
import { Hint } from '@/components/ui/tooltip'
import { ClampedDescription, SettingsGroup, SettingsRow } from '@/components/settings-group'
import {
  DurationSetting,
  NumericSetting,
  useSettingsQuery,
  useSettingsSave,
} from '@/components/setting-controls'

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
      <DialogPopup size="md">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Settings2Icon aria-hidden="true" />
            {t('客户端接入', 'Client access')}
          </DialogTitle>
          <DialogDescription>
            {t(
              '配置客户端接入地址、身份验证 Key 和 Claude Code 接入片段。',
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
  const { t } = useI18n()
  const settingsQuery = useSettingsQuery()
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

  const save = useSettingsSave((key: string) => setApiKey(key), {
    onSuccess: () => {
      setClearKeyOpen(false)
    },
    success: (settings) => ({
      title: settings.api_key
        ? t('接入 Key 已保存', 'Access key saved')
        : t('接入 Key 已清除', 'Access key cleared'),
      description: settings.api_key
        ? t('新的客户端接入 Key 已生效。', 'The new client access key is now active.')
        : t('代理将不再校验客户端请求。', 'The proxy will no longer authenticate client requests.'),
    }),
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
            '复制客户端接入地址，并配置代理用来验证客户端请求的 Key。',
            'Copy the client endpoint and configure the key the proxy uses to authenticate client requests.',
          )}
        >
          <Field className="p-4 sm:p-5">
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

          <Field className="p-4 sm:p-5">
            <FieldLabel>
              {t('接入 Key', 'Access key')}
              <code className="font-mono text-xs font-normal text-muted-foreground">ANTHROPIC_AUTH_TOKEN</code>
            </FieldLabel>
            <InputGroup>
              <InputGroupInput
                aria-label={t('接入 Key', 'Access key')}
                onChange={(event) => setDraft(event.target.value)}
                placeholder={envManaged ? '' : t('留空则不校验客户端请求', 'Leave blank to disable client authentication')}
                readOnly={envManaged}
                type={show ? 'text' : 'password'}
                value={draft}
              />
              <InputGroupAddon className="gap-4" align="inline-end">
                <Hint label={show ? t('隐藏', 'Hide') : t('显示', 'Show')}>
                  <Button
                    aria-label={show
                      ? t('隐藏接入 Key', 'Hide access key')
                      : t('显示接入 Key', 'Show access key')}
                    size="icon-sm"
                    variant="ghost"
                    onClick={() => setShow((visible) => !visible)}
                  >
                    {show ? <EyeOffIcon /> : <EyeIcon />}
                  </Button>
                </Hint>
                {/* 只复制已保存的 Key：生成后没点保存就拿去配客户端，请求会一律 401。 */}
                <CopyButton
                  text={currentKey}
                  disabledReason={draft.trim() !== currentKey
                    ? t('先保存再复制', 'Save the key before copying it')
                    : undefined}
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
                  disabled={draft.trim() === currentKey}
                  // 删空再保存等于清除 Key，同样要过确认框。
                  onClick={() => (draft.trim() ? save.mutate(draft.trim()) : setClearKeyOpen(true))}
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
                {t(' 管理，此处只读。', '; this page is read-only.')}
              </FieldDescription>
            )}
          </Field>

          <Field className="p-4 sm:p-5">
            <div className="flex w-full min-w-0 items-center justify-between gap-2">
              <FieldLabel>{t('Claude Code 接入片段', 'Claude Code setup snippet')}</FieldLabel>
              <div className="flex shrink-0 items-center gap-3">
                {currentKey && (
                  <Hint label={showSnippetKey ? t('隐藏 Key', 'Hide key') : t('显示 Key', 'Show key')}>
                    <Button
                      type="button"
                      aria-label={showSnippetKey
                        ? t('隐藏接入片段中的 Key', 'Hide the key in the setup snippet')
                        : t('显示接入片段中的 Key', 'Show the key in the setup snippet')}
                      size="icon"
                      variant="ghost"
                      onClick={() => setRevealedSnippetKey((revealed) => (
                        revealed === currentKey ? null : currentKey
                      ))}
                    >
                      {showSnippetKey ? <EyeOffIcon /> : <EyeIcon />}
                    </Button>
                  </Hint>
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
        <AlertDialogPopup>
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
  const settingsQuery = useSettingsQuery()

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
      {/* 这里原有一张「当前策略」汇总卡，把下面各组的已保存值原样再列一遍（每一项下面本来就有输入框
          和换算后的读数），同一页上同一个值出现三次，已去掉。 */}
      <SettingsGroup
        icon={GaugeIcon}
        title={t('设备绑定与容量', 'Device bindings & capacity')}
        description={t(
          '适用于带设备身份的客户端请求：设置设备占用账号名额的时长、名额释放后优先使用原账号的期限，以及每个账号默认可容纳的设备数。转发设置中「设备指纹归一化」与「改写设备 ID」均启用时，上游看到的设备数已经收敛，名额改按会话计算，设备上限不生效；设备绑定只用于让同一台设备的新会话优先使用原账号。',
          'For client requests with a device identity: how long a device holds its slot, how long it keeps preferring its original account, and how many devices each account holds by default. When both "Normalize device fingerprint" and "Rewrite device ID" are on in forwarding settings, upstream sees only a few devices per account, so slots are counted per session and the device limit does not apply; device bindings then only steer a device\'s new sessions back to its original account.',
        )}
      >
        <DeviceBindingTtl />
        <DeviceBindingRetention />
        {/* 设备按会话占名额时设备上限不生效，这一行隐去；关掉归一化或改写设备 ID 后回来。 */}
        {!settingsQuery.data?.devices_by_session && <DefaultDeviceLimit />}
      </SettingsGroup>

      <SettingsGroup
        icon={MessagesSquareIcon}
        title={t('会话绑定与容量', 'Session bindings & capacity')}
        description={t(
          '客户端请求按对话占用会话名额：设备上限不生效时的真实客户端，以及经模拟路径、没有设备身份的请求。设置对话闲置多久后释放名额、名额释放后优先使用原账号的期限，以及每个账号默认可同时活跃的会话数。',
          'Client requests take one session slot per conversation: real clients whenever the device limit does not apply, and requests on the simulation path without a device identity. Set how long an idle conversation keeps its slot, how long it keeps preferring its original account, and how many sessions each account may keep active by default.',
        )}
      >
        <SessionBindingTtl />
        <SessionBindingRetention />
        <DefaultSessionLimit />
      </SettingsGroup>

      <SettingsGroup
        icon={TimerIcon}
        title={t('转发速率', 'Request rate')}
        description={t(
          '限制单个账号、单台设备和单个会话每分钟可转发的请求数，以及单个会话的最大并发数。RPM 与账号列表中 RPM 列的统计口径一致；并发上限用于防止 Claude Desktop 缓存预热时的突发请求超出上游速率限制。',
          'Cap how many requests a single account, device, or session forwards per minute, and the maximum concurrency per session. RPM is counted the same way as the RPM column in the account list; the concurrency cap keeps Claude Desktop\'s cache-warming burst from exceeding upstream rate limits.',
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
          '依据 User-Agent 中声明的 claude-cli 版本进行判断：拦截版本过旧的 Claude Code，并将声明版本高于官方最新版的客户端识别为非官方客户端；其他客户端不受影响。',
          'Based on the claude-cli version self-reported in the User-Agent: block outdated Claude Code builds, and treat clients claiming a version newer than the latest official release as unofficial. Other clients are unaffected.',
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
          '控制管理控制台的登录权限，不影响客户端通过代理发起的请求。',
          'Control who can sign in to the admin console; this does not affect proxied client requests.',
        )}
      >
        <AdminPassword />
      </SettingsGroup>
      <SettingsGroup
        icon={EyeIcon}
        title={t('访客密码', 'Viewer password')}
        description={t(
          '用访客密码登录的人可以查看控制台的全部页面，但不能做任何修改；接入 Key 与代理密码对访客打码显示，也不能导出数据。',
          'People who sign in with the viewer password can see every page of the console but cannot change anything. The client key and proxy passwords are masked for them, and export is unavailable.',
        )}
      >
        <ViewerPassword />
      </SettingsGroup>
    </div>
  )
}

/** 设备绑定有效期：设备超过该时长无请求即释放名额（绑定本身按保留期留着）。0 = 永不过期。 */
function DeviceBindingTtl() {
  const { language, t } = useI18n()
  return (
    <DurationSetting
      field="device_binding_ttl_secs"
      save={setDeviceTtl}
      defaultUnit="hour"
      invalidateCredentials
      label={t('活跃名额有效期', 'Active slot lifetime')}
      description={t(
        '设备在此时长内无请求时，释放其占用的账号名额；与原账号的关联仍按保留期保留。',
        'A device releases its account slot after this much inactivity; its affinity with the original account is still kept for the retention period.',
      )}
      note={(parsed) => (
        <Badge variant="secondary" size="sm">
          {parsed > 0
            ? t(
                `闲置 ${formatDuration(parsed, language)} 后释放名额`,
                `Releases the slot after ${formatDuration(parsed, language)} idle`,
              )
            : t('名额不自动释放', 'Slots are not released automatically')}
        </Badge>
      )}
      success={() => ({
        title: t('设备策略已更新', 'Device policy updated'),
        description: t('设备绑定有效期已保存。', 'The device binding lifetime has been saved.'),
      })}
    />
  )
}

/** 保留期短于有效期是自相矛盾的配置，后端会按有效期兜底（等于关掉软绑定），界面先提示一句。 */
function retentionConflict(retention: number, ttl: number): boolean {
  return retention > 0 && ttl > 0 && retention < ttl
}

/**
 * 软绑定保留期：绑定超过有效期后不再占名额，但在这段时间内设备再来仍优先回原账号
 * （原账号还得有空位）。0 = 永久保留。
 */
function DeviceBindingRetention() {
  const { language, t } = useI18n()
  // 默认按天填：保留期通常是「几天几周」这个量级；要调到分钟级（比如设备频繁换号、希望
  // 绑定尽快清掉）就切单位。接口仍收秒，单位只是这一格的输入方式。
  return (
    <DurationSetting
      field="device_binding_retention_secs"
      save={setDeviceRetention}
      defaultUnit="day"
      invalidateCredentials
      label={t('原账号关联保留期', 'Account affinity retention')}
      description={(parsed, data) =>
        retentionConflict(parsed, data?.device_binding_ttl_secs ?? 0)
          ? t(
              '保留期短于有效期时按有效期计算，相当于停用软绑定。',
              'A retention shorter than the lifetime is treated as the lifetime, which effectively disables soft binding.',
            )
          : t(
              '名额释放后，设备在此期限内再次请求时仍优先使用原账号，以减少 thinking 签名跨账号导致的降级重试。',
              'After its slot is released, a device that returns within this period still prefers its original account, reducing downgrade retries caused by thinking signatures crossing accounts.',
            )}
      note={(parsed, data) => (
        <Badge
          variant={retentionConflict(parsed, data?.device_binding_ttl_secs ?? 0) ? 'warning' : 'secondary'}
          size="sm"
        >
          {parsed > 0
            ? t(
                `优先使用原账号：${formatDuration(parsed, language)}`,
                `Prefer the original account for ${formatDuration(parsed, language)}`,
              )
            : t('始终优先使用原账号', 'Always prefer the original account')}
        </Badge>
      )}
      success={() => ({
        title: t('设备策略已更新', 'Device policy updated'),
        description: t('原账号关联保留期已保存。', 'The account affinity retention has been saved.'),
      })}
    />
  )
}

/**
 * 模拟会话绑定有效期：对话超过该时长无请求即释放会话槽位（会话 id 让给下一个对话复用），
 * 绑定本身按会话保留期留着。与设备那一项分开配：设备是一台机器，会话是一段对话。0 = 永不过期。
 */
function SessionBindingTtl() {
  const { language, t } = useI18n()
  return (
    <DurationSetting
      field="session_binding_ttl_secs"
      save={setSessionTtl}
      defaultUnit="minute"
      invalidateCredentials
      label={t('会话有效期', 'Session lifetime')}
      description={
        <ClampedDescription text={t(
          '对话在此时长内无请求时，释放其占用的会话名额；经模拟路径的对话，其会话 ID 留待下一个对话复用。与原账号的关联按下方的保留期保留。此项与设备有效期分别配置：设备对应一台机器，会话对应一段对话。',
          'A conversation frees its session slot after this much inactivity; on the simulation path its session ID is then reused by the next conversation. Affinity with the original account is kept for the retention period below. This is configured separately from the device lifetime: a device is a machine, a session is one conversation.',
        )} />
      }
      note={(parsed) => (
        <Badge variant="secondary" size="sm">
          {parsed > 0
            ? t(
                `对话闲置 ${formatDuration(parsed, language)} 后释放槽位`,
                `Frees the slot after ${formatDuration(parsed, language)} idle`,
              )
            : t('槽位不自动释放', 'Slots are not released automatically')}
        </Badge>
      )}
      success={() => ({
        title: t('会话策略已更新', 'Session policy updated'),
        description: t('会话有效期已保存。', 'The session lifetime has been saved.'),
      })}
    />
  )
}

/** 模拟会话的原账号关联保留期：槽位释放后，对话在此期限内回来仍优先回原号。0 = 永久保留。 */
function SessionBindingRetention() {
  const { language, t } = useI18n()
  return (
    <DurationSetting
      field="session_binding_retention_secs"
      save={setSessionRetention}
      defaultUnit="day"
      invalidateCredentials
      label={t('会话原账号关联保留期', 'Session affinity retention')}
      description={(parsed, data) =>
        retentionConflict(parsed, data?.session_binding_ttl_secs ?? 0)
          ? t(
              '保留期短于有效期时按有效期计算，相当于停用软绑定。',
              'A retention shorter than the lifetime is treated as the lifetime, which effectively disables soft binding.',
            )
          : t(
              '名额释放后，对话在此期限内再次请求时仍优先使用原账号（经模拟路径的对话在原槽位空闲时回到原槽位，会话 ID 不变）；过期后清除绑定记录，此后的请求按新对话处理。',
              'After its slot is freed, a conversation that returns within this period still prefers its original account (on the simulation path it also gets its old slot back if that is free, keeping the same session ID); after this period the binding is removed and the conversation is treated as new.',
            )}
      note={(parsed, data) => (
        <Badge
          variant={retentionConflict(parsed, data?.session_binding_ttl_secs ?? 0) ? 'warning' : 'secondary'}
          size="sm"
        >
          {parsed > 0
            ? t(
                `优先使用原账号：${formatDuration(parsed, language)}`,
                `Prefer the original account for ${formatDuration(parsed, language)}`,
              )
            : t('始终优先使用原账号', 'Always prefer the original account')}
        </Badge>
      )}
      success={() => ({
        title: t('会话策略已更新', 'Session policy updated'),
        description: t('会话的原账号关联保留期已保存。', 'The session affinity retention has been saved.'),
      })}
    />
  )
}

/** 全局默认设备上限：账号未单独配置时套用。 */
function DefaultDeviceLimit() {
  const { t } = useI18n()
  return (
    <NumericSetting
      field="default_device_limit"
      save={setDefaultDeviceLimit}
      invalidateCredentials
      label={t('默认设备上限', 'Default device limit')}
      description={t(
        '未单独配置的账号使用此上限；账号独立设置优先。',
        'Accounts without an individual limit use this value; account-specific settings take priority.',
      )}
      note={(parsed) => (
        <div className="flex flex-wrap items-center gap-2">
          <Badge variant="secondary" size="sm">
            {parsed > 0
              ? t(
                  `每个账号最多 ${parsed} 台设备`,
                  `Up to ${parsed} ${parsed === 1 ? 'device' : 'devices'} per account`,
                )
              : t('不限（不设默认上限）', 'Unlimited (no default limit)')}
          </Badge>
        </div>
      )}
      success={(settings) => ({
        title: t('默认设备上限已更新', 'Default device limit updated'),
        description: settings.default_device_limit > 0
          ? t(
              `每个账号最多绑定 ${settings.default_device_limit} 台设备。`,
              `Each account can bind up to ${settings.default_device_limit} ${settings.default_device_limit === 1 ? 'device' : 'devices'}.`,
            )
          : t('默认设备上限已取消。', 'The default device limit has been removed.'),
      })}
    />
  )
}

/** 全局默认会话上限：账号未单独配置时套用。设备上限不生效时的真实客户端与模拟路径上没有设备身份的来访都按它算。 */
function DefaultSessionLimit() {
  const { t } = useI18n()
  return (
    <NumericSetting
      field="default_session_limit"
      save={setDefaultSessionLimit}
      invalidateCredentials
      label={t('默认会话上限', 'Default session limit')}
      description={t(
        '每个对话绑定一个账号并占用一个名额。真实客户端按其自带的会话 ID 识别对话，同一台设备的新会话优先使用该设备上次的账号；Claude Code 启动时的额度探测不占名额。经模拟路径、没有设备身份的请求优先按自带的会话 ID 识别对话，缺失时按缓存前缀与首条用户消息识别，其出站会话 ID 由槽位派生、释放后复用。有效期与原账号关联保留期沿用上方两项会话设置。未单独配置的账号使用此上限；账号独立设置优先。名额用尽后，新会话分流到其他账号；所有账号均用尽时，请求返回 429。',
        'Each conversation binds to an account and takes one slot. Real clients are identified by their own session ID, and a device\'s new sessions prefer the account it used last; the quota probe Claude Code sends at startup takes no slot. Requests on the simulation path without a device identity are identified by their own session ID, or failing that by the cache prefix plus the first user message; their outbound session ID is derived from the slot and reused once it is freed. Lifetime and affinity follow the two session settings above. Accounts without an individual limit use this value; account-specific settings take priority. Once an account is full, new sessions go to another account; when every account is full they get a 429.',
      )}
      note={(parsed) => (
        <Badge variant="secondary" size="sm">
          {parsed > 0
            ? t(`每个账号最多 ${parsed} 条活跃会话`, `Up to ${parsed} active ${parsed === 1 ? 'session' : 'sessions'} per account`)
            : t('不限（不设默认上限）', 'Unlimited (no default limit)')}
        </Badge>
      )}
      success={(settings) => ({
        title: t('默认会话上限已更新', 'Default session limit updated'),
        description: settings.default_session_limit > 0
          ? t(
              `每个账号最多同时活跃 ${settings.default_session_limit} 条会话。`,
              `Each account can keep up to ${settings.default_session_limit} active ${settings.default_session_limit === 1 ? 'session' : 'sessions'}.`,
            )
          : t('默认会话上限已取消。', 'The default session limit has been removed.'),
      })}
    />
  )
}

/**
 * 全局默认账号 RPM 上限：账号未单独配置时套用。
 *
 * 窗口固定 60 秒，与账号列表那列 RPM 同一个口径（含失败的、含 count_tokens），
 * 所以「上限 30」和「当前 12」可以直接比。
 */
function DefaultRpmLimit() {
  const { t } = useI18n()
  return (
    <NumericSetting
      field="default_rpm_limit"
      save={setDefaultRpmLimit}
      invalidateCredentials
      label={t('默认 RPM 上限', 'Default RPM limit')}
      description={t(
        '未单独配置的账号使用此上限；账号独立设置优先。达到上限后新请求分流到其他账号，已绑定的设备收到 429 与 retry-after。',
        'Accounts without an individual limit use this value; account-specific settings take priority. Once the limit is reached, new requests go to another account and already-bound devices get a 429 with retry-after.',
      )}
      note={(parsed) => (
        <Badge variant="secondary" size="sm">
          {parsed > 0
            ? t(`每个账号每分钟最多 ${parsed} 条`, `Up to ${parsed} requests per minute per account`)
            : t('不限（不设默认上限）', 'Unlimited (no default limit)')}
        </Badge>
      )}
      success={(settings) => ({
        title: t('默认 RPM 上限已更新', 'Default RPM limit updated'),
        description: settings.default_rpm_limit > 0
          ? t(
              `每个账号每分钟最多转发 ${settings.default_rpm_limit} 条请求。`,
              `Each account forwards at most ${settings.default_rpm_limit} requests per minute.`,
            )
          : t('默认 RPM 上限已取消。', 'The default RPM limit has been removed.'),
      })}
    />
  )
}

/**
 * 每设备 RPM 上限：单台设备最近 60 秒最多转发多少条，超了直接 429，不换号。
 *
 * 与账号 RPM 各管一头：账号那道防的是「一个号被打爆」，这道防的是「一台机器把同账号下
 * 其他设备的额度挤没」。两道都配了的话一条请求要先过设备、再过账号。
 */
function DeviceRpmLimit() {
  const { t } = useI18n()
  return (
    <NumericSetting
      field="device_rpm_limit"
      save={setDeviceRpmLimit}
      label={t('设备 RPM 上限', 'Per-device RPM limit')}
      description={t(
        '单台设备每分钟最多转发的请求数，超出后直接返回 429 并附带 retry-after。超出时不切换账号，因为无论切换到哪个账号，持续发送请求的都是同一台机器。0 表示不限。',
        'How many requests a single device may forward per minute; beyond that it gets a 429 with retry-after. The request is not moved to another account: whichever account it lands on, it is the same machine sending the requests. 0 means unlimited.',
      )}
      note={(parsed, data) => {
        // 关掉设备身份校验后，裸请求没有 device_id，落不进设备的桶——这时只有裸请求速率上限管得着。
        const bareAllowed = data?.require_device_id === false
        return (
          <div className="flex flex-wrap items-center gap-2">
            <Badge variant="secondary" size="sm">
              {parsed > 0
                ? t(`每台设备每分钟最多 ${parsed} 条`, `Up to ${parsed} requests per minute per device`)
                : t('不限', 'Unlimited')}
            </Badge>
            {parsed > 0 && bareAllowed && (
              <Badge variant="warning" size="sm">
                {t('无设备身份请求不受此上限约束', 'Requests without device identity bypass this limit')}
              </Badge>
            )}
          </div>
        )
      }}
      success={(settings) => ({
        title: t('设备 RPM 上限已更新', 'Per-device RPM limit updated'),
        description: settings.device_rpm_limit > 0
          ? t(
              `每台设备每分钟最多转发 ${settings.device_rpm_limit} 条请求。`,
              `Each device forwards at most ${settings.device_rpm_limit} requests per minute.`,
            )
          : t('设备 RPM 上限已取消。', 'The per-device RPM limit has been removed.'),
      })}
    />
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
  const { t } = useI18n()
  return (
    <NumericSetting
      field="session_rpm_limit"
      save={setSessionRpmLimit}
      label={t('会话 RPM 上限', 'Per-session RPM limit')}
      description={t(
        '单个会话每分钟最多转发的请求数，超出后直接返回 429 并附带 retry-after。此项比设备 RPM 上限粒度更细：同一台机器上的多个会话各自计算额度，互不挤占。0 表示不限。',
        'How many requests a single session may forward per minute; beyond that it gets a 429 with retry-after. It is one level finer than the per-device limit: sessions on the same machine each get their own budget instead of competing for one. 0 means unlimited.',
      )}
      note={(parsed, data) => {
        const deviceLimit = data?.device_rpm_limit ?? 0
        // 两种配错法各提示一句，都不代为改数字：改与不改是运维的判断，替他改比让他看见更糟。
        // 1) 设备闸没配：客户端换个会话 id 就是满血的新桶，这道闸等于没有护栏。
        const noDeviceBackstop = parsed > 0 && deviceLimit === 0
        // 2) 设备上限不比会话大：设备的桶总是先满，会话这道永远轮不到判定，等于白配。
        const shadowedByDevice = parsed > 0 && deviceLimit > 0 && deviceLimit <= parsed
        return (
          <div className="flex flex-wrap items-center gap-2">
            <Badge variant="secondary" size="sm">
              {parsed > 0
                ? t(`每个会话每分钟最多 ${parsed} 条`, `Up to ${parsed} requests per minute per session`)
                : t('不限', 'Unlimited')}
            </Badge>
            {noDeviceBackstop && (
              <Badge variant="warning" size="sm">
                {t('更换会话 ID 即可绕过此限制，建议同时配置设备 RPM 上限', 'A new session ID bypasses this; set a per-device limit too')}
              </Badge>
            )}
            {shadowedByDevice && (
              <Badge variant="warning" size="sm">
                {t(
                  `设备 RPM 上限（${deviceLimit}）总会先触发，此项不会生效`,
                  `The per-device limit of ${deviceLimit} always trips first, so this limit never applies`,
                )}
              </Badge>
            )}
          </div>
        )
      }}
      success={(settings) => ({
        title: t('会话 RPM 上限已更新', 'Per-session RPM limit updated'),
        description: settings.session_rpm_limit > 0
          ? t(
              `每个会话每分钟最多转发 ${settings.session_rpm_limit} 条请求。`,
              `Each session forwards at most ${settings.session_rpm_limit} requests per minute.`,
            )
          : t('会话 RPM 上限已取消。', 'The per-session RPM limit has been removed.'),
      })}
    />
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
  const { t } = useI18n()
  return (
    <NumericSetting
      field="session_concurrency_limit"
      save={setSessionConcurrencyLimit}
      label={t('会话并发上限', 'Per-session concurrency limit')}
      description={t(
        '单个会话同时在途的最大请求数，超出后直接返回 429 并附带 retry-after。用于抑制 Claude Desktop 启动时缓存预热产生的突发请求（20 条以上并发），避免超出上游速率限制。0 表示不限。',
        'The maximum number of in-flight requests per session; beyond that it gets a 429 with retry-after. This curbs the cache-warming burst Claude Desktop fires on startup (20+ concurrent requests) so it does not exceed upstream rate limits. 0 means unlimited.',
      )}
      note={(parsed) => (
        <div className="flex flex-wrap items-center gap-2">
          <Badge variant="secondary" size="sm">
            {parsed > 0
              ? t(`每个会话最多 ${parsed} 条并发`, `Up to ${parsed} concurrent per session`)
              : t('不限', 'Unlimited')}
          </Badge>
        </div>
      )}
      success={(settings) => ({
        title: t('会话并发上限已更新', 'Per-session concurrency limit updated'),
        description: settings.session_concurrency_limit > 0
          ? t(
              `每个会话最多同时有 ${settings.session_concurrency_limit} 条请求在途。`,
              `Each session may have at most ${settings.session_concurrency_limit} requests in flight.`,
            )
          : t('会话并发上限已取消。', 'The per-session concurrency limit has been removed.'),
      })}
    />
  )
}

/** 设备身份校验开关：关掉后放行无 metadata.user_id 的裸请求。 */
function RequireDeviceIdToggle() {
  const { t } = useI18n()
  const { data } = useSettingsQuery()
  const required = data?.require_device_id ?? true

  const save = useSettingsSave((next: boolean) => setRequireDeviceId(next), {
    success: (settings) => ({
      title: settings.require_device_id
        ? t('设备身份校验已启用', 'Device identity checks enabled')
        : t('设备身份校验已停用', 'Device identity checks disabled'),
      description: settings.require_device_id
        ? t('缺少设备身份的请求会被拒绝。', 'Requests without a device identity will be rejected.')
        : t('无设备身份的请求将被放行。', 'Requests without a device identity will be allowed.'),
    }),
  })

  return (
    <SettingsRow
      inlineControl
      htmlFor="require-device-id"
      label={t('设备身份校验', 'Device identity checks')}
      badge={
        <Badge variant={required ? 'success' : 'warning'} size="sm" aria-live="polite">
          {required ? t('严格模式', 'Strict') : t('兼容模式', 'Compatible')}
        </Badge>
      }
      description={required
        ? t(
            '缺少设备身份的请求会被拒绝。',
            'Requests without a device identity will be rejected.',
          )
        : t(
            '无设备身份的请求将被放行，且不受设备上限限制。',
            'Requests without a device identity will be allowed and will not count toward device limits.',
          )}
    >
      <Switch
        id="require-device-id"
        checked={required}
        disabled={save.isPending}
        onCheckedChange={(next) => save.mutate(next)}
      />
    </SettingsRow>
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
  const { t } = useI18n()
  const { data } = useSettingsQuery()
  const [draft, setDraft] = useState('')

  useEffect(() => {
    if (data) setDraft(data.latest_cc_release)
  }, [data?.latest_cc_release])

  const save = useSettingsSave((version: string) => setLatestCcRelease(version), {
    success: (settings, version: string) => ({
      title: version
        ? t('官方最新版本已更新', 'Latest official release updated')
        : t('官方最新版本已清除', 'Latest official release cleared'),
      description: version
        ? t(
            `声明版本高于 ${effectiveLatestRelease(settings)} 的客户端将按非官方客户端处理；自动检查获取到更高版本时会覆盖此值。`,
            `Clients claiming a version newer than ${effectiveLatestRelease(settings)} are treated as unofficial; a newer version fetched by the automatic check will replace this value.`,
          )
        : t(
            `已恢复为基线 ${settings.cc_version_base}，下次自动检查（30 分钟内）将重新获取。`,
            `Back to the baseline ${settings.cc_version_base}; the next automatic check (within 30 minutes) will fetch it again.`,
          ),
    }),
  })

  const value = draft.trim()
  const current = data?.latest_cc_release ?? ''
  const base = data?.cc_version_base ?? ''
  const malformed = value !== '' && !RELEASE_RE.test(value)
  // 填一个不高于基线的值没有效果（上限取大），提示一下免得以为生效了。
  const belowBase = !malformed && value !== '' && base !== '' && compareRelease(value, base) < 0

  return (
    <SettingsRow
      htmlFor="latest-cc-release"
      label={t('官方最新 Claude Code 版本', 'Latest official Claude Code release')}
      description={t(
        `User-Agent 中声明版本高于此值的 claude-cli 不按官方客户端处理（改走模拟路径）。该值每 30 分钟从 downloads.claude.ai 自动获取一次，只升不降，重启后保留。官方刚发布新版而自动检查尚未执行时，可在此手动填写；清除后恢复为基线 ${base || '—'}，等待下次自动获取。`,
        `A claude-cli User-Agent claiming a version newer than this is not treated as an official client (it takes the simulation path). The value is fetched automatically from downloads.claude.ai every 30 minutes, never downgraded, and kept across restarts. Fill it in by hand when a new release has just shipped and the automatic check has not run yet; clearing it falls back to the baseline ${base || '—'} until the next automatic check.`,
      )}
      note={
        <Badge variant={malformed || belowBase ? 'warning' : 'secondary'} size="sm">
          {malformed
            ? t('格式示例：2.1.260','Expected something like 2.1.260')
            : belowBase
              ? t(`低于基线 ${base}，不会生效`, `Below the baseline ${base}; has no effect`)
              : current
                ? t(`当前上限 ${data ? effectiveLatestRelease(data) : current}`, `Current cap ${data ? effectiveLatestRelease(data) : current}`)
                : t(`尚未获取，按基线 ${base}`, `Not fetched yet; using the baseline ${base}`)}
        </Badge>
      }
    >
      <Input
        id="latest-cc-release"
        className="min-w-0 flex-1 sm:w-40 sm:flex-none"
        placeholder={base || '2.1.260'}
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
      />
      <Button
        loading={save.isPending && save.variables !== ''}
        disabled={malformed || value === '' || value === current}
        onClick={() => save.mutate(value)}
      >
        <SaveIcon />
        {t('保存', 'Save')}
      </Button>
      <Button
        variant="outline"
        loading={save.isPending && save.variables === ''}
        disabled={current === ''}
        onClick={() => save.mutate('')}
        aria-label={t('清除', 'Clear')}
      >
        <Trash2Icon />
        {t('清除', 'Clear')}
      </Button>
    </SettingsRow>
  )
}

/**
 * 最低 Claude Code 版本：UA 自报 `claude-cli/<版本>` 且低于此值的请求直接 403。
 *
 * 只是引导升级用的闸，不是安全边界——UA 是客户端自报的，改一个头就能绕过。
 */
function MinClientVersion() {
  const { t } = useI18n()
  const { data } = useSettingsQuery()
  const [draft, setDraft] = useState('')

  useEffect(() => {
    if (data) setDraft(data.min_client_version)
  }, [data?.min_client_version])

  const save = useSettingsSave((version: string) => setMinClientVersion(version), {
    success: (settings) => ({
      title: settings.min_client_version
        ? t('最低客户端版本已更新', 'Minimum client version updated')
        : t('最低客户端版本已取消', 'Minimum client version removed'),
      description: settings.min_client_version
        ? t(
            `低于 ${settings.min_client_version} 的 Claude Code 将被拒绝。`,
            `Claude Code older than ${settings.min_client_version} will be rejected.`,
          )
        : t('不再按版本拦截客户端。', 'Clients are no longer filtered by version.'),
    }),
  })

  const value = draft.trim()
  const current = data?.min_client_version ?? ''
  // 空串是合法输入（= 取消限制），只有写错格式才拦下——后端同样会 400，这里先拦一道免得白跑。
  const malformed = value !== '' && !VERSION_RE.test(value)

  return (
    <SettingsRow
      htmlFor="min-client-version"
      label={t('最低 Claude Code 版本', 'Minimum Claude Code version')}
      description={t(
        '低于该版本的 Claude Code 会收到 403 与升级提示，留空表示不限。仅检查 User-Agent 中的 claude-cli 版本，SDK、浏览器等其他客户端一律放行。注意：User-Agent 可被客户端伪造，此项仅用于引导升级，不能作为安全边界。',
        'Claude Code builds older than this get a 403 with an upgrade hint; leave empty for no limit. Only the claude-cli version in the User-Agent is checked, and SDKs, browsers, and other clients always pass. Note: a User-Agent can be forged, so treat this as an upgrade nudge, not a security boundary.',
      )}
      note={
        <Badge variant={malformed ? 'warning' : 'secondary'} size="sm">
          {malformed
            ? t('格式示例：2.1.220','Expected something like 2.1.220')
            : value
              ? t(`要求 ${value} 及以上`, `Requires ${value} or newer`)
              : t('不限版本', 'Any version')}
        </Badge>
      }
    >
      <Input
        id="min-client-version"
        className="min-w-0 flex-1 sm:w-40 sm:flex-none"
        placeholder="2.1.220"
        value={draft}
        onChange={(e) => setDraft(e.target.value)}
      />
      <Button
        loading={save.isPending}
        disabled={malformed || value === current}
        onClick={() => save.mutate(value)}
      >
        <SaveIcon />
        {t('保存', 'Save')}
      </Button>
    </SettingsRow>
  )
}

/** 裸请求速率上限：单个账号在窗口内最多接收的无设备身份请求。 */
function BareRateLimit() {
  const { language, t } = useI18n()
  const { data } = useSettingsQuery()
  const [draft, setDraft] = useState<number | null>(null)
  const [windowDraft, setWindowDraft] = useState<number | null>(null)

  useEffect(() => {
    if (data) {
      setDraft(data.bare_rate_limit)
      setWindowDraft(data.bare_rate_window_secs)
    }
  }, [data?.bare_rate_limit, data?.bare_rate_window_secs])

  const save = useSettingsSave(
    ({ limit, win }: { limit: number; win: number }) =>
      setBareRateLimit(limit, win),
    {
      success: (settings) => ({
        title: t(
          '无设备身份请求策略已更新',
          'Policy for requests without device identity updated',
        ),
        description: settings.bare_rate_limit > 0
          ? t(
              `每个账号 ${settings.bare_rate_limit} 条 / ${formatDuration(settings.bare_rate_window_secs, language)}。`,
              `${settings.bare_rate_limit} ${settings.bare_rate_limit === 1 ? 'request' : 'requests'} per account / ${formatDuration(settings.bare_rate_window_secs, language)}.`,
            )
          : t(
              '无设备身份请求的速率限制已取消。',
              'The rate limit for requests without a device identity has been removed.',
            ),
      }),
    },
  )

  const limit = Math.max(0, Math.floor(draft ?? 0))
  const win = Math.max(1, Math.floor(windowDraft ?? 60))
  const inactive = data?.require_device_id ?? true
  const unchanged = limit === (data?.bare_rate_limit ?? 0)
    && win === (data?.bare_rate_window_secs ?? 60)

  return (
    <SettingsRow
      label={t(
        '无设备身份请求上限（每个账号）',
        'Limit for requests without device identity (per account)',
      )}
      badge={
        <Badge variant={inactive ? 'secondary' : 'info'} size="sm">
          {inactive ? t('当前不生效', 'Inactive') : t('正在生效', 'Active')}
        </Badge>
      }
      description={inactive
        ? t(
            '设备身份校验已启用，无设备身份的请求会直接被拒绝。当前配置将保留，切换到兼容模式后自动生效。',
            'Device identity checks are enabled, so requests without device identity are rejected first. The configuration is kept and takes effect automatically in compatible mode.',
          )
        : t(
            '限制兼容模式下放行的无设备身份请求，0 表示不限速。',
            'Limits the requests without device identity that compatible mode allows; 0 means unlimited.',
          )}
      // 不生效时，「当前不生效」徽章加上面那句说明已经说全了；底部原来再说一遍「配置会保留、
      // 关闭校验后恢复」，并进了说明里。生效时底部才有东西要补（计数口径）。
      footer={!inactive && (
        <p className="mt-2 w-full border-t pt-4 text-xs leading-5 text-muted-foreground">
          {t(
            '仅统计无设备身份的消息请求，token 计数接口不计入；单个账号达到上限后自动切换账号，所有账号均达到上限时才拒绝请求。服务重启后重新计数。',
            'Only message requests without device identity are counted; token-counting requests are excluded. The proxy switches accounts when one reaches its limit and rejects only when every account is capped. Counters reset after a service restart.',
          )}
        </p>
      )}
    >
        <div className="grid w-full grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto] grid-rows-[auto_auto] items-center gap-3 sm:w-88">
          <Field className="row-span-2 grid grid-rows-subgrid gap-1.5">
            <FieldLabel>{t('请求数', 'Request count')}</FieldLabel>
            <NumberField disabled={inactive} min={0} value={draft} onValueChange={setDraft}>
              <NumberFieldGroup>
                <NumberFieldDecrement
                  aria-label={t(
                    '减少无设备身份请求上限',
                    'Decrease the limit for requests without device identity',
                  )}
                />
                <NumberFieldInput
                  aria-label={t(
                    '无设备身份请求上限（条）',
                    'Limit for requests without device identity',
                  )}
                />
                <NumberFieldIncrement
                  aria-label={t(
                    '增加无设备身份请求上限',
                    'Increase the limit for requests without device identity',
                  )}
                />
              </NumberFieldGroup>
            </NumberField>
          </Field>
          <Field className="row-span-2 grid grid-rows-subgrid gap-1.5">
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
          {/* 与转发页那行同理：按钮要和 NumberField 等高，故用默认尺寸而不是 `sm`。 */}
          <Button
            className="col-start-3 row-start-2 max-sm:size-9 max-sm:px-0"
            loading={save.isPending}
            disabled={inactive || unchanged || draft === null || windowDraft === null}
            onClick={() => save.mutate({ limit, win })}
          >
            <SaveIcon />
            <span className="max-sm:sr-only">{t('保存', 'Save')}</span>
          </Button>
        </div>
    </SettingsRow>
  )
}

/**
 * 管理密码：修改/清除（环境接管时只读）。
 *
 * 没有「首次设置」这一支：未设密码时管理接口一律拒绝，进不到设置页，首次设置只在初始化页
 * （`SetupPage`）做，且要带启动日志里的初始化口令。
 */
function AdminPassword() {
  const { language, t } = useI18n()
  const authQuery = useQuery({ queryKey: ['auth-state'], queryFn: getAuthState })
  const { data } = authQuery
  const [password, setPassword] = useState('')
  const [clearOpen, setClearOpen] = useState(false)

  const save = useMutation({
    mutationFn: changePassword,
    onSuccess: (_result, nextPassword) => {
      setClearOpen(false)
      if (nextPassword) {
        setPassword('')
        toastManager.add({
          title: t('管理密码已设置', 'Admin password set'),
          description: t('新的管理密码已生效，其他设备上的登录已退出。', 'The new admin password is active; sign-ins on other devices were signed out.'),
          type: 'success',
        })
        // 当前会话保留，不必重载。
        return
      } else {
        clearToken()
        toastManager.add({
          title: t('管理密码已清除', 'Admin password cleared'),
          description: t(
            '控制台已停用，须使用服务日志中的初始化口令重新设置密码。',
            'The console is locked until a new password is set with the setup token from the server log.',
          ),
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

  if (authQuery.isPending) {
    return (
      <Field className="p-4 sm:p-5">
        <FieldLabel>{t('管理密码（登录控制台所需）', 'Admin password (required for sign-in)')}</FieldLabel>
        <span className="inline-flex items-center gap-1.5 text-xs text-muted-foreground" role="status">
          <Spinner className="size-3" />
          {t('正在加载', 'Loading')}
        </span>
      </Field>
    )
  }

  if (authQuery.isError) {
    return (
      <Field className="p-4 sm:p-5">
        <FieldLabel>{t('管理密码（登录控制台所需）', 'Admin password (required for sign-in)')}</FieldLabel>
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
      <Field className="p-4 sm:p-5">
        <FieldLabel>{t('管理密码（登录控制台所需）', 'Admin password (required for sign-in)')}</FieldLabel>
        {envManaged ? (
          <FieldDescription>
            {t('由环境变量', 'Managed by environment variable')}{' '}
            <code className="font-mono">LUBAN_ADMIN_PASSWORD</code>
            {t(' 管理，此处只读。', '; this page is read-only.')}
          </FieldDescription>
        ) : (
          <>
            <div className="grid w-full gap-2 sm:grid-cols-[minmax(0,1fr)_auto_auto]">
              <Input
                aria-label={t('新管理密码', 'New admin password')}
                onChange={(event) => setPassword(event.target.value)}
                placeholder={t('输入新密码', 'Enter a new password')}
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
                {t('修改', 'Change')}
              </Button>
              <Button
                size="sm"
                variant="destructive-outline"
                disabled={save.isPending}
                onClick={() => setClearOpen(true)}
              >
                <Trash2Icon />
                {t('清除', 'Clear')}
              </Button>
            </div>
          </>
        )}
      </Field>

      <AlertDialog
        open={clearOpen}
        onOpenChange={(nextOpen) => {
          if (!save.isPending) setClearOpen(nextOpen)
        }}
      >
        <AlertDialogPopup>
          <AlertDialogHeader>
            <AlertDialogTitle>{t('清除管理密码', 'Clear admin password')}</AlertDialogTitle>
            <AlertDialogDescription>
              {t(
                '清除后控制台立即停用，包括本机访问；须使用服务日志中的初始化口令重新设置密码后才能再次进入。',
                'After clearing it, the console is locked for everyone, including this machine, until a new password is set with the setup token from the server log.',
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

/** 只读访客密码：设置 / 修改 / 停用。 */
function ViewerPassword() {
  const { language, t } = useI18n()
  const qc = useQueryClient()
  const me = useMe()
  const [password, setPassword] = useState('')
  const [clearOpen, setClearOpen] = useState(false)

  const save = useMutation({
    mutationFn: setViewerPassword,
    onSuccess: (_result, nextPassword) => {
      setClearOpen(false)
      setPassword('')
      toastManager.add({
        title: nextPassword
          ? t('访客密码已设置', 'Viewer password set')
          : t('访客访问已停用', 'Viewer access turned off'),
        description: nextPassword
          ? t('访客可用该密码以只读身份登录。', 'Viewers can now sign in read-only with this password.')
          : t('已登录的访客将在下次请求时退出。', 'Signed-in viewers are signed out on their next request.'),
        type: 'success',
      })
      void qc.invalidateQueries({ queryKey: ['auth-me'] })
    },
    onError: (error) => {
      toastManager.add({
        title: t('操作失败', 'Operation failed'),
        description: extractError(error, language),
        type: 'error',
      })
    },
  })

  const label = t('访客密码（只读登录）', 'Viewer password (read-only sign-in)')
  // 占位数据是登录时记下的身份，读不出是否已设；等真数据回来再画，免得「停用」按钮闪一下。
  if (me.isPending || me.isPlaceholderData) {
    return (
      <Field className="p-4 sm:p-5">
        <FieldLabel>{label}</FieldLabel>
        <span className="inline-flex items-center gap-1.5 text-xs text-muted-foreground" role="status">
          <Spinner className="size-3" />
          {t('正在加载', 'Loading')}
        </span>
      </Field>
    )
  }
  const configured = me.data?.viewer_configured ?? false

  return (
    <>
      <Field className="p-4 sm:p-5">
        <FieldLabel>{label}</FieldLabel>
        {me.data?.viewer_env_managed ? (
          <FieldDescription>
            {t('由环境变量', 'Managed by environment variable')}{' '}
            <code className="font-mono">LUBAN_VIEWER_PASSWORD</code>
            {t(' 管理，此处只读。访客以用户名 viewer 登录。', '; this page is read-only. Viewers sign in with the username viewer.')}
          </FieldDescription>
        ) : (
          <>
            <div className="grid w-full gap-2 sm:grid-cols-[minmax(0,1fr)_auto_auto]">
              <Input
                aria-label={t('新访客密码', 'New viewer password')}
                onChange={(event) => setPassword(event.target.value)}
                placeholder={configured ? t('输入新密码', 'Enter a new password') : t('设置访客密码', 'Set a viewer password')}
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
                  {t('停用', 'Turn off')}
                </Button>
              )}
            </div>
            <FieldDescription>
              {configured
                ? t('访客访问已启用，访客以用户名 viewer 登录。密码至少 4 个字符。', 'Viewer access is on; viewers sign in with the username viewer. At least 4 characters.')
                : t('尚未设置，访客访问处于停用状态。设置后访客以用户名 viewer 登录，密码至少 4 个字符。', 'Not set; viewer access is off. Once set, viewers sign in with the username viewer. At least 4 characters.')}
            </FieldDescription>
          </>
        )}
      </Field>

      <AlertDialog
        open={clearOpen}
        onOpenChange={(nextOpen) => {
          if (!save.isPending) setClearOpen(nextOpen)
        }}
      >
        <AlertDialogPopup>
          <AlertDialogHeader>
            <AlertDialogTitle>{t('停用访客访问', 'Turn off viewer access')}</AlertDialogTitle>
            <AlertDialogDescription>
              {t(
                '清除访客密码后，已登录的访客会在下次请求时被退出，之后无法再以访客身份登录，直到重新设置。',
                'Clearing the viewer password signs out any viewer on their next request, and nobody can sign in as a viewer until a new one is set.',
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
              {t('确认停用', 'Turn off')}
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
  disabledReason,
  size = 'icon-xs',
}: {
  text: string
  /** 给了就禁用按钮，并把原因放进提示。 */
  disabledReason?: string
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
      {/* 提示挂在外层 span 上：禁用的 Button 带 pointer-events-none，挂在它自己身上时
          「为什么不能复制」那句永远悬停不出来。 */}
      <Hint label={copied ? successLabel : disabledReason ?? idleLabel}>
        <span className="inline-flex">
          <Button
            type="button"
            aria-label={copied ? successLabel : disabledReason ?? idleLabel}
            className={copied ? 'text-success-foreground' : undefined}
            disabled={disabledReason !== undefined}
            size={size}
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
        </span>
      </Hint>
      <span className="sr-only" role="status" aria-live="polite">
        {copied ? successLabel : ''}
      </span>
    </>
  )
}
