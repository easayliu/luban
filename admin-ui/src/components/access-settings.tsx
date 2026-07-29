import { useEffect, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  Cog6ToothIcon, EyeIcon, EyeSlashIcon, ClipboardDocumentIcon, CheckIcon, SparklesIcon,
  ArrowDownTrayIcon, TrashIcon, ArrowPathIcon, LockClosedIcon, KeyIcon,
} from '@heroicons/react/24/outline'
import { toast } from 'sonner'
import {
  getSettings, setApiKey, setDefaultDeviceLimit, setDeviceTtl, setRequireDeviceId,
  type Settings,
} from '@/api/settings'
import { getAuthState, setup as setupPassword, changePassword } from '@/api/auth'
import { setPw, clearPw } from '@/api/client'
import { cn, copyText, extractError, formatDuration } from '@/lib/utils'
import {
  Dialog, DialogContent, DialogHeader, DialogBody, DialogTitle, DialogDescription,
} from '@/components/ui/dialog'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Badge } from '@/components/ui/badge'
import { Switch } from '@/components/ui/switch'
import { ConfirmDialog } from '@/components/ui/confirm-dialog'

export function AccessSettings({
  open, onOpenChange,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const qc = useQueryClient()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })

  const [draft, setDraft] = useState('')
  const [show, setShow] = useState(false)
  const [clearKeyOpen, setClearKeyOpen] = useState(false)
  useEffect(() => { setDraft(data?.api_key ?? '') }, [data?.api_key])

  const save = useMutation({
    mutationFn: (key: string) => setApiKey(key),
    onSuccess: (s: Settings) => {
      setClearKeyOpen(false)
      toast.success(s.api_key ? '接入 Key 已保存' : '已清除，代理不再校验来访')
      qc.invalidateQueries({ queryKey: ['settings'] })
    },
    onError: (e) => toast.error('保存失败', { description: extractError(e) }),
  })

  const baseUrl = window.location.origin
  const envManaged = data?.env_managed ?? false
  const currentKey = data?.api_key ?? ''

  const generate = () => {
    const bytes = new Uint8Array(24)
    crypto.getRandomValues(bytes)
    const hex = Array.from(bytes).map((b) => b.toString(16).padStart(2, '0')).join('')
    setDraft('luban-' + hex)
    setShow(true)
  }

  const snippet =
    `export ANTHROPIC_BASE_URL=${baseUrl}\n` +
    (currentKey ? `export ANTHROPIC_AUTH_TOKEN=${currentKey}` : '# 未设置 Key，无需 ANTHROPIC_AUTH_TOKEN')

  return (
    <>
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-2xl">
        <DialogHeader>
          <DialogTitle>
            <Cog6ToothIcon className="size-4" />
            接入设置
            {envManaged && (
              <Badge variant="outline" className="gap-1"><LockClosedIcon className="size-3" />环境接管</Badge>
            )}
          </DialogTitle>
          <DialogDescription>管理客户端接入、设备策略和控制台安全。</DialogDescription>
        </DialogHeader>
        <DialogBody className="space-y-3 bg-muted/20">
          <SettingsSection title="客户端接入">
            <Field label="接入地址" code="ANTHROPIC_BASE_URL">
              <div className="flex items-center gap-2">
                <Input readOnly value={baseUrl} className="font-mono" aria-label="接入地址" />
                <CopyBtn text={baseUrl} />
              </div>
            </Field>

            <Field label="接入 Key" code="ANTHROPIC_AUTH_TOKEN">
              <div className="space-y-2">
                <div className="flex min-w-0 items-center gap-1">
                  <Input
                    type={show ? 'text' : 'password'}
                    value={draft}
                    onChange={(e) => setDraft(e.target.value)}
                    readOnly={envManaged}
                    placeholder={envManaged ? '' : '留空则不校验来访'}
                    className="font-mono"
                    aria-label="接入 Key"
                  />
                  <Button size="icon" variant="ghost" className="shrink-0" onClick={() => setShow((s) => !s)} title={show ? '隐藏' : '显示'} aria-label={show ? '隐藏接入 Key' : '显示接入 Key'}>
                    {show ? <EyeSlashIcon /> : <EyeIcon />}
                  </Button>
                  <CopyBtn text={draft} />
                </div>
                {!envManaged && (
                  <div className="flex flex-wrap items-center gap-2">
                    <Button size="sm" variant="outline" onClick={generate}><SparklesIcon />生成</Button>
                    <Button size="sm" onClick={() => save.mutate(draft.trim())} disabled={save.isPending || draft === currentKey}>
                      {save.isPending ? <ArrowPathIcon className="animate-spin" /> : <ArrowDownTrayIcon />}保存
                    </Button>
                    {currentKey && (
                      <Button size="sm" variant="ghost" className="text-bad hover:text-bad"
                        onClick={() => setClearKeyOpen(true)}>
                        <TrashIcon />清空
                      </Button>
                    )}
                  </div>
                )}
              </div>
              {envManaged && (
                <p className="text-xs text-muted-foreground">
                  由环境变量 <code className="font-mono">LUBAN_API_KEY</code> 接管，网页只读。
                </p>
              )}
            </Field>

            <Field label="Claude Code 接入片段">
              <div className="relative">
                <pre className="scrollbar-dialog overflow-x-auto rounded-md border border-border bg-muted/40 p-3 pr-11 font-mono text-2xs leading-5">{snippet}</pre>
                <div className="absolute right-1.5 top-1.5"><CopyBtn text={snippet} /></div>
              </div>
            </Field>
          </SettingsSection>

          <SettingsSection title="设备策略">
            <DeviceBindingTtl />
            <DefaultDeviceLimit />
            <RequireDeviceIdToggle />
          </SettingsSection>

          <SettingsSection title="控制台安全">
            <AdminPassword />
          </SettingsSection>
        </DialogBody>
      </DialogContent>
    </Dialog>
    <ConfirmDialog
      open={clearKeyOpen}
      onOpenChange={setClearKeyOpen}
      title="清除接入 Key"
      description="清除后，代理将不再校验客户端身份。"
      confirmText="确认清除"
      pending={save.isPending}
      onConfirm={() => save.mutate('')}
    />
    </>
  )
}

