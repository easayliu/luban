import React from 'react'
import ReactDOM from 'react-dom/client'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import {
  ShieldCheckIcon, ExclamationTriangleIcon, SignalIcon, DevicePhoneMobileIcon,
  MagnifyingGlassIcon, PlusIcon, Cog6ToothIcon, FunnelIcon, Squares2X2Icon,
  Bars3Icon, QueueListIcon, ArrowsUpDownIcon, EllipsisVerticalIcon,
} from '@heroicons/react/24/outline'
import { CredentialCard } from '@/components/credential-card'
import { CredentialListHeader, CredentialRow } from '@/components/credential-row'
import { AddAccount } from '@/components/add-account'
import { AccessSettings } from '@/components/access-settings'
import { ForwardingSettings } from '@/components/forwarding-settings'
import { SettingsPage, type SettingsSection } from '@/components/settings-page'
import { AppFooter } from '@/components/app-footer'
import { OverviewMetric } from '@/components/overview-metric'
import { LogoMark } from '@/components/logo-mark'
import { BatchActionsBar } from '@/App'
import { Button } from '@/components/ui/button'
import { Table, TableBody, TableCaption } from '@/components/ui/table'
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
  rate_limited_secs: 0,
  quota: {
    ts: now,
    unified_status: 'allowed',
    rl_5h_utilization: 0.82,
    rl_5h_reset: now + 9 * 60,
    rl_7d_utilization: 0.44,
    rl_7d_reset: now + 3 * 24 * 3600,
    rl_representative: null,
    cost_5h: 87.77,
    cost_7d: 162.35,
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
  rate_limited_secs: 0,
  quota: {
    ts: now,
    unified_status: 'allowed',
    rl_5h_utilization: 0.15,
    rl_5h_reset: now + 99 * 60,
    rl_7d_utilization: 0.63,
    rl_7d_reset: now + 5 * 24 * 3600,
    rl_representative: null,
    cost_5h: 6.85,
    cost_7d: 42.18,
  },
}

const queryClient = new QueryClient({
  defaultOptions: { queries: { staleTime: Infinity, refetchOnWindowFocus: false } },
})
const previewParams = new URLSearchParams(window.location.search)
const previewDialog = previewParams.get('dialog')
const previewSettings = previewParams.get('settings') as SettingsSection | null
const previewView = previewParams.get('view') === 'list' ? 'list' : 'card'
const previewBatch = previewParams.get('batch') === '1'
const previewSelected = new Set([banned.id])

