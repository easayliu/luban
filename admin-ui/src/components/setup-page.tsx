import { useEffect, useState } from 'react'
import { useMutation } from '@tanstack/react-query'
import { KeyRoundIcon, ShieldCheckIcon } from 'lucide-react'
import { setup, type LoginResult } from '@/api/auth'
import { extractError } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Card, CardDescription, CardHeader, CardPanel, CardTitle } from '@/components/ui/card'
import { Field, FieldDescription, FieldError, FieldLabel } from '@/components/ui/field'
import { Form } from '@/components/ui/form'
import { Input } from '@/components/ui/input'
import { LanguageSwitcher } from '@/components/language-switcher'
import { ThemeSwitcher } from '@/components/theme-switcher'
import { LogoMark } from '@/components/logo-mark'
import { MIN_PASSWORD_LENGTH, PasswordInput } from '@/components/password-input'
import { useI18n } from '@/lib/i18n'
import { useDocumentTitle } from '@/lib/use-document-title'

/** 地址栏里 `#setup_token=` 带来的初始化口令（`luban --open` 打开浏览器时附上的）。 */
function tokenFromHash(): string {
  return new URLSearchParams(window.location.hash.slice(1)).get('setup_token')?.trim() ?? ''
}

/**
 * 初始化管理密码页：未设密码时展示，本机访问也一样。
 *
 * 这种情况下管理接口一律拒绝，设密码须带服务启动日志里的初始化口令——证明来人能看到这台
 * 服务的日志，而不是恰好先连上端口的陌生人。设置成功即登录，回调 onSuccess(会话)。
 */
export function SetupPage({ onSuccess }: { onSuccess: (result: LoginResult) => void }) {
  const { t, language } = useI18n()
  const [token, setToken] = useState(tokenFromHash)

  // 口令读进输入框后就从地址栏抹掉：免得留在浏览历史里，或随截图、复制链接带出去。
  useEffect(() => {
    if (window.location.hash.includes('setup_token=')) {
      window.history.replaceState(null, '', window.location.pathname + window.location.search)
    }
  }, [])
  const [password, setPassword] = useState('')

  useDocumentTitle(t('初始化管理密码 · Luban', 'Set up admin password · Luban'))

  const tooShort = password.trim().length > 0 && password.trim().length < MIN_PASSWORD_LENGTH
  const doSetup = useMutation({
    mutationFn: () => setup(password, token.trim()),
    onSuccess,
  })
  const canSubmit = token.trim().length > 0 && password.trim().length >= MIN_PASSWORD_LENGTH

  return (
    <div className="app-shell relative grid min-h-dvh place-items-center px-4 py-8 text-foreground sm:py-10">
      <div className="absolute end-4 top-4 flex items-center gap-2 sm:end-6 sm:top-6">
        <LanguageSwitcher />
        <ThemeSwitcher />
      </div>
      <div className="w-full max-w-sm">
        <div className="mb-6 flex flex-col items-center text-center">
          <div className="brand-mark flex size-12 items-center justify-center rounded-xl">
            <LogoMark className="size-7" />
          </div>
          <div className="mt-4 text-base font-semibold leading-none tracking-tight">Luban</div>
          <div className="mt-1.5 text-xs font-medium uppercase tracking-wide text-muted-foreground">
            Claude Code Gateway
          </div>
        </div>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2 text-base leading-tight">
              <ShieldCheckIcon aria-hidden="true" className="size-4 text-muted-foreground" />
              {t('初始化管理密码', 'Set up admin password')}
            </CardTitle>
            <CardDescription>
              {t(
                '尚未设置管理密码，须先设置密码才能使用控制台。管理员用户名为 admin，此后请使用该用户名与此密码登录。',
                'No admin password is set. Set one before using the console. The admin username is admin; sign in with it and this password from now on.',
              )}
            </CardDescription>
          </CardHeader>
          <CardPanel>
            <Form
              className="space-y-4"
              onSubmit={(event) => {
                event.preventDefault()
                if (canSubmit) doSetup.mutate()
              }}
            >
              <Field invalid={doSetup.isError}>
                <FieldLabel htmlFor="setup-token">{t('初始化口令', 'Setup token')}</FieldLabel>
                <Input
                  id="setup-token"
                  autoComplete="off"
                  autoFocus={!token}
                  className="font-mono"
                  onChange={(event) => setToken(event.target.value)}
                  spellCheck={false}
                  value={token}
                />
                <FieldDescription>
                  {t(
                    '见服务启动日志中的 setup_token 一项；Docker 部署可执行 docker logs luban 2>&1 | grep setup_token 查看。',
                    'Find setup_token in the server startup log; for Docker, run docker logs luban 2>&1 | grep setup_token.',
                  )}
                </FieldDescription>
              </Field>
              <Field invalid={tooShort || doSetup.isError}>
                <FieldLabel htmlFor="setup-password">{t('管理密码', 'Admin password')}</FieldLabel>
                <PasswordInput
                  id="setup-password"
                  autoFocus={!!token}
                  invalid={tooShort || doSetup.isError}
                  onChange={setPassword}
                  value={password}
                />
                {/* `match`：错误来自前端校验或接口，不是原生表单校验，得显式打开才显示。 */}
                {tooShort && (
                  <FieldError match>
                    {t(`密码至少 ${MIN_PASSWORD_LENGTH} 位`, `At least ${MIN_PASSWORD_LENGTH} characters`)}
                  </FieldError>
                )}
                {!tooShort && doSetup.isError && (
                  <FieldError match>{extractError(doSetup.error, language)}</FieldError>
                )}
              </Field>
              <Button className="w-full" disabled={!canSubmit} loading={doSetup.isPending} type="submit">
                <KeyRoundIcon aria-hidden="true" />
                {t('设置并进入控制台', 'Set password and continue')}
              </Button>
            </Form>
          </CardPanel>
        </Card>
      </div>
    </div>
  )
}
