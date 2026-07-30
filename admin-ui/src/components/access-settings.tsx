import { useEffect, useState, type ReactNode } from 'react'
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
  setDeviceTtl,
  setRequireDeviceId,
  type Settings,
} from '@/api/settings'
import { changePassword, getAuthState, setup as setupPassword } from '@/api/auth'
import { clearPw, setPw } from '@/api/client'
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
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogPopup className="sm:max-w-2xl">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <Settings2Icon aria-hidden="true" />
            接入设置
          </DialogTitle>
          <DialogDescription>配置客户端接入、设备策略和控制台安全。</DialogDescription>
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
  const settingsQuery = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const { data } = settingsQuery

  const [draft, setDraft] = useState('')
  const [show, setShow] = useState(false)
  const [clearKeyOpen, setClearKeyOpen] = useState(false)

  useEffect(() => {
    setDraft(data?.api_key ?? '')
  }, [data?.api_key])

  const save = useMutation({
    mutationFn: (key: string) => setApiKey(key),
    onSuccess: (settings: Settings) => {
      setClearKeyOpen(false)
      toastManager.add({
        title: settings.api_key ? '接入 Key 已保存' : '接入 Key 已清除',
        description: settings.api_key
          ? '新的客户端接入 Key 已生效。'
          : '代理将不再校验来访客户端。',
        type: 'success',
      })
      qc.setQueryData(['settings'], settings)
    },
    onError: (error) => {
      toastManager.add({
        title: '保存失败',
        description: extractError(error),
        type: 'error',
      })
    },
  })

  const baseUrl = window.location.origin
  const envManaged = data?.env_managed ?? false
  const currentKey = data?.api_key ?? ''

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
      : '# 未设置 Key，无需 ANTHROPIC_AUTH_TOKEN')

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
    <>
      <div className="space-y-4">
        <SettingsSection title="客户端接入">
          <Field className="p-5">
            <FieldLabel>
              接入地址
              <code className="text-xs font-normal text-muted-foreground">ANTHROPIC_BASE_URL</code>
            </FieldLabel>
            <InputGroup>
              <InputGroupInput
                aria-label="接入地址"
                readOnly
                value={baseUrl}
              />
              <InputGroupAddon align="inline-end">
                <CopyButton text={baseUrl} />
              </InputGroupAddon>
            </InputGroup>
          </Field>

          <Field className="p-5">
            <FieldLabel>
              接入 Key
              <code className="text-xs font-normal text-muted-foreground">ANTHROPIC_AUTH_TOKEN</code>
            </FieldLabel>
            <InputGroup>
              <InputGroupInput
                aria-label="接入 Key"
                onChange={(event) => setDraft(event.target.value)}
                placeholder={envManaged ? '' : '留空则不校验来访'}
                readOnly={envManaged}
                type={show ? 'text' : 'password'}
                value={draft}
              />
              <InputGroupAddon align="inline-end">
                <Button
                  aria-label={show ? '隐藏接入 Key' : '显示接入 Key'}
                  size="icon-xs"
                  title={show ? '隐藏' : '显示'}
                  variant="ghost"
                  onClick={() => setShow((visible) => !visible)}
                >
                  {show ? <EyeOffIcon /> : <EyeIcon />}
                </Button>
                <CopyButton text={draft} />
              </InputGroupAddon>
            </InputGroup>
            {!envManaged && (
              <div className="flex flex-wrap items-center gap-2">
                <Button size="sm" variant="outline" onClick={generate}>
                  <SparklesIcon />
                  生成
                </Button>
                <Button
                  size="sm"
                  loading={save.isPending}
                  disabled={draft === currentKey}
                  onClick={() => save.mutate(draft.trim())}
                >
                  <SaveIcon />
                  保存
                </Button>
                {currentKey && (
                  <Button
                    size="sm"
                    variant="destructive-outline"
                    onClick={() => setClearKeyOpen(true)}
                  >
                    <Trash2Icon />
                    清空
                  </Button>
                )}
              </div>
            )}
            {envManaged && (
              <FieldDescription>
                由环境变量 <code>LUBAN_API_KEY</code> 接管，网页只读。
              </FieldDescription>
            )}
          </Field>

          <Field className="p-5">
            <FieldLabel>Claude Code 接入片段</FieldLabel>
            <div className="relative w-full min-w-0">
              <pre className="max-w-full overflow-x-auto rounded-lg border bg-muted/72 p-3 pr-10 text-xs leading-5">
                {snippet}
              </pre>
              <div className="absolute right-2 top-2">
                <CopyButton text={snippet} />
              </div>
            </div>
          </Field>
        </SettingsSection>

        <SettingsSection title="设备策略">
          <DeviceBindingTtl />
          <DefaultDeviceLimit />
          <RequireDeviceIdToggle />
          <BareRateLimit />
        </SettingsSection>

        <SettingsSection title="控制台安全">
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
            <AlertDialogTitle>清除接入 Key</AlertDialogTitle>
            <AlertDialogDescription>清除后，代理将不再校验客户端身份。</AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogClose render={<Button disabled={save.isPending} variant="ghost" />}>
              取消
            </AlertDialogClose>
            <Button
              loading={save.isPending}
              variant="destructive"
              onClick={() => save.mutate('')}
            >
              确认清除
            </Button>
          </AlertDialogFooter>
        </AlertDialogPopup>
      </AlertDialog>
    </>
  )
}

