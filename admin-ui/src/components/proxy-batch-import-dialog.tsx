import { useState } from 'react'
import { useMutation, useQueryClient } from '@tanstack/react-query'
import { addProxies, type BatchProxyItem, type ProxyTestResult } from '@/api/proxies'
import { useI18n } from '@/lib/i18n'
import { extractError } from '@/lib/utils'
import { proxyMaskedUrl } from '@/components/credential-shared'
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
import { Spinner } from '@/components/ui/spinner'
import { Textarea } from '@/components/ui/textarea'
import { toastManager } from '@/components/ui/toast'
import { Hint } from '@/components/ui/tooltip'

/** 批量测试的并发数：每条测试最长 15s，全串行几十条要等好几分钟；并发太高又会同时打满 ip-api 的限流。 */
export const BATCH_TEST_CONCURRENCY = 4

/**
 * 测过且通的地址，名称留空时按出口地区起名（「Japan Tokyo」），比后端兜底的 host:port 好认；
 * 与已有名称撞了就加序号。没测过或不通时返回空串，交给后端按 host:port 起名。
 */
export function locationLabel(result: ProxyTestResult | undefined, existing: string[]): string {
  if (!result?.ok) return ''
  const parts = [result.country, result.city].filter((v): v is string => !!v)
  const base = [...new Set(parts)].join(' ')
  if (!base) return ''
  if (!existing.includes(base)) return base
  for (let n = 2; ; n++) {
    const candidate = `${base} #${n}`
    if (!existing.includes(candidate)) return candidate
  }
}

/** 并发跑一批测试，同一时刻最多 [BATCH_TEST_CONCURRENCY] 条。 */
export async function runConcurrently(
  urls: string[],
  run: (url: string) => Promise<ProxyTestResult>,
): Promise<{ ok: number; failed: number }> {
  const queue = [...urls]
  let ok = 0
  let failed = 0
  await Promise.all(
    Array.from({ length: Math.min(BATCH_TEST_CONCURRENCY, queue.length) }, async () => {
      for (let url = queue.shift(); url !== undefined; url = queue.shift()) {
        if ((await run(url)).ok) ok++
        else failed++
      }
    }),
  )
  return { ok, failed }
}

interface ParsedLine {
  /** 在输入框里的行号（从 1 起），预览里拿它指回原文。 */
  line: number
  label: string
  url: string
}

/**
 * 一行一条：只写地址，或「名称 地址」（名称与地址之间用空格、Tab 或逗号隔开，名称里可以带空格，
 * 最后一段才是地址）。空行和 `#` 开头的注释行跳过。
 */
function parseLines(text: string): ParsedLine[] {
  const out: ParsedLine[] = []
  text.split(/\r?\n/).forEach((raw, i) => {
    const line = raw.trim()
    if (!line || line.startsWith('#')) return
    const m = /^(.*?\S)[\s,，]+(\S+)$/.exec(line)
    out.push(m ? { line: i + 1, label: m[1].trim(), url: m[2] } : { line: i + 1, label: '', url: line })
  })
  return out
}

interface Preview {
  lines: ParsedLine[]
  items: BatchProxyItem[]
  testFirst: boolean
}

