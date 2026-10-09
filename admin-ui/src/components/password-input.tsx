import { useState } from 'react'
import { DicesIcon, EyeIcon, EyeOffIcon } from 'lucide-react'
import { useI18n } from '@/lib/i18n'
import { Button } from '@/components/ui/button'
import { InputGroup, InputGroupAddon, InputGroupInput } from '@/components/ui/input-group'
import { Hint } from '@/components/ui/tooltip'

/** 控制台密码的最短长度（去掉首尾空白后），与后端 `auth::MIN_PASSWORD_LEN` 一致。 */
export const MIN_PASSWORD_LENGTH = 4

/** 生成一个好读好抄的初始密码：12 位，去掉 0/O、1/l/I 这类容易看错的字符。 */
export function generatePassword(): string {
  const alphabet = 'abcdefghjkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789'
  const bytes = new Uint32Array(12)
  crypto.getRandomValues(bytes)
  return Array.from(bytes, (b) => alphabet[b % alphabet.length]).join('')
}

/**
 * 全站唯一的密码输入框：可切换明文，`generate` 时多一枚「随机生成」（生成后自动显示，方便
 * 核对）。登录、初始化、改密码、开账号、重置密码、访客密码都用它，行为一致。
 */
export function PasswordInput({
  value,
  onChange,
  id,
  name,
  autoFocus,
  autoComplete = 'new-password',
  placeholder,
  invalid,
  generate = false,
  ariaLabel,
  size,
}: {
  value: string
  onChange: (value: string) => void
  id?: string
  name?: string
  autoFocus?: boolean
  autoComplete?: string
  placeholder?: string
  invalid?: boolean
  /** 显示「随机生成」按钮（开账号、重置密码时用）。 */
  generate?: boolean
  ariaLabel?: string
  size?: 'sm' | 'default'
}) {
  const { t } = useI18n()
  const [show, setShow] = useState(false)
  const toggleLabel = show ? t('隐藏密码', 'Hide password') : t('显示密码', 'Show password')
  return (
    <InputGroup>
      <InputGroupInput
        aria-invalid={invalid || undefined}
        aria-label={ariaLabel}
        autoComplete={autoComplete}
        autoFocus={autoFocus}
        id={id}
        name={name}
        onChange={(event) => onChange(event.target.value)}
        placeholder={placeholder}
        size={size}
        type={show ? 'text' : 'password'}
        value={value}
      />
      <InputGroupAddon align="inline-end">
        <Hint label={toggleLabel}>
          <Button aria-label={toggleLabel} size="icon-xs" type="button" variant="ghost" onClick={() => setShow((v) => !v)}>
            {show ? <EyeOffIcon /> : <EyeIcon />}
          </Button>
        </Hint>
        {generate && (
          <Hint label={t('随机生成', 'Generate')}>
            <Button
              aria-label={t('随机生成密码', 'Generate a password')}
              size="icon-xs"
              type="button"
              variant="ghost"
              onClick={() => { onChange(generatePassword()); setShow(true) }}
            >
              <DicesIcon />
            </Button>
          </Hint>
        )}
      </InputGroupAddon>
    </InputGroup>
  )
}
