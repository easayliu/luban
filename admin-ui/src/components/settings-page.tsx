import { useEffect, useState } from 'react'
import { ArrowLeftIcon, Settings2Icon, SlidersHorizontalIcon } from 'lucide-react'
import { AccessSettingsContent } from '@/components/access-settings'
import { AppFooter } from '@/components/app-footer'
import { ForwardingSettingsContent } from '@/components/forwarding-settings'
import { LogoMark } from '@/components/logo-mark'
import { Button } from '@/components/ui/button'
import { Tabs, TabsList, TabsPanel, TabsTab } from '@/components/ui/tabs'

export type SettingsSection = 'access' | 'forwarding'

const sections = [
  {
    key: 'access',
    label: '接入与安全',
    description: '客户端、设备和登录',
    icon: Settings2Icon,
  },
  {
    key: 'forwarding',
    label: '转发策略',
    description: '兼容、缓存与错误恢复',
    icon: SlidersHorizontalIcon,
  },
] as const

function useDesktopSettingsNavigation() {
  const [isDesktop, setIsDesktop] = useState(
    () => typeof window !== 'undefined' && window.matchMedia('(min-width: 64rem)').matches,
  )

  useEffect(() => {
    const media = window.matchMedia('(min-width: 64rem)')
    const sync = () => setIsDesktop(media.matches)
    sync()
    media.addEventListener('change', sync)
    return () => media.removeEventListener('change', sync)
  }, [])

  return isDesktop
}

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
  const desktopNavigation = useDesktopSettingsNavigation()

  useEffect(() => {
    const previousTitle = document.title
    document.title = `${active.label} · Luban`
    return () => {
      document.title = previousTitle
    }
  }, [active.label])

  return (
    <div className="app-shell flex min-h-dvh flex-col text-foreground">
      <header className="app-header sticky top-0 z-20 border-b bg-background/92 backdrop-blur-md">
        <div className="page-frame flex h-14 items-center justify-between gap-3 sm:h-16">
          <Button
            aria-label="返回账号页"
            className="-ml-2 h-auto min-w-0 justify-start gap-2.5 px-2 py-1.5 sm:gap-3"
            title="返回账号页"
            variant="ghost"
            onClick={onBack}
          >
            <span className="brand-mark flex size-8 shrink-0 items-center justify-center rounded-lg text-white">
              <LogoMark className="size-[1.125rem]" />
            </span>
            <span className="min-w-0 text-left">
              <span className="block text-sm font-semibold leading-none tracking-tight">Luban</span>
              <span className="mt-1 hidden whitespace-nowrap text-xs font-normal text-muted-foreground sm:block">
                Claude Code Gateway
              </span>
            </span>
          </Button>
          <Button size="sm" variant="outline" onClick={onBack}>
            <ArrowLeftIcon aria-hidden="true" />
            返回账号
          </Button>
        </div>
      </header>

      <main className="page-frame relative flex-1 py-5 pb-8 sm:py-8 sm:pb-12">
        <div className="space-y-4 sm:space-y-6">
          <section aria-labelledby="settings-page-title">
            <h1
              className="min-w-0 text-lg font-semibold tracking-tight sm:text-xl"
              id="settings-page-title"
            >
              系统设置
            </h1>
          </section>

          <Tabs
            className="min-w-0 gap-5 lg:gap-8"
            orientation={desktopNavigation ? 'vertical' : 'horizontal'}
            value={section}
            onValueChange={(value) => {
              if (value === 'access' || value === 'forwarding') onSectionChange(value)
            }}
          >
            <div className="settings-tabs-bar sticky z-10 min-w-0 self-start border-b bg-background/95 backdrop-blur lg:top-24 lg:w-56 lg:shrink-0 lg:border-s lg:border-b-0 lg:bg-transparent lg:backdrop-blur-none">
              <TabsList
                aria-label="设置分类"
                className="justify-start data-[orientation=vertical]:items-stretch"
                variant="underline"
              >
                {sections.map((item) => {
                  const Icon = item.icon
                  return (
                    <TabsTab
                      className="min-w-0 data-[orientation=vertical]:h-auto data-[orientation=vertical]:min-h-14 data-[orientation=vertical]:grow-0 data-[orientation=vertical]:items-start data-[orientation=vertical]:px-3 data-[orientation=vertical]:py-2.5"
                      key={item.key}
                      value={item.key}
                    >
                      <Icon aria-hidden="true" className="size-4 shrink-0 lg:mt-0.5" />
                      <span className="min-w-0">
                        <span className="block whitespace-nowrap font-medium">{item.label}</span>
                        <span className="mt-1 hidden text-xs leading-4 text-muted-foreground lg:block">
                          {item.description}
                        </span>
                      </span>
                    </TabsTab>
                  )
                })}
              </TabsList>
            </div>

            <TabsPanel className="min-w-0 pt-1 lg:pt-0" value="access">
              {section === 'access' && <AccessSettingsContent />}
            </TabsPanel>
            <TabsPanel className="min-w-0 pt-1 lg:pt-0" value="forwarding">
              {section === 'forwarding' && <ForwardingSettingsContent />}
            </TabsPanel>
          </Tabs>
        </div>
      </main>

      <AppFooter />
    </div>
  )
}
