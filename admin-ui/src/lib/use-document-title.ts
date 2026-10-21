import { useEffect } from 'react'
import { useInstanceName } from '@/lib/instance'

/** 标签页标题：命名了实例就冠在最前，同时开着几个 luban 时标签页一眼分得清。 */
export function withInstance(title: string, instance: string | null): string {
  return instance ? `${instance} · ${title}` : title
}

/**
 * 页面挂着时把标签页标题设成 `title`（冠以实例名称），卸载时还原。各页原来各写一份同样的 effect。
 */
export function useDocumentTitle(raw: string): void {
  const title = withInstance(raw, useInstanceName())
  useEffect(() => {
    const previous = document.title
    document.title = title
    return () => {
      document.title = previous
    }
  }, [title])
}
