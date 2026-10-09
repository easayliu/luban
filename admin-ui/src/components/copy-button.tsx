import { useEffect, useRef, useState, type ReactNode } from 'react'
import { CheckIcon, ClipboardIcon } from 'lucide-react'
import { useI18n } from '@/lib/i18n'
import { copyText } from '@/lib/utils'
import { Button, type ButtonProps } from '@/components/ui/button'
import { toastManager } from '@/components/ui/toast'
import { Hint } from '@/components/ui/tooltip'

/**
 * 全站唯一的「复制到剪贴板」按钮。此前各页各写一份（图标版、带字版、只换图标不报错的…），
 * 成功提示的时长、失败要不要报错、读屏能不能听到都不一样。
 *
 * - 成功：图标换成对勾、文字换成「已复制」，1.2 秒后复原；读屏通过 live region 念出来；
 * - 失败：弹错误提示，让人手动复制（`errorDescription` 可换说法）；
 * - `disabledReason` 给了就禁用，并把原因放进悬浮提示——提示挂在外层 span 上，禁用的 Button
 *   带 pointer-events-none，挂在它自己身上时那句永远悬停不出来；
 * - 不给 `children` 是图标按钮（默认 `icon-xs`），给了就是带字按钮（默认 `outline`）。
 */
export function CopyButton({
  text,
  label,
  copiedLabel,
  errorDescription,
  disabledReason,
  size,
  variant,
  className,
  children,
}: {
  text: string
  /** 悬浮提示与读屏名称，缺省「复制」。 */
  label?: string
  copiedLabel?: string
  errorDescription?: string
  disabledReason?: string
  size?: ButtonProps['size']
  variant?: ButtonProps['variant']
  className?: string
  /** 带字按钮的文字；不给就是图标按钮。 */
  children?: ReactNode
}) {
  const { t } = useI18n()
  const [copied, setCopied] = useState(false)
  const attemptRef = useRef(0)
  const resetTimerRef = useRef<number | null>(null)
  const idleLabel = label ?? t('复制', 'Copy')
  const successLabel = copiedLabel ?? t('已复制', 'Copied')
  const labelled = children != null

  // 要复制的内容换了就复原，免得新内容旁边还挂着上一份的「已复制」。
  useEffect(() => {
    attemptRef.current += 1
    if (resetTimerRef.current !== null) {
      window.clearTimeout(resetTimerRef.current)
      resetTimerRef.current = null
    }
    setCopied(false)
    return () => {
      attemptRef.current += 1
      if (resetTimerRef.current !== null) window.clearTimeout(resetTimerRef.current)
    }
  }, [text])

  const copy = async () => {
    if (!text) return
    const attempt = ++attemptRef.current
    const ok = await copyText(text)
    if (attempt !== attemptRef.current) return
    if (ok) {
      if (resetTimerRef.current !== null) window.clearTimeout(resetTimerRef.current)
      setCopied(true)
      resetTimerRef.current = window.setTimeout(() => {
        setCopied(false)
        resetTimerRef.current = null
      }, 1200)
      return
    }
    toastManager.add({
      title: t('复制失败', 'Copy failed'),
      description: errorDescription ?? t('请手动选择并复制内容。', 'Select the content and copy it manually.'),
      type: 'error',
    })
  }

  const hint = copied ? successLabel : disabledReason ?? idleLabel
  return (
    <>
      <Hint label={hint}>
        <span className="inline-flex">
          <Button
            type="button"
            aria-label={labelled ? undefined : hint}
            className={[copied && !labelled ? 'text-success-foreground' : '', className ?? ''].join(' ').trim() || undefined}
            disabled={disabledReason !== undefined}
            size={size ?? (labelled ? 'default' : 'icon-xs')}
            variant={variant ?? (labelled ? 'outline' : 'ghost')}
            onClick={() => void copy()}
          >
            {copied ? <CheckIcon /> : <ClipboardIcon />}
            {labelled && (copied ? successLabel : children)}
          </Button>
        </span>
      </Hint>
      <span className="sr-only" role="status" aria-live="polite">
        {copied ? successLabel : ''}
      </span>
    </>
  )
}
