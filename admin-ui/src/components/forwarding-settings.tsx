import { useEffect, useState } from 'react'
import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  AdjustmentsHorizontalIcon, ArrowPathIcon, ChevronDownIcon, CircleStackIcon,
  CommandLineIcon, IdentificationIcon, InformationCircleIcon, ServerStackIcon,
} from '@heroicons/react/24/outline'
import { toast } from 'sonner'
import {
  getSettings, setForwarding, setRateLimitRetryMax,
  type ForwardingKey, type Settings,
} from '@/api/settings'
import { extractError } from '@/lib/utils'
import {
  Dialog, DialogContent, DialogHeader, DialogBody, DialogTitle,
} from '@/components/ui/dialog'
import { Switch } from '@/components/ui/switch'
import { Button } from '@/components/ui/button'
import { Input } from '@/components/ui/input'

/**
 * 转发形态开关。
 *
 * 这些改动都不是「能不能用」的必需项——实测（8 发对照请求）上游唯一强制的是 `system` 里
 * 那句 `You are Claude Code, …`，而它由客户端自己发；luban 唯一必需的改动是注入
 * `Authorization`。所以下面每一项都可以单独关掉，用来排查「是不是某一项反而成了判据」。
 */
export function ForwardingSettings({
  open, onOpenChange,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-3xl" aria-describedby={undefined}>
        <DialogHeader>
          <DialogTitle>
            <AdjustmentsHorizontalIcon className="size-4" />
            转发形态
          </DialogTitle>
        </DialogHeader>
        <DialogBody>
          <ForwardingSettingsContent />
        </DialogBody>
      </DialogContent>
    </Dialog>
  )
}

export function ForwardingSettingsContent() {
  const settingsQuery = useQuery({ queryKey: ['settings'], queryFn: getSettings })

  if (settingsQuery.isPending) {
    return (
      <div className="flex min-h-40 items-center justify-center gap-2 text-sm text-muted-foreground" role="status">
        <ArrowPathIcon className="size-4 animate-spin" />
        正在加载设置
      </div>
    )
  }

  if (settingsQuery.isError) {
    return (
      <div className="flex min-h-40 flex-col items-center justify-center gap-3 text-center" role="alert">
        <p className="text-sm font-medium">无法读取当前设置</p>
        <Button size="sm" variant="outline" onClick={() => settingsQuery.refetch()} disabled={settingsQuery.isFetching}>
          <ArrowPathIcon className={settingsQuery.isFetching ? 'animate-spin' : undefined} />
          重试
        </Button>
      </div>
    )
  }

  return (
    <div className="space-y-8">
      <div className="border-b border-border/80 pb-5">
        <div className="flex items-start gap-2.5 border-l-2 border-border py-1 pl-3 text-xs leading-5 text-muted-foreground">
            <InformationCircleIcon className="size-4 shrink-0" />
            <span>下列开关不影响必要的 <code className="font-mono text-foreground">Authorization</code> 注入。</span>
        </div>
      </div>

          <SettingsGroup
            icon={IdentificationIcon}
            title="身份与订阅"
          >
            <Toggle
              k="spoof_identity"
              label="身份一致性"
              summary="让客户端身份与当前账号、设备保持一致；关闭后原样转发。"
            />
            <Toggle
              k="billing_cch"
              label="订阅计费标识"
              summary="补齐订阅客户端所需的计费标识。"
            />
          </SettingsGroup>

          <SettingsGroup
            icon={ServerStackIcon}
            title="协议与请求头"
          >
            <Toggle
              k="merge_beta"
              label="Beta 标记"
              summary="合并客户端 Beta 标记并补齐订阅所需标记。"
            />
            <Toggle
              k="fill_client_headers"
              label="客户端请求头"
              summary="补齐缺失的版本、编码和请求标识，不覆盖已有值。"
            />
            <Toggle
              k="orig_header_case"
              label="请求头形态"
              summary="还原官方客户端的请求头拼写与顺序；仅在排查兼容问题时关闭。"
            />
          </SettingsGroup>

          <SettingsGroup
            icon={CircleStackIcon}
            title="缓存优化"
          >
            <Toggle
              k="system_shape"
              label="系统提示词缓存"
              summary="调整系统提示词分块，提高跨会话缓存复用。"
              desc={
                <>
                  只调整分块与缓存时间，不改变提示词文本。1 小时缓存写入单价是 5 分钟的 2 倍，
                  但能延长复用时间；无法识别切点时原样转发。
                </>
              }
            />
          </SettingsGroup>

          <SettingsGroup
            icon={CommandLineIcon}
            title="非官方客户端"
          >
            <Toggle
              k="simulate_cc"
              label="模拟 Claude Code"
              summary="让 SDK 和第三方客户端按 Claude Code 请求形态转发。"
              desc={
                <>
                  仅改写非 Claude Code 请求。开启后会增加系统提示词和客户端请求头，
                  可能提高 Token 成本并改变输出风格。此类请求通常没有设备身份，
                  需先关闭「设备身份校验」。
                </>
              }
            />
          </SettingsGroup>

          <SettingsGroup
            icon={ArrowPathIcon}
            title="限流与错误恢复"
          >
            <Toggle
              k="rate_limit_retry"
              label="429 自动换号"
              summary="遇到限流后，冷却受限账号或模型，并换用其他账号重试。"
              desc={
                <>
                  账号额度耗尽时冷却整个账号；只有当前模型受限时仅冷却该模型。默认分别冷却
                  60 / 30 秒，并优先采用上游等待时间。换号会改绑有设备身份的请求，也可能降低缓存命中率；
                  达到重试上限或没有其他账号时返回 <code>429</code>。
                </>
              }
            />
            <RetryMax />
            <Toggle
              k="thinking_signature_retry"
              label="thinking 签名兜底"
              summary="账号切换导致历史 thinking 签名失效时，自动降级并重试一次。"
              desc={
                <>
                  无法验证的历史 <code>thinking</code> 会转为普通文本，并用同一账号重试一次，
                  不会删除原内容。工具续跑仍可能失败；重试会增加一次请求成本，失败时返回原始 400。
                </>
              }
            />
          </SettingsGroup>
    </div>
  )
}

