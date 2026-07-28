import React from 'react'
import ReactDOM from 'react-dom/client'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import {
  ShieldCheckIcon, ExclamationTriangleIcon, SignalIcon, DevicePhoneMobileIcon,
  MagnifyingGlassIcon, PlusIcon, Cog6ToothIcon, FunnelIcon, Squares2X2Icon,
  Bars3Icon, QueueListIcon, ArrowsUpDownIcon,
} from '@heroicons/react/24/outline'
import { CredentialCard } from '@/components/credential-card'
import { AddAccount } from '@/components/add-account'
import { AccessSettings } from '@/components/access-settings'
import { ForwardingSettings } from '@/components/forwarding-settings'
import { Button } from '@/components/ui/button'
import type { Credential } from '@/api/credentials'
import './index.css'

// 离线预览：造封禁/正常两条假数据，直接看 CredentialCard 渲染，不连后端。
// 仅用于本地目视对比卡片布局，未接入 App/路由。

const mq = window.matchMedia('(prefers-color-scheme: dark)')
const applyTheme = (dark: boolean) => document.documentElement.classList.toggle('dark', dark)
applyTheme(mq.matches)
mq.addEventListener('change', (e) => applyTheme(e.matches))

const now = Math.floor(Date.now() / 1000)

// 已封禁：额度接近满，有效期文案短（「已封禁」）。
const banned: Credential = {
  id: 1,
  label: 'burksupperclassmens946205@yahoo.com',
  tier: 'Max 5x',
  priority: 0,
  disabled: false,
  expires_in: 3600,
  expires_at: now + 3600,
  expired: false,
  created_at: now - 7 * 3600,
  updated_at: now - 120,
  device_limit: 3,
  device_limit_effective: 3,
  device_count: 0,
  ban_reason: '账号已被上游封禁',
  token_hint: 'sk-ant-ort01-…XQAA',
  last_used: now - 120,
  cost_total: 87.77,
  quota: {
    ts: now,
    unified_status: 'allowed',
    rl_5h_utilization: 0.82,
    rl_5h_reset: now + 9 * 60,
    rl_7d_utilization: null,
    rl_7d_reset: null,
    rl_representative: null,
    cost_5h: 87.77,
    cost_7d: null,
  },
}

// 正常：有效期跨到明天（「明天 03:00 过期」），是元信息行里最长的一种文案。
const normal: Credential = {
  id: 4,
  label: 'robertsbeth812904@yahoo.com',
  tier: 'Max 5x',
  priority: 0,
  disabled: false,
  expires_in: 7 * 3600 + 40 * 60,
  expires_at: now + 7 * 3600 + 40 * 60,
  expired: false,
  created_at: now - 3 * 3600,
  updated_at: now - 5,
  device_limit: 3,
  device_limit_effective: 3,
  device_count: 2,
  ban_reason: null,
  token_hint: 'sk-ant-ort01-…igAA',
  last_used: now - 5,
  cost_total: 6.85,
  quota: {
    ts: now,
    unified_status: 'allowed',
    rl_5h_utilization: 0.15,
    rl_5h_reset: now + 99 * 60,
    rl_7d_utilization: null,
    rl_7d_reset: null,
    rl_representative: null,
    cost_5h: 6.85,
    cost_7d: null,
  },
}

const queryClient = new QueryClient({
  defaultOptions: { queries: { staleTime: 5000, refetchOnWindowFocus: false } },
})
const previewDialog = new URLSearchParams(window.location.search).get('dialog')

