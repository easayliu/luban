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
    <div className="grid min-h-screen place-items-center bg-background px-5 py-10 text-foreground">
      <div className="w-full max-w-sm">
        {/* 品牌标识 */}
        <div className="mb-6 flex flex-col items-center text-center">
          <div className="flex size-12 items-center justify-center rounded-xl bg-foreground text-background shadow-card">
            <span className="font-mono text-lg font-bold">鲁</span>
          </div>
          <div className="mt-3 text-base font-semibold leading-none tracking-tight">luban</div>
          <div className="label-eyebrow mt-1.5">Claude Code 授权代理</div>
        </div>

        {/* 登录卡片 */}
        <div className="rounded-2xl border border-border/70 bg-card p-6 shadow-card sm:p-7">
          <h1 className="flex items-center gap-2 text-lg font-semibold tracking-tight">
            <LockClosedIcon className="size-5 text-muted-foreground" />
            管理登录
          </h1>
          <p className="mt-1 text-sm text-muted-foreground">输入管理密码以进入控制台。</p>

          <form
            className="mt-6 space-y-3"
            onSubmit={(e) => { e.preventDefault(); if (password) doLogin.mutate() }}
          >
            <div className="relative">
              <Input
                type={show ? 'text' : 'password'}
                value={password}
                onChange={(e) => setPassword(e.target.value)}
                placeholder="管理密码"
                className="pr-10"
                autoFocus
              />
              <Button
                type="button"
                size="icon"
                variant="ghost"
                className="absolute right-1 top-1/2 size-7 -translate-y-1/2 text-muted-foreground"
                onClick={() => setShow((s) => !s)}
                title={show ? '隐藏密码' : '显示密码'}
              >
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
    </div>
  )
}
