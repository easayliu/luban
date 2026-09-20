import { useEffect } from 'react'
import {
  ArrowRightLeftIcon,
  CableIcon,
  GlobeIcon,
  LockKeyholeIcon,
  SlidersHorizontalIcon,
  SmartphoneIcon,
} from 'lucide-react'
import {
  AccessSettingsContent,
  DeviceSettingsContent,
  SecuritySettingsContent,
} from '@/components/access-settings'
import { AppFooter } from '@/components/app-footer'
import { ForwardingSettingsContent } from '@/components/forwarding-settings'
import { MigrationSettingsContent } from '@/components/migration-settings'
import { ProxyPoolSettingsContent } from '@/components/proxy-pool-settings'
import { AppHeader, Breadcrumb, PreferencesMenu } from '@/components/app-header'
import {
  Select,
  SelectItem,
  SelectPopup,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { Tabs, TabsList, TabsPanel, TabsTab } from '@/components/ui/tabs'
import { useI18n } from '@/lib/i18n'
import { useMediaQuery } from '@/lib/use-media-query'

export type SettingsSection = 'access' | 'devices' | 'proxies' | 'forwarding' | 'security' | 'migration'

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
      label: t('客户端接入', 'Client access'),
      navDescription: t('地址、Key 与配置片段', 'Endpoint, key, and setup'),
      icon: CableIcon,
    },
    {
      key: 'devices',
      label: t('设备策略', 'Device policies'),
      navDescription: t('绑定、容量与身份校验', 'Bindings, capacity, and identity'),
      icon: SmartphoneIcon,
    },
    {
      key: 'proxies',
      label: t('代理池', 'Proxy pool'),
      navDescription: t('出站代理地址管理', 'Outbound proxy management'),
      icon: GlobeIcon,
    },
    {
      key: 'forwarding',
      label: t('转发策略', 'Forwarding policy'),
      navDescription: t('授权、兼容与错误恢复', 'Scopes, compatibility, and recovery'),
      icon: SlidersHorizontalIcon,
    },
    {
      key: 'migration',
      label: t('迁移', 'Migration'),
      navDescription: t('导出与导入账号', 'Export and import accounts'),
      icon: ArrowRightLeftIcon,
    },
    {
      key: 'security',
      label: t('控制台安全', 'Console security'),
      navDescription: t('登录与管理密码', 'Sign-in and admin password'),
      icon: LockKeyholeIcon,
    },
  ] as const
  const active = sections.find((item) => item.key === section) ?? sections[0]
  const ActiveIcon = active.icon
  const desktopNavigation = useMediaQuery('(min-width: 64rem)')
  const selectItems = sections.map((item) => ({ label: item.label, value: item.key }))

  const changeSection = (value: string | null) => {
    if (value && sections.some((item) => item.key === value)) {
      onSectionChange(value as SettingsSection)
    }
  }

  useEffect(() => {
    const previousTitle = document.title
    document.title = `${active.label} · Luban`
    return () => {
      document.title = previousTitle
    }
  }, [active.label])

  return (
    <div className="app-shell flex min-h-dvh flex-col text-foreground">
      <AppHeader actions={<PreferencesMenu />} onNavigateHome={onBack} />

      <main className="page-frame relative flex-1 py-5 pb-8 sm:py-8 sm:pb-12">
        <div className="space-y-5 sm:space-y-7">
          {/* 层级线：既交代「我在哪儿」，也是回账号页的入口之一（顶栏那枚 logo 是另一个，
              两者都是标准做法；撤掉的是右上角那枚与 logo 完全重复的「返回账号」按钮）。 */}
          <Breadcrumb
            current={t('系统设置', 'System settings')}
            parent={t('账号池', 'Account pool')}
            onNavigateParent={onBack}
          />

          <section aria-labelledby="settings-page-title" className="max-w-2xl">
            <h1
              className="min-w-0 text-xl font-semibold tracking-tight sm:text-2xl"
              id="settings-page-title"
            >
              {t('系统设置', 'System settings')}
            </h1>
            <p className="mt-1.5 text-sm leading-6 text-muted-foreground">
              {t(
                '集中管理 Luban 的客户端接入、设备绑定、转发行为与控制台安全。',
                'Manage client access, device bindings, forwarding behaviour, and console security.',
              )}
            </p>
          </section>

          <Tabs
            className="min-w-0 gap-5 lg:items-start lg:gap-8"
            orientation={desktopNavigation ? 'vertical' : 'horizontal'}
            value={section}
            onValueChange={changeSection}
          >
            <div className="settings-tabs-bar sticky z-10 min-w-0 self-start bg-surface-page py-2 lg:top-24 lg:w-60 lg:shrink-0 lg:bg-transparent lg:py-0">
              <div className="lg:hidden">
                <label className="sr-only" htmlFor="settings-section-select">
                  {t('设置分类', 'Settings category')}
                </label>
                <Select items={selectItems} value={section} onValueChange={changeSection}>
                  <SelectTrigger id="settings-section-select" aria-label={t('设置分类', 'Settings category')}>
                    <ActiveIcon aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
                    <SelectValue />
                  </SelectTrigger>
                  <SelectPopup>
                    {sections.map((item) => {
                      const Icon = item.icon
                      return (
                        <SelectItem key={item.key} value={item.key}>
                          <span className="flex min-w-0 items-center gap-2">
                            <Icon aria-hidden="true" className="size-4 shrink-0 text-muted-foreground" />
                            <span className="truncate">{item.label}</span>
                          </span>
                        </SelectItem>
                      )
                    })}
                  </SelectPopup>
                </Select>
              </div>

              <TabsList
                aria-label={t('设置分类', 'Settings categories')}
                className="hidden w-full items-stretch rounded-xl p-1 lg:flex"
              >
                {sections.map((item) => {
                  const Icon = item.icon
                  return (
                    <TabsTab
                      className="h-auto min-h-15 min-w-0 grow-0 items-start whitespace-normal px-3 py-2.5 text-left"
                      key={item.key}
                      value={item.key}
                    >
                      <span className="mt-0.5 flex size-5 shrink-0 items-center justify-center">
                        <Icon aria-hidden="true" className="size-4" />
                      </span>
                      <span className="min-w-0 flex-1 text-left">
                        <span className="block font-medium">{item.label}</span>
                        <span className="mt-1 block max-w-full text-xs leading-4 text-muted-foreground">
                          {item.navDescription}
                        </span>
                      </span>
                    </TabsTab>
                  )
                })}
              </TabsList>
            </div>

            {/* 内容列铺满，不要给它封 `max-w-*`。
                这里试过封到 56rem，想缩短「标签 → 控件」的扫视距离（`page-frame` 的 88rem 是
                当初为账号列表 12 列定的，见 index.css）。结果是内容右边界比顶栏短了 240px：
                240(导航) + 32(间距) + 896(内容) = 1168，而顶栏铺满 1408，一眼就看得出没对齐。
                顶栏是全站 chrome、改不得，所以该让步的是这里。
                行长的问题另有出路且已经解决——每行说明各自封在 `max-w-xl`（见 SettingsRow），
                真正难读的是正文行长，不是卡片宽度。 */}
            <div className="min-w-0 flex-1">
              {/* 分区头只剩一行标题：原来这里是「图标徽章 + 标题 + 一句话」，可同一个图标左边导航
                  刚画过、同一句话下面第一张卡片又要再说一遍（「代理池」那一区最明显：三个字出现
                  三次、描述出现两次）。左导航已经交代了「在哪一区」，这里留一个 h2 接住标题层级
                  与 aria 就够，各区具体讲什么交给卡片头自己说。 */}
              <h2 className="mb-4 font-semibold text-lg tracking-tight">{active.label}</h2>

              <TabsPanel className="min-w-0" value="access">
                {section === 'access' && <AccessSettingsContent />}
              </TabsPanel>
              <TabsPanel className="min-w-0" value="devices">
                {section === 'devices' && <DeviceSettingsContent />}
              </TabsPanel>
              <TabsPanel className="min-w-0" value="proxies">
                {section === 'proxies' && <ProxyPoolSettingsContent />}
              </TabsPanel>
              <TabsPanel className="min-w-0" value="forwarding">
                {section === 'forwarding' && <ForwardingSettingsContent />}
              </TabsPanel>
              <TabsPanel className="min-w-0" value="migration">
                {section === 'migration' && <MigrationSettingsContent />}
              </TabsPanel>
              <TabsPanel className="min-w-0" value="security">
                {section === 'security' && <SecuritySettingsContent />}
              </TabsPanel>
            </div>
          </Tabs>
        </div>
      </main>

      <AppFooter />
    </div>
  )
}
