import { useState, type ReactNode } from 'react'
import {
  ChevronRightIcon,
  EllipsisVerticalIcon,
  LanguagesIcon,
  LogOutIcon,
  MonitorIcon,
  MoonIcon,
  SunIcon,
} from 'lucide-react'
import { LogoMark } from '@/components/logo-mark'
import { buttonVariants } from '@/components/ui/button'
import {
  Menu,
  MenuItem,
  MenuPopup,
  MenuSeparator,
  MenuTrigger,
} from '@/components/ui/menu'
import { useI18n } from '@/lib/i18n'
import { readThemeMode, writeThemeMode, THEME_MODES, type ThemeMode } from '@/lib/theme'

/**
 * 全站顶栏：左边品牌、右边动作，两个页面共用同一个壳。
 *
 * 过去账号页与设置页各写一遍，同一块品牌区在账号页是死的 `div`、在设置页是一个 ghost
 * `Button`——同一个东西两种行为，而且那个「可点」不 hover 看不出来。现在统一成一件事：
 * **它永远可点**，悬浮反馈两页一致（Cloudflare 顶栏那枚 logo 也是永远存在的链接）。
 * 去处由调用方给：子页面回首页，首页本身回到顶部（见 [scrollToTop]）——不留「看着能点、
 * 点下去什么都不发生」的死控件，也不留「同一个东西这页能点那页不能」的不一致。
 *
 * 右上角那枚与它重复的「返回账号」按钮撤掉了——重复的是那一枚，不是 logo。
 *
 * 副标题「Claude Code Gateway」也去掉了：Cloudflare 那条顶栏里只有 logo，产品名之外的
 * 说明不挤在 56–64px 高的条里。
 */
export function AppHeader({
  actions,
  homeLabel,
  onNavigateHome,
}: {
  actions?: ReactNode
  /** logo 的无障碍名与悬浮提示；不给就按「回到账号池」。 */
  homeLabel?: string
  onNavigateHome?: () => void
}) {
  const { t } = useI18n()
  const label = homeLabel ?? t('回到账号池', 'Back to the account pool')
  const brand = (
    <>
      <span className="brand-mark flex size-8 shrink-0 items-center justify-center rounded-lg text-white">
        <LogoMark className="size-[1.125rem]" />
      </span>
      <span className="min-w-0 truncate text-sm font-semibold tracking-tight">Luban</span>
    </>
  )

  return (
    <header className="app-header sticky top-0 z-20 border-b bg-background">
      <div className="page-frame flex h-14 items-center justify-between gap-3 sm:h-16">
        {onNavigateHome ? (
          <button
            aria-label={label}
            className="-mx-2 flex min-w-0 cursor-pointer items-center gap-2.5 rounded-lg px-2 py-1.5 transition-colors hover:bg-accent focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-1 focus-visible:ring-offset-background sm:gap-3"
            title={label}
            type="button"
            onClick={onNavigateHome}
          >
            {brand}
          </button>
        ) : (
          <div className="flex min-w-0 items-center gap-2.5 sm:gap-3">{brand}</div>
        )}
        {actions && <div className="flex items-center gap-2">{actions}</div>}
      </div>
    </header>
  )
}

/**
 * 首页上那枚 logo 的去处：滚回顶部。账号列表能拉很长，这是 logo 在「已经到家」时的常见用途。
 *
 * 平滑滚动尊重系统的「减少动态效果」——`window.scrollTo` 不走 CSS，那条 media query
 * 拦不住它，得在这儿自己判。
 */
export function scrollToTop() {
  const reduced = window.matchMedia('(prefers-reduced-motion: reduce)').matches
  window.scrollTo({ top: 0, behavior: reduced ? 'auto' : 'smooth' })
}

/**
 * 顶栏右侧的收纳菜单：页面自己的次级动作（`children`）在上，偏好与退出在下。
 *
 * 原来桌面端把语言、外观、请求查询、封号记录、系统设置、添加账号、退出登录七枚按钮
 * 平铺在顶栏，手机端反倒早已收进菜单——收纳方向是反的。现在两端同一套：顶栏只留一枚
 * 主动作加这个菜单，偏好（语言 / 外观）和退出属于「账号」，不该占顶栏的位置。
 */
