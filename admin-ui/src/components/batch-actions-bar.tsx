import { useEffect, useState, type ReactNode } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import { ChevronDownIcon, GlobeIcon, PauseIcon, PlayIcon, Trash2Icon, XIcon } from 'lucide-react'
import {
  deleteCredentials, setCredentialQuotaPausePcts, setDeviceLimits, setDisabledMany, setPriorities,
  setProxies, setRpmLimits,
  type Credential,
} from '@/api/credentials'
import { listProxies } from '@/api/proxies'
import { useI18n } from '@/lib/i18n'
import { cn, extractError } from '@/lib/utils'
import {
  AlertDialog, AlertDialogClose, AlertDialogDescription, AlertDialogFooter,
  AlertDialogHeader, AlertDialogPopup, AlertDialogTitle,
} from '@/components/ui/alert-dialog'
import { Button } from '@/components/ui/button'
import {
  Card,
} from '@/components/ui/card'
import { Checkbox } from '@/components/ui/checkbox'
import { Input } from '@/components/ui/input'
import {
  NumberField, NumberFieldDecrement, NumberFieldGroup, NumberFieldIncrement,
  NumberFieldInput,
} from '@/components/ui/number-field'
import { Select, SelectItem, SelectPopup, SelectTrigger, SelectValue } from '@/components/ui/select'
import { toastManager } from '@/components/ui/toast'
import { Toolbar } from '@/components/ui/toolbar'

const LIMIT_MODE_ITEMS = [
  { value: 'default', chinese: '跟随默认', english: 'Use default' },
  { value: 'unlimited', chinese: '不限设备', english: 'Unlimited devices' },
  { value: 'custom', chinese: '独立上限', english: 'Custom limit' },
] as const

const RPM_MODE_ITEMS = [
  { value: 'default', chinese: '跟随默认', english: 'Use default' },
  { value: 'unlimited', chinese: '不限速', english: 'Unlimited' },
  { value: 'custom', chinese: '独立上限', english: 'Custom limit' },
] as const

/** 提前停调度阈值的三态：与后端取值一一对应（null / 0 / 1..100），5h 与 7d 两档各用一份。 */
const QUOTA_MODE_ITEMS = [
  { value: 'default', chinese: '跟随全局', english: 'Use global' },
  { value: 'off', chinese: '不停', english: 'Off' },
  { value: 'custom', chinese: '独立阈值', english: 'Custom' },
] as const
type QuotaMode = (typeof QUOTA_MODE_ITEMS)[number]['value']

function quotaPctOf(mode: QuotaMode, custom: number): number | null {
  if (mode === 'default') return null
  if (mode === 'off') return 0
  return Math.min(100, Math.max(1, Math.floor(custom)))
}

function describeQuotaPct(pct: number | null, t: (zh: string, en: string) => string): string {
  if (pct === null) return t('跟随全局', 'global')
  if (pct <= 0) return t('不停', 'off')
  return `${pct}%`
}

/** 策略下拉的统一宽度：基础组件默认 `w-full`，放进行内会把整行撑开，这里钉成一个固定宽度。 */
const MODE_SELECT_CLASS = 'w-40'

/**
 * 「更多设置」里的一行：左栏标题 + 一句说明，中栏控件，右栏那一项自己的「应用」。
 *
 * 每项各自应用、互不牵连，所以按钮跟在各自那行里而不是面板底部一个总的。窄屏退成上下
 * 三段，宽屏三栏对齐，标题栏定宽让各行的控件竖向对齐。`stacked` 给多档控件（如 5h / 7d
 * 两行）用，把中栏改成纵向排列。
 */
function SettingRow({
  title, hint, action, stacked = false, children,
}: {
  title: ReactNode
  hint: string
  action: ReactNode
  stacked?: boolean
  children: ReactNode
}) {
  return (
    <div className="grid gap-x-6 gap-y-2 px-4 py-3 sm:grid-cols-[minmax(10rem,14rem)_minmax(0,1fr)_auto] sm:items-center">
      <div className="min-w-0">
        <div className="text-xs font-medium">{title}</div>
        <div className="text-xs text-muted-foreground">{hint}</div>
      </div>
      <div className={cn('flex min-w-0 gap-2', stacked ? 'flex-col items-start' : 'flex-wrap items-center')}>
        {children}
      </div>
      <div className="flex sm:justify-end">{action}</div>
    </div>
  )
}

