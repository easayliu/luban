import { useEffect } from 'react'
import {
  AdjustmentsHorizontalIcon, ArrowLeftIcon, Cog6ToothIcon,
} from '@heroicons/react/24/outline'
import { AccessSettingsContent } from '@/components/access-settings'
import { ForwardingSettingsContent } from '@/components/forwarding-settings'
import { AppFooter } from '@/components/app-footer'
import { LogoMark } from '@/components/logo-mark'
import { Button } from '@/components/ui/button'
import { Toaster } from '@/components/ui/sonner'
import { cn } from '@/lib/utils'

export type SettingsSection = 'access' | 'forwarding'

const sections = [
  {
    key: 'access',
    label: '接入与安全',
    description: '客户端、设备和登录',
    icon: Cog6ToothIcon,
  },
  {
    key: 'forwarding',
    label: '转发策略',
    description: '请求形态与错误恢复',
    icon: AdjustmentsHorizontalIcon,
  },
] as const

export function SettingsPage({
  section,
  onSectionChange,
  onBack,
}: {
  section: SettingsSection
  onSectionChange: (section: SettingsSection) => void
  onBack: () => void
}) {
  const active = sections.find((item) => item.key === section) ?? sections[0]
  const ActiveIcon = active.icon

  useEffect(() => {
    const previousTitle = document.title
    document.title = `${active.label} · Luban`
    return () => { document.title = previousTitle }
  }, [active.label])

  return (
    <div className="app-shell flex min-h-screen flex-col bg-background text-foreground">
      <header className="sticky top-0 z-20 border-b border-border/80 bg-background/95 backdrop-blur-xl">
        <div className="mx-auto flex max-w-7xl items-center justify-between gap-3 px-4 py-2.5 sm:py-3 lg:px-6">
          <Button
            type="button"
            variant="ghost"
            className="h-auto min-w-0 justify-start gap-2.5 p-0 text-left hover:bg-transparent sm:gap-3"
            onClick={onBack}
            aria-label="返回账号管理"
          >
            <span className="brand-mark flex size-8 shrink-0 items-center justify-center rounded-md text-white sm:size-9 sm:rounded-lg">
              <LogoMark className="size-5" />
            </span>
            <span className="min-w-0">
              <span className="block text-[0.9375rem] font-semibold leading-none tracking-tight">Luban</span>
              <span className="label-eyebrow mt-1.5 hidden whitespace-nowrap sm:block">系统设置</span>
            </span>
          </Button>
          <Button size="sm" variant="outline" onClick={onBack}>
            <ArrowLeftIcon />
            返回账号
          </Button>
        </div>
      </header>

      <main className="mx-auto w-full max-w-7xl flex-1 px-4 py-5 pb-10 sm:py-7 lg:px-6">
        <div className="mb-5 sm:mb-7">
          <h1 className="text-xl font-semibold tracking-tight sm:text-2xl">系统设置</h1>
          <p className="mt-1.5 text-xs leading-5 text-muted-foreground">
            配置客户端接入、设备策略与请求转发行为。
          </p>
        </div>

        <div className="grid items-start gap-6 lg:grid-cols-[13rem_minmax(0,1fr)] lg:gap-8">
          <nav
            className="sticky top-[3.55rem] z-10 -mx-4 grid grid-cols-2 gap-1 border-y border-border/80 bg-background/95 px-4 py-2 backdrop-blur-xl lg:top-20 lg:mx-0 lg:flex lg:flex-col lg:border-0 lg:bg-transparent lg:p-0 lg:backdrop-blur-none"
            aria-label="设置分类"
          >
            {sections.map((item) => {
              const Icon = item.icon
              const selected = item.key === section
              return (
                <Button
                  key={item.key}
                  type="button"
                  variant={selected ? 'secondary' : 'ghost'}
                  className={cn(
                    'h-auto min-w-0 justify-start gap-2 px-3 py-2.5 text-left',
                    selected && 'font-semibold',
                  )}
                  aria-current={selected ? 'page' : undefined}
                  onClick={() => onSectionChange(item.key)}
                >
                  <Icon className="size-4 shrink-0" />
                  <span className="min-w-0">
                    <span className="block truncate text-xs sm:text-sm">{item.label}</span>
                    <span className="mt-0.5 hidden truncate text-2xs font-normal text-muted-foreground lg:block">
                      {item.description}
                    </span>
                  </span>
                </Button>
              )
            })}
          </nav>

          <section className="min-w-0 lg:border-l lg:border-border/80 lg:pl-8">
            <div className="border-b border-border/80 pb-4 sm:pb-5">
              <div className="flex items-center gap-2">
                <ActiveIcon className="size-5 text-muted-foreground" />
                <h2 className="text-lg font-semibold tracking-tight">{active.label}</h2>
              </div>
              <p className="mt-1.5 text-xs leading-5 text-muted-foreground">
                {section === 'access'
                  ? '管理客户端连接、设备绑定规则和控制台登录。'
                  : '管理请求兼容策略、缓存形态和错误恢复。'}
              </p>
            </div>
            <div className="max-w-3xl pt-6 sm:pt-7">
              {section === 'access' ? <AccessSettingsContent /> : <ForwardingSettingsContent />}
            </div>
          </section>
        </div>
      </main>

      <AppFooter />
      <Toaster position="top-right" />
    </div>
  )
}