/** 设备绑定有效期：设备超过该时长无请求即释放绑定、腾出凭证名额。0 = 永不过期。 */
function DeviceBindingTtl() {
  const qc = useQueryClient()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const [draft, setDraft] = useState<number | null>(null)

  useEffect(() => {
    if (data) setDraft(data.device_binding_ttl_secs)
  }, [data?.device_binding_ttl_secs])

  const save = useMutation({
    mutationFn: (seconds: number) => setDeviceTtl(seconds),
    onSuccess: (settings: Settings) => {
      toastManager.add({
        title: '设备策略已更新',
        description: '设备绑定有效期已保存。',
        type: 'success',
      })
      qc.setQueryData(['settings'], settings)
      qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (error) => {
      toastManager.add({ title: '保存失败', description: extractError(error), type: 'error' })
    },
  })

  const current = data?.device_binding_ttl_secs ?? 0
  const parsed = Math.max(0, Math.floor(draft ?? 0))
  const hint = parsed > 0 ? `相当于 ${formatDuration(parsed)}` : '永不过期（绑定长期保留）'

  return (
    <Field className="p-5">
      <FieldLabel>设备绑定有效期（秒）</FieldLabel>
      <div className="flex w-full flex-wrap items-center gap-2">
        <NumberField
          className="w-full sm:w-44"
          min={0}
          value={draft}
          onValueChange={setDraft}
        >
          <NumberFieldGroup>
            <NumberFieldDecrement />
            <NumberFieldInput aria-label="设备绑定有效期（秒）" />
            <NumberFieldIncrement />
          </NumberFieldGroup>
        </NumberField>
        <Button
          size="sm"
          loading={save.isPending}
          disabled={parsed === current}
          onClick={() => save.mutate(parsed)}
        >
          <SaveIcon />
          保存
        </Button>
        <span className="text-xs text-muted-foreground">{hint}</span>
      </div>
      <FieldDescription>超时无请求的设备会自动释放绑定。</FieldDescription>
    </Field>
  )
}

/** 全局默认设备上限：账号未单独配置时套用。 */
function DefaultDeviceLimit() {
  const qc = useQueryClient()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const [draft, setDraft] = useState<number | null>(null)

  useEffect(() => {
    if (data) setDraft(data.default_device_limit)
  }, [data?.default_device_limit])

  const save = useMutation({
    mutationFn: (limit: number) => setDefaultDeviceLimit(limit),
    onSuccess: (settings: Settings) => {
      toastManager.add({
        title: '默认设备上限已更新',
        description: settings.default_device_limit > 0
          ? `每个账号最多绑定 ${settings.default_device_limit} 台设备。`
          : '默认设备上限已取消。',
        type: 'success',
      })
      qc.setQueryData(['settings'], settings)
      qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (error) => {
      toastManager.add({ title: '保存失败', description: extractError(error), type: 'error' })
    },
  })

  const current = data?.default_device_limit ?? 0
  const parsed = Math.max(0, Math.floor(draft ?? 0))

  return (
    <Field className="p-5">
      <FieldLabel>默认设备上限（每个账号）</FieldLabel>
      <div className="flex w-full flex-wrap items-center gap-2">
        <NumberField
          className="w-full sm:w-44"
          min={0}
          value={draft}
          onValueChange={setDraft}
        >
          <NumberFieldGroup>
            <NumberFieldDecrement />
            <NumberFieldInput aria-label="默认设备上限" />
            <NumberFieldIncrement />
          </NumberFieldGroup>
        </NumberField>
        <Button
          size="sm"
          loading={save.isPending}
          disabled={parsed === current}
          onClick={() => save.mutate(parsed)}
        >
          <SaveIcon />
          保存
        </Button>
        <span className="text-xs text-muted-foreground">
          {parsed > 0 ? `每个账号最多 ${parsed} 台设备` : '不限（不设默认上限）'}
        </span>
      </div>
      <FieldDescription>未单独配置的账号使用此上限；账号独立设置优先。</FieldDescription>
    </Field>
  )
}

/** 设备身份校验开关：关掉后放行无 metadata.user_id 的裸请求。 */
function RequireDeviceIdToggle() {
  const qc = useQueryClient()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const required = data?.require_device_id ?? true

  const save = useMutation({
    mutationFn: (next: boolean) => setRequireDeviceId(next),
    onSuccess: (settings: Settings) => {
      toastManager.add({
        title: settings.require_device_id ? '设备身份校验已开启' : '设备身份校验已关闭',
        description: settings.require_device_id
          ? '缺少设备身份的请求会被拒绝。'
          : '无设备身份的请求将被放行。',
        type: 'success',
      })
      qc.setQueryData(['settings'], settings)
    },
    onError: (error) => {
      toastManager.add({ title: '保存失败', description: extractError(error), type: 'error' })
    },
  })

  return (
    <Field className="p-5">
      <div className="flex w-full items-start justify-between gap-4">
        <div className="min-w-0 space-y-1">
          <FieldLabel htmlFor="require-device-id">设备身份校验</FieldLabel>
          <FieldDescription>
            {required
              ? '缺少设备身份的请求会被拒绝。'
              : '无设备身份的请求将被放行，且不受设备上限限制。'}
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
        title: '无设备身份请求策略已更新',
        description: settings.bare_rate_limit > 0
          ? `每个账号 ${settings.bare_rate_limit} 条 / ${formatDuration(settings.bare_rate_window_secs)}。`
          : '裸请求速率限制已取消。',
        type: 'success',
      })
      qc.setQueryData(['settings'], settings)
    },
    onError: (error) => {
      toastManager.add({ title: '保存失败', description: extractError(error), type: 'error' })
    },
  })

  const limit = Math.max(0, Math.floor(draft ?? 0))
  const win = Math.max(1, Math.floor(windowDraft ?? 60))
  const unchanged = limit === (data?.bare_rate_limit ?? 0)
    && win === (data?.bare_rate_window_secs ?? 60)

  return (
    <div className="space-y-3 p-5">
      <div>
        <p className="font-medium text-sm">无设备身份请求上限（每个账号）</p>
        <p className="mt-1 text-xs text-muted-foreground">分别配置请求数和统计窗口。</p>
      </div>
      <div className="grid gap-3 sm:grid-cols-2">
        <Field>
          <FieldLabel>请求数</FieldLabel>
          <NumberField min={0} value={draft} onValueChange={setDraft}>
            <NumberFieldGroup>
              <NumberFieldDecrement />
              <NumberFieldInput aria-label="无设备身份请求上限（条）" />
              <NumberFieldIncrement />
            </NumberFieldGroup>
          </NumberField>
        </Field>
        <Field>
          <FieldLabel>时间窗口（秒）</FieldLabel>
          <NumberField min={1} value={windowDraft} onValueChange={setWindowDraft}>
            <NumberFieldGroup>
              <NumberFieldDecrement />
              <NumberFieldInput aria-label="无设备身份请求窗口（秒）" />
              <NumberFieldIncrement />
            </NumberFieldGroup>
          </NumberField>
        </Field>
      </div>
      <Button
        size="sm"
        loading={save.isPending}
        disabled={unchanged}
        onClick={() => save.mutate({ limit, win })}
      >
        <SaveIcon />
        保存
      </Button>
      <p className="text-xs leading-5 text-muted-foreground">
        仅统计无设备身份的消息请求，Token 计数接口不计入；单个账号达到上限后会自动换号，
        全部达到上限才拒绝。服务重启后重新计数。
      </p>
    </div>
  )
}

