import { useState } from 'react'
import { useMutation, useQuery } from '@tanstack/react-query'
import { ArrowRightIcon, LockKeyholeIcon } from 'lucide-react'
import { getAuthState, login } from '@/api/auth'
import { setToken } from '@/api/client'
import { rememberRole } from '@/lib/role'
import { extractError } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Card, CardDescription, CardHeader, CardPanel, CardTitle } from '@/components/ui/card'
import { Field, FieldError, FieldLabel } from '@/components/ui/field'
import { Form } from '@/components/ui/form'
import { Input } from '@/components/ui/input'
import { LanguageSwitcher } from '@/components/language-switcher'
import { ThemeSwitcher } from '@/components/theme-switcher'
import { LogoMark } from '@/components/logo-mark'
import { PasswordInput } from '@/components/password-input'
import { useI18n } from '@/lib/i18n'
import { useDocumentTitle } from '@/lib/use-document-title'

/** 控制台登录页（已设置管理密码时展示）：用户名 + 密码。登录成功回调 onSuccess(会话 token)。 */
export function LoginPage({ onSuccess }: { onSuccess: (token: string) => void }) {
  const { t, language } = useI18n()
  const [username, setUsername] = useState('')
  const [password, setPassword] = useState('')
  // App 已拉过这份鉴权状态，这里直接命中缓存。
  const { data: authState } = useQuery({ queryKey: ['auth-state'], queryFn: getAuthState })
  const viewerEnabled = authState?.viewer_enabled ?? false

  useDocumentTitle(t('控制台登录 · Luban', 'Console sign-in · Luban'))

  const doLogin = useMutation({
    mutationFn: () => login(username.trim(), password),
    onSuccess: (result) => {
      setToken(result.token)
      rememberRole(result.role)
      onSuccess(result.token)
    },
  })

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
              <LockKeyholeIcon aria-hidden="true" className="size-4 text-muted-foreground" />
              {t('控制台登录', 'Console sign-in')}
            </CardTitle>
            {/* 说明只留给读屏：与下面的「用户名」「密码」标签、「登录」按钮说的是同一件事。 */}
            <CardDescription className="sr-only">
              {t('输入用户名和密码以继续访问控制台。', 'Enter your username and password to continue to the console.')}
            </CardDescription>
          </CardHeader>
          <CardPanel>
            <Form
              className="space-y-4"
              onSubmit={(event) => {
                event.preventDefault()
                if (username.trim() && password) doLogin.mutate()
              }}
            >
              <Field invalid={doLogin.isError}>
                <FieldLabel htmlFor="console-username">{t('用户名', 'Username')}</FieldLabel>
                <Input
                  id="console-username"
                  autoFocus
                  autoCapitalize="none"
                  autoComplete="username"
                  aria-invalid={doLogin.isError || undefined}
                  onChange={(event) => setUsername(event.target.value)}
                  spellCheck={false}
                  value={username}
                />
              </Field>
              <Field invalid={doLogin.isError}>
                <FieldLabel htmlFor="console-password">{t('密码', 'Password')}</FieldLabel>
                <PasswordInput
                  id="console-password"
                  autoComplete="current-password"
                  invalid={doLogin.isError}
                  onChange={setPassword}
                  value={password}
                />
                {/* `match`：Base UI 的 Field.Error 默认只跟着原生表单校验显示，这里的错误来自接口，得显式
                    打开，否则登录失败时框变红了却看不到原因。 */}
                {doLogin.isError && <FieldError match>{extractError(doLogin.error, language)}</FieldError>}
                {viewerEnabled && !doLogin.isError && (
                  <p className="text-xs text-muted-foreground">
                    {t('以访客账号 viewer 登录只能查看，不能修改。', 'Signing in as the viewer account gives view-only access.')}
                  </p>
                )}
              </Field>
              <Button
                className="w-full"
                disabled={!username.trim() || !password}
                loading={doLogin.isPending}
                type="submit"
              >
                <ArrowRightIcon aria-hidden="true" />
                {t('登录', 'Sign in')}
              </Button>
            </Form>
          </CardPanel>
        </Card>
      </div>
    </div>
  )
}
