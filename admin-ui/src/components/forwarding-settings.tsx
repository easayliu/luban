import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import {
  AdjustmentsHorizontalIcon, ChevronDownIcon, CircleStackIcon,
  IdentificationIcon, InformationCircleIcon, ServerStackIcon,
} from '@heroicons/react/24/outline'
import { toast } from 'sonner'
import { getSettings, setForwarding, type ForwardingKey, type Settings } from '@/api/settings'
import { extractError } from '@/lib/utils'
import {
  Dialog, DialogContent, DialogHeader, DialogBody, DialogTitle, DialogDescription,
} from '@/components/ui/dialog'
import { Badge } from '@/components/ui/badge'
import { Switch } from '@/components/ui/switch'

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
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const enabledCount = data
    ? [
        data.spoof_identity, data.billing_cch, data.fill_client_headers,
        data.merge_beta, data.cache_scope_global, data.orig_header_case,
      ].filter(Boolean).length
    : 0
  const allOff = data ? enabledCount === 0 : false

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-3xl">
        <DialogHeader>
          <DialogTitle>
            <AdjustmentsHorizontalIcon className="size-4" />
            转发形态
            {data && (
              <Badge variant="outline" className="font-normal text-muted-foreground">
                {allOff ? '零改写' : `${enabledCount} / 6 已开启`}
              </Badge>
            )}
          </DialogTitle>
          <DialogDescription>调整身份、请求头与缓存兼容策略，修改后即时生效。</DialogDescription>
        </DialogHeader>
        <DialogBody className="space-y-4 bg-muted/20">
          <div className="flex items-center gap-2.5 rounded-lg border border-border bg-card px-3 py-2.5 text-xs text-muted-foreground shadow-sm sm:px-4">
            <InformationCircleIcon className="size-4 shrink-0" />
            <span>仅影响兼容性改写；必要的 <code className="font-mono text-foreground">Authorization</code> 注入始终保留。</span>
          </div>

          <SettingsGroup
            icon={IdentificationIcon}
            title="身份与订阅"
            description="统一账号身份及订阅请求特征。"
          >
            <Toggle
              k="spoof_identity"
              label="身份一致性"
              summary="保持 metadata.user_id 与账号、设备指纹一致。"
              desc={
                <>
                  将 <code>account_uuid</code> 和 <code>device_id</code> 改写为当前凭证的自洽身份，
                  避免账号与设备不匹配。关闭后原样透传客户端身份。
                </>
              }
            />
            <Toggle
              k="billing_cch"
              label="订阅计费标识"
              summary="补齐 x-anthropic-billing-header 请求头。"
              desc={
                <>
                  订阅客户端会发送 <code>cch=&lt;5 位 hex&gt;</code>，API-key 模式通常不发送。
                  开启后会补充稳定占位值，使请求形态更接近订阅客户端。
                </>
              }
            />
          </SettingsGroup>

          <SettingsGroup
            icon={ServerStackIcon}
            title="协议与请求头"
            description="统一 Beta 标记和客户端请求头。"
          >
            <Toggle
              k="merge_beta"
              label="Beta 标记"
              summary="按官方顺序合并并补齐 anthropic-beta。"
              desc={
                <>
                  重排客户端 Beta 标记并补入 <code>oauth-2025-04-20</code>。
                  关闭后将原样转发客户端提供的内容。
                </>
              }
            />
            <Toggle
              k="fill_client_headers"
              label="客户端请求头"
              summary="补齐缺失的版本、编码和请求标识。"
              desc={
                <>
                  按需补充 <code>accept-encoding</code>、<code>anthropic-version</code> 和
                  <code>x-client-request-id</code>。已存在的请求头不会被重复覆盖。
                </>
              }
            />
            <Toggle
              k="orig_header_case"
              label="请求头形态"
              summary="还原官方客户端的头名拼写与排列顺序。"
              desc={
                <>
                  调整标准头、自定义头的大小写及顺序。关闭后请求头会退回默认小写形态，
                  仅建议在兼容性排查时临时关闭。
                </>
              }
            />
          </SettingsGroup>

          <SettingsGroup
            icon={CircleStackIcon}
            title="缓存优化"
            description="提高静态内容的跨会话复用率。"
          >
            <Toggle
              k="cache_scope_global"
              label="全局缓存"
              summary="为最长的静态 system 块启用 global scope。"
              desc={
                <>
                  添加 <code>cache_control.scope = &quot;global&quot;</code> 以提升跨会话缓存命中率。
                  关闭可能增加重复计算与费用，仅在追求零改写时使用。
                </>
              }
            />
          </SettingsGroup>
        </DialogBody>
      </DialogContent>
    </Dialog>
  )
}

/** 单个开关：读写都走 ['settings']，改完让账号列表也失效（形态影响缓存命中与计费）。 */
function Toggle({
  k, label, summary, desc,
}: {
  k: ForwardingKey
  label: string
  summary: string
  desc: React.ReactNode
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
        <Switch
          className="mt-0.5 shrink-0 sm:mt-1"
          variant="success"
          checked={enabled}
          disabled={save.isPending}
          aria-label={label}
          onCheckedChange={(next) => save.mutate(next)}
        />
      </div>
      <details className="group mt-2 text-xs text-muted-foreground">
        <summary className="flex w-fit cursor-pointer list-none items-center gap-1.5 rounded-sm text-2xs font-medium text-muted-foreground transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring [&::-webkit-details-marker]:hidden">
          技术说明
          <ChevronDownIcon className="size-3 transition-transform group-open:rotate-180" />
        </summary>
        <div className="mt-2 rounded-md bg-muted/50 px-3 py-2.5 leading-5 [&_code]:rounded-sm [&_code]:bg-background [&_code]:px-1 [&_code]:py-0.5 [&_code]:font-mono [&_code]:text-foreground">
          {desc}
        </div>
      </details>
    </div>
  )
}

function SettingsGroup({
  icon: Icon, title, description, children,
}: {
  icon: typeof IdentificationIcon
  title: string
  description: string
  children: React.ReactNode
}) {
  return (
    <section className="overflow-hidden rounded-lg border border-border bg-card shadow-sm">
      <div className="flex items-center gap-3 border-b border-border bg-muted/30 px-3 py-3 sm:px-4">
        <span className="grid size-8 shrink-0 place-items-center rounded-md border border-border bg-background text-muted-foreground">
          <Icon className="size-4" />
        </span>
        <div className="min-w-0">
          <h3 className="text-sm font-semibold">{title}</h3>
          <p className="mt-0.5 text-2xs text-muted-foreground">{description}</p>
        </div>
      </div>
      <div className="divide-y divide-border">{children}</div>
    </section>
  )
}
