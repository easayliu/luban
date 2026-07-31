import { useEffect, useState } from 'react'
import { ArrowLeftIcon, Settings2Icon, SlidersHorizontalIcon } from 'lucide-react'
import { AccessSettingsContent } from '@/components/access-settings'
import { AppFooter } from '@/components/app-footer'
import { ForwardingSettingsContent } from '@/components/forwarding-settings'
import { LanguageSwitcher } from '@/components/language-switcher'
import { LogoMark } from '@/components/logo-mark'
import { Button } from '@/components/ui/button'
import { Tabs, TabsList, TabsPanel, TabsTab } from '@/components/ui/tabs'
import { useI18n } from '@/lib/i18n'

export type SettingsSection = 'access' | 'forwarding'

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
  const { t } = useI18n()
  const sections = [
    {
      key: 'access',
      label: t('接入与安全', 'Access & security'),
      description: t('客户端、设备和登录', 'Clients, devices, and sign-in'),
      icon: Settings2Icon,
    },
    {
      key: 'forwarding',
      label: t('转发策略', 'Forwarding policy'),
      description: t('兼容、缓存与错误恢复', 'Compatibility, caching, and error recovery'),
      icon: SlidersHorizontalIcon,
    },
  ] as const
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
            aria-label={t('返回账号页', 'Back to accounts')}
            className="-ml-2 h-auto min-w-0 justify-start gap-2.5 px-2 py-1.5 sm:gap-3"
            title={t('返回账号页', 'Back to accounts')}
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
          <div className="flex items-center gap-2">
            <LanguageSwitcher compact />
            <Button
              aria-label={t('返回账号', 'Back to accounts')}
              className="max-sm:size-10 max-sm:px-0"
              size="sm"
              title={t('返回账号', 'Back to accounts')}
              variant="outline"
              onClick={onBack}
            >
              <ArrowLeftIcon aria-hidden="true" />
              <span className="max-sm:sr-only">{t('返回账号', 'Back to accounts')}</span>
            </Button>
          </div>
        </div>
      </header>

      <main className="page-frame relative flex-1 py-5 pb-8 sm:py-8 sm:pb-12">
        <div className="space-y-4 sm:space-y-6">
          <section aria-labelledby="settings-page-title">
            <h1
              className="min-w-0 text-lg font-semibold tracking-tight sm:text-xl"
              id="settings-page-title"
            >
              {t('系统设置', 'System settings')}
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
            <div className="settings-tabs-bar sticky z-10 w-full min-w-0 max-w-full self-start overflow-x-auto border-b bg-background/95 backdrop-blur lg:top-24 lg:w-64 lg:shrink-0 lg:overflow-visible lg:border-s lg:border-b-0 lg:bg-transparent lg:backdrop-blur-none">
              <TabsList
                aria-label={t('设置分类', 'Settings categories')}
                className="justify-start data-[orientation=horizontal]:max-w-full data-[orientation=vertical]:w-full data-[orientation=vertical]:max-w-full data-[orientation=vertical]:items-stretch"
                variant="underline"
              >
                {sections.map((item) => {
                  const Icon = item.icon
                  return (
                    <TabsTab
                      className="min-w-0 max-[22rem]:px-1 data-[orientation=vertical]:h-auto data-[orientation=vertical]:min-h-14 data-[orientation=vertical]:grow-0 data-[orientation=vertical]:items-start data-[orientation=vertical]:whitespace-normal data-[orientation=vertical]:px-3 data-[orientation=vertical]:py-2.5 data-[orientation=vertical]:text-left"
                      key={item.key}
                      value={item.key}
                    >
                      <Icon aria-hidden="true" className="size-4 shrink-0 max-[22rem]:hidden lg:mt-0.5" />
                      <span className="min-w-0 flex-1 text-left">
                        <span className="block font-medium">{item.label}</span>
                        <span className="mt-1 hidden max-w-full break-words text-xs leading-4 text-muted-foreground lg:block">
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
