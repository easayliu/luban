import { useEffect, useState } from 'react'
import { GaugeIcon } from 'lucide-react'
import { type Credential } from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import { displayCredentialLabel } from '@/lib/utils'
import { ClampedDescription } from '@/components/settings-group'
import { type CredentialActions } from '@/components/credential-shared'
import { Alert, AlertDescription } from '@/components/ui/alert'
import { Button } from '@/components/ui/button'
import {
  Dialog,
  DialogClose,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogPanel,
  DialogPopup,
  DialogTitle,
} from '@/components/ui/dialog'
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import {
  NumberField, NumberFieldDecrement, NumberFieldGroup, NumberFieldIncrement, NumberFieldInput,
} from '@/components/ui/number-field'
import { Select, SelectItem, SelectPopup, SelectTrigger, SelectValue } from '@/components/ui/select'

/** 三态策略；与后端的 `rpm_limit` 取值一一对应（0 / -1 / 正数）。 */
type RpmPolicy = 'default' | 'unlimited' | 'custom'

const POLICY_ITEMS = [
  { value: 'default', chinese: '跟随默认', english: 'Use default' },
  { value: 'unlimited', chinese: '不限', english: 'Unlimited' },
  { value: 'custom', chinese: '独立上限', english: 'Custom limit' },
] as const

function policyFromLimit(limit: number): RpmPolicy {
  if (limit === 0) return 'default'
  if (limit < 0) return 'unlimited'
  return 'custom'
}

/**
 * 逐账号 RPM 上限的编辑框：这个号最近 60 秒最多转发多少条请求。
 *
 * 口径与列表里那列「RPM」完全一致（同一个 60 秒窗口、同样含失败与 count_tokens），
 * 所以两个数可以直接比着看。三态与设备上限对齐：跟随全局默认 / 明确不限 / 独立上限。
 */
export function CredentialRpmDialog({
  cred,
  open,
  onOpenChange,
  rpmLimit,
}: {
  cred: Credential
  open: boolean
  onOpenChange: (open: boolean) => void
  rpmLimit: CredentialActions['rpmLimit']
}) {
  const { t, language, locale } = useI18n()
  const credentialLabel = displayCredentialLabel(cred.label, language)
  const [policy, setPolicy] = useState<RpmPolicy>(() => policyFromLimit(cred.rpm_limit))
  // 自定义值的初值：本来就是独立上限就沿用它，否则拿生效值起步（多半就是想在它附近调），
  // 再兜底一个 60。
  const [custom, setCustom] = useState(() =>
    Math.max(1, cred.rpm_limit > 0 ? cred.rpm_limit : cred.rpm_limit_effective || 60))

  // 每次打开都从服务端那份重置：上次改了一半没保存就关掉的残留留到下次，会让人以为它已经生效。
  useEffect(() => {
    if (!open) return
    setPolicy(policyFromLimit(cred.rpm_limit))
    setCustom(Math.max(1, cred.rpm_limit > 0 ? cred.rpm_limit : cred.rpm_limit_effective || 60))
  }, [open, cred.rpm_limit, cred.rpm_limit_effective])

  const policyItems = POLICY_ITEMS.map((item) => ({
    value: item.value,
    label: t(item.chinese, item.english),
  }))
  const next = policy === 'default' ? 0 : policy === 'unlimited' ? -1 : Math.max(1, Math.floor(custom))
  const dirty = next !== cred.rpm_limit
  const effective = cred.rpm_limit_effective > 0
    ? t(
      `${cred.rpm_limit_effective.toLocaleString(locale)} 条 / 分钟`,
      `${cred.rpm_limit_effective.toLocaleString(locale)} req/min`,
    )
    : t('不限', 'Unlimited')

  const save = () => rpmLimit.mutate(next, { onSuccess: () => onOpenChange(false) })

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogPopup>
        <DialogHeader>
          <DialogTitle>{t('RPM 上限', 'RPM limit')}</DialogTitle>
          <DialogDescription className="mt-1 truncate" title={credentialLabel}>
            {credentialLabel}
          </DialogDescription>
        </DialogHeader>

        <DialogPanel className="space-y-4">
          <div className="flex items-baseline justify-between gap-2 rounded-lg border px-3 py-2">
            <span className="text-muted-foreground text-xs">{t('当前 / 生效上限', 'Current / effective limit')}</span>
            <span className="font-medium text-sm tabular-nums">
              {cred.rpm.toLocaleString(locale)}
              <span className="text-muted-foreground">
                {' / '}
                {cred.rpm_limit_effective > 0 ? cred.rpm_limit_effective.toLocaleString(locale) : '∞'}
              </span>
            </span>
          </div>

          <div className="grid gap-4 sm:grid-cols-2">
            <Field>
              <FieldLabel>{t('上限策略', 'Limit policy')}</FieldLabel>
              <Select
                items={policyItems}
                value={policy}
                onValueChange={(value) => { if (value) setPolicy(value as RpmPolicy) }}
              >
                <SelectTrigger aria-label={t('上限策略', 'Limit policy')}>
                  <SelectValue />
                </SelectTrigger>
                <SelectPopup>
                  {policyItems.map((item) => (
                    <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>
                  ))}
                </SelectPopup>
              </Select>
              <FieldDescription>
                {t(
                  `「跟随默认」套用全局设置，当前为 ${effective}。`,
                  `"Use default" applies the global setting, currently ${effective}.`,
                )}
              </FieldDescription>
            </Field>

            {policy === 'custom' && (
              <Field>
                <FieldLabel>{t('每分钟最多请求数', 'Maximum requests per minute')}</FieldLabel>
                <NumberField
                  value={custom}
                  min={1}
                  step={1}
                  onValueChange={(value) => setCustom(Math.max(1, Math.floor(value ?? 1)))}
                >
                  <NumberFieldGroup>
                    <NumberFieldDecrement />
                    <NumberFieldInput aria-label={t('自定义 RPM 上限', 'Custom RPM limit')} />
                    <NumberFieldIncrement />
                  </NumberFieldGroup>
                </NumberField>
                <FieldDescription>
                  {t('该设置只影响当前账号。', 'This setting only affects the current account.')}
                </FieldDescription>
              </Field>
            )}
          </div>

          <Alert>
            <GaugeIcon />
            <AlertDescription>
              {/* 长说明默认收两行、末尾「了解更多」，同设置页的 ClampedDescription：超过 140 字各宽度都收，60–140 字只在手机上收。 */}
              <ClampedDescription text={t(
                '达到上限后：尚未确定账号的请求会自动分流到其他账号；已绑定到该账号的设备则直接收到 429 与 retry-after，等窗口内腾出名额后再继续（中途换账号会导致改绑，该会话之后每一轮都要先遇到一次 thinking 签名 400）。计数保存在服务端内存中，重启即清零。',
                'Once the limit is reached: requests not yet pinned to an account spill over to other accounts, while devices already bound to this account get a 429 with retry-after and resume when the window frees a slot (swapping accounts mid-session rebinds the device, which costs a thinking-signature 400 on every later turn of that session). Counts live in server memory and reset on restart.',
              )} />
            </AlertDescription>
          </Alert>
        </DialogPanel>

        <DialogFooter>
          <DialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</DialogClose>
          <Button onClick={save} disabled={!dirty || rpmLimit.isPending} loading={rpmLimit.isPending}>
            {t('保存', 'Save')}
          </Button>
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  )
}