/**
 * 批量操作条：全选/清空 + 优先级 / 设备上限 / RPM 上限 / 启停 / 删除。
 *
 * 所有操作都作用于**当前筛选结果里被勾选的账号**（跨页保留勾选）。写操作走各自的批量
 * 接口，后端在单事务内完成，不会出现「改了一半」的中间态。
 */
export function BatchActionsBar({
  all, selected, onSelectedChange, onClear,
}: {
  all: Credential[]
  selected: Set<number>
  onSelectedChange: (next: Set<number>) => void
  onClear: () => void
}) {
  const { t, language, locale } = useI18n()
  const qc = useQueryClient()
  const [priority, setPriority] = useState(0)
  const [limitMode, setLimitMode] = useState<'default' | 'unlimited' | 'custom'>('default')
  const [customLimit, setCustomLimit] = useState(1)
  const [rpmMode, setRpmMode] = useState<'default' | 'unlimited' | 'custom'>('default')
  const [customRpm, setCustomRpm] = useState(60)
  const [quotaShortMode, setQuotaShortMode] = useState<QuotaMode>('default')
  const [quotaShortCustom, setQuotaShortCustom] = useState(90)
  const [quotaLongMode, setQuotaLongMode] = useState<QuotaMode>('default')
  const [quotaLongCustom, setQuotaLongCustom] = useState(95)
  const [proxyMode, setProxyMode] = useState<'direct' | 'pool' | 'custom'>('direct')
  const [selectedProxyUrl, setSelectedProxyUrl] = useState('')
  const [customProxyUrl, setCustomProxyUrl] = useState('')
  const [confirmDelete, setConfirmDelete] = useState(false)
  const [advancedOpen, setAdvancedOpen] = useState(false)

  const proxiesQuery = useQuery({
    queryKey: ['proxies'],
    queryFn: listProxies,
    enabled: advancedOpen,
  })
  const savedProxies = proxiesQuery.data ?? []

  useEffect(() => {
    if (savedProxies.length > 0 && !selectedProxyUrl) {
      setSelectedProxyUrl(savedProxies[0].url)
    }
  }, [savedProxies, selectedProxyUrl])

  const ids = [...selected]
  const n = selected.size
  const formattedCount = n.toLocaleString(locale)
  const formattedTotal = all.length.toLocaleString(locale)
  const englishAccountCount = `${formattedCount} ${n === 1 ? 'account' : 'accounts'}`
  const limitModeItems = LIMIT_MODE_ITEMS.map((item) => ({
    value: item.value,
    label: t(item.chinese, item.english),
  }))
  const rpmModeItems = RPM_MODE_ITEMS.map((item) => ({
    value: item.value,
    label: t(item.chinese, item.english),
  }))
  const quotaModeItems = QUOTA_MODE_ITEMS.map((item) => ({
    value: item.value,
    label: t(item.chinese, item.english),
  }))
  const notify = (msg: string, clearSelection = false) => {
    toastManager.add({ title: msg, type: 'success' })
    qc.invalidateQueries({ queryKey: ['credentials'] })
    if (clearSelection) onSelectedChange(new Set())
  }
  const onError = (error: unknown) => toastManager.add({
    title: t('批量操作失败', 'Batch operation failed'),
    description: extractError(error, language),
    type: 'error',
  })

  const applyPriority = useMutation({
    mutationFn: (p: number) => setPriorities(ids, p),
    onSuccess: (_r, p) => notify(t(
      `已把 ${formattedCount} 个账号设为 P${p}`,
      `Set ${englishAccountCount} to P${p}`,
    )),
    onError,
  })
  const applyLimit = useMutation({
    mutationFn: (v: number) => setDeviceLimits(ids, v),
    onSuccess: (_r, v) =>
      notify(
        v > 0 ? t(
          `已把 ${formattedCount} 个账号的设备上限设为 ${v.toLocaleString(locale)}`,
          `Set the device limit for ${englishAccountCount} to ${v.toLocaleString(locale)}`,
        )
          : v === 0 ? t(
            `已把 ${formattedCount} 个账号改为跟随全局默认上限`,
            `Set ${englishAccountCount} to use the global default limit`,
          )
            : t(
              `已把 ${formattedCount} 个账号设为不限设备数`,
              `Set ${englishAccountCount} to unlimited devices`,
            ),
      ),
    onError,
  })
  const applyRpmLimit = useMutation({
    mutationFn: (v: number) => setRpmLimits(ids, v),
    onSuccess: (_r, v) =>
      notify(
        v > 0 ? t(
          `已把 ${formattedCount} 个账号的 RPM 上限设为 ${v.toLocaleString(locale)}`,
          `Set the RPM limit for ${englishAccountCount} to ${v.toLocaleString(locale)}`,
        )
          : v === 0 ? t(
            `已把 ${formattedCount} 个账号改为跟随全局默认 RPM 上限`,
            `Set ${englishAccountCount} to use the global default RPM limit`,
          )
            : t(
              `已把 ${formattedCount} 个账号设为不限 RPM`,
              `Set ${englishAccountCount} to unlimited RPM`,
            ),
      ),
    onError,
  })
  const applyQuotaPause = useMutation({
    mutationFn: ({ pct, pct7d }: { pct: number | null; pct7d: number | null }) =>
      setCredentialQuotaPausePcts(ids, pct, pct7d),
    onSuccess: (_r, { pct, pct7d }) =>
      notify(t(
        `已把 ${formattedCount} 个账号的提前停调度阈值设为：5 小时 ${describeQuotaPct(pct, t)} · 7 天 ${describeQuotaPct(pct7d, t)}`,
        `Set the early pause threshold for ${englishAccountCount}: 5h ${describeQuotaPct(pct, t)} · 7d ${describeQuotaPct(pct7d, t)}`,
      )),
    onError,
  })
  const applyProxy = useMutation({
    mutationFn: (url: string | null) => setProxies(ids, url),
    onSuccess: (_r, url) =>
      notify(
        url
          ? t(
            `已把 ${formattedCount} 个账号的出站代理设为所选地址`,
            `Set the outbound proxy for ${englishAccountCount}`,
          )
          : t(
            `已把 ${formattedCount} 个账号改回直连`,
            `Set ${englishAccountCount} to direct connection`,
          ),
      ),
    onError,
  })
  const applyDisabled = useMutation({
    mutationFn: (d: boolean) => setDisabledMany(ids, d),
    onSuccess: (_r, d) => notify(t(
      `已${d ? '停用' : '启用'} ${formattedCount} 个账号`,
      `${d ? 'Disabled' : 'Enabled'} ${englishAccountCount}`,
    )),
    onError,
  })
  const applyDelete = useMutation({
    mutationFn: () => deleteCredentials(ids),
    // 账号已不存在，留着勾选没有意义，顺手清空。批量条不会随之卸载，确认框得自己关。
    onSuccess: () => {
      setConfirmDelete(false)
      notify(t(`已删除 ${formattedCount} 个账号`, `Deleted ${englishAccountCount}`), true)
    },
    onError: (e) => { setConfirmDelete(false); onError(e) },
  })

  const busy =
    applyPriority.isPending || applyLimit.isPending || applyRpmLimit.isPending ||
    applyQuotaPause.isPending || applyProxy.isPending || applyDisabled.isPending ||
    applyDelete.isPending
  const allSelected = all.length > 0 && all.every((item) => selected.has(item.id))
  const deviceLimit = limitMode === 'default' ? 0 : limitMode === 'unlimited' ? -1 : Math.max(1, Math.floor(customLimit))
  const rpmLimit = rpmMode === 'default' ? 0 : rpmMode === 'unlimited' ? -1 : Math.max(1, Math.floor(customRpm))
  const quotaPct = quotaPctOf(quotaShortMode, quotaShortCustom)
  const quotaPct7d = quotaPctOf(quotaLongMode, quotaLongCustom)
  const proxyUrl = proxyMode === 'direct' ? null : proxyMode === 'pool' ? selectedProxyUrl : customProxyUrl.trim()
  const proxyModeItems = [
    { value: 'direct', label: t('直连', 'Direct') },
    ...(savedProxies.length > 0 ? [{ value: 'pool', label: t('从代理池选', 'From pool') }] : []),
    { value: 'custom', label: t('自定义地址', 'Custom URL') },
  ]

  return (
    <Card render={<section aria-label={t('批量操作', 'Batch actions')} />} className="rounded-xl">
        <div className="flex min-h-14 flex-wrap items-center gap-3 p-3">
          <label className="mr-auto flex cursor-pointer items-center gap-2 text-xs">
            <Checkbox
              checked={allSelected}
              indeterminate={n > 0 && !allSelected}
              onCheckedChange={(checked) => onSelectedChange(checked ? new Set(all.map((item) => item.id)) : new Set())}
            />
            <span aria-live="polite">
              {t('已选', 'Selected')}{' '}
              <span className="tnum font-semibold text-foreground">{formattedCount}</span>
              <span className="text-muted-foreground"> / {formattedTotal}</span>
            </span>
          </label>

          <Toolbar className="gap-3 border-0 bg-transparent p-0 shadow-none">
            <Button size="sm" variant="outline" aria-label={t('启用所选账号', 'Enable selected accounts')} disabled={busy} loading={applyDisabled.isPending && applyDisabled.variables === false} onClick={() => applyDisabled.mutate(false)}>
              <PlayIcon /><span className="max-sm:sr-only">{t('启用', 'Enable')}</span>
            </Button>
            <Button size="sm" variant="outline" aria-label={t('停用所选账号', 'Disable selected accounts')} disabled={busy} loading={applyDisabled.isPending && applyDisabled.variables === true} onClick={() => applyDisabled.mutate(true)}>
              <PauseIcon /><span className="max-sm:sr-only">{t('停用', 'Disable')}</span>
            </Button>
            <Button size="sm" variant="destructive-outline" aria-label={t('删除所选账号', 'Delete selected accounts')} disabled={busy} onClick={() => setConfirmDelete(true)}>
              <Trash2Icon /><span className="max-sm:sr-only">{t('删除', 'Delete')}</span>
            </Button>
          </Toolbar>

          <Button
            size="sm"
            variant="outline"
            aria-expanded={advancedOpen}
            aria-controls="batch-advanced-settings"
            onClick={() => setAdvancedOpen((open) => !open)}
          >
            {t('更多设置', 'More settings')}
            <ChevronDownIcon className={cn('size-4 transition-transform', advancedOpen && 'rotate-180')} />
          </Button>

          <Button size="icon-sm" variant="ghost" onClick={onClear} title={t('清空选择', 'Clear selection')} aria-label={t('清空选择', 'Clear selection')}>
            <XIcon />
          </Button>
        </div>

        {advancedOpen && (
          <div id="batch-advanced-settings" className="divide-y border-t">
            <SettingRow
              title={t('调度优先级', 'Scheduling priority')}
              hint={t('数值越小越优先', 'Lower values have higher priority')}
              action={
                <Button size="sm" loading={applyPriority.isPending} disabled={busy} onClick={() => applyPriority.mutate(priority)}>
                  {t('应用', 'Apply')}
                </Button>
              }
            >
              <NumberField
                id="batch-priority"
                value={priority}
                min={0}
                step={1}
                size="sm"
                className="w-32"
                onValueChange={(value) => setPriority(Math.max(0, Math.floor(value ?? 0)))}
              >
                <NumberFieldGroup>
                  <NumberFieldDecrement />
                  <NumberFieldInput aria-label={t('批量设置优先级', 'Set priority for selected accounts')} />
                  <NumberFieldIncrement />
                </NumberFieldGroup>
              </NumberField>
            </SettingRow>

            <SettingRow
              title={t('设备上限', 'Device limit')}
              hint={t('默认、不限或独立上限', 'Default, unlimited, or custom')}
              action={
                <Button size="sm" loading={applyLimit.isPending} disabled={busy} onClick={() => applyLimit.mutate(deviceLimit)}>
                  {t('应用', 'Apply')}
                </Button>
              }
            >
              <Select items={limitModeItems} value={limitMode} onValueChange={(value) => value && setLimitMode(value as typeof limitMode)}>
                <SelectTrigger aria-label={t('批量设置设备上限策略', 'Set device limit policy for selected accounts')} size="sm" className={MODE_SELECT_CLASS}><SelectValue /></SelectTrigger>
                <SelectPopup>
                  {limitModeItems.map((item) => (
                    <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>
                  ))}
                </SelectPopup>
              </Select>
              {limitMode === 'custom' && (
                <NumberField value={customLimit} min={1} step={1} size="sm" className="w-32" onValueChange={(value) => setCustomLimit(Math.max(1, Math.floor(value ?? 1)))}>
                  <NumberFieldGroup>
                    <NumberFieldDecrement />
                    <NumberFieldInput aria-label={t('批量设置独立设备上限', 'Set a custom device limit for selected accounts')} />
                    <NumberFieldIncrement />
                  </NumberFieldGroup>
                </NumberField>
              )}
            </SettingRow>

            <SettingRow
              title={t('RPM 上限', 'RPM limit')}
              hint={t('每分钟最多转发多少条', 'Max requests forwarded per minute')}
              action={
                <Button size="sm" loading={applyRpmLimit.isPending} disabled={busy} onClick={() => applyRpmLimit.mutate(rpmLimit)}>
                  {t('应用', 'Apply')}
                </Button>
              }
            >
              <Select items={rpmModeItems} value={rpmMode} onValueChange={(value) => value && setRpmMode(value as typeof rpmMode)}>
                <SelectTrigger aria-label={t('批量设置 RPM 上限策略', 'Set RPM limit policy for selected accounts')} size="sm" className={MODE_SELECT_CLASS}><SelectValue /></SelectTrigger>
                <SelectPopup>
                  {rpmModeItems.map((item) => (
                    <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>
                  ))}
                </SelectPopup>
              </Select>
              {rpmMode === 'custom' && (
                <NumberField value={customRpm} min={1} step={1} size="sm" className="w-32" onValueChange={(value) => setCustomRpm(Math.max(1, Math.floor(value ?? 1)))}>
                  <NumberFieldGroup>
                    <NumberFieldDecrement />
                    <NumberFieldInput aria-label={t('批量设置独立 RPM 上限', 'Set a custom RPM limit for selected accounts')} />
                    <NumberFieldIncrement />
                  </NumberFieldGroup>
                </NumberField>
              )}
            </SettingRow>

            {/* 提前停调度阈值：5h / 7d 两档各自三态，一次整份覆盖所选账号（覆盖设置页的全局值）。 */}
            <SettingRow
              title={t('提前停调度阈值', 'Early pause threshold')}
              hint={t('额度用到多少就挪出调度池，两档一起覆盖全局', 'Leave the pool at this utilization; both windows override the global value')}
              action={
                <Button size="sm" loading={applyQuotaPause.isPending} disabled={busy} onClick={() => applyQuotaPause.mutate({ pct: quotaPct, pct7d: quotaPct7d })}>
                  {t('应用', 'Apply')}
                </Button>
              }
              stacked
            >
              {([
                ['short', t('5 小时', '5h'), quotaShortMode, setQuotaShortMode, quotaShortCustom, setQuotaShortCustom,
                  t('批量设置 5 小时窗口阈值策略', 'Set the 5h window threshold policy for selected accounts'),
                  t('批量设置 5 小时窗口阈值（%）', 'Set a custom 5h window threshold (%) for selected accounts')],
                ['long', t('7 天', '7d'), quotaLongMode, setQuotaLongMode, quotaLongCustom, setQuotaLongCustom,
                  t('批量设置 7 天窗口阈值策略', 'Set the 7d window threshold policy for selected accounts'),
                  t('批量设置 7 天窗口阈值（%）', 'Set a custom 7d window threshold (%) for selected accounts')],
              ] as const).map(([key, label, mode, setMode, custom, setCustom, modeAria, customAria]) => (
                <div key={key} className="flex flex-wrap items-center gap-2">
                  <span className="w-12 shrink-0 text-xs text-muted-foreground">{label}</span>
                  <Select items={quotaModeItems} value={mode} onValueChange={(value) => value && setMode(value as QuotaMode)}>
                    <SelectTrigger aria-label={modeAria} size="sm" className={MODE_SELECT_CLASS}><SelectValue /></SelectTrigger>
                    <SelectPopup>
                      {quotaModeItems.map((item) => (
                        <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>
                      ))}
                    </SelectPopup>
                  </Select>
                  {mode === 'custom' && (
                    <>
                      <NumberField value={custom} min={1} max={100} step={1} size="sm" className="w-32" onValueChange={(value) => setCustom(Math.min(100, Math.max(1, Math.floor(value ?? 1))))}>
                        <NumberFieldGroup>
                          <NumberFieldDecrement />
                          <NumberFieldInput aria-label={customAria} />
                          <NumberFieldIncrement />
                        </NumberFieldGroup>
                      </NumberField>
                      <span className="text-xs text-muted-foreground">%</span>
                    </>
                  )}
                </div>
              ))}
            </SettingRow>

            <SettingRow
              title={(
                <span className="inline-flex items-center gap-1.5">
                  <GlobeIcon className="size-3.5" />
                  {t('出站代理', 'Outbound proxy')}
                </span>
              )}
              hint={t('统一设置出站代理或改回直连', 'Set outbound proxy or switch to direct')}
              action={
                <Button
                  size="sm"
                  loading={applyProxy.isPending}
                  disabled={busy || (proxyMode === 'custom' && !customProxyUrl.trim()) || (proxyMode === 'pool' && !selectedProxyUrl)}
                  onClick={() => applyProxy.mutate(proxyUrl || null)}
                >
                  {t('应用', 'Apply')}
                </Button>
              }
            >
              <Select items={proxyModeItems} value={proxyMode} onValueChange={(value) => value && setProxyMode(value as typeof proxyMode)}>
                <SelectTrigger aria-label={t('批量设置出站代理策略', 'Set outbound proxy policy for selected accounts')} size="sm" className={MODE_SELECT_CLASS}><SelectValue /></SelectTrigger>
                <SelectPopup>
                  {proxyModeItems.map((item) => (
                    <SelectItem key={item.value} value={item.value}>{item.label}</SelectItem>
                  ))}
                </SelectPopup>
              </Select>
              {proxyMode === 'pool' && savedProxies.length > 0 && (
                <Select
                  items={savedProxies.map((p) => ({ value: p.url, label: p.label }))}
                  value={selectedProxyUrl}
                  onValueChange={(value) => value && setSelectedProxyUrl(value)}
                >
                  <SelectTrigger aria-label={t('选择代理', 'Select proxy')} size="sm" className="w-56"><SelectValue /></SelectTrigger>
                  <SelectPopup>
                    {savedProxies.map((p) => (
                      <SelectItem key={p.id} value={p.url}>{p.label}</SelectItem>
                    ))}
                  </SelectPopup>
                </Select>
              )}
              {proxyMode === 'custom' && (
                <Input
                  value={customProxyUrl}
                  onChange={(event) => setCustomProxyUrl(event.target.value)}
                  placeholder="socks5://127.0.0.1:1080"
                  spellCheck={false}
                  autoComplete="off"
                  size="sm"
                  className="w-72 max-w-full"
                  aria-label={t('自定义代理地址', 'Custom proxy URL')}
                />
              )}
            </SettingRow>
          </div>
        )}

        <AlertDialog open={confirmDelete} onOpenChange={setConfirmDelete}>
          <AlertDialogPopup>
            <AlertDialogHeader>
              <AlertDialogTitle>
                {t(`删除 ${formattedCount} 个账号`, `Delete ${englishAccountCount}`)}
              </AlertDialogTitle>
              <AlertDialogDescription>
                {t(
                  `确定删除选中的 ${formattedCount} 个账号？历史用量记录与设备绑定将一并清除，且无法恢复。`,
                  `Delete the selected ${englishAccountCount}? Usage history and device bindings will also be removed and cannot be recovered.`,
                )}
              </AlertDialogDescription>
            </AlertDialogHeader>
            <AlertDialogFooter>
              <AlertDialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</AlertDialogClose>
              <Button variant="destructive" loading={applyDelete.isPending} onClick={() => applyDelete.mutate()}>
                {t(`删除 ${formattedCount} 个`, `Delete ${formattedCount}`)}
              </Button>
            </AlertDialogFooter>
          </AlertDialogPopup>
        </AlertDialog>
    </Card>
  )
}
