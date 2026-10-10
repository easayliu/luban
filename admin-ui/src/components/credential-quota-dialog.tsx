import { useEffect, useState } from 'react'
import { PercentIcon } from 'lucide-react'
import { type Credential } from '@/api/credentials'
import { useI18n } from '@/lib/i18n'
import { useMemberCaps, useReadOnly } from '@/lib/role'
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
import { Hint } from '@/components/ui/tooltip'

/** 三态策略；与后端的取值一一对应：跟随全局 = null，不停 = 0，独立阈值 = 1..100。 */
type QuotaPolicy = 'default' | 'off' | 'custom'

const POLICY_ITEMS = [
  { value: 'default', chinese: '跟随全局', english: 'Use global' },
  { value: 'off', chinese: '该窗口停用', english: 'Off for this window' },
  { value: 'custom', chinese: '独立阈值', english: 'Custom threshold' },
] as const

function policyFromPct(pct: number | null): QuotaPolicy {
  if (pct === null) return 'default'
  if (pct <= 0) return 'off'
  return 'custom'
}

/**
 * 「该窗口停用」能不能选。`cap` 是代理和用户能设的最高阈值（见 [MemberCaps]），`0` 为不设边
 * （管理员，或全局这一档本来就不停）。有边时一般不能选——除非这一档现在就是「停用」（管理员
 * 给的）：两档是整份提交的，号主只改另一档时得能把它原样带回去，后端也放行原值。
 */
function allowOff(cap: number, current: number | null): boolean {
  return cap === 0 || current === 0
}

/**
 * 自定义阈值输入框能到的最大值：不设边（`cap` 为 0）是 100；有边是 `cap`，但这一档现在就是
 * 高过 `cap` 的独立阈值（管理员给的）时放到现值——原样保留后端不写也不拦，压到 `cap` 就等于
 * 只改另一档时把这一档的宽限悄悄收掉。介于 `cap` 与现值之间的数后端会拒。
 */
function customMax(cap: number, current: number | null): number {
  return cap ? Math.max(cap, current ?? 0) : 100
}

function pctFromPolicy(policy: QuotaPolicy, custom: number, max: number): number | null {
  if (policy === 'default') return null
  if (policy === 'off') return 0
  return Math.min(max, Math.max(1, Math.floor(custom)))
}

