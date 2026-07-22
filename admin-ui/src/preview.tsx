import React from 'react'
import ReactDOM from 'react-dom/client'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import { CredentialCard } from '@/components/credential-card'
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
  expired: false,
  created_at: now - 7 * 3600,
  updated_at: now - 120,
  device_limit: 3,
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

// 正常：有效期文案长（「剩余 7 小时 40 分钟」），触发原先的换行差异。
const normal: Credential = {
  id: 4,
  label: 'robertsbeth812904@yahoo.com',
  tier: 'Max 5x',
  priority: 0,
  disabled: false,
  expires_in: 7 * 3600 + 40 * 60,
  expired: false,
  created_at: now - 3 * 3600,
  updated_at: now - 5,
  device_limit: 3,
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

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <main className="@container mx-auto w-full max-w-5xl space-y-6 px-5 py-8">
        <h2 className="text-sm font-semibold tracking-tight">账号列表（离线预览）</h2>
        <div className="grid grid-cols-1 gap-4 @4xl:grid-cols-2">
          <CredentialCard cred={banned} />
          <CredentialCard cred={normal} />
        </div>
      </main>
    </QueryClientProvider>
  </React.StrictMode>,
)
