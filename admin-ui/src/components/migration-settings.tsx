import { useRef, useState } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  AlertTriangleIcon,
  DownloadIcon,
  FileJsonIcon,
  UploadIcon,
} from 'lucide-react'
import { getAuthState } from '@/api/auth'
import {
  exportAll,
  importAll,
  type ExportFile,
  type ImportMode,
  type ImportResult,
} from '@/api/settings'
import { SettingsGroup, SettingsRow } from '@/components/settings-group'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import { Checkbox } from '@/components/ui/checkbox'
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
  Select,
  SelectItem,
  SelectPopup,
  SelectTrigger,
  SelectValue,
} from '@/components/ui/select'
import { toastManager } from '@/components/ui/toast'
import { useI18n } from '@/lib/i18n'
import { extractError } from '@/lib/utils'

/** 迁移文件的 kind 标记，与后端 `EXPORT_KIND` 同值。 */
const EXPORT_KIND = 'luban-export'

/**
 * 导出：把全部账号与设置存成一个 JSON 文件。
 *
 * 文件里是**明文 token**，所以这一格的说明和徽章都得把这件事说死——它不是「配置备份」，
 * 它就是那些账号本身。未设管理密码时后端直接拒（管理接口那时是敞开的），这里同样先把按钮
 * 关掉并说明原因，免得点下去只拿到一句 403。
 */
function ExportPanel() {
  const { language, t } = useI18n()
  const { data: auth } = useQuery({ queryKey: ['auth-state'], queryFn: getAuthState })
  const locked = auth ? !auth.configured : false

  const run = useMutation({
    mutationFn: exportAll,
    onSuccess: (file: ExportFile) => {
      // 下载走 Blob + 临时 <a>：接口要带管理密码头，直接用 <a href="/api/export"> 会因为
      // 浏览器不带那个头而拿到 401。
      const stamp = new Date(file.exported_at * 1000)
        .toISOString()
        .slice(0, 19)
        .replace(/[:T]/g, '')
      const url = URL.createObjectURL(
        new Blob([JSON.stringify(file, null, 2)], { type: 'application/json' }),
      )
      const a = document.createElement('a')
      a.href = url
      a.download = `luban-export-${stamp}.json`
      a.click()
      URL.revokeObjectURL(url)
      toastManager.add({
        title: t('导出完成', 'Export ready'),
        description: t(
          `${file.credentials.length} 个账号、${Object.keys(file.settings).length} 项设置已存成文件。`,
          `${file.credentials.length} accounts and ${Object.keys(file.settings).length} settings saved to a file.`,
        ),
        type: 'success',
      })
    },
    onError: (error) => {
      toastManager.add({
        title: t('导出失败', 'Export failed'),
        description: extractError(error, language),
        type: 'error',
      })
    },
  })

  return (
    <SettingsRow
      label={t('导出账号与设置', 'Export accounts & settings')}
      description={t(
        '把全部账号（含登录凭据）与系统设置存成一个 JSON 文件，拿到新机器上导入即可。用量历史、设备绑定与管理密码不在其中。',
        'Save every account (including its login credentials) and the system settings to one JSON file, then import it on the new machine. Usage history, device bindings, and the admin password are not included.',
      )}
      note={
        <div className="flex flex-wrap items-center gap-2">
          <Badge variant="warning" size="sm">
            <AlertTriangleIcon />
            {t('文件含明文登录凭据，等同账号本身', 'The file contains plaintext login credentials, equivalent to the accounts themselves')}
          </Badge>
          {locked && (
            <Badge variant="secondary" size="sm">
              {t('需先设置管理密码', 'Set an admin password first')}
            </Badge>
          )}
        </div>
      }
    >
      <Button
        className="w-full sm:w-auto"
        disabled={locked}
        loading={run.isPending}
        title={
          locked
            ? t(
                '控制台未设管理密码时，任何能连到端口的人都能访问管理接口，这种状态下不能导出这个文件。',
                'Without an admin password, anyone who can reach the port can use the management API, so this file cannot be exported in that state.',
              )
            : undefined
        }
        onClick={() => run.mutate()}
      >
        <DownloadIcon />
        {t('导出文件', 'Export file')}
      </Button>
    </SettingsRow>
  )
}

/**
 * 导入：读一个迁移文件，确认后写进本机。
 *
 * 选完文件先在本地认一眼 `kind`——把别的 JSON 误投进来是最容易发生的事，让它在点「确认」
 * 之前就报错，比等服务端回 400 少一次心跳。
 */