/** 自定义值的初值：本来就是独立阈值就沿用它，否则拿生效值起步（多半就是想在它附近调），再兜底 90；不超过 `max`。 */
function customSeed(pct: number | null, effective: number, max: number): number {
  const seed = pct !== null && pct > 0 ? pct : effective > 0 ? effective : 90
  return Math.min(seed, max)
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
  const readOnly = useReadOnly()
  const caps = useMemberCaps(open)
  const shortCap = caps?.quota_pause_pct ?? 0
  const longCap = caps?.quota_pause_pct_7d ?? 0
  const shortMax = customMax(shortCap, cred.quota_pause_pct)
  const longMax = customMax(longCap, cred.quota_pause_pct_7d)
  const credentialLabel = displayCredentialLabel(cred.label, language)
  const [shortPolicy, setShortPolicy] = useState<QuotaPolicy>(() => policyFromPct(cred.quota_pause_pct))
  const [shortCustom, setShortCustom] = useState(() =>
    customSeed(cred.quota_pause_pct, cred.quota_pause_pct_effective, shortMax))
  const [longPolicy, setLongPolicy] = useState<QuotaPolicy>(() => policyFromPct(cred.quota_pause_pct_7d))
  const [longCustom, setLongCustom] = useState(() =>
    customSeed(cred.quota_pause_pct_7d, cred.quota_pause_pct_7d_effective, longMax))

  // 每次打开都从服务端那份重置：上次改了一半没保存就关掉的残留留到下次，会让人以为它已经生效。
  useEffect(() => {
    if (!open) return
    setShortPolicy(policyFromPct(cred.quota_pause_pct))
    setShortCustom(customSeed(cred.quota_pause_pct, cred.quota_pause_pct_effective, shortMax))
    setLongPolicy(policyFromPct(cred.quota_pause_pct_7d))
    setLongCustom(customSeed(cred.quota_pause_pct_7d, cred.quota_pause_pct_7d_effective, longMax))
  }, [
    open,
    cred.quota_pause_pct,
    cred.quota_pause_pct_effective,
    cred.quota_pause_pct_7d,
    cred.quota_pause_pct_7d_effective,
    shortMax,
    longMax,
  ])

  const policyItemsFor = (cap: number, current: number | null) => POLICY_ITEMS
    .filter((item) => item.value !== 'off' || allowOff(cap, current))
    .map((item) => ({ value: item.value, label: t(item.chinese, item.english) }))
  const nextShort = pctFromPolicy(shortPolicy, shortCustom, shortMax)
  const nextLong = pctFromPolicy(longPolicy, longCustom, longMax)
  const dirty = nextShort !== cred.quota_pause_pct || nextLong !== cred.quota_pause_pct_7d
  const describeEffective = (pct: number) =>
    pct > 0 ? `${pct}%` : t('停用', 'off')

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
    cap: number,
    current: number | null,
  ) => {
    const policyItems = policyItemsFor(cap, current)
    const max = customMax(cap, current)
    return (
    <div className="grid gap-4 sm:grid-cols-2" key={key}>
      <Field>
        <FieldLabel>{label}</FieldLabel>
        <Select
          items={policyItems}
          disabled={readOnly}
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
            disabled={readOnly}
            value={custom}
            min={1}
            max={max}
            step={1}
            onValueChange={(value) => setCustom(Math.min(max, Math.max(1, Math.floor(value ?? 1))))}
          >
            <NumberFieldGroup>
              <NumberFieldDecrement />
              <NumberFieldInput aria-label={`${label} ${t('阈值', 'threshold')}`} />
              <NumberFieldIncrement />
            </NumberFieldGroup>
          </NumberField>
          <FieldDescription>
            {cap
              ? t(`该设置只影响当前账号，最高 ${cap}%（不高于全局）。`, `This setting only affects the current account; at most ${cap}% (no higher than the global setting).`)
              : t('该设置只影响当前账号。', 'This setting only affects the current account.')}
          </FieldDescription>
        </Field>
      )}
    </div>
    )
  }

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogPopup>
        <DialogHeader>
          <DialogTitle>{t('提前暂停调度阈值', 'Early pause threshold')}</DialogTitle>
          <Hint label={credentialLabel}>
            <DialogDescription className="mt-1 truncate">
              {credentialLabel}
            </DialogDescription>
          </Hint>
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
            shortCap,
            cred.quota_pause_pct,
          )}
          {window(
            'long',
            t('7 天窗口', '7d window'),
            longPolicy,
            setLongPolicy,
            longCustom,
            setLongCustom,
            cred.quota_pause_pct_7d_effective,
            longCap,
            cred.quota_pause_pct_7d,
          )}

          <Alert>
            <PercentIcon />
            <AlertDescription>
              {/* 长说明默认收两行、末尾「了解更多」，同设置页的 ClampedDescription：超过 140 字各宽度都收，60–140 字只在手机上收。 */}
              <ClampedDescription text={t(
                '上游每条响应都会报告该账号的额度使用率；达到阈值时即将账号移出调度池，无需等到下一条请求触发 429，并在触发暂停的窗口重置后自动恢复。两个窗口分别覆盖设置页中的全局值：因 5 小时窗口暂停最多持续数小时，因 7 天窗口暂停则需等到下一次周重置，因此建议将 7 天窗口的阈值设得高于 5 小时窗口。设置自下一条携带限流响应头的响应起生效。注意：已按旧阈值暂停的账号不会因调高阈值而自动回到调度池，可手动启用或执行一次连通性测试以恢复。',
                'Every upstream response reports this account’s utilization; once it reaches the threshold the account leaves the scheduling pool instead of waiting for the next request to hit a 429, and comes back when the window that triggered the pause resets. Each window overrides the global value on the settings page: a 5h pause lasts a few hours at most, while a 7d pause lasts until the next weekly reset, so set the 7d threshold higher than the 5h one. Takes effect from the next response carrying rate limit headers. Note: an account already paused under the old threshold does not return to the pool by itself when you raise it; re-enable it manually or run a connectivity test.',
              )} />
            </AlertDescription>
          </Alert>
        </DialogPanel>

        <DialogFooter>
          <DialogClose render={<Button variant="outline" />}>
            {readOnly ? t('关闭', 'Close') : t('取消', 'Cancel')}
          </DialogClose>
          {!readOnly && (
            <Button onClick={save} disabled={!dirty || quotaPause.isPending} loading={quotaPause.isPending}>
              {t('保存', 'Save')}
            </Button>
          )}
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  )
}
