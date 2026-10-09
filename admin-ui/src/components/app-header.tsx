import { useState, type ReactNode } from 'react'
import {
  EllipsisVerticalIcon,
  KeyRoundIcon,
  LayersIcon,
  ReceiptIcon,
  SettingsIcon,
  LogOutIcon,
  UsersIcon,
} from 'lucide-react'
import { LogoMark } from '@/components/logo-mark'
import { LanguageMenuItem } from '@/components/language-switcher'
import { ThemeMenuItem } from '@/components/theme-switcher'
import { Badge } from '@/components/ui/badge'
import {
  Breadcrumb as BreadcrumbRoot,
  BreadcrumbItem,
  BreadcrumbLink,
  BreadcrumbList,
  BreadcrumbPage,
  BreadcrumbSeparator,
} from '@/components/ui/breadcrumb'
import { Button, buttonVariants } from '@/components/ui/button'
import { Hint } from '@/components/ui/tooltip'
import {
  Menu,
  MenuItem,
  MenuPopup,
  MenuSeparator,
  MenuTrigger,
} from '@/components/ui/menu'
import { useI18n } from '@/lib/i18n'
import { useCanManageUsers, useIsAdmin, useMe, useReadOnly } from '@/lib/role'
import { ChangePasswordDialog } from '@/components/change-password-dialog'

/**
 * 顶栏动作按钮的窄屏形态：手机上只留图标，撑成 40px 见方的点按目标；文字由按钮里的
 * `max-sm:sr-only` 留给读屏。各页顶栏的主动作和「更多操作」都用这一份，别再各抄一遍。
 */
export const HEADER_ACTION_CLASS = 'max-sm:size-10 max-sm:px-0'

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
  nav,
  onNavigateHome,
}: {
  actions?: ReactNode
  /** logo 的无障碍名与悬浮提示；不给就按「回到账号池」。 */
  homeLabel?: string
  /** 品牌右侧的主导航（见 [MainNav]）。 */
  nav?: ReactNode
  onNavigateHome?: () => void
}) {
  const { t } = useI18n()
  const readOnly = useReadOnly()
  const me = useMe().data
  const label = homeLabel ?? t('返回账号池', 'Back to the account pool')
  const brand = (
    <>
      <span className="brand-mark flex size-8 shrink-0 items-center justify-center rounded-lg text-white">
        {/* `opacity-100` 躲开 Button 给图标统一加的 80% 透明度：logo 是品牌色块里的白字，不该发灰。 */}
        <LogoMark className="size-[1.125rem] opacity-100" />
      </span>
      <span className="min-w-0 truncate text-sm font-semibold tracking-tight">Luban</span>
    </>
  )
  // 访客登录时常驻一枚「只读」：按钮都藏了，不说明的话像是页面坏了。代理和用户常驻身份与
  // 用户名：他们看到的账号池只有自己名下的号，得一眼看出是以谁的身份在看。
  const readOnlyBadge = readOnly ? (
    <Hint label={t('以访客身份登录：仅可查看，不可修改', 'Signed in as a viewer: read-only access')}>
      <Badge className="shrink-0" variant="warning">
        {t('只读', 'Read-only')}
      </Badge>
    </Hint>
  ) : me && (me.role === 'agent' || me.role === 'user') && (
    <Hint label={me.role === 'agent'
      ? t('以代理身份登录：可管理自己的账号与下属用户', 'Signed in as an agent: you can manage your own accounts and your users')
      : t('以用户身份登录：可管理自己的账号', 'Signed in as a user: you can manage your own accounts')}
    >
      <Badge className="max-w-40 shrink-0 truncate" variant="secondary">
        {me.role === 'agent' ? t('代理', 'Agent') : t('用户', 'User')}
        {me.username && ` · ${me.username}`}
      </Badge>
    </Hint>
  )

  return (
    <header className="app-header sticky top-0 z-20 border-b bg-background">
      <div className="page-frame flex h-14 items-center justify-between gap-3 sm:h-16">
        <div className="flex min-w-0 items-center gap-2">
        {onNavigateHome ? (
          <Hint label={label}>
            {/* 高度随内容（32px 色块 + 上下留白），不吃 Button 的固定尺寸；内边距扣掉 Button 那 1px
                透明边框，留白仍是横 8px、纵 6px。`shrink` 覆盖 Button 的 `shrink-0`，窄屏上品牌名才能截断。 */}
            <Button
              aria-label={label}
              className="-mx-2 h-auto min-w-0 shrink justify-start gap-2.5 px-[calc(--spacing(2)-1px)] py-[calc(--spacing(1.5)-1px)] sm:h-auto sm:gap-3"
              variant="ghost"
              onClick={onNavigateHome}
            >
              {brand}
            </Button>
          </Hint>
        ) : (
          <div className="flex min-w-0 items-center gap-2.5 sm:gap-3">{brand}</div>
        )}
        {nav && <div aria-hidden="true" className="mx-1 h-5 w-px shrink-0 bg-border max-sm:hidden" />}
        {nav}
        {readOnlyBadge}
        </div>
        {actions && <div className="flex items-center gap-2">{actions}</div>}
      </div>
    </header>
  )
}