queryClient.setQueryData(['settings'], {
  api_key: 'luban-preview-key',
  env_managed: false,
  device_binding_ttl_secs: 86400,
  default_device_limit: 3,
  require_device_id: true,
  bare_rate_limit: 0,
  bare_rate_window_secs: 60,
  rate_limit_retry_max: 2,
  spoof_identity: true,
  billing_cch: true,
  fill_client_headers: true,
  merge_beta: true,
  system_shape: true,
  orig_header_case: true,
  thinking_signature_retry: true,
  simulate_cc: true,
  rate_limit_retry: true,
})
queryClient.setQueryData(['auth-state'], { configured: true, env_managed: false })
queryClient.setQueryData(['credential-devices', 1], [])
queryClient.setQueryData(['credential-devices', 4], [
  {
    device_id: 'user_9fd2b847c21a4e51a98d0e07',
    request_count: 286,
    created_at: now - 12 * 24 * 3600,
    last_seen_at: now - 80,
    cost_usd: 4.72,
    cost_usd_all: 12.38,
  },
  {
    device_id: 'user_1a48d0fc93e64cb4b783217d',
    request_count: 74,
    created_at: now - 3 * 24 * 3600,
    last_seen_at: now - 18 * 60,
    cost_usd: 2.13,
    cost_usd_all: 2.13,
  },
])

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      {previewSettings ? (
        <SettingsPage
          section={previewSettings === 'forwarding' ? 'forwarding' : 'access'}
          onSectionChange={() => undefined}
          onBack={() => undefined}
        />
      ) : (
      <>
      <div className="app-shell flex min-h-screen flex-col bg-background text-foreground">
        <header className="sticky top-0 z-20 border-b border-border/80 bg-background/95 backdrop-blur-xl">
          <div className="mx-auto flex max-w-7xl items-center justify-between gap-3 px-4 py-2.5 sm:py-3 lg:px-6">
            <div className="flex items-center gap-2.5 sm:gap-3">
              <div className="brand-mark flex size-8 items-center justify-center rounded-md text-white sm:size-9 sm:rounded-lg">
                <LogoMark className="size-5" />
              </div>
              <div>
                <div className="text-[0.9375rem] font-semibold leading-none tracking-tight">Luban</div>
                <div className="label-eyebrow mt-1.5 hidden whitespace-nowrap sm:block">Claude Code Gateway</div>
              </div>
            </div>
            <div className="flex items-center gap-2 sm:hidden">
              <Button size="icon" className="size-9" aria-label="添加账号"><PlusIcon /></Button>
              <Button size="icon" variant="outline" className="size-9" aria-label="更多操作"><EllipsisVerticalIcon /></Button>
            </div>
            <div className="hidden items-center gap-2 sm:flex">
              <Button size="sm" variant="outline" aria-label="接入设置"><Cog6ToothIcon />接入设置</Button>
              <Button size="sm" aria-label="添加账号"><PlusIcon />添加账号</Button>
            </div>
          </div>
        </header>

        <main className="mx-auto w-full max-w-7xl flex-1 space-y-4 px-4 py-5 pb-8 sm:space-y-5 sm:py-6 lg:px-6">
          <section className="space-y-4">
            <div className="flex items-center justify-between">
              <h1 className="text-xl font-semibold tracking-tight sm:text-2xl">账号管理</h1>
              <span className="hidden text-2xs text-muted-foreground sm:inline">每 30 秒自动刷新</span>
            </div>
            <div className="grid grid-cols-2 border-y border-border/80 md:grid-cols-4">
              <OverviewMetric label="账号总数" value="2" status="2 已启用" icon={ShieldCheckIcon} tone="ok" className="border-b border-r border-border/80 md:border-b-0" />
              <OverviewMetric label="异常账号" value="1" status="需处理" icon={ExclamationTriangleIcon} tone="bad" className="border-b border-border/80 md:border-b-0 md:border-r" />
              <OverviewMetric label="额度预警" value="0" status="无预警" icon={SignalIcon} tone="neutral" className="border-r border-border/80" />
              <OverviewMetric label="活跃设备" value="2" icon={DevicePhoneMobileIcon} tone="neutral" />
            </div>
          </section>

          <section className="min-w-0">
          <div className="grid gap-3 border-b border-border/80 py-3.5 lg:grid-cols-[auto_minmax(0,1fr)] lg:items-center">
            <div className="flex items-baseline gap-2">
              <div className="text-sm font-semibold sm:text-base">账号列表</div>
              <div className="text-xs text-muted-foreground">共 2 个</div>
            </div>
            <div className="min-w-0 space-y-2 lg:flex lg:items-center lg:justify-end lg:space-y-0">
              <div className="flex h-9 w-full items-center gap-2 rounded-md border border-border bg-background px-3 text-xs text-muted-foreground shadow-sm lg:mr-2 lg:w-64">
                <MagnifyingGlassIcon className="size-3.5" />搜索名称或 #id
              </div>
              <div className="scrollbar-none -mx-1 flex items-center gap-2 overflow-x-auto px-1 pb-1 lg:mx-0 lg:overflow-visible lg:px-0 lg:pb-0">
                <Button size="sm" variant="outline" className="h-9 text-xs sm:h-8"><FunnelIcon />全部</Button>
                <div className="flex shrink-0 overflow-hidden rounded-md border border-border">
                  <Button size="icon" variant="ghost" className="size-9 rounded-none focus-visible:z-10 sm:size-8" aria-label="卡片视图" aria-pressed={previewView === 'card'}><Squares2X2Icon className="size-4" /></Button>
                  <Button size="icon" variant="ghost" className="size-9 rounded-none border-l border-border focus-visible:z-10 sm:size-8" aria-label="紧凑列表视图" aria-pressed={previewView === 'list'}><Bars3Icon className="size-4" /></Button>
                </div>
                <Button size="sm" variant={previewBatch ? 'secondary' : 'outline'} className="h-9 text-xs sm:h-8"><QueueListIcon />批量</Button>
                <Button size="sm" variant="outline" className="h-9 text-xs sm:h-8"><ArrowsUpDownIcon />优先级↑</Button>
              </div>
            </div>
          </div>

          {previewBatch && (
            <div className="border-b border-border bg-muted/15 p-3 sm:p-4">
            <BatchActionsBar
              all={[banned, normal]}
              selected={previewSelected}
              onSelectedChange={() => undefined}
              onClose={() => undefined}
            />
            </div>
          )}

          {previewView === 'list' ? (
            <div>
              <Table className="table-fixed">
                <TableCaption className="sr-only">账号列表</TableCaption>
                <CredentialListHeader
                  selectable={previewBatch}
                  sort="priority"
                  dir="asc"
                  onSortChange={() => undefined}
                  allSelected={false}
                  onSelectAll={() => undefined}
                />
                <TableBody>
                  <CredentialRow cred={banned} selectable={previewBatch} selected={previewBatch} onSelectedChange={() => undefined} />
                  <CredentialRow cred={normal} selectable={previewBatch} selected={false} onSelectedChange={() => undefined} />
                </TableBody>
              </Table>
            </div>
          ) : (
            <div className="grid grid-cols-1 items-start gap-3 pt-3 sm:gap-4 sm:pt-4 lg:grid-cols-2">
              <CredentialCard cred={banned} selectable={previewBatch} selected={previewBatch} onSelectedChange={() => undefined} />
              <CredentialCard cred={normal} selectable={previewBatch} selected={false} onSelectedChange={() => undefined} />
            </div>
          )}
          </section>
        </main>
        <AppFooter />
      </div>
      <AddAccount open={previewDialog === 'add'} onOpenChange={() => undefined} />
      <AccessSettings open={previewDialog === 'access'} onOpenChange={() => undefined} />
      <ForwardingSettings open={previewDialog === 'forwarding'} onOpenChange={() => undefined} />
      </>
      )}
    </QueryClientProvider>
  </React.StrictMode>,
)
