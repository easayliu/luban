import React from 'react'
import ReactDOM from 'react-dom/client'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import {
  ArrowUpDownIcon, EllipsisVerticalIcon, GaugeIcon, LayoutGridIcon, ListChecksIcon,
  ListFilterIcon, ListIcon, PlusIcon, SearchIcon, SettingsIcon, ShieldCheckIcon,
  SmartphoneIcon, TriangleAlertIcon,
} from 'lucide-react'
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
import { CardFrame } from '@/components/ui/card'
import { InputGroup, InputGroupAddon, InputGroupInput } from '@/components/ui/input-group'
import {
  Menu, MenuItem, MenuPopup, MenuRadioGroup, MenuRadioItem, MenuSeparator, MenuTrigger,
} from '@/components/ui/menu'
import { Select, SelectItem, SelectPopup, SelectTrigger, SelectValue } from '@/components/ui/select'
import { Table, TableBody, TableCaption } from '@/components/ui/table'
import {
  ToggleGroup, ToggleGroupItem, ToggleGroupSeparator,
} from '@/components/ui/toggle-group'
import { AnchoredToastProvider, ToastProvider } from '@/components/ui/toast'
import type { Credential } from '@/api/credentials'
import './index.css'

// 离线预览：造封禁/正常两条假数据，直接看 CredentialCard 渲染，不连后端。
// 仅用于本地目视对比卡片布局，未接入 App/路由。

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
const previewSettingsParam = previewParams.get('settings')
const previewSettings: SettingsSection | null = previewSettingsParam === 'forwarding'
  ? 'forwarding'
  : previewSettingsParam === 'access'
    ? 'access'
    : null
