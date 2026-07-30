import { useState } from 'react'
import { useMutation } from '@tanstack/react-query'
import { ArrowRightIcon, EyeIcon, EyeOffIcon, LockKeyholeIcon } from 'lucide-react'
import { login } from '@/api/auth'
import { setPw } from '@/api/client'
import { extractError } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Card, CardDescription, CardHeader, CardPanel, CardTitle } from '@/components/ui/card'
import { Field, FieldError, FieldLabel } from '@/components/ui/field'
import { Form } from '@/components/ui/form'
import {
  InputGroup,
  InputGroupAddon,
  InputGroupInput,
} from '@/components/ui/input-group'
import { LogoMark } from '@/components/logo-mark'

/** 管理登录页（已设置密码时展示）。登录成功回调 onSuccess(password)。 */
export function LoginPage({ onSuccess }: { onSuccess: (password: string) => void }) {
  const [password, setPassword] = useState('')
  const [show, setShow] = useState(false)

  const doLogin = useMutation({
    mutationFn: () => login(password),
    onSuccess: () => {
      setPw(password)
      onSuccess(password)
    },
  })

  return (
    <div className="app-shell grid min-h-dvh place-items-center px-4 py-8 text-foreground sm:py-10">
      <div className="w-full max-w-sm">
        <div className="mb-6 flex flex-col items-center text-center">
          <div className="brand-mark flex size-12 items-center justify-center rounded-xl text-brand-foreground shadow-brand">
            <LogoMark className="size-7" />
          </div>
          <div className="mt-4 text-lg font-semibold leading-none tracking-tight">Luban</div>
          <div className="mt-1.5 text-xs font-medium uppercase tracking-wide text-muted-foreground">
            Claude Code Gateway
          </div>
        </div>

        <Card>
          <CardHeader>
            <CardTitle className="flex items-center gap-2">
              <LockKeyholeIcon aria-hidden="true" className="text-muted-foreground" />
              管理登录
            </CardTitle>
            <CardDescription>输入管理密码以继续访问控制台。</CardDescription>
          </CardHeader>
          <CardPanel>
            <Form
              className="space-y-4"
              onSubmit={(event) => {
                event.preventDefault()
                if (password) doLogin.mutate()
              }}
            >
              <Field invalid={doLogin.isError}>
                <FieldLabel htmlFor="admin-password">管理密码</FieldLabel>
                <InputGroup>
                  <InputGroupInput
                    id="admin-password"
                    autoFocus
                    aria-invalid={doLogin.isError || undefined}
                    onChange={(event) => setPassword(event.target.value)}
                    placeholder="请输入管理密码"
                    type={show ? 'text' : 'password'}
                    value={password}
                  />
                  <InputGroupAddon align="inline-end">
                    <Button
                      aria-label={show ? '隐藏密码' : '显示密码'}
                      size="icon-xs"
                      title={show ? '隐藏密码' : '显示密码'}
                      type="button"
                      variant="ghost"
                      onClick={() => setShow((visible) => !visible)}
                    >
                      {show ? <EyeOffIcon /> : <EyeIcon />}
                    </Button>
                  </InputGroupAddon>
                </InputGroup>
                {doLogin.isError && <FieldError>{extractError(doLogin.error)}</FieldError>}
              </Field>
              <Button
                className="w-full"
                disabled={!password}
                loading={doLogin.isPending}
                type="submit"
              >
                <ArrowRightIcon aria-hidden="true" />
                登录
              </Button>
            </Form>
          </CardPanel>
        </Card>
      </div>
    </div>
  )
}
