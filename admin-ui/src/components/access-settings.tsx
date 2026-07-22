import { useEffect, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  Settings2, Eye, EyeOff, Copy, Check, Dices, Save, Trash2, Loader2, Lock, KeyRound,
} from 'lucide-react'
import { toast } from 'sonner'
import { getSettings, setApiKey, setDeviceTtl, type Settings } from '@/api/settings'
import { getAuthState, setup as setupPassword, changePassword } from '@/api/auth'
import { setPw, clearPw } from '@/api/client'
import { cn, copyText, extractError, formatDuration } from '@/lib/utils'
import { Card, CardContent, CardHeader, CardTitle, CardDescription } from '@/components/ui/card'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'
import { Badge } from '@/components/ui/badge'

export function AccessSettings() {
  const qc = useQueryClient()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })

  const [draft, setDraft] = useState('')
  const [show, setShow] = useState(false)
  useEffect(() => { setDraft(data?.api_key ?? '') }, [data?.api_key])

  const save = useMutation({
    mutationFn: (key: string) => setApiKey(key),
    onSuccess: (s: Settings) => {
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
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Settings2 className="size-4" />
          接入设置
          {envManaged && (
            <Badge variant="outline" className="gap-1"><Lock className="size-3" />环境接管</Badge>
          )}
        </CardTitle>
        <CardDescription>Claude Code 用下面的地址与 Key 接入 luban。</CardDescription>
      </CardHeader>
      <CardContent className="space-y-4">
        {/* 接入地址 */}
        <Field label="接入地址（ANTHROPIC_BASE_URL）">
          <div className="flex items-center gap-2">
            <Input readOnly value={baseUrl} className="font-mono" />
            <CopyBtn text={baseUrl} />
          </div>
        </Field>

        {/* API Key */}
        <Field label="接入 Key（ANTHROPIC_AUTH_TOKEN）">
          <div className="flex flex-wrap items-center gap-2">
            <div className="flex min-w-0 flex-1 items-center gap-2">
              <Input
                type={show ? 'text' : 'password'}
                value={draft}
                onChange={(e) => setDraft(e.target.value)}
                readOnly={envManaged}
                placeholder={envManaged ? '' : '留空则不校验来访（仅本机）'}
                className="font-mono"
              />
              <Button size="icon" variant="ghost" className="h-9 w-9 shrink-0" onClick={() => setShow((s) => !s)}>
                {show ? <EyeOff /> : <Eye />}
              </Button>
              <CopyBtn text={draft} />
            </div>
            {!envManaged && (
              <div className="flex items-center gap-2">
                <Button size="sm" variant="outline" onClick={generate}><Dices />生成</Button>
                <Button size="sm" onClick={() => save.mutate(draft.trim())} disabled={save.isPending || draft === currentKey}>
                  {save.isPending ? <Loader2 className="animate-spin" /> : <Save />}保存
                </Button>
                {currentKey && (
                  <Button size="sm" variant="ghost" className="text-bad hover:text-bad"
                    onClick={() => { if (confirm('清除后代理将不校验来访身份，确定？')) save.mutate('') }}>
                    <Trash2 />清空
                  </Button>
                )}
              </div>
            )}
          </div>
          {envManaged && (
            <p className="mt-1.5 text-xs text-muted-foreground">
              由环境变量 <code className="font-mono">LUBAN_API_KEY</code> 接管，网页只读。
            </p>
          )}
        </Field>

        {/* 一键接入片段 */}
        <Field label="Claude Code 接入片段">
          <div className="relative">
            <pre className="overflow-x-auto rounded-lg border border-border bg-surface-2 p-3 pr-11 font-mono text-2xs leading-5">{snippet}</pre>
            <div className="absolute right-2 top-2"><CopyBtn text={snippet} /></div>
          </div>
        </Field>

        {/* 设备绑定有效期 */}
        <div className="border-t border-border pt-4">
          <DeviceBindingTtl />
        </div>

        {/* 管理密码 */}
        <div className="border-t border-border pt-4">
          <AdminPassword />
        </div>
      </CardContent>
    </Card>
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
          className="w-40 font-mono"
        />
        <Button size="sm" onClick={() => save.mutate(parsed)} disabled={save.isPending || parsed === current}>
          {save.isPending ? <Loader2 className="animate-spin" /> : <Save />}保存
        </Button>
        <span className="text-xs text-muted-foreground">{hint}</span>
      </div>
      <p className="mt-1.5 text-xs text-muted-foreground">
        设备超过该时长无请求，绑定自动释放、腾出凭证设备名额；期间同一设备始终命中同一凭证。0 表示永不过期。
      </p>
    </Field>
  )
}

/** 管理密码：未设置→设置；已设置→修改/清除（环境接管时只读）。 */
function AdminPassword() {
  const { data } = useQuery({ queryKey: ['auth-state'], queryFn: getAuthState })
  const [pw, setPwInput] = useState('')

  const save = useMutation({
    mutationFn: async (password: string) => {
      if (data?.configured) await changePassword(password)
      else await setupPassword(password)
    },
    onSuccess: (_r, password) => {
      if (password) { setPw(password); toast.success('管理密码已设置') }
      else { clearPw(); toast.success('已清除管理密码') }
      window.location.reload()
    },
    onError: (e) => toast.error('操作失败', { description: extractError(e) }),
  })

  const envManaged = data?.env_managed ?? false
  const configured = data?.configured ?? false

  return (
    <Field label="管理密码（登录网页所需）">
      {envManaged ? (
        <p className="text-xs text-muted-foreground">
          由环境变量 <code className="font-mono">LUBAN_ADMIN_PASSWORD</code> 接管，网页只读。
        </p>
      ) : (
        <>
          <div className="flex flex-wrap items-center gap-2">
            <Input
              type="password"
              value={pw}
              onChange={(e) => setPwInput(e.target.value)}
              placeholder={configured ? '输入新密码以修改' : '设置密码（至少 4 位，之后登录需要）'}
              className="min-w-0 flex-1"
            />
            <Button size="sm" onClick={() => save.mutate(pw.trim())} disabled={save.isPending || pw.trim().length < 4}>
              {save.isPending ? <Loader2 className="animate-spin" /> : <KeyRound />}
              {configured ? '修改' : '设置'}
            </Button>
            {configured && (
              <Button size="sm" variant="ghost" className="text-bad hover:text-bad"
                onClick={() => { if (confirm('清除后网页将不再需要登录，确定？')) save.mutate('') }}
                disabled={save.isPending}>
                <Trash2 />清除
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
  )
}

function Field({ label, children }: { label: string; children: React.ReactNode }) {
  return (
    <div>
      <div className="label-eyebrow mb-1.5">{label}</div>
      {children}
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
      {ok ? <Check /> : <Copy />}
    </Button>
  )
}