export function ProxyBatchImportDialog({
  open,
  onOpenChange,
  poolLabels,
  results,
  testing,
  runTest,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
  /** 池里已有的名称：按出口地区起名时要避开。 */
  poolLabels: string[]
  /** 与代理池页共用的测试结果（按归一化后的地址记），导入后新行直接带着结果。 */
  results: Record<string, ProxyTestResult>
  testing: Set<string>
  runTest: (url: string) => Promise<ProxyTestResult>
}) {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  const [text, setText] = useState('')
  const [testFirst, setTestFirst] = useState(false)
  const [preview, setPreview] = useState<Preview | null>(null)
  const [testRunning, setTestRunning] = useState(false)

  const lines = parseLines(text)

  const reset = () => {
    setText('')
    setPreview(null)
    setTestRunning(false)
  }

  const check = useMutation({
    mutationFn: () => addProxies(lines.map(({ label, url }) => ({ label, url })), true),
    onSuccess: async (items) => {
      setPreview({ lines, items, testFirst })
      if (!testFirst) return
      const urls = items.filter((it) => it.status === 'added' && it.url).map((it) => it.url!)
      if (urls.length === 0) return
      setTestRunning(true)
      await runConcurrently(urls, runTest)
      setTestRunning(false)
    },
    onError: (e) =>
      toastManager.add({
        title: t('检查失败', 'Check failed'),
        description: extractError(e, language),
        type: 'error',
      }),
  })

  // 预览里「会被导入」的条目：地址可导入；勾了导入前测试的，还得测通。
  const importable = preview
    ? preview.items
        .map((item, i) => ({ item, parsed: preview.lines[i] }))
        .filter(({ item }) => item.status === 'added' && item.url && (!preview.testFirst || results[item.url]?.ok))
    : []
  const skipped = preview ? preview.items.length - importable.length : 0

  const submit = useMutation({
    mutationFn: () => {
      // 名称留空、测通了的按出口地区起名；本批里起过的名字也要避开。其余留空交给后端按 host:port 起。
      const taken = [...poolLabels]
      const payload = importable.map(({ item, parsed }) => {
        const label = parsed.label || locationLabel(results[item.url!], taken)
        if (label) taken.push(label)
        return { label, url: item.url! }
      })
      return addProxies(payload, false)
    },
    onSuccess: (items) => {
      const added = items.filter((it) => it.id != null).length
      const skippedTotal = (preview?.items.length ?? 0) - added
      toastManager.add({
        title: t(`已导入 ${added} 条代理`, `Imported ${added} prox${added === 1 ? 'y' : 'ies'}`),
        description: skippedTotal > 0 ? t(`跳过 ${skippedTotal} 条`, `${skippedTotal} skipped`) : undefined,
        type: 'success',
      })
      qc.invalidateQueries({ queryKey: ['proxies'] })
      reset()
      onOpenChange(false)
    },
    onError: (e) =>
      toastManager.add({
        title: t('导入失败', 'Import failed'),
        description: extractError(e, language),
        type: 'error',
      }),
  })

  const statusBadge = (item: BatchProxyItem) => {
    if (item.status === 'invalid') return <Badge size="sm" variant="error">{t('格式错误', 'Invalid')}</Badge>
    if (item.status === 'exists') return <Badge size="sm" variant="outline">{t('池中已有', 'Already in pool')}</Badge>
    if (item.status === 'duplicate') {
      const first = preview?.lines[item.duplicate_of ?? -1]?.line
      return (
        <Badge size="sm" variant="outline">
          {first ? t(`与第 ${first} 行重复`, `Same as line ${first}`) : t('重复', 'Duplicate')}
        </Badge>
      )
    }
    if (preview?.testFirst && item.url) {
      if (testing.has(item.url)) {
        return (
          <Badge size="sm" variant="outline">
            <Spinner className="size-2.5" />
            {t('测试中', 'Testing')}
          </Badge>
        )
      }
      const r = results[item.url]
      if (r && !r.ok) return <Badge size="sm" variant="error">{t('测试未通过', 'Test failed')}</Badge>
      if (r?.ok) return <Badge size="sm" variant="success">{t('可用', 'Working')}</Badge>
    }
    return <Badge size="sm" variant="success">{t('可导入', 'Ready')}</Badge>
  }

  const detail = (item: BatchProxyItem) => {
    if (item.status === 'invalid') return item.error
    const r = item.url ? results[item.url] : undefined
    if (!preview?.testFirst || !r) return null
    return r.ok
      ? [[r.city, r.region, r.country].filter(Boolean).join(', '), `${r.latency_ms}ms`].filter(Boolean).join(' · ')
      : r.error
  }

  return (
    <Dialog
      open={open}
      onOpenChange={(next) => {
        onOpenChange(next)
        if (!next) reset()
      }}
    >
      <DialogPopup size="md">
        <DialogHeader>
          <DialogTitle>{t('批量导入代理', 'Import proxies')}</DialogTitle>
          <DialogDescription>
            {preview
              ? t(
                  `可导入 ${importable.length} 条，跳过 ${skipped} 条`,
                  `${importable.length} to import, ${skipped} skipped`,
                )
              : t(
                  '每行一条，格式为「地址」或「名称 地址」（以空格、Tab 或逗号分隔）。未填写名称时自动命名；重复的地址与代理池中已有的地址将被跳过。',
                  'One per line: a URL, or "name URL" (separated by a space, tab or comma). Empty names are auto-generated; duplicates and addresses already in the pool are skipped.',
                )}
          </DialogDescription>
        </DialogHeader>

        <DialogPanel className="space-y-3">
          {preview ? (
            <ul className="divide-y rounded-md border" role="list">
              {preview.items.map((item, i) => {
                const parsed = preview.lines[i]
                const info = detail(item)
                const name = item.label ?? parsed.label
                return (
                  <li key={i} className="flex items-start gap-3 px-3 py-2">
                    <span className="w-6 shrink-0 pt-0.5 text-right text-xs text-muted-foreground tabular-nums">
                      {parsed.line}
                    </span>
                    <div className="min-w-0 flex-1">
                      {name && <p className="truncate text-sm">{name}</p>}
                      <Hint label={item.url ? proxyMaskedUrl(item.url) : parsed.url}>
                        <p className="truncate text-xs text-muted-foreground">
                          {item.url ? proxyMaskedUrl(item.url) : parsed.url}
                        </p>
                      </Hint>
                      {info && (
                        <p
                          className={`break-all text-xs ${item.status === 'invalid' || (item.url && results[item.url]?.ok === false) ? 'text-destructive-foreground' : 'text-muted-foreground'}`}
                        >
                          {info}
                        </p>
                      )}
                    </div>
                    <div className="shrink-0">{statusBadge(item)}</div>
                  </li>
                )
              })}
            </ul>
          ) : (
            <>
              <Textarea
                value={text}
                onChange={(event) => setText(event.target.value)}
                placeholder={'socks5://user:pass@1.2.3.4:1080\n日本 1 http://5.6.7.8:3128'}
                spellCheck={false}
                autoComplete="off"
                rows={10}
                className="font-mono text-xs"
                aria-label={t('代理地址，每行一条', 'Proxy URLs, one per line')}
              />
              <label className="flex cursor-pointer items-center gap-2 text-sm">
                <Checkbox checked={testFirst} onCheckedChange={(next) => setTestFirst(next === true)} />
                {t('导入前先测试，仅导入可用代理', 'Test before importing and only import working proxies')}
              </label>
            </>
          )}
        </DialogPanel>

        <DialogFooter>
          {preview ? (
            <>
              <Button variant="outline" disabled={submit.isPending} onClick={() => setPreview(null)}>
                {t('返回修改', 'Back')}
              </Button>
              <Button
                disabled={importable.length === 0 || testRunning}
                loading={submit.isPending}
                onClick={() => submit.mutate()}
              >
                {testRunning
                  ? t('测试中…', 'Testing…')
                  : t(`导入 ${importable.length} 条`, `Import ${importable.length}`)}
              </Button>
            </>
          ) : (
            <>
              {lines.length > 0 && (
                <p className="mr-auto self-center text-xs text-muted-foreground tabular-nums">
                  {t(`共 ${lines.length} 条`, `${lines.length} entries`)}
                </p>
              )}
              <DialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</DialogClose>
              <Button disabled={lines.length === 0} loading={check.isPending} onClick={() => check.mutate()}>
                {t('下一步', 'Next')}
              </Button>
            </>
          )}
        </DialogFooter>
      </DialogPopup>
    </Dialog>
  )
}
