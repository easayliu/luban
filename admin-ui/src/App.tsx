import { useState } from 'react'
import { useQuery } from '@tanstack/react-query'
import { Plus, Users, Settings2 } from 'lucide-react'
import { listCredentials } from '@/api/credentials'
import { CredentialCard } from '@/components/credential-card'
import { AddAccount } from '@/components/add-account'
import { AccessSettings } from '@/components/access-settings'
import { Button } from '@/components/ui/button'
import { Toaster } from '@/components/ui/sonner'

function App() {
  const [adding, setAdding] = useState(false)
  const [showSettings, setShowSettings] = useState(false)
  const { data: creds, isLoading } = useQuery({
    queryKey: ['credentials'],
    queryFn: listCredentials,
    refetchInterval: 30_000,
  })

  const count = creds?.length ?? 0
  const active = creds?.filter((c) => !c.disabled).length ?? 0

  return (
    <div className="min-h-screen bg-background px-5 py-10 text-foreground sm:py-14">
      <div className="mx-auto w-full max-w-2xl space-y-5">
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
          </div>
        </header>

        {showSettings && <AccessSettings />}

        {/* 概览 */}
        {count > 0 && (
          <div className="flex items-center gap-1.5 text-xs text-muted-foreground">
            <Users className="size-3.5" />
            共 {count} 个账号，{active} 个启用
          </div>
        )}

        {/* 添加面板 */}
        {adding && <AddAccount onClose={() => setAdding(false)} />}

        {/* 列表 */}
        {isLoading ? (
          <div className="py-16 text-center text-sm text-muted-foreground">加载中…</div>
        ) : count === 0 && !adding ? (
          <EmptyState onAdd={() => setAdding(true)} />
        ) : (
          <div className="space-y-3">
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
