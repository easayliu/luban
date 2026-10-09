import { LanguagesIcon } from 'lucide-react'
import { Button } from '@/components/ui/button'
import { MenuItem } from '@/components/ui/menu'
import { Hint } from '@/components/ui/tooltip'
import { useI18n } from '@/lib/i18n'

/** 中 / 英切换的独立按钮（登录 / 初始化页）。顶栏里用的是下面的 [LanguageMenuItem]。 */
export function LanguageSwitcher({ compact = false }: { compact?: boolean }) {
  const { language, toggleLanguage } = useI18n()
  const switchingToEnglish = language === 'zh-CN'
  const label = switchingToEnglish ? '切换至英文界面' : 'Switch interface to Chinese'

  return (
    <Hint label={label}>
      <Button
        type="button"
        size={compact ? 'icon-lg' : 'sm'}
        variant="outline"
        onClick={toggleLanguage}
        aria-label={label}
      >
        <LanguagesIcon />
        {!compact && <span>{switchingToEnglish ? 'EN' : '中文'}</span>}
      </Button>
    </Hint>
  )
}

/** 菜单里的中 / 英切换（顶栏账号菜单）。与上面那枚按钮同一份逻辑，只是换了个壳。 */
export function LanguageMenuItem() {
  const { language, toggleLanguage } = useI18n()
  const zh = language === 'zh-CN'

  return (
    <MenuItem closeOnClick={false} onClick={toggleLanguage}>
      <LanguagesIcon />
      <span className="flex min-w-0 flex-1 items-center justify-between gap-4">
        <span>{zh ? '语言' : 'Language'}</span>
        <span className="text-xs text-muted-foreground">{zh ? '中文' : 'English'}</span>
      </span>
    </MenuItem>
  )
}
