import { useEffect } from 'react'
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

  useEffect(() => {
    const previousTitle = document.title
    document.title = `${active.label} · Luban`
    return () => {
      document.title = previousTitle
    }
  }, [active.label])

  return (
    <div className="app-shell flex min-h-dvh flex-col text-foreground">
      <header className="app-header sticky top-0 z-20 border-b bg-background/95 backdrop-blur">
        <div className="page-frame flex min-h-16 items-center justify-between gap-3 py-3">
          <div className="flex min-w-0 items-center gap-2">
            <span className="brand-mark flex size-7 shrink-0 items-center justify-center rounded-lg text-brand-foreground">
              <LogoMark className="size-4" />
            </span>
            <span className="text-left">
              <span className="block font-semibold leading-none tracking-tight">Luban</span>
              <span className="mt-1 hidden text-xs font-normal text-muted-foreground sm:block">系统设置</span>
            </span>
          </div>
          <Button size="sm" variant="outline" onClick={onBack}>
            <ArrowLeftIcon aria-hidden="true" />
            返回账号
          </Button>
        </div>
      </header>

      <main className="page-frame flex-1 py-6 sm:py-8">
        <div className="mx-auto w-full max-w-5xl">
          <div className="mb-5 space-y-1 sm:mb-6">
            <h1 className="text-xl font-semibold tracking-tight sm:text-2xl">系统设置</h1>
            <p className="text-sm text-muted-foreground">管理客户端接入、设备限制与请求转发策略。</p>
          </div>

          <Tabs
            className="gap-0"
            value={section}
            onValueChange={(value) => {
              if (value === 'access' || value === 'forwarding') onSectionChange(value)
            }}
          >
            <div className="sticky top-16 z-10 border-b bg-background/95 backdrop-blur">
              <TabsList
                aria-label="设置分类"
                className="w-full justify-start sm:w-fit"
                variant="underline"
              >
                {sections.map((item) => {
                  const Icon = item.icon
                  return (
                    <TabsTab className="flex-1 sm:flex-none" key={item.key} value={item.key}>
                      <Icon aria-hidden="true" />
                      {item.label}
                    </TabsTab>
                  )
                })}
              </TabsList>
            </div>

            <TabsPanel className="min-w-0 pt-6" value="access">
              {section === 'access' && <AccessSettingsContent />}
            </TabsPanel>
            <TabsPanel className="min-w-0 pt-6" value="forwarding">
              {section === 'forwarding' && <ForwardingSettingsContent />}
            </TabsPanel>
          </Tabs>
        </div>
      </main>

      <AppFooter />
    </div>
  )
}
