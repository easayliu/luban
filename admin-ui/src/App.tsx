import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { Plus, Users, Settings2, LogOut, Activity } from 'lucide-react'
import { listCredentials } from '@/api/credentials'
import { getAuthState } from '@/api/auth'
import { getPw, setPw, clearPw } from '@/api/client'
import { formatUsd } from '@/lib/utils'
import { CredentialCard } from '@/components/credential-card'
import { AddAccount } from '@/components/add-account'
import { AccessSettings } from '@/components/access-settings'
import { UsagePanel } from '@/components/usage-panel'
import { LoginPage } from '@/components/login-page'
import { Button } from '@/components/ui/button'
import { Toaster } from '@/components/ui/sonner'

function App() {
  const [adding, setAdding] = useState(false)
  const [showSettings, setShowSettings] = useState(false)
  const [showUsage, setShowUsage] = useState(false)
  const [pw, setPwState] = useState<string | null>(getPw())

  const { data: authState, isLoading: authLoading } = useQuery({
    queryKey: ['auth-state'],
    queryFn: getAuthState,
  })

  const needLogin = authState?.configured && !pw

  const { data: creds, isLoading } = useQuery({
    queryKey: ['credentials'],
    queryFn: listCredentials,
    refetchInterval: 30_000,
    enabled: !needLogin && !authLoading, // 未登录时不请求受保护接口
  })

  if (authLoading || !authState) {
    return <div className="grid min-h-screen place-items-center text-sm text-muted-foreground">加载中…</div>
  }

  if (needLogin) {
    return (
      <>
        <LoginPage onSuccess={(p) => { setPw(p); setPwState(p) }} />
        <Toaster position="top-right" />
      </>
    )
  }

  const count = creds?.length ?? 0
  const active = creds?.filter((c) => !c.disabled).length ?? 0
  const costTotal = creds?.reduce((s, c) => s + (c.cost_total ?? 0), 0) ?? 0

  return (
    <div className="min-h-screen bg-background px-5 py-10 text-foreground sm:py-14">
      <div className="mx-auto w-full max-w-3xl space-y-6">
        {/* 品牌 + 操作 */}
        <header className="flex items-center justify-between">
          <div className="flex items-center gap-2.5">
            <div className="flex h-9 w-9 items-center justify-center rounded-md bg-foreground text-background">
              <span className="font-mono text-sm font-bold">鲁</span>
            </div>
            <div>
              <div className="text-sm font-semibold leading-none tracking-tight">luban</div>
              <div className="label-eyebrow mt-1">Claude Code 授权代理</div>
            </div>
          </div>
          <div className="flex items-center gap-2">
            <Button
              size="sm"
              variant={showUsage ? 'secondary' : 'outline'}
              onClick={() => setShowUsage((s) => !s)}
              title="用量与额度"
            >
              <Activity />
              用量
            </Button>
            <Button
              size="sm"
              variant={showSettings ? 'secondary' : 'outline'}
              onClick={() => setShowSettings((s) => !s)}
              title="接入设置"
            >
              <Settings2 />
              接入设置
            </Button>
            {!adding && (
              <Button size="sm" onClick={() => setAdding(true)}>
                <Plus />
                添加账号
              </Button>
            )}
            {authState.configured && pw && (
              <Button size="sm" variant="ghost" title="退出登录"
                onClick={() => { clearPw(); setPwState(null) }}>
                <LogOut />
              </Button>
            )}
          </div>
        </header>

        {showUsage && <UsagePanel />}
        {showSettings && <AccessSettings />}

        {/* 添加面板 */}
        {adding && <AddAccount onClose={() => setAdding(false)} />}

        {/* 概览 */}
        {count > 0 && (
          <div className="flex items-center gap-2 px-0.5 text-xs text-muted-foreground">
            <span className="flex items-center gap-1.5">
              <Users className="size-3.5" />
              共 {count} 个账号，{active} 个启用
            </span>
            <span className="opacity-40">·</span>
            <span title="所有账号历史累计等价 API 费用（按官方定价估算，仅供参考）">
              总花费 {formatUsd(costTotal)}
            </span>
          </div>
        )}

        {/* 列表 */}
        {isLoading ? (
          <div className="py-16 text-center text-sm text-muted-foreground">加载中…</div>
        ) : count === 0 && !adding ? (
          <EmptyState onAdd={() => setAdding(true)} />
        ) : (
          <div className="space-y-4">
            {creds!.map((c) => (
              <CredentialCard key={c.id} cred={c} />
            ))}
          </div>
        )}
      </div>
      <Toaster position="top-right" />
    </div>
  )
}

function EmptyState({ onAdd }: { onAdd: () => void }) {
  return (
    <div className="rounded-2xl border border-dashed border-border py-16 text-center">
      <p className="text-sm text-muted-foreground">还没有账号</p>
      <Button className="mt-4" onClick={onAdd}>
        <Plus />
        添加第一个账号
      </Button>
    </div>
  )
}

export default App