/** 管理密码：未设置→设置；已设置→修改/清除（环境接管时只读）。 */
function AdminPassword() {
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
          title: '管理密码已设置',
          description: '新的管理密码已生效。',
          type: 'success',
        })
      } else {
        clearPw()
        toastManager.add({
          title: '管理密码已清除',
          description: '控制台将不再要求登录。',
          type: 'success',
        })
      }
      window.location.reload()
    },
    onError: (error) => {
      toastManager.add({ title: '操作失败', description: extractError(error), type: 'error' })
    },
  })

  const envManaged = data?.env_managed ?? false
  const configured = data?.configured ?? false

  if (authQuery.isPending) {
    return (
      <Field className="p-5">
        <FieldLabel>管理密码（登录网页所需）</FieldLabel>
        <span className="inline-flex items-center gap-1.5 text-xs text-muted-foreground" role="status">
          <Spinner className="size-3" />
          正在加载
        </span>
      </Field>
    )
  }

  if (authQuery.isError) {
    return (
      <Field className="p-5">
        <FieldLabel>管理密码（登录网页所需）</FieldLabel>
        <div className="flex flex-wrap items-center gap-2">
          <span className="text-xs text-destructive-foreground">无法读取登录状态</span>
          <Button
            size="sm"
            variant="outline"
            loading={authQuery.isFetching}
            onClick={() => authQuery.refetch()}
          >
            重试
          </Button>
        </div>
      </Field>
    )
  }

  return (
    <>
      <Field className="p-5">
        <FieldLabel>管理密码（登录网页所需）</FieldLabel>
        {envManaged ? (
          <FieldDescription>
            由环境变量 <code>LUBAN_ADMIN_PASSWORD</code> 接管，网页只读。
          </FieldDescription>
        ) : (
          <>
            <div className="grid w-full gap-2 sm:grid-cols-[minmax(0,1fr)_auto_auto]">
              <Input
                aria-label={configured ? '新管理密码' : '管理密码'}
                onChange={(event) => setPassword(event.target.value)}
                placeholder={configured ? '输入新密码' : '至少 4 位'}
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
                {configured ? '修改' : '设置'}
              </Button>
              {configured && (
                <Button
                  size="sm"
                  variant="destructive-outline"
                  disabled={save.isPending}
                  onClick={() => setClearOpen(true)}
                >
                  <Trash2Icon />
                  清除
                </Button>
              )}
            </div>
            {!configured && (
              <FieldDescription>
                未设置密码时，任何能访问控制台的设备都无需登录；对外开放时建议设置。
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
            <AlertDialogTitle>清除管理密码</AlertDialogTitle>
            <AlertDialogDescription>清除后，控制台将不再要求登录。</AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogClose render={<Button disabled={save.isPending} variant="ghost" />}>
              取消
            </AlertDialogClose>
            <Button
              loading={save.isPending}
              variant="destructive"
              onClick={() => save.mutate('')}
            >
              确认清除
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

function CopyButton({ text }: { text: string }) {
  const [copied, setCopied] = useState(false)

  return (
    <Button
      aria-label={copied ? '已复制' : '复制'}
      className={copied ? 'text-success' : undefined}
      size="icon-xs"
      title="复制"
      variant="ghost"
      onClick={async () => {
        if (!text) return
        if (await copyText(text)) {
          setCopied(true)
          window.setTimeout(() => setCopied(false), 1200)
          return
        }
        toastManager.add({
          title: '复制失败',
          description: '请手动选择并复制内容。',
          type: 'error',
        })
      }}
    >
      {copied ? <CheckIcon /> : <ClipboardIcon />}
    </Button>
  )
}
