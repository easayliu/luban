import React from 'react'
import ReactDOM from 'react-dom/client'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import {
  EllipsisVerticalIcon, PlusIcon, SettingsIcon,
} from 'lucide-react'
import { AddAccount } from '@/components/add-account'
import { AccessSettings } from '@/components/access-settings'
import { ForwardingSettings } from '@/components/forwarding-settings'
import { SettingsPage, type SettingsSection } from '@/components/settings-page'
import { AppFooter } from '@/components/app-footer'
import {
  CREDENTIAL_PAGE_SIZES,
  CredentialWorkspace,
  type CredentialFilterKey,
  type CredentialPageSize,
  type CredentialViewMode,
} from '@/components/credential-workspace'
import type { SortDir, SortKey } from '@/components/credential-shared'
import { LogoMark } from '@/components/logo-mark'
import { Button, buttonVariants } from '@/components/ui/button'
import { Menu, MenuItem, MenuPopup, MenuTrigger } from '@/components/ui/menu'
import { AnchoredToastProvider, ToastProvider } from '@/components/ui/toast'
import { TooltipProvider } from '@/components/ui/tooltip'
import type { Credential } from '@/api/credentials'
import './index.css'

// 离线预览：覆盖正常、额度风险、冷却、封禁与停用，通过生产共用的 CredentialWorkspace
// 验收卡片层级、筛选与响应式，不连接后端。
// 设置页和弹窗仍由查询参数单独打开，账号工作区不再维护第二套组件树。

const mq = window.matchMedia('(prefers-color-scheme: dark)')
const applyTheme = (dark: boolean) => document.documentElement.classList.toggle('dark', dark)
applyTheme(mq.matches)
mq.addEventListener('change', (e) => applyTheme(e.matches))

const now = Math.floor(Date.now() / 1000)

// 已封禁：故意使用较长的上游错误，覆盖「错误摘要把同一行卡片撑乱」的回归场景。
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
  // 覆盖生产里最常见的「跟随默认设备上限」排版场景。
  device_limit: 0,
  device_limit_effective: 3,
  device_count: 0,
  ban_reason: '[401] authentication_error: OAuth access token has been revoked.',
  token_hint: 'sk-ant-ort01-…XQAA',
  last_used: now - 120,
  cost_total: 87.77,
  rate_limited_secs: 0,
  quota: {
    ts: now,
    unified_status: 'allowed',
    // 覆盖「5h 已重置、7d 仍有用量」时两列保持同一排版节奏的回归场景。
    rl_5h_utilization: 0.82,
    rl_5h_reset: now - 60,
    rl_7d_utilization: 0.76,
    rl_7d_reset: now + 3 * 24 * 3600,
    rl_representative: null,
    overage_in_use: null,
    cost_5h: 0.0086,
    cost_7d: 0.055,
    requests_5h: 3,
    requests_7d: 28,
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
    overage_in_use: null,
    cost_5h: 6.85,
    cost_7d: 42.18,
    requests_5h: 128,
    requests_7d: 914,
  },
}

// 当前窗口已满且由 overage 放行：状态、概览与「额度风险」筛选都应使用同一套红色风险语义。
const overage: Credential = {
  id: 2,
  label: 'design-system-overage@example.com',
  tier: 'Max 20x',
  priority: 1,
  disabled: false,
  expires_in: 5 * 3600,
  expires_at: now + 5 * 3600,
  expired: false,
  created_at: now - 15 * 24 * 3600,
  updated_at: now - 20,
  device_limit: -1,
  device_limit_effective: 0,
  device_count: 7,
  ban_reason: null,
  token_hint: 'sk-ant-ort01-…oVAA',
  last_used: now - 18,
  cost_total: 122.48,
  rate_limited_secs: 0,
  quota: {
    ts: now - 18,
    unified_status: 'allowed',
    rl_5h_utilization: 1.04,
    rl_5h_reset: now + 45 * 60,
    rl_7d_utilization: 0.91,
    rl_7d_reset: now + 4 * 24 * 3600,
    rl_representative: '5h',
    overage_in_use: true,
    cost_5h: 18.23,
    cost_7d: 91.62,
    requests_5h: 412,
    requests_7d: 3218,
  },
}