export function PreferencesMenu({
  children,
  onSignOut,
}: {
  children?: ReactNode
  onSignOut?: () => void
}) {
  const { t } = useI18n()

  return (
    <Menu>
      <MenuTrigger
        aria-label={t('更多操作', 'More actions')}
        className={buttonVariants({ size: 'sm', variant: 'outline', className: 'max-sm:size-10 max-sm:px-0' })}
        title={t('更多操作', 'More actions')}
      >
        <EllipsisVerticalIcon />
      </MenuTrigger>
      <MenuPopup align="end" className="w-52">
        {children}
        {children && <MenuSeparator />}
        <LanguageMenuItem />
        <ThemeMenuItem />
        {onSignOut && (
          <>
            <MenuSeparator />
            <MenuItem variant="destructive" onClick={onSignOut}>
              <LogOutIcon />
              {t('退出登录', 'Sign out')}
            </MenuItem>
          </>
        )}
      </MenuPopup>
    </Menu>
  )
}

/** 中 / 英切换。与顶栏上那枚独立按钮同一份逻辑，只是换了个壳。 */
function LanguageMenuItem() {
  const { language, toggleLanguage } = useI18n()
  const toEnglish = language === 'zh-CN'

  return (
    <MenuItem closeOnClick={false} onClick={toggleLanguage}>
      <LanguagesIcon />
      <span className="flex min-w-0 flex-1 items-center justify-between gap-4">
        <span>{toEnglish ? '语言' : 'Language'}</span>
        <span className="text-xs text-muted-foreground">{toEnglish ? '中文' : 'English'}</span>
      </span>
    </MenuItem>
  )
}

const THEME_ICONS: Record<ThemeMode, typeof MonitorIcon> = {
  system: MonitorIcon,
  light: SunIcon,
  dark: MoonIcon,
}

/** 系统 → 浅色 → 深色 循环。三态本来就少，点一下换一档，不必再套一层子菜单。 */
function ThemeMenuItem() {
  const { t } = useI18n()
  const [mode, setMode] = useState<ThemeMode>(readThemeMode)
  const next = THEME_MODES[(THEME_MODES.indexOf(mode) + 1) % THEME_MODES.length]
  const Icon = THEME_ICONS[mode]
  const name = (value: ThemeMode) => ({
    system: t('跟随系统', 'System'),
    light: t('浅色', 'Light'),
    dark: t('深色', 'Dark'),
  })[value]

  return (
    <MenuItem
      aria-label={t(`外观：${name(mode)}，点击切换到${name(next)}`, `Appearance: ${name(mode)}. Switch to ${name(next)}`)}
      closeOnClick={false}
      onClick={() => {
        setMode(next)
        writeThemeMode(next)
      }}
    >
      <Icon />
      <span className="flex min-w-0 flex-1 items-center justify-between gap-4">
        <span>{t('外观', 'Appearance')}</span>
        <span className="text-xs text-muted-foreground">{name(mode)}</span>
      </span>
    </MenuItem>
  )
}

/**
 * 顶栏底下那条层级线，Cloudflare 的设置页就是靠它回上一级的。
 *
 * 它和顶栏那枚 logo 各管一件事：logo 回首页，面包屑回**上一级**并且交代「我在哪儿」。
 * 撤掉的是原先右上角那枚「返回账号」按钮——它和 logo 做的是同一件事，纯重复。
 */
export function Breadcrumb({
  parent,
  current,
  onNavigateParent,
}: {
  parent: string
  current: string
  onNavigateParent: () => void
}) {
  const { t } = useI18n()

  return (
    <nav aria-label={t('层级导航', 'Breadcrumb')}>
      <ol className="flex min-w-0 items-center gap-1 text-sm text-muted-foreground">
        <li className="min-w-0">
          <button
            className="truncate rounded-sm underline-offset-4 transition-colors hover:text-foreground hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background"
            type="button"
            onClick={onNavigateParent}
          >
            {parent}
          </button>
        </li>
        <li aria-hidden="true" className="flex shrink-0 items-center">
          <ChevronRightIcon className="size-3.5" />
        </li>
        <li aria-current="page" className="min-w-0 truncate font-medium text-foreground">
          {current}
        </li>
      </ol>
    </nav>
  )
}
