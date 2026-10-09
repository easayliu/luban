import { useEffect } from 'react'

/**
 * 页面挂着时把标签页标题设成 `title`，卸载时还原。各页原来各写一份同样的 effect。
 */
export function useDocumentTitle(title: string): void {
  useEffect(() => {
    const previous = document.title
    document.title = title
    return () => {
      document.title = previous
    }
  }, [title])
}
