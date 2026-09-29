import { createContext, useContext } from 'react'
import type { Credential } from '@/api/credentials'

/**
 * 打开「重新授权」对话框。对话框由 `ReauthorizeProvider`（add-account.tsx）挂在应用根部：
 * ⋯ 菜单一关，挂在它里面的弹窗会跟着卸载，而卡片、列表、详情页三处都有这份菜单，
 * 各挂一个对话框不如全局挂一个。没有 Provider（预览页等）时为 null，菜单不出这一项。
 */
export const ReauthorizeContext = createContext<((cred: Credential) => void) | null>(null)

export function useReauthorize() {
  return useContext(ReauthorizeContext)
}
