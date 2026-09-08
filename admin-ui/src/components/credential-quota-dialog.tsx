import { useEffect, useState } from 'react'
import { PercentIcon } from 'lucide-react'
import { type Credential } from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import { displayCredentialLabel } from '@/lib/utils'
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

/** 三态策略；与后端的取值一一对应：跟随全局 = null，不停 = 0，独立阈值 = 1..100。 */
type QuotaPolicy = 'default' | 'off' | 'custom'

const POLICY_ITEMS = [
  { value: 'default', chinese: '跟随全局', english: 'Use global' },
  { value: 'off', chinese: '这一档不停', english: 'Off for this account' },
  { value: 'custom', chinese: '独立阈值', english: 'Custom threshold' },
] as const

function policyFromPct(pct: number | null): QuotaPolicy {
  if (pct === null) return 'default'
  if (pct <= 0) return 'off'
  return 'custom'
}

function pctFromPolicy(policy: QuotaPolicy, custom: number): number | null {
  if (policy === 'default') return null
  if (policy === 'off') return 0
  return Math.min(100, Math.max(1, Math.floor(custom)))
}

/** 自定义值的初值：本来就是独立阈值就沿用它，否则拿生效值起步（多半就是想在它附近调），再兜底 90。 */
function customSeed(pct: number | null, effective: number): number {
  if (pct !== null && pct > 0) return pct
  return effective > 0 ? effective : 90
}

/**
 * 逐账号的「额度用到多少就提前停调度」编辑框：5h / 7d 两档各自三态——跟随全局 / 这一档
 * 不停 / 独立阈值。口径与设置页的全局阈值完全一致，只是作用域缩到这一个账号。
 */
export function CredentialQuotaDialog({
  cred,
  open,
  onOpenChange,
  quotaPause,
}: {
  cred: Credential
  open: boolean
  onOpenChange: (open: boolean) => void
  quotaPause: CredentialActions['quotaPause']
}) {
  const { t, language } = useI18n()
  const credentialLabel = displayCredentialLabel(cred.label, language)
  const [shortPolicy, setShortPolicy] = useState<QuotaPolicy>(() => policyFromPct(cred.quota_pause_pct))
  const [shortCustom, setShortCustom] = useState(() =>
    customSeed(cred.quota_pause_pct, cred.quota_pause_pct_effective))
  const [longPolicy, setLongPolicy] = useState<QuotaPolicy>(() => policyFromPct(cred.quota_pause_pct_7d))
  const [longCustom, setLongCustom] = useState(() =>
    customSeed(cred.quota_pause_pct_7d, cred.quota_pause_pct_7d_effective))

  // 每次打开都从服务端那份重置：上次改了一半没保存就关掉的残留留到下次，会让人以为它已经生效。
  useEffect(() => {
    if (!open) return
    setShortPolicy(policyFromPct(cred.quota_pause_pct))
    setShortCustom(customSeed(cred.quota_pause_pct, cred.quota_pause_pct_effective))
    setLongPolicy(policyFromPct(cred.quota_pause_pct_7d))
    setLongCustom(customSeed(cred.quota_pause_pct_7d, cred.quota_pause_pct_7d_effective))
  }, [
    open,
    cred.quota_pause_pct,
    cred.quota_pause_pct_effective,
    cred.quota_pause_pct_7d,
    cred.quota_pause_pct_7d_effective,
  ])

  const policyItems = POLICY_ITEMS.map((item) => ({
    value: item.value,
    label: t(item.chinese, item.english),
  }))
  const nextShort = pctFromPolicy(shortPolicy, shortCustom)
  const nextLong = pctFromPolicy(longPolicy, longCustom)
  const dirty = nextShort !== cred.quota_pause_pct || nextLong !== cred.quota_pause_pct_7d
  const describeEffective = (pct: number) =>
    pct > 0 ? `${pct}%` : t('不停', 'off')

  const save = () =>
    quotaPause.mutate({ pct: nextShort, pct7d: nextLong }, { onSuccess: () => onOpenChange(false) })

  const window = (
    key: 'short' | 'long',
    label: string,
    policy: QuotaPolicy,
    setPolicy: (p: QuotaPolicy) => void,
    custom: number,
    setCustom: (n: number) => void,
    effective: number,
  ) => (
    <div className="grid gap-4 sm:grid-cols-2" key={key}>
      <Field>
        <FieldLabel>{label}</FieldLabel>
        <Select
          items={policyItems}
          value={policy}
          onValueChange={(value) => { if (value) setPolicy(value as QuotaPolicy) }}
        >
          <SelectTrigger aria-label={label}>
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
            `当前生效：${describeEffective(effective)}。`,
            `Currently effective: ${describeEffective(effective)}.`,
          )}
        </FieldDescription>
      </Field>
      {policy === 'custom' && (
        <Field>
          <FieldLabel>{t('使用率阈值（%）', 'Utilization threshold (%)')}</FieldLabel>
          <NumberField
            value={custom}
            min={1}
            max={100}
            step={1}
            onValueChange={(value) => setCustom(Math.min(100, Math.max(1, Math.floor(value ?? 1))))}
          >
            <NumberFieldGroup>
              <NumberFieldDecrement />
              <NumberFieldInput aria-label={`${label} ${t('阈值', 'threshold')}`} />
              <NumberFieldIncrement />
            </NumberFieldGroup>
          </NumberField>
          <FieldDescription>
            {t('该设置只影响当前账号。', 'This setting only affects the current account.')}
          </FieldDescription>
        </Field>
      )}
    </div>
  )

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogPopup className="max-w-lg">
        <DialogHeader>
          <DialogTitle>{t('提前停调度阈值', 'Early pause threshold')}</DialogTitle>
          <DialogDescription className="mt-1 truncate" title={credentialLabel}>
            {credentialLabel}
          </DialogDescription>
        </DialogHeader>

        <DialogPanel className="space-y-4">
          {window(
            'short',
            t('5 小时窗口', '5h window'),
            shortPolicy,
            setShortPolicy,
            shortCustom,
            setShortCustom,
            cred.quota_pause_pct_effective,
          )}
          {window(
            'long',
            t('7 天窗口', '7d window'),
            longPolicy,
            setLongPolicy,
            longCustom,
            setLongCustom,
            cred.quota_pause_pct_7d_effective,
          )}

          <Alert>
            <PercentIcon />
            <AlertDescription>
              {t(
                '上游每条响应都报着这个账号的额度使用率，到达阈值就把它挪出调度池，不必等下一条请求去撞 429；按触发的那个窗口的重置时刻自动恢复。两档各自覆盖设置页里的全局值：5 小时窗口停号最多歇几小时，7 天窗口停号是歇到下个周重置，配 7 天那档建议比 5 小时更高。下一条带限流头的响应起生效；已经按旧阈值停下的号不会因为调高而自动回池，可手动启用或用连通性测试放回。',
                'Every upstream response reports this account’s utilization; once it reaches the threshold the account leaves the scheduling pool instead of waiting for the next request to hit a 429, and comes back when the window that triggered it resets. Each window overrides the global value on the settings page: a 5h pause lasts a few hours at most, a 7d pause lasts until the weekly reset, so set the 7d one higher. Takes effect from the next response carrying rate-limit headers; an account already paused under the old threshold does not return by itself when you raise it — re-enable it by hand or with a connectivity test.',
              )}
            </AlertDescription>
          </Alert>
        </DialogPanel>

        <DialogFooter>
          <DialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</DialogClose>
          <Button onClick={save} disabled={!dirty || quotaPause.isPending} loading={quotaPause.isPending}>
            {t('保存', 'Save')}
          </Button>
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  )
}
