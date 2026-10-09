import { useState } from 'react'
import { MonitorIcon, MoonIcon, SunIcon } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { MenuItem } from '@/components/ui/menu'
import { Hint } from '@/components/ui/tooltip'
import { useI18n } from '@/lib/i18n'
import { readThemeMode, writeThemeMode, THEME_MODES, type ThemeMode } from '@/lib/theme'

const THEME_ICONS: Record<ThemeMode, typeof MonitorIcon> = {
  system: MonitorIcon,
  light: SunIcon,
  dark: MoonIcon,
}

/**
 * 系统 → 浅色 → 深色 循环。登录 / 初始化页的独立按钮与顶栏菜单里那一项共用这一份：
 * 当前档、下一档、图标、名称与读屏标签只在这里算。
 */
function useThemeCycle() {
  const { t } = useI18n()
  const [mode, setMode] = useState<ThemeMode>(readThemeMode)
  const next = THEME_MODES[(THEME_MODES.indexOf(mode) + 1) % THEME_MODES.length]
  const name = (value: ThemeMode) => ({
    system: t('跟随系统', 'System'),
    light: t('浅色', 'Light'),
    dark: t('深色', 'Dark'),
  })[value]
  return {
    Icon: THEME_ICONS[mode],
    modeName: name(mode),
    label: t(`外观：${name(mode)}，点击切换为${name(next)}`, `Appearance: ${name(mode)}. Switch to ${name(next)}`),
    cycle: () => {
      setMode(next)
      writeThemeMode(next)
    },
  }
}

/**
 * 做成循环按钮而不是下拉：三态本来就少，头部按钮位紧张，多一个弹层不划算；
 * 当前模式由图标直接表达。
 */
export function ThemeSwitcher({ compact = false }: { compact?: boolean }) {
  const { Icon, label, cycle } = useThemeCycle()

  return (
    <Hint label={label}>
      <Button
        type="button"
        size={compact ? 'icon-lg' : 'icon-sm'}
        variant="outline"
        onClick={cycle}
        aria-label={label}
      >
        <Icon />
      </Button>
    </Hint>
  )
}

/** 菜单里的那一项：点一下换一档，不必再套一层子菜单；右侧写出当前档。 */
export function ThemeMenuItem() {
  const { t } = useI18n()
  const { Icon, modeName, label, cycle } = useThemeCycle()

  return (
    <MenuItem aria-label={label} closeOnClick={false} onClick={cycle}>
      <Icon />
      <span className="flex min-w-0 flex-1 items-center justify-between gap-4">
        <span>{t('外观', 'Appearance')}</span>
        <span className="text-xs text-muted-foreground">{modeName}</span>
      </span>
    </MenuItem>
  )
}