queryClient.setQueryData(['settings'], {
  api_key: 'luban-preview-key',
  env_managed: false,
  device_binding_ttl_secs: 86400,
  default_device_limit: 3,
  require_device_id: true,
  spoof_identity: true,
  billing_cch: true,
  fill_client_headers: true,
  merge_beta: true,
  cache_scope_global: true,
  orig_header_case: true,
})
queryClient.setQueryData(['auth-state'], { configured: true, env_managed: false })

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <div className="app-shell min-h-screen bg-background text-foreground">
        <header className="border-b border-border/70 bg-surface/85 shadow-[0_1px_0_hsl(var(--border)/0.25)] backdrop-blur-xl">
          <div className="mx-auto flex max-w-7xl items-center justify-between gap-3 px-4 py-3 lg:px-6">
            <div className="flex items-center gap-2.5 sm:gap-3">
              <div className="brand-mark flex size-9 items-center justify-center rounded-xl text-white shadow-brand sm:size-10">
                <span className="relative font-mono text-sm font-bold">鲁</span>
              </div>
              <div>
                <div className="text-[0.9375rem] font-semibold leading-none tracking-tight">Luban</div>
                <div className="label-eyebrow mt-1.5 hidden whitespace-nowrap sm:block">Claude Code Gateway</div>
              </div>
            </div>
            <div className="flex items-center gap-2">
              <Button size="sm" variant="outline"><Cog6ToothIcon /><span className="hidden sm:inline">接入设置</span></Button>
              <Button size="sm"><PlusIcon /><span className="hidden sm:inline">添加账号</span></Button>
            </div>
          </div>
        </header>

        <main className="@container mx-auto w-full max-w-7xl space-y-4 px-4 py-4 md:space-y-6 md:py-6 lg:px-6">
          <section className="space-y-3 sm:space-y-4">
            <div className="flex items-center justify-between">
              <h1 className="text-xl font-semibold tracking-[-0.035em] sm:text-2xl">账号概览</h1>
              <span className="text-2xs text-muted-foreground">每 30 秒自动刷新</span>
            </div>
            <div className="grid grid-cols-2 overflow-hidden rounded-xl border border-border bg-card shadow-card @3xl:grid-cols-4">
              <PreviewMetric label="账号总数" value="2" status="2 已启用" icon={ShieldCheckIcon} tone="ok" className="border-b border-r @3xl:border-b-0" />
              <PreviewMetric label="异常账号" value="1" status="需处理" icon={ExclamationTriangleIcon} tone="bad" className="border-b @3xl:border-b-0 @3xl:border-r" />
              <PreviewMetric label="额度预警" value="0" status="无预警" icon={SignalIcon} tone="neutral" className="border-r" />
              <PreviewMetric label="活跃设备" value="2" icon={DevicePhoneMobileIcon} tone="neutral" />
            </div>
          </section>

          <div className="grid gap-3 rounded-xl border border-border bg-card p-3 shadow-card sm:p-4 @4xl:grid-cols-[auto_minmax(0,1fr)] @4xl:items-center">
            <div className="flex items-baseline gap-2">
              <div className="text-sm font-semibold">账号列表</div>
              <div className="text-xs text-muted-foreground">共 2 个</div>
            </div>
            <div className="min-w-0 space-y-2 @4xl:flex @4xl:items-center @4xl:justify-end @4xl:space-y-0">
              <div className="flex h-9 w-full items-center gap-2 rounded-lg border border-border bg-background/70 px-2.5 text-xs text-muted-foreground shadow-sm @4xl:mr-2 @4xl:h-8 @4xl:w-48">
                <MagnifyingGlassIcon className="size-3.5" />搜索名称或 #id
              </div>
              <div className="scrollbar-none -mx-1 flex items-center gap-2 overflow-x-auto px-1 pb-1 @4xl:mx-0 @4xl:overflow-visible @4xl:px-0 @4xl:pb-0">
                <Button size="sm" variant="outline" className="h-8 text-xs"><FunnelIcon />全部</Button>
                <div className="flex shrink-0 overflow-hidden rounded-md border border-border">
                  <button className="grid size-8 place-items-center bg-muted"><Squares2X2Icon className="size-4" /></button>
                  <button className="grid size-8 place-items-center border-l border-border text-muted-foreground"><Bars3Icon className="size-4" /></button>
                </div>
                <Button size="sm" variant="outline" className="h-8 text-xs"><QueueListIcon />批量</Button>
                <Button size="sm" variant="outline" className="h-8 text-xs"><ArrowsUpDownIcon />优先级↑</Button>
              </div>
            </div>
          </div>

          <div className="grid grid-cols-1 gap-4">
            <CredentialCard cred={banned} />
            <CredentialCard cred={normal} />
          </div>
        </main>
      </div>
      <AddAccount open={previewDialog === 'add'} onOpenChange={() => undefined} />
      <AccessSettings open={previewDialog === 'access'} onOpenChange={() => undefined} />
      <ForwardingSettings open={previewDialog === 'forwarding'} onOpenChange={() => undefined} />
    </QueryClientProvider>
  </React.StrictMode>,
)

function PreviewMetric({ label, value, status, icon: Icon, tone, className }: {
  label: string
  value: string
  status?: string
  icon: typeof ShieldCheckIcon
  tone: 'ok' | 'bad' | 'neutral'
  className?: string
}) {
  const colors = {
    ok: 'bg-ok-soft text-ok',
    bad: 'bg-bad-soft text-bad',
    neutral: 'bg-muted text-muted-foreground',
  }[tone]
  return (
    <div className={`min-w-0 p-3.5 sm:p-5 ${className ?? ''}`}>
      <div className="flex items-center gap-2 text-xs font-medium text-muted-foreground">
        <Icon className="size-4" />
        {label}
      </div>
      <div className="mt-2.5 flex items-end justify-between gap-2 sm:mt-3">
        <span className="text-2xl font-semibold leading-none tnum sm:text-3xl">{value}</span>
        {status && (
          <span className={`rounded-full px-2 py-1 text-[0.625rem] font-medium leading-none sm:text-2xs ${colors}`}>
            {status}
          </span>
        )}
      </div>
    </div>
  )
}
