import {
  createContext,
  useCallback,
  useContext,
  useEffect,
  useMemo,
  useState,
  type ReactNode,
} from 'react'

export const LANGUAGES = ['zh-CN', 'en'] as const
export type Language = (typeof LANGUAGES)[number]

const STORAGE_KEY = 'luban.language'

function isLanguage(value: string | null): value is Language {
  return value === 'zh-CN' || value === 'en'
}

function readLanguage(): Language {
  try {
    const stored = localStorage.getItem(STORAGE_KEY)
    if (isLanguage(stored)) return stored
  } catch {
    // 存储不可用时保持原有中文默认，不影响页面使用。
  }
  return 'zh-CN'
}

function persistLanguage(language: Language): void {
  try {
    localStorage.setItem(STORAGE_KEY, language)
  } catch {
    // 隐私模式或禁用存储时只保留本次会话状态。
  }
}

export function localize(language: Language, chinese: string, english: string): string {
  return language === 'zh-CN' ? chinese : english
}

interface I18nContextValue {
  language: Language
  locale: 'zh-CN' | 'en-US'
  setLanguage: (language: Language) => void
  toggleLanguage: () => void
  t: (chinese: string, english: string) => string
}

const I18nContext = createContext<I18nContextValue | null>(null)

export function LanguageProvider({ children }: { children: ReactNode }) {
  const [language, setLanguageState] = useState<Language>(readLanguage)

  const setLanguage = useCallback((next: Language) => {
    setLanguageState(next)
    persistLanguage(next)
  }, [])

  const toggleLanguage = useCallback(() => {
    setLanguageState((current) => {
      const next = current === 'zh-CN' ? 'en' : 'zh-CN'
      persistLanguage(next)
      return next
    })
  }, [])

  const t = useCallback(
    (chinese: string, english: string) => localize(language, chinese, english),
    [language],
  )

  useEffect(() => {
    document.documentElement.lang = language
  }, [language])

  useEffect(() => {
    const syncAcrossTabs = (event: StorageEvent) => {
      if (event.key === STORAGE_KEY && isLanguage(event.newValue)) {
        setLanguageState(event.newValue)
      }
    }
    window.addEventListener('storage', syncAcrossTabs)
    return () => window.removeEventListener('storage', syncAcrossTabs)
  }, [])

  const value = useMemo<I18nContextValue>(() => ({
    language,
    locale: language === 'zh-CN' ? 'zh-CN' : 'en-US',
    setLanguage,
    toggleLanguage,
    t,
  }), [language, setLanguage, t, toggleLanguage])

  return <I18nContext.Provider value={value}>{children}</I18nContext.Provider>
}

export function useI18n(): I18nContextValue {
  const context = useContext(I18nContext)
  if (!context) throw new Error('useI18n must be used inside LanguageProvider')
  return context
}