// 正好 90% 才进入风险；7d 的 89.9% 应统一显示 89%，不能一边黄字一边误报 90%。
const nearLimit: Credential = {
  id: 3,
  label: 'quota-boundary-90-percent@example.com',
  tier: 'Pro',
  priority: 2,
  disabled: false,
  expires_in: 2 * 3600,
  expires_at: now + 2 * 3600,
  expired: false,
  created_at: now - 4 * 24 * 3600,
  updated_at: now - 90,
  device_limit: 2,
  device_limit_effective: 2,
  device_count: 2,
  ban_reason: null,
  token_hint: 'sk-ant-ort01-…q90A',
  last_used: now - 90,
  cost_total: 31.09,
  rate_limited_secs: 0,
  quota: {
    ts: now - 90,
    unified_status: 'allowed_warning',
    rl_5h_utilization: 0.9,
    rl_5h_reset: now + 75 * 60,
    rl_7d_utilization: 0.899,
    rl_7d_reset: now + 2 * 24 * 3600,
    rl_representative: '5h',
    overage_in_use: false,
    cost_5h: 9.24,
    cost_7d: 29.81,
    requests_5h: 226,
    requests_7d: 1288,
  },
}

// 满额的 5h 已重置、但 7d 仍在当前窗口：前端不能证明 overage 已结束，降为琥珀色待确认。
const unknownOverage: Credential = {
  id: 7,
  label: 'overage-window-needs-confirmation@example.com',
  tier: 'Max 5x',
  priority: 2,
  disabled: false,
  expires_in: 80 * 60,
  expires_at: now + 80 * 60,
  expired: false,
  created_at: now - 6 * 24 * 3600,
  updated_at: now - 4 * 60,
  device_limit: 0,
  device_limit_effective: 3,
  device_count: 1,
  ban_reason: null,
  token_hint: 'sk-ant-ort01-…uNKN',
  last_used: now - 4 * 60,
  cost_total: 52.36,
  rate_limited_secs: 0,
  quota: {
    ts: now - 4 * 60,
    unified_status: 'allowed',
    rl_5h_utilization: 1,
    rl_5h_reset: now - 2 * 60,
    rl_7d_utilization: 0.82,
    rl_7d_reset: now + 36 * 3600,
    rl_representative: '5h',
    overage_in_use: true,
    cost_5h: 12.18,
    cost_7d: 51.92,
    requests_5h: 296,
    requests_7d: 1842,
  },
}

const cooldown: Credential = {
  id: 5,
  label: 'cooldown-without-quota@example.com',
  tier: 'Free',
  priority: 3,
  disabled: false,
  expires_in: 45 * 60,
  expires_at: now + 45 * 60,
  expired: false,
  created_at: now - 2 * 24 * 3600,
  updated_at: now - 15,
  device_limit: 0,
  device_limit_effective: 3,
  device_count: 1,
  ban_reason: null,
  token_hint: 'sk-ant-ort01-…cDWN',
  last_used: now - 15,
  cost_total: 0.84,
  rate_limited_secs: 725,
  quota: null,
}

// 停用账号的快照记录过 overage：只做历史快照提示，不再算当前额度风险。
const disabledHistoricalOverage: Credential = {
  id: 6,
  label: 'disabled-historical-overage@example.com',
  tier: null,
  priority: 4,
  disabled: true,
  expires_in: 0,
  expires_at: now - 6 * 3600,
  expired: true,
  created_at: now - 31 * 24 * 3600,
  updated_at: now - 6 * 3600,
  device_limit: 0,
  device_limit_effective: 3,
  device_count: 0,
  ban_reason: null,
  token_hint: 'sk-ant-ort01-…hIST',
  last_used: now - 6 * 3600,
  cost_total: 44.2,
  rate_limited_secs: 0,
  quota: {
    ts: now - 6 * 3600,
    unified_status: 'allowed',
    rl_5h_utilization: 1,
    rl_5h_reset: now - 5 * 3600,
    rl_7d_utilization: 1,
    rl_7d_reset: now - 4 * 3600,
    rl_representative: '7d',
    overage_in_use: true,
    cost_5h: 11.72,
    cost_7d: 43.98,
    requests_5h: 338,
    requests_7d: 2104,
  },
}

const previewCredentials = [
  banned,
  normal,
  overage,
  nearLimit,
  unknownOverage,
  cooldown,
  disabledHistoricalOverage,
]

const queryClient = new QueryClient({
  defaultOptions: { queries: { staleTime: Infinity, refetchOnWindowFocus: false } },
})
const previewParams = new URLSearchParams(window.location.search)
const previewDialog = previewParams.get('dialog')
const previewSettingsParam = previewParams.get('settings')
const previewSettings: SettingsSection | null = previewSettingsParam === 'forwarding'
  ? 'forwarding'
  : previewSettingsParam === 'access'
    ? 'access'
    : null

function navigatePreview(search = '') {
  window.location.assign(`${window.location.pathname}${search}`)
}

function closePreviewDialog(nextOpen: boolean) {
  if (!nextOpen) navigatePreview()
}