/** 设备绑定有效期：设备超过该时长无请求即释放绑定、腾出凭证名额。0 = 永不过期。 */
function DeviceBindingTtl() {
  const qc = useQueryClient()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const [draft, setDraft] = useState('')
  useEffect(() => {
    if (data) setDraft(String(data.device_binding_ttl_secs))
  }, [data?.device_binding_ttl_secs])

  const save = useMutation({
    mutationFn: (secs: number) => setDeviceTtl(secs),
    onSuccess: () => {
      toast.success('设备绑定有效期已更新')
      qc.invalidateQueries({ queryKey: ['settings'] })
      qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (e) => toast.error('保存失败', { description: extractError(e) }),
  })

  const current = data?.device_binding_ttl_secs ?? 0
  const parsed = Math.max(0, Math.floor(Number(draft) || 0))
  const hint = parsed > 0 ? `= ${formatDuration(parsed)}` : '永不过期（绑定长期保留）'

  return (
    <Field label="设备绑定有效期（秒）">
      <div className="flex flex-wrap items-center gap-2">
        <Input
          type="number"
          min={0}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          className="w-full font-mono sm:w-40"
          aria-label="设备绑定有效期（秒）"
        />
        <Button size="sm" onClick={() => save.mutate(parsed)} disabled={save.isPending || parsed === current}>
          {save.isPending ? <ArrowPathIcon className="animate-spin" /> : <ArrowDownTrayIcon />}保存
        </Button>
        <span className="text-xs text-muted-foreground">{hint}</span>
      </div>
      <p className="mt-1.5 text-xs leading-5 text-muted-foreground">
        超时无请求的设备会自动释放绑定；填 0 表示永不过期。
      </p>
    </Field>
  )
}

/** 全局默认设备上限：账号未单独配置（卡片上显示「默认」）时套用，免去逐个账号设置。 */
function DefaultDeviceLimit() {
  const qc = useQueryClient()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const [draft, setDraft] = useState('')
  useEffect(() => {
    if (data) setDraft(String(data.default_device_limit))
  }, [data?.default_device_limit])

  const save = useMutation({
    mutationFn: (n: number) => setDefaultDeviceLimit(n),
    onSuccess: (s: Settings) => {
      toast.success(s.default_device_limit > 0
        ? `默认设备上限已设为 ${s.default_device_limit}`
        : '默认设备上限已取消（默认不限）')
      qc.invalidateQueries({ queryKey: ['settings'] })
      // 生效上限随之变化，账号卡片上的「设备 x/y」需要重取。
      qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (e) => toast.error('保存失败', { description: extractError(e) }),
  })

  const current = data?.default_device_limit ?? 0
  const parsed = Math.max(0, Math.floor(Number(draft) || 0))

  return (
    <Field label="默认设备上限（每个账号）">
      <div className="flex flex-wrap items-center gap-2">
        <Input
          type="number"
          min={0}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          className="w-full font-mono sm:w-40"
          aria-label="默认设备上限"
        />
        <Button size="sm" onClick={() => save.mutate(parsed)} disabled={save.isPending || parsed === current}>
          {save.isPending ? <ArrowPathIcon className="animate-spin" /> : <ArrowDownTrayIcon />}保存
        </Button>
        <span className="text-xs text-muted-foreground">
          {parsed > 0 ? `每个账号最多 ${parsed} 台设备` : '不限（不设默认上限）'}
        </span>
      </div>
      <p className="mt-1.5 text-xs leading-5 text-muted-foreground">
        未单独配置的账号跟随此值；账号卡片中的独立设置会覆盖它。
      </p>
    </Field>
  )
}

/** 设备身份校验开关：关掉后放行无 metadata.user_id 的裸请求（它们不占设备名额、不受上限约束）。 */
function RequireDeviceIdToggle() {
  const qc = useQueryClient()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const required = data?.require_device_id ?? true

  const save = useMutation({
    mutationFn: (next: boolean) => setRequireDeviceId(next),
    onSuccess: (s: Settings) => {
      toast.success(s.require_device_id ? '已开启设备身份校验' : '已关闭，无设备身份的请求将被放行')
      qc.invalidateQueries({ queryKey: ['settings'] })
    },
    onError: (e) => toast.error('保存失败', { description: extractError(e) }),
  })

  return (
    <Field label="设备身份校验">
      <div className="flex items-center justify-between gap-3 rounded-md border border-border px-3 py-2.5">
        <span className="text-sm">{required ? '已开启' : '已关闭'}</span>
        <Switch
          variant="success"
          checked={required}
          disabled={save.isPending}
          aria-label="设备身份校验"
          onCheckedChange={(next) => save.mutate(next)}
        />
      </div>
      <p className="mt-1.5 text-xs leading-5 text-muted-foreground">
        开启后，缺少 <code className="font-mono">metadata.user_id</code> 的请求会返回 403。
        {!required && (
          <span className="text-warn">
            {' '}当前已关闭：这类请求会被转发，但不绑定账号、不占设备名额，因而绕过设备上限。
          </span>
        )}
      </p>
    </Field>
  )
}

/** 管理密码：未设置→设置；已设置→修改/清除（环境接管时只读）。 */
function AdminPassword() {
  const { data } = useQuery({ queryKey: ['auth-state'], queryFn: getAuthState })
  const [pw, setPwInput] = useState('')
  const [clearOpen, setClearOpen] = useState(false)

  const save = useMutation({
    mutationFn: async (password: string) => {
      if (data?.configured) await changePassword(password)
      else await setupPassword(password)
    },
    onSuccess: (_r, password) => {
      setClearOpen(false)
      if (password) { setPw(password); toast.success('管理密码已设置') }
      else { clearPw(); toast.success('已清除管理密码') }
      window.location.reload()
    },
    onError: (e) => toast.error('操作失败', { description: extractError(e) }),
  })

  const envManaged = data?.env_managed ?? false
  const configured = data?.configured ?? false

  return (
    <>
    <Field label="管理密码（登录网页所需）">
      {envManaged ? (
        <p className="text-xs text-muted-foreground">
          由环境变量 <code className="font-mono">LUBAN_ADMIN_PASSWORD</code> 接管，网页只读。
        </p>
      ) : (
        <>
          <div className="grid gap-2 sm:flex sm:items-center">
            <Input
              type="password"
              value={pw}
              onChange={(e) => setPwInput(e.target.value)}
              placeholder={configured ? '输入新密码以修改' : '设置密码（至少 4 位，之后登录需要）'}
              className="min-w-0 flex-1"
              aria-label={configured ? '新管理密码' : '管理密码'}
            />
            <Button size="sm" className="w-full sm:w-auto" onClick={() => save.mutate(pw.trim())} disabled={save.isPending || pw.trim().length < 4}>
              {save.isPending ? <ArrowPathIcon className="animate-spin" /> : <KeyIcon />}
              {configured ? '修改' : '设置'}
            </Button>
            {configured && (
              <Button size="sm" variant="ghost" className="w-full text-bad hover:text-bad sm:w-auto"
                onClick={() => setClearOpen(true)}
                disabled={save.isPending}>
                <TrashIcon />清除
              </Button>
            )}
          </div>
          {!configured && (
            <p className="mt-1.5 text-xs text-muted-foreground">
              未设置时网页对同网段开放；绑定 0.0.0.0 时建议设置。
            </p>
          )}
        </>
      )}
    </Field>
    <ConfirmDialog
      open={clearOpen}
      onOpenChange={setClearOpen}
      title="清除管理密码"
      description="清除后，控制台将不再要求登录。"
      confirmText="确认清除"
      pending={save.isPending}
      onConfirm={() => save.mutate('')}
    />
    </>
  )
}

function SettingsSection({ title, children }: { title: string; children: React.ReactNode }) {
  return (
    <section className="overflow-hidden rounded-lg border border-border bg-card">
      <h3 className="border-b border-border bg-muted/30 px-3 py-3 text-sm font-semibold sm:px-4">{title}</h3>
      <div className="divide-y divide-border px-3 sm:px-4 [&>*]:py-4 [&>*:first-child]:pt-4 [&>*:last-child]:pb-4">{children}</div>
    </section>
  )
}

function Field({ label, code, children }: { label: string; code?: string; children: React.ReactNode }) {
  return (
    <div className="space-y-2">
      <div className="flex flex-wrap items-baseline gap-1.5 text-sm font-medium">
        <span>{label}</span>
        {code && <code className="font-mono text-2xs font-normal text-muted-foreground">{code}</code>}
      </div>
      <div>{children}</div>
    </div>
  )
}

function CopyBtn({ text }: { text: string }) {
  const [ok, setOk] = useState(false)
  return (
    <Button
      size="icon"
      variant="ghost"
      className={cn('h-9 w-9 shrink-0', ok && 'text-ok')}
      title="复制"
      aria-label={ok ? '已复制' : '复制'}
      onClick={async () => {
        if (!text) return
        if (await copyText(text)) {
          setOk(true)
          setTimeout(() => setOk(false), 1200)
        } else {
          toast.error('复制失败，请手动选择复制')
        }
      }}
    >
      {ok ? <CheckIcon /> : <ClipboardDocumentIcon />}
    </Button>
  )
}