/** 429 后追加尝试的账号数（不含首次请求；0 = 不重试；后端夹到 0~10）。 */
function RetryMax() {
  const qc = useQueryClient()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const [draft, setDraft] = useState('')
  useEffect(() => {
    if (data) setDraft(String(data.rate_limit_retry_max))
  }, [data?.rate_limit_retry_max])

  const save = useMutation({
    mutationFn: (n: number) => setRateLimitRetryMax(n),
    onSuccess: (s: Settings) => {
      toast.success(s.rate_limit_retry_max > 0
        ? `429 后最多追加尝试 ${s.rate_limit_retry_max} 个账号`
        : '已设为直接透传 429（不冷却、不换号）')
      qc.setQueryData(['settings'], s)
    },
    onError: (e) => toast.error('保存失败', { description: extractError(e) }),
  })

  const n = Math.min(10, Math.max(0, Math.floor(Number(draft) || 0)))
  const enabled = data?.rate_limit_retry ?? true

  return (
    <div className="px-3 py-3 sm:px-4 sm:py-3.5">
      <div className="flex flex-wrap items-center justify-between gap-3">
        <div className="min-w-0">
          <div className="text-sm font-medium">追加重试账号数</div>
          <p className="mt-0.5 text-xs leading-5 text-muted-foreground">
            填 2 时最多尝试 3 个账号（含首次）。
          </p>
        </div>
        <div className="flex items-center gap-2">
          <Input
            type="number"
            min={0}
            max={10}
            value={draft}
            disabled={!enabled}
            onChange={(e) => setDraft(e.target.value)}
            className="w-20 font-mono"
            aria-label="429 追加重试账号数"
          />
          <Button
            size="sm"
            onClick={() => save.mutate(n)}
            disabled={save.isPending || !enabled || n === (data?.rate_limit_retry_max ?? 2)}
          >
            保存
          </Button>
        </div>
      </div>
    </div>
  )
}

/** 单个开关：读写都走 ['settings']，改完让账号列表也失效（形态影响缓存命中与计费）。 */
function Toggle({
  k, label, summary, desc,
}: {
  k: ForwardingKey
  label: string
  summary: string
  desc?: React.ReactNode
}) {
  const qc = useQueryClient()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const enabled = data?.[k] ?? true

  const save = useMutation({
    mutationFn: (next: boolean) => setForwarding(k, next),
    onSuccess: (s: Settings) => {
      toast.success(`${label}：${s[k] ? '已开启' : '已关闭'}`)
      qc.setQueryData(['settings'], s)
      qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (e) => toast.error('保存失败', { description: extractError(e) }),
  })

  return (
    <div className="px-3 py-3 sm:px-4 sm:py-3.5">
      <div className="flex items-start justify-between gap-4">
        <div className="min-w-0">
          <div className="text-sm font-medium">{label}</div>
          <p className="mt-0.5 text-xs leading-5 text-muted-foreground">{summary}</p>
        </div>
        <span className="flex h-5 shrink-0 items-center">
          <Switch
            variant="success"
            checked={enabled}
            disabled={save.isPending}
            aria-label={label}
            onCheckedChange={(next) => save.mutate(next)}
          />
        </span>
      </div>
      {desc && (
        <details className="group mt-2 text-xs text-muted-foreground">
          <summary className="flex w-fit cursor-pointer list-none items-center gap-1.5 rounded-sm text-2xs font-medium text-muted-foreground transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring [&::-webkit-details-marker]:hidden">
            影响与限制
            <ChevronDownIcon className="size-3 transition-transform group-open:rotate-180" />
          </summary>
          <div className="mt-2 border-l-2 border-border pl-3 leading-5 [&_code]:rounded-sm [&_code]:bg-muted [&_code]:px-1 [&_code]:py-0.5 [&_code]:font-mono [&_code]:text-foreground">
            {desc}
          </div>
        </details>
      )}
    </div>
  )
}

function SettingsGroup({
  icon: Icon, title, children,
}: {
  icon: typeof IdentificationIcon
  title: string
  children: React.ReactNode
}) {
  return (
    <section>
      <div className="flex items-center gap-3 pb-2.5">
        <span className="grid size-8 shrink-0 place-items-center rounded-md bg-muted text-muted-foreground">
          <Icon className="size-4" />
        </span>
        <h3 className="text-sm font-semibold">{title}</h3>
      </div>
      <div className="divide-y divide-border border-y border-border/80">{children}</div>
    </section>
  )
}
