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
    description: '兼容、缓存与错误恢复',
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
    <div className="app-shell flex min-h-dvh flex-col text-foreground">
      <header className="app-header sticky top-0 z-20 border-b border-border/70 bg-background/90 backdrop-blur-xl">
        <div className="page-frame flex items-center justify-between gap-3 py-2.5 sm:py-3">
          <Button
            type="button"
            variant="ghost"
            className="h-auto min-w-0 justify-start gap-2.5 p-0 text-left hover:bg-transparent sm:gap-3"
            onClick={onBack}
            aria-label="返回账号管理"
          >
            <span className="brand-mark flex size-8 shrink-0 items-center justify-center rounded-md text-brand-foreground sm:size-9 sm:rounded-lg">
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

      <main className="page-frame flex-1 py-5 pb-10 sm:py-7">
        <div className="mb-4 sm:mb-6">
          <h1 className="text-xl font-semibold tracking-tight sm:text-2xl">系统设置</h1>
        </div>

        <div className="grid items-start gap-6 lg:grid-cols-[13rem_minmax(0,1fr)] lg:gap-8">
          <nav
            className="sticky top-[calc(3.25rem+env(safe-area-inset-top))] z-10 -mx-4 grid grid-cols-2 gap-1 border-y border-border/80 bg-background/95 px-4 py-2 backdrop-blur-xl sm:top-[calc(3.75rem+env(safe-area-inset-top))] sm:-mx-6 sm:px-6 lg:top-20 lg:mx-0 lg:flex lg:flex-col lg:border-0 lg:bg-transparent lg:p-0 lg:backdrop-blur-none"
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
            <div className="border-b border-border/80 pb-3.5 sm:pb-4">
              <div className="flex items-center gap-2">
                <ActiveIcon className="size-5 text-muted-foreground" />
                <h2 className="text-lg font-semibold tracking-tight">{active.label}</h2>
              </div>
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
