import { useEffect, useState, type ReactNode } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { SaveIcon } from 'lucide-react'
import { getSettings, type Settings } from '@/api/settings'
import { useI18n } from '@/lib/i18n'
import { extractError } from '@/lib/utils'
import { Button } from '@/components/ui/button'
import { Group } from '@/components/ui/group'
import {
  NumberField,
  NumberFieldDecrement,
  NumberFieldGroup,
  NumberFieldIncrement,
  NumberFieldInput,
} from '@/components/ui/number-field'
import {
  Select,
  SelectItem,
  SelectPopup,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { toastManager } from '@/components/ui/toast'
import { SettingsRow } from '@/components/settings-group'

/** 接入设置的查询：各设置项共用同一份 `['settings']` 缓存。 */
export function useSettingsQuery() {
  return useQuery({ queryKey: ['settings'], queryFn: getSettings })
}

/** 保存成功时的提示文案。 */
export interface SaveToast {
  title: string
  description?: string
}

/**
 * 设置项的保存：成功后弹提示、把返回的设置写回 `['settings']` 缓存（需要时顺带刷新账号
 * 列表），失败统一弹「保存失败」。各设置项只需给出保存函数与成功提示。
 */
export function useSettingsSave<T>(
  mutationFn: (value: T) => Promise<Settings>,
  {
    success,
    invalidateCredentials = false,
    onSuccess,
  }: {
    /** 成功提示，拿到的是保存后的设置与这次提交的值。 */
    success: (settings: Settings, value: T) => SaveToast
    /** 改动会影响账号列表的显示（名额、调度状态等）时为 true，保存后顺带刷新。 */
    invalidateCredentials?: boolean
    /** 弹提示之前的界面收尾（关确认框之类）。 */
    onSuccess?: (settings: Settings) => void
  },
) {
  const qc = useQueryClient()
  const { language, t } = useI18n()
  return useMutation({
    mutationFn,
    onSuccess: (settings: Settings, value: T) => {
      onSuccess?.(settings)
      toastManager.add({ ...success(settings, value), type: 'success' })
      qc.setQueryData(['settings'], settings)
      if (invalidateCredentials) qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (error) => {
      toastManager.add({
        title: t('保存失败', 'Save failed'),
        description: extractError(error, language),
        type: 'error',
      })
    },
  })
}

/** `Settings` 里取值为数字的字段。 */
export type NumericSettingKey = {
  [K in keyof Settings]: Settings[K] extends number ? K : never
}[keyof Settings]

/** 英文标签拼进「Decrease …」时首字母小写；中文原样。 */
function lowerFirst(s: string): string {
  return s.charAt(0).toLowerCase() + s.slice(1)
}

/** 数值设置项共有的配置。 */
interface SettingControlProps {
  field: NumericSettingKey
  save: (value: number) => Promise<Settings>
  /** 当前语言下的名称；也是输入框的读屏标签，加减按钮的标签由它拼出。 */
  label: string
  description: ReactNode | ((parsed: number, settings: Settings | undefined) => ReactNode)
  /** 输入框旁的读数，拿到的是输入框里（规整后）的值与当前设置。 */
  note: (parsed: number, settings: Settings | undefined) => ReactNode
  success: (settings: Settings) => SaveToast
  invalidateCredentials?: boolean
}

function renderDescription(
  description: SettingControlProps['description'],
  parsed: number,
  settings: Settings | undefined,
): ReactNode {
  return typeof description === 'function' ? description(parsed, settings) : description
}

function SaveButton({ pending, disabled, onClick }: {
  pending: boolean
  disabled: boolean
  onClick: () => void
}) {
  const { t } = useI18n()
  return (
    <Button loading={pending} disabled={disabled} onClick={onClick}>
      <SaveIcon />
      {t('保存', 'Save')}
    </Button>
  )
}

/** 非负整数设置项：数值框 + 保存。0 一律表示「不限」，由各项自己的读数说明。 */
export function NumericSetting({
  field,
  save,
  label,
  description,
  note,
  success,
  invalidateCredentials,
}: SettingControlProps) {
  const { t } = useI18n()
  const { data } = useSettingsQuery()
  const [draft, setDraft] = useState<number | null>(null)

  useEffect(() => {
    if (data) setDraft(data[field])
  }, [data?.[field]])

  const mutation = useSettingsSave(save, { success, invalidateCredentials })
  const current = data?.[field] ?? 0
  const parsed = Math.max(0, Math.floor(draft ?? 0))

  return (
    <SettingsRow
      label={label}
      description={renderDescription(description, parsed, data)}
      note={note(parsed, data)}
    >
      <NumberField
        className="min-w-0 flex-1 sm:w-40 sm:flex-none"
        min={0}
        value={draft}
        onValueChange={setDraft}
      >
        <NumberFieldGroup>
          <NumberFieldDecrement aria-label={t(`减少${label}`, `Decrease ${lowerFirst(label)}`)} />
          <NumberFieldInput aria-label={label} />
          <NumberFieldIncrement aria-label={t(`增加${label}`, `Increase ${lowerFirst(label)}`)} />
        </NumberFieldGroup>
      </NumberField>
      <SaveButton
        pending={mutation.isPending}
        disabled={draft === null || parsed === current}
        onClick={() => mutation.mutate(parsed)}
      />
    </SettingsRow>
  )
}

const SECS_PER_MINUTE = 60
const SECS_PER_HOUR = 3600
const SECS_PER_DAY = 86400

/**
 * 秒 → 以 `size` 秒为单位的数值，取能**原样还原**的最短小数位。
 *
 * 604800 按天给 7、43200 按天给 0.5；而除不尽的（只可能来自接口直接改的库）任何短写法都
 * 还原不回去，就照实给完整小数——宁可难看，也不能让用户一按保存就把一个自己没动过的值
 * 悄悄改掉。
 */
function secsIn(secs: number, size: number): number {
  const exact = secs / size
  for (const digits of [0, 1, 2, 3]) {
    const rounded = Number(exact.toFixed(digits))
    if (Math.round(rounded * size) === secs) return rounded
  }
  return exact
}

export type DurationUnit = 'minute' | 'hour' | 'day'

const UNIT_SECS: Record<DurationUnit, number> = {
  minute: SECS_PER_MINUTE,
  hour: SECS_PER_HOUR,
  day: SECS_PER_DAY,
}

/**
 * 秒 → 输入框里的「数值 + 单位」：取能整除的最大单位（3600 显示成 1 小时、1800 显示成
 * 30 分钟）；连分钟都除不尽的落到分钟带小数。0 没有「合适的单位」，用这一项惯常的量级。
 */
function splitDuration(secs: number, fallback: DurationUnit): { value: number; unit: DurationUnit } {
  if (secs <= 0) return { value: 0, unit: fallback }
  const unit = (['day', 'hour', 'minute'] as const).find((u) => secs % UNIT_SECS[u] === 0) ?? 'minute'
  return { value: secsIn(secs, UNIT_SECS[unit]), unit }
}

/** 「数值 + 单位」→ 秒；空值与负数按 0。 */
function joinDuration(value: number | null, unit: DurationUnit): number {
  return Math.max(0, Math.round((value ?? 0) * UNIT_SECS[unit]))
}

/**
 * 带单位切换的时长输入：数值框 + 分钟 / 小时 / 天。切换单位只换单位、不换算数值
 * （30 分钟切到小时就是 30 小时），旁边的读数徽章会跟着显示换算后的时长。
 * `label` 传当前语言下的名称，英文用小写开头，会拼进「减少 / 增加」的读屏标签里。
 */
function DurationField({
  label,
  value,
  unit,
  onValueChange,
  onUnitChange,
}: {
  label: string
  value: number | null
  unit: DurationUnit
  onValueChange: (value: number | null) => void
  onUnitChange: (unit: DurationUnit) => void
}) {
  const { t } = useI18n()
  const units: { value: DurationUnit; label: string }[] = [
    { value: 'minute', label: t('分钟', 'Minutes') },
    { value: 'hour', label: t('小时', 'Hours') },
    { value: 'day', label: t('天', 'Days') },
  ]

  // 数值框与单位下拉拼成一个整体（Group 收掉相接处的圆角与重复边框）：两者说的是同一个量，
  // 分开摆读起来像两项设置。Group 只认直接子元素，而数值框的边框画在里层的 NumberFieldGroup
  // 上，所以右侧圆角得在那一层自己收。
  return (
    <Group className="min-w-0 flex-1 sm:flex-none">
      <NumberField
        className="min-w-0 flex-1 sm:w-32 sm:flex-none"
        min={0}
        step={1}
        smallStep={0.5}
        value={value}
        onValueChange={onValueChange}
      >
        <NumberFieldGroup className="rounded-e-none before:rounded-e-none">
          <NumberFieldDecrement aria-label={t(`减少${label}`, `Decrease ${label}`)} />
          <NumberFieldInput aria-label={label} />
          <NumberFieldIncrement className="rounded-e-none" aria-label={t(`增加${label}`, `Increase ${label}`)} />
        </NumberFieldGroup>
      </NumberField>
      <Select
        items={units}
        value={unit}
        onValueChange={(next) => {
          if (next) onUnitChange(next)
        }}
      >
        <SelectTrigger className="w-auto min-w-20 shrink-0" aria-label={t(`${label}单位`, `${label} unit`)}>
          <SelectValue />
        </SelectTrigger>
        {/* 默认的 alignItemWithTrigger 会把选中项叠到触发框上，弹层整块盖住左边的数值框；这里改成在下方展开。 */}
        <SelectPopup alignItemWithTrigger={false}>
          {units.map((u) => (
            <SelectItem key={u.value} value={u.value}>{u.label}</SelectItem>
          ))}
        </SelectPopup>
      </Select>
    </Group>
  )
}

/**
 * 时长设置项（存的是秒）：数值 + 单位 + 保存。`defaultUnit` 是值为 0 时输入框落在哪个
 * 单位上，取这一项惯常的量级（有效期按分钟 / 小时、保留期按天）。
 */
export function DurationSetting({
  field,
  save,
  defaultUnit,
  label,
  description,
  note,
  success,
  invalidateCredentials,
}: SettingControlProps & { defaultUnit: DurationUnit }) {
  const { data } = useSettingsQuery()
  const [draft, setDraft] = useState<number | null>(null)
  const [unit, setUnit] = useState<DurationUnit>(defaultUnit)

  useEffect(() => {
    if (!data) return
    const d = splitDuration(data[field], defaultUnit)
    setDraft(d.value)
    setUnit(d.unit)
  }, [data?.[field]])

  const mutation = useSettingsSave(save, { success, invalidateCredentials })
  const current = data?.[field] ?? 0
  const parsed = joinDuration(draft, unit)

  return (
    <SettingsRow
      label={label}
      description={renderDescription(description, parsed, data)}
      note={note(parsed, data)}
    >
      <DurationField
        label={lowerFirst(label)}
        value={draft}
        unit={unit}
        onValueChange={setDraft}
        onUnitChange={setUnit}
      />
      <SaveButton
        pending={mutation.isPending}
        disabled={draft === null || parsed === current}
        onClick={() => mutation.mutate(parsed)}
      />
    </SettingsRow>
  )
}