/** 顶栏主导航的一级页面。 */
export type MainSection = 'pool' | 'billing' | 'users' | 'settings'

/**
 * 顶栏主导航：账号池、费用、用户管理、系统设置是平级的一级页面，不是谁挂在谁下面——每一页
 * 都能直接去别的页，不必先回账号池。用户管理只给管理员与代理，系统设置只给管理员。窄屏只留
 * 图标，文字留给读屏与悬浮提示。
 */
export function MainNav({
  current,
  onNavigate,
}: {
  current: MainSection
  onNavigate: (section: MainSection) => void
}) {
  const { t } = useI18n()
  const canManageUsers = useCanManageUsers()
  const isAdmin = useIsAdmin()
  const items = [
    { key: 'pool' as const, label: t('账号池', 'Accounts'), icon: LayersIcon },
    { key: 'billing' as const, label: t('费用', 'Billing'), icon: ReceiptIcon },
    ...(canManageUsers ? [{ key: 'users' as const, label: t('成员管理', 'Members'), icon: UsersIcon }] : []),
    ...(isAdmin ? [{ key: 'settings' as const, label: t('系统设置', 'Settings'), icon: SettingsIcon }] : []),
  ]
  return (
    <nav aria-label={t('主导航', 'Main navigation')} className="flex shrink-0 items-center gap-0.5">
      {items.map(({ key, label, icon: Icon }) => {
        const active = current === key
        return (
          <Hint key={key} label={label}>
            {/* 当前页靠 `aria-current` 着色：它是导航里的「所在位置」，不是按下的开关，所以不用
                `data-pressed`。悬浮只给半档底色，与当前页的整档区分开。`sm:h-8` 让桌面也保持 32px
                高（size="sm" 在桌面是 28px），导航项在 64px 的顶栏里不显得局促。 */}
            <Button
              aria-current={active ? 'page' : undefined}
              aria-label={label}
              className="text-muted-foreground hover:bg-accent/60 hover:text-foreground sm:h-8 aria-[current=page]:bg-accent aria-[current=page]:text-foreground"
              size="sm"
              variant="ghost"
              onClick={() => { if (!active) onNavigate(key) }}
            >
              <Icon aria-hidden="true" className="size-4" />
              <span className="max-sm:sr-only">{label}</span>
            </Button>
          </Hint>
        )
      })}
    </nav>
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
      <Hint label={t('更多操作', 'More actions')}>
        <MenuTrigger
          aria-label={t('更多操作', 'More actions')}
          className={buttonVariants({ size: 'sm', variant: 'outline', className: HEADER_ACTION_CLASS })}
        >
          <EllipsisVerticalIcon />
        </MenuTrigger>
      </Hint>
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

/**
 * 所有一级页面共用的账号菜单：页面自己的工具（`children`）在上，修改密码、语言、外观、退出
 * 在下。修改密码给代理和用户（管理员的密码在系统设置的「控制台安全」里改，那里还能清除；
 * 访客的密码由管理员设）。每页都用它，菜单里有什么不再随所在页面变。
 */
export function AccountMenu({
  children,
  onSignOut,
}: {
  children?: ReactNode
  onSignOut?: () => void
}) {
  const { t } = useI18n()
  const role = useMe().data?.role
  const [passwordOpen, setPasswordOpen] = useState(false)
  const canChangePassword = role === 'agent' || role === 'user'
  return (
    <>
      <PreferencesMenu onSignOut={onSignOut}>
        {children}
        {canChangePassword && (
          <MenuItem onClick={() => setPasswordOpen(true)}>
            <KeyRoundIcon />{t('修改密码', 'Change password')}
          </MenuItem>
        )}
      </PreferencesMenu>
      {canChangePassword && <ChangePasswordDialog open={passwordOpen} onOpenChange={setPasswordOpen} />}
    </>
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
    <BreadcrumbRoot aria-label={t('层级导航', 'Breadcrumb')}>
      {/* coss 的列表默认可换行、间距更宽；这里收成单行、两段各自截断，与顶栏下这条窄带的高度相称。 */}
      <BreadcrumbList className="min-w-0 flex-nowrap gap-1 sm:gap-1">
        <BreadcrumbItem className="min-w-0">
          <BreadcrumbLink
            className="truncate rounded-sm underline-offset-4 hover:underline focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-2 focus-visible:ring-offset-background"
            render={<button type="button" />}
            onClick={onNavigateParent}
          >
            {parent}
          </BreadcrumbLink>
        </BreadcrumbItem>
        <BreadcrumbSeparator className="flex shrink-0 items-center [&>svg]:size-3.5" />
        <BreadcrumbItem className="min-w-0">
          <BreadcrumbPage className="truncate font-medium">{current}</BreadcrumbPage>
        </BreadcrumbItem>
      </BreadcrumbList>
    </BreadcrumbRoot>
  )
}