function PreviewSettingsRoute({ initialSection }: { initialSection: SettingsSection }) {
  const [section, setSection] = React.useState<SettingsSection>(initialSection)

  const changeSection = (next: SettingsSection) => {
    setSection(next)
    const url = new URL(window.location.href)
    url.searchParams.set('settings', next)
    window.history.replaceState(null, '', url)
  }

  return (
    <SettingsPage
      section={section}
      onSectionChange={changeSection}
      onBack={() => navigatePreview()}
    />
  )
}

function PreviewCredentialWorkspace() {
  const [query, setQuery] = React.useState('')
  const [filter, setFilter] = React.useState<CredentialFilterKey>('all')
  const [sort, setSort] = React.useState<SortKey>('priority')
  const [dir, setDir] = React.useState<SortDir>('asc')
  const [view, setView] = React.useState<CredentialViewMode>(
    previewParams.get('view') === 'list' ? 'list' : 'card',
  )
  const [selected, setSelected] = React.useState<Set<number>>(new Set())
  const [page, setPage] = React.useState(1)
  const [pageSize, setPageSize] = React.useState<CredentialPageSize>(CREDENTIAL_PAGE_SIZES[0])

  return (
    <CredentialWorkspace
      data={{
        credentials: previewCredentials,
        isLoading: false,
        isError: false,
        isRefetchError: false,
        isFetching: false,
      }}
      state={{
        query,
        filter,
        sort,
        dir,
        view,
        selected,
        page,
        pageSize,
      }}
      actions={{
        onQueryChange: setQuery,
        onFilterChange: setFilter,
        onSortChange: (key, nextDir) => {
          setSort(key)
          setDir(nextDir)
        },
        onViewChange: setView,
        onSelectedChange: setSelected,
        onPageChange: setPage,
        onPageSizeChange: setPageSize,
        onRetry: () => undefined,
        onAdd: () => navigatePreview('?dialog=add'),
      }}
    />
  )
}

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
  fill_metadata: true,
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
      <ToastProvider position="top-right">
        <TooltipProvider>
          <AnchoredToastProvider>
            <div className="relative isolate min-h-dvh">
            {previewSettings ? (
              <PreviewSettingsRoute initialSection={previewSettings} />
            ) : (
              <>
                <div className="app-shell flex min-h-dvh flex-col text-foreground">
                  <header className="app-header sticky top-0 z-20 border-b bg-background/92 backdrop-blur-md">
                    <div className="page-frame flex h-14 items-center justify-between gap-3 sm:h-16">
                      <div className="flex min-w-0 items-center gap-2.5 sm:gap-3">
                        <div className="brand-mark flex size-8 shrink-0 items-center justify-center rounded-lg text-white">
                          <LogoMark className="size-[1.125rem]" />
                        </div>
                        <div className="min-w-0">
                          <div className="text-sm font-semibold leading-none tracking-tight">Luban</div>
                          <div className="mt-1 hidden whitespace-nowrap text-xs text-muted-foreground sm:block">
                            Claude Code Gateway
                          </div>
                        </div>
                      </div>

                      <div className="flex items-center gap-2 sm:hidden">
                        <Button size="icon-lg" aria-label="添加账号" onClick={() => navigatePreview('?dialog=add')}><PlusIcon /></Button>
                        <Menu>
                          <MenuTrigger
                            className={buttonVariants({ size: 'icon-lg', variant: 'outline' })}
                            aria-label="更多操作"
                          >
                            <EllipsisVerticalIcon />
                          </MenuTrigger>
                          <MenuPopup align="end">
                            <MenuItem onClick={() => navigatePreview('?settings=access')}><SettingsIcon />系统设置</MenuItem>
                          </MenuPopup>
                        </Menu>
                      </div>

                      <div className="hidden items-center gap-2 sm:flex">
                        <Button size="sm" variant="outline" onClick={() => navigatePreview('?settings=access')}><SettingsIcon />系统设置</Button>
                        <Button size="sm" onClick={() => navigatePreview('?dialog=add')}><PlusIcon />添加账号</Button>
                      </div>
                    </div>
                  </header>

                  <main className="page-frame flex-1 py-5 pb-8 sm:py-8 sm:pb-12">
                    <PreviewCredentialWorkspace />
                  </main>
                  <AppFooter />
                </div>

                <AddAccount open={previewDialog === 'add'} onOpenChange={closePreviewDialog} />
                <AccessSettings open={previewDialog === 'access'} onOpenChange={closePreviewDialog} />
                <ForwardingSettings open={previewDialog === 'forwarding'} onOpenChange={closePreviewDialog} />
              </>
            )}
            </div>
          </AnchoredToastProvider>
        </TooltipProvider>
      </ToastProvider>
    </QueryClientProvider>
  </React.StrictMode>,
)
