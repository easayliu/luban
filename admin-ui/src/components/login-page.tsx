import { useState } from 'react'
import { useMutation } from '@tanstack/react-query'
import {
  LockClosedIcon, ArrowRightIcon, ArrowPathIcon, EyeIcon, EyeSlashIcon,
} from '@heroicons/react/24/outline'
import { login } from '@/api/auth'
import { setPw } from '@/api/client'
import { extractError } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'

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
    <div className="flex min-h-screen items-center justify-center bg-background px-5 text-foreground">
      <div className="w-full max-w-sm">
        <div className="mb-8 flex items-center gap-2.5">
          <div className="flex h-9 w-9 items-center justify-center rounded-md bg-foreground text-background">
            <span className="font-mono text-sm font-bold">鲁</span>
          </div>
          <div>
            <div className="text-sm font-semibold leading-none tracking-tight">luban</div>
            <div className="label-eyebrow mt-1">Claude Code 授权代理</div>
          </div>
        </div>

        <h1 className="flex items-center gap-2 text-2xl font-semibold tracking-tight">
          <LockClosedIcon className="size-5" />
          登录
        </h1>
        <p className="mt-1.5 text-sm text-muted-foreground">输入管理密码以进入控制台。</p>

        <form
          className="mt-7 space-y-3"
          onSubmit={(e) => { e.preventDefault(); if (password) doLogin.mutate() }}
        >
          <div className="flex items-center gap-2">
            <Input
              type={show ? 'text' : 'password'}
              value={password}
              onChange={(e) => setPassword(e.target.value)}
              placeholder="管理密码"
              autoFocus
            />
            <Button type="button" size="icon" variant="ghost" className="h-9 w-9 shrink-0" onClick={() => setShow((s) => !s)}>
              {show ? <EyeSlashIcon /> : <EyeIcon />}
            </Button>
          </div>
          {doLogin.isError && (
            <p className="text-sm text-bad">{extractError(doLogin.error)}</p>
          )}
          <Button type="submit" className="w-full" disabled={doLogin.isPending || !password}>
            {doLogin.isPending ? <ArrowPathIcon className="animate-spin" /> : <ArrowRightIcon />}
            登录
          </Button>
        </form>
      </div>
    </div>
  )
}