const previewView = previewParams.get('view') === 'list' ? 'list' : 'card'
const previewBatch = previewParams.get('batch') === '1'
const previewSelected = new Set([banned.id])
const previewSortItems = [
  { label: '优先级升序', value: 'priority' },
  { label: '最近使用', value: 'recent' },
  { label: '累计花费', value: 'cost' },
]

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
      <ToastProvider position="top-right">
        <AnchoredToastProvider>
          <div className="relative isolate min-h-dvh">
            {previewSettings ? (
              <PreviewSettingsRoute initialSection={previewSettings} />
            ) : (
              <>
                <div className="app-shell flex min-h-dvh flex-col text-foreground">
                  <header className="app-header sticky top-0 z-20 border-b border-border bg-card/95 backdrop-blur">
                    <div className="page-frame flex h-16 items-center justify-between gap-3">
                      <div className="flex items-center gap-2.5 sm:gap-3">
                        <div className="brand-mark flex size-8 items-center justify-center rounded-lg text-brand-foreground">
                          <LogoMark className="size-[1.125rem]" />
                        </div>
                        <div>
                          <div className="font-heading text-sm font-semibold leading-none">Luban</div>
                          <div className="mt-1 hidden whitespace-nowrap text-xs text-muted-foreground sm:block">
                            Claude Code Gateway
                          </div>
                        </div>
                      </div>

                      <div className="flex items-center gap-2 sm:hidden">
                        <Button size="icon" aria-label="添加账号" onClick={() => navigatePreview('?dialog=add')}><PlusIcon /></Button>
                        <Menu>
                          <MenuTrigger render={<Button size="icon" variant="outline" aria-label="更多操作" />}>
                            <EllipsisVerticalIcon />
                          </MenuTrigger>
                          <MenuPopup align="end">
                            <MenuItem onClick={() => navigatePreview('?settings=access')}><SettingsIcon />系统设置</MenuItem>
                            <MenuSeparator />
                            <MenuItem><ListChecksIcon />批量操作</MenuItem>
                          </MenuPopup>
                        </Menu>
                      </div>

                      <div className="hidden items-center gap-2 sm:flex">
                        <Button size="sm" variant="outline" onClick={() => navigatePreview('?settings=access')}><SettingsIcon />系统设置</Button>
                        <Button size="sm" onClick={() => navigatePreview('?dialog=add')}><PlusIcon />添加账号</Button>
                      </div>
                    </div>
                  </header>

                  <main className="page-frame flex-1 py-6 pb-10 sm:py-8 sm:pb-12">
                    <div className="space-y-6 sm:space-y-8">
                      <section className="sm:flex sm:items-end sm:justify-between sm:gap-8" aria-labelledby="preview-page-title">
                        <div className="min-w-0">
                          <h1 id="preview-page-title" className="font-heading text-2xl font-semibold tracking-tight sm:text-3xl">
                            账号调度中心
                          </h1>
                          <p className="mt-2 max-w-2xl text-sm leading-6 text-muted-foreground">
                            统一查看账号健康、额度与设备容量，快速处理会影响转发的状态。
                          </p>
                        </div>
                        <div className="mt-4 flex shrink-0 items-center gap-1.5 text-xs text-muted-foreground sm:mt-0 sm:pb-0.5">
                          <span className="size-1.5 rounded-full bg-success" aria-hidden />
                          每 30 秒自动刷新
                        </div>
                      </section>

                      <CardFrame
                        aria-label="账号池概览"
                        className="grid grid-cols-2 overflow-hidden lg:grid-cols-4"
                      >
                        <OverviewMetric label="可调度账号" value="1/2" status="1 暂不可用" icon={ShieldCheckIcon} tone="ok" className="border-b border-r lg:border-b-0" />
                        <OverviewMetric label="需处理" value="1" status="1 异常" icon={TriangleAlertIcon} tone="bad" className="border-b lg:border-b-0 lg:border-r" />
                        <OverviewMetric label="额度预警" value="0" icon={GaugeIcon} tone="neutral" className="border-r" />
                        <OverviewMetric label="绑定设备" value="2" status="共 6 个名额" icon={SmartphoneIcon} tone="neutral" />
                      </CardFrame>

                      <section className="min-w-0" aria-labelledby="preview-account-list-title">
                        <CardFrame className="min-w-0">
                          <div className="relative grid gap-4 px-4 py-4 sm:px-6 xl:grid-cols-[auto_minmax(0,1fr)] xl:items-center">
                            <div className="min-w-0">
                              <h2 id="preview-account-list-title" className="font-heading text-sm font-semibold">账号列表</h2>
                              <p className="mt-1 text-sm text-muted-foreground">共 2 个账号</p>
                            </div>

                            <div className="min-w-0 space-y-2 xl:flex xl:items-center xl:justify-end xl:space-y-0">
                              <InputGroup className="xl:mr-2 xl:w-56 2xl:w-64">
                                <InputGroupAddon><SearchIcon aria-hidden /></InputGroupAddon>
                                <InputGroupInput aria-label="搜索账号" placeholder="搜索名称或 #id" readOnly />
                              </InputGroup>

                              <div className="grid grid-cols-2 gap-2 sm:flex sm:flex-wrap sm:items-center sm:justify-end">
                                <Menu>
                                  <MenuTrigger render={<Button className="w-full sm:w-auto" variant="outline" />}>
                                    <ListFilterIcon />全部
                                  </MenuTrigger>
                                  <MenuPopup align="end">
                                    <MenuRadioGroup value="all">
                                      <MenuRadioItem value="all">全部</MenuRadioItem>
                                      <MenuRadioItem value="schedulable">可调度</MenuRadioItem>
                                      <MenuRadioItem value="attention">需处理</MenuRadioItem>
                                    </MenuRadioGroup>
                                  </MenuPopup>
                                </Menu>

                                <Select items={previewSortItems} value="priority">
                                  <SelectTrigger aria-label="账号排序" className="w-full min-w-0 sm:w-40">
                                    <ArrowUpDownIcon />
                                    <SelectValue />
                                  </SelectTrigger>
                                  <SelectPopup>
                                    {previewSortItems.map((item) => (
                                      <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>
                                    ))}
                                  </SelectPopup>
                                </Select>

                                <ToggleGroup
                                  aria-label="账号视图"
                                  className="w-full sm:w-fit"
                                  value={[previewView]}
                                  variant="outline"
                                >
                                  <ToggleGroupItem className="flex-1 sm:flex-none" value="card" aria-label="卡片视图">
                                    <LayoutGridIcon />
                                  </ToggleGroupItem>
                                  <ToggleGroupSeparator />
                                  <ToggleGroupItem className="flex-1 sm:flex-none" value="list" aria-label="紧凑列表视图">
                                    <ListIcon />
                                  </ToggleGroupItem>
                                </ToggleGroup>

                                <Button
                                  className="w-full sm:w-auto"
                                  variant={previewBatch ? 'secondary' : 'outline'}
                                >
                                  <ListChecksIcon />批量
                                </Button>
                              </div>
                            </div>
                          </div>

                          {previewBatch && (
                            <div className="relative border-t p-3 sm:px-6 sm:py-4">
                              <BatchActionsBar
                                all={[banned, normal]}
                                selected={previewSelected}
                                onSelectedChange={() => undefined}
                                onClose={() => undefined}
                              />
                            </div>
                          )}

                          {previewView === 'list' && (
                            <Table className="table-fixed" variant="card">
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
                          )}
                        </CardFrame>

                        {previewView === 'card' && (
                          <div className="mt-4 grid items-stretch gap-4 [grid-template-columns:repeat(auto-fill,minmax(min(100%,27rem),1fr))]">
                            <CredentialCard cred={banned} selectable={previewBatch} selected={previewBatch} onSelectedChange={() => undefined} />
                            <CredentialCard cred={normal} selectable={previewBatch} selected={false} onSelectedChange={() => undefined} />
                          </div>
                        )}
                      </section>
                    </div>
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
      </ToastProvider>
    </QueryClientProvider>
  </React.StrictMode>,
)
