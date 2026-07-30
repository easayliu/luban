import React from 'react'
import ReactDOM from 'react-dom/client'
import { QueryClient, QueryClientProvider } from '@tanstack/react-query'
import App from './App'
import { AnchoredToastProvider, ToastProvider } from '@/components/ui/toast'
import { TooltipProvider } from '@/components/ui/tooltip'
import './index.css'

// 跟随系统深浅色：给 <html> 切换 .dark（配合 darkMode: 'class'）。
const mq = window.matchMedia('(prefers-color-scheme: dark)')
const applyTheme = (dark: boolean) => document.documentElement.classList.toggle('dark', dark)
applyTheme(mq.matches)
mq.addEventListener('change', (e) => applyTheme(e.matches))

const queryClient = new QueryClient({
  defaultOptions: {
    queries: { staleTime: 5000, refetchOnWindowFocus: false },
  },
})

ReactDOM.createRoot(document.getElementById('root')!).render(
  <React.StrictMode>
    <QueryClientProvider client={queryClient}>
      <ToastProvider position="top-right">
        <TooltipProvider>
          <AnchoredToastProvider>
            <div className="relative isolate min-h-svh">
              <App />
            </div>
          </AnchoredToastProvider>
        </TooltipProvider>
      </ToastProvider>
    </QueryClientProvider>
  </React.StrictMode>,
)