function ImportPanel() {
  const qc = useQueryClient()
  const { language, locale, t } = useI18n()
  const fileRef = useRef<HTMLInputElement>(null)
  const [file, setFile] = useState<ExportFile | null>(null)
  const [filename, setFilename] = useState('')
  const [mode, setMode] = useState<ImportMode>('merge')
  const [withSettings, setWithSettings] = useState(false)

  const close = () => {
    setFile(null)
    setFilename('')
    setMode('merge')
    setWithSettings(false)
  }

  const run = useMutation({
    mutationFn: () => importAll(file as ExportFile, mode, withSettings),
    onSuccess: (r: ImportResult) => {
      const parts = [
        t(`新增 ${r.added}`, `${r.added} added`),
        t(`更新 ${r.updated}`, `${r.updated} updated`),
        r.cleared > 0 ? t(`清除 ${r.cleared}`, `${r.cleared} removed`) : null,
        r.failed > 0 ? t(`失败 ${r.failed}`, `${r.failed} failed`) : null,
        r.settings_applied > 0
          ? t(`设置 ${r.settings_applied} 项`, `${r.settings_applied} settings`)
          : null,
      ].filter(Boolean)
      toastManager.add({
        title: t('导入完成', 'Import finished'),
        description: parts.join(' · '),
        // 有失败条目时用 warning：整体是成功的，但确实有东西没进来，不能用一个绿勾盖过去。
        type: r.failed > 0 ? 'warning' : 'success',
      })
      qc.invalidateQueries({ queryKey: ['credentials'] })
      qc.invalidateQueries({ queryKey: ['settings'] })
      qc.invalidateQueries({ queryKey: ['metrics'] })
      close()
    },
    onError: (error) => {
      toastManager.add({
        title: t('导入失败', 'Import failed'),
        description: extractError(error, language),
        type: 'error',
      })
    },
  })

  const pick = async (input: HTMLInputElement) => {
    const picked = input.files?.[0]
    // 选同一个文件两次也要能触发 change，故读完就清空 input 的值。
    input.value = ''
    if (!picked) return
    try {
      const parsed = JSON.parse(await picked.text()) as ExportFile
      if (parsed?.kind !== EXPORT_KIND) {
        throw new Error(
          t('这不是 luban 导出的迁移文件。', 'This is not a luban export file.'),
        )
      }
      setFilename(picked.name)
      setFile(parsed)
    } catch (error) {
      toastManager.add({
        title: t('无法读取该文件', 'Could not read that file'),
        description: error instanceof Error ? error.message : String(error),
        type: 'error',
      })
    }
  }

  // 选项只写模式名，含义交给下面那句说明：原来选项里括着「保留本机已有账号」、下面又说「本机有而
  // 文件中没有的账号保持不变」，清空模式更是选项、说明、按钮三处都在说「先清空再导入」。
  const modeItems = [
    { label: t('合并', 'Merge'), value: 'merge' },
    { label: t('清空后导入', 'Replace'), value: 'replace' },
  ]
  const count = file?.credentials.length ?? 0
  const settingsCount = file ? Object.keys(file.settings).length : 0
  const exportedAt = file ? new Date(file.exported_at * 1000).toLocaleString(locale) : ''

  return (
    <>
      <SettingsRow
        label={t('导入迁移文件', 'Import a migration file')}
        description={t(
          '读入另一台机器导出的文件。本机已有的同一账号（先按账号 UUID 匹配，其次按登录凭据）会被文件中的版本覆盖，其余作为新账号添加。',
          'Read a file exported from another machine. An account already on this machine (matched by account UUID first, then by its login credentials) is overwritten by the version in the file; the rest are added as new accounts.',
        )}
      >
        <input
          ref={fileRef}
          accept="application/json,.json"
          className="hidden"
          type="file"
          onChange={(e) => void pick(e.currentTarget)}
        />
        <Button
          className="w-full sm:w-auto"
          variant="outline"
          onClick={() => fileRef.current?.click()}
        >
          <UploadIcon />
          {t('选择文件', 'Choose file')}
        </Button>
      </SettingsRow>

      <Dialog
        open={file !== null}
        onOpenChange={(next) => {
          if (!next && !run.isPending) close()
        }}
      >
        <DialogPopup>
          <DialogHeader>
            <DialogTitle>{t('确认导入', 'Confirm import')}</DialogTitle>
            {/* 文件名完整显示、放不下就折行：这是「确认导入」，用户正要核对导入的是不是这份文件。
                原来 `truncate` 挂在 flex 容器上，文字节点不吃省略号，窄屏上直接被切掉一截。 */}
            <DialogDescription className="mt-1 flex items-start gap-1.5" title={filename}>
              <FileJsonIcon className="mt-0.5 size-3.5 shrink-0" />
              <span className="min-w-0 [overflow-wrap:anywhere]">{filename}</span>
            </DialogDescription>
          </DialogHeader>

          <DialogPanel className="space-y-4">
            <div className="grid gap-2 rounded-lg border px-3 py-2 text-sm">
              <div className="flex items-baseline justify-between gap-2">
                <span className="text-xs text-muted-foreground">{t('账号', 'Accounts')}</span>
                <span className="font-medium tabular-nums">{count.toLocaleString(locale)}</span>
              </div>
              <div className="flex items-baseline justify-between gap-2">
                <span className="text-xs text-muted-foreground">{t('设置项', 'Settings')}</span>
                <span className="font-medium tabular-nums">
                  {settingsCount.toLocaleString(locale)}
                </span>
              </div>
              <div className="flex items-baseline justify-between gap-2">
                <span className="text-xs text-muted-foreground">{t('导出于', 'Exported')}</span>
                <span className="truncate font-medium">
                  {exportedAt}
                  <span className="text-muted-foreground"> · v{file?.luban_version}</span>
                </span>
              </div>
            </div>

            <Field>
              <FieldLabel>{t('导入方式', 'Import mode')}</FieldLabel>
              <Select
                items={modeItems}
                value={mode}
                onValueChange={(value) => {
                  if (value) setMode(value as ImportMode)
                }}
              >
                <SelectTrigger aria-label={t('导入方式', 'Import mode')}>
                  <SelectValue />
                </SelectTrigger>
                <SelectPopup>
                  {modeItems.map((item) => (
                    <SelectItem key={item.value} value={item.value}>
                      {item.label}
                    </SelectItem>
                  ))}
                </SelectPopup>
              </Select>
              <FieldDescription className="leading-5">
                {mode === 'replace'
                  ? t(
                      '先删除本机现有的全部账号（连同它们的用量历史与设备绑定），再写入文件中的账号。',
                      'Every account on this machine is deleted first (along with its usage history and device bindings), then the accounts in the file are written.',
                    )
                  : t(
                      '本机有而文件中没有的账号保持不变。',
                      'Accounts on this machine that are absent from the file are left untouched.',
                    )}
              </FieldDescription>
            </Field>

            <label className="flex cursor-pointer items-start gap-2.5">
              <Checkbox
                checked={withSettings}
                className="mt-0.5"
                onCheckedChange={(next) => setWithSettings(next === true)}
              />
              <span className="min-w-0 space-y-1">
                <span className="block text-sm font-medium">
                  {t('一并导入系统设置', 'Import the system settings too')}
                </span>
                <span className="block text-xs leading-5 text-muted-foreground">
                  {t(
                    '包括接入 Key、限流与转发开关等；只覆盖文件中包含的设置项，其余保持本机原值。管理密码不在其中。',
                    'Includes the access key, rate limits, forwarding switches, and so on. Only settings present in the file are overwritten; the rest keep their current values. The admin password is not included.',
                  )}
                </span>
              </span>
            </label>
          </DialogPanel>

          <DialogFooter>
            <DialogClose render={<Button disabled={run.isPending} variant="outline" />}>
              {t('取消', 'Cancel')}
            </DialogClose>
            <Button
              loading={run.isPending}
              variant={mode === 'replace' ? 'destructive' : 'default'}
              onClick={() => run.mutate()}
            >
              {mode === 'replace'
                ? t('清空并导入', 'Replace and import')
                : t('导入', 'Import')}
            </Button>
          </DialogFooter>
        </DialogPopup>
      </Dialog>
    </>
  )
}

/** 设置页「迁移」分区：把这台机器的账号搬到另一台。 */
export function MigrationSettingsContent() {
  const { t } = useI18n()

  return (
    <div className="space-y-4">
      <SettingsGroup
        icon={DownloadIcon}
        title={t('导出', 'Export')}
        description={t(
          '把账号与设置打包成一个文件，用于换机、重装或留作备份。',
          'Pack accounts and settings into a single file for moving to a new machine, reinstalling, or keeping a backup.',
        )}
      >
        <ExportPanel />
      </SettingsGroup>

      <SettingsGroup
        icon={UploadIcon}
        title={t('导入', 'Import')}
        description={t(
          '读入迁移文件。导入后账号立即参与调度，登录凭据到期时会自动刷新。',
          'Read a migration file. Imported accounts join scheduling immediately and refresh their login credentials on their own when they expire.',
        )}
      >
        <ImportPanel />
      </SettingsGroup>
    </div>
  )
}
