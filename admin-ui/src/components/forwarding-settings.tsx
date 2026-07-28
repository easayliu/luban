import { useQuery, useMutation, useQueryClient } from '@tanstack/react-query'
import { AdjustmentsHorizontalIcon, BeakerIcon } from '@heroicons/react/24/outline'
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
  const allOff = data
    ? !data.spoof_identity && !data.billing_cch && !data.fill_client_headers &&
      !data.merge_beta && !data.cache_scope_global && !data.orig_header_case
    : false

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent className="max-w-2xl">
        <DialogHeader>
          <DialogTitle>
            <AdjustmentsHorizontalIcon className="size-4" />
            转发形态
            {allOff && <Badge variant="outline">零改写</Badge>}
          </DialogTitle>
          <DialogDescription>
            逐项控制 luban 在转发时对请求做的形态对齐。全部关掉即除注入鉴权外原样透传。
          </DialogDescription>
        </DialogHeader>
        <DialogBody className="space-y-4">
          <div className="flex gap-2.5 rounded-lg border border-border bg-surface-2 p-3 text-xs leading-5 text-muted-foreground">
            <BeakerIcon className="mt-0.5 size-4 shrink-0" />
            <div>
              实测结论：这些项<strong className="font-medium text-foreground">全关也照样 200</strong>。上游唯一强制的是{' '}
              <code className="font-mono">system</code> 里那句{' '}
              <code className="font-mono">You are Claude Code, …</code>（由客户端自己发，缺了会收到
              伪装成限流的 <code className="font-mono">429 rate_limit_error</code>）；luban 侧唯一
              必需的改动是注入 <code className="font-mono">Authorization</code>。
              所以这里关掉任何一项都不影响可用性，只影响与官方客户端的形态贴合度。
            </div>
          </div>

          <Toggle
            k="spoof_identity"
            label="身份伪装（metadata.user_id）"
            desc={
              <>
                把请求 <code className="font-mono">metadata.user_id</code> 里的{' '}
                <code className="font-mono">account_uuid</code> 与{' '}
                <code className="font-mono">device_id</code>{' '}
                改写成所用凭证的自洽身份（真实账号 + 按设备指纹稳定派生的 device_id），
                避免「真账号 + 陌生设备」的矛盾。关闭则原样透传客户端身份（API-key 模式的 CC
                发的 <code className="font-mono">account_uuid</code> 是空串）。
              </>
            }
          />

          <Toggle
            k="billing_cch"
            label="补 cch（x-anthropic-billing-header）"
            desc={
              <>
                官方客户端只在订阅模式下发 <code className="font-mono">cch=&lt;5 位 hex&gt;</code>，
                API-key 模式不发，于是「OAuth token + 无 cch」是个确定性判据。注意补的是一个
                跨账号恒定的占位值，真算法无法从抓包反推。
              </>
            }
          />

          <Toggle
            k="merge_beta"
            label="合并重排 anthropic-beta"
            desc={
              <>
                把客户端的 beta 串按官方顺序重排并塞入{' '}
                <code className="font-mono">oauth-2025-04-20</code>，使其与官方订阅客户端逐字节
                一致。实测不带这个 beta 也能 200。关闭则原样转发客户端那串。
              </>
            }
          />

          <Toggle
            k="fill_client_headers"
            label="补齐缺失的客户端头"
            desc={
              <>
                客户端没带时补上 <code className="font-mono">accept-encoding</code>（官方取值）、
                <code className="font-mono">anthropic-version</code>、
                <code className="font-mono">x-client-request-id</code>（每请求一个 uuid v4）。
                关闭后 accept-encoding 仍由上游 client 的默认头兜底，不会退化成非官方取值。
              </>
            }
          />

          <Toggle
            k="orig_header_case"
            label="头名大小写与顺序"
            desc={
              <>
                按官方客户端的原始拼写与顺序发出头名：标准头首字母大写（
                <code className="font-mono">Accept-Encoding</code>）、SDK 自定义头全小写（
                <code className="font-mono">anthropic-beta</code>）、
                <code className="font-mono">X-Stainless-OS</code> 的 OS 全大写。同一张表还决定
                头序，所以 <code className="font-mono">Host</code>/
                <code className="font-mono">User-Agent</code>/
                <code className="font-mono">Content-Length</code>{' '}
                也能落在官方位置而不是被追加到队尾。
                <span className="text-warn">
                  {' '}关掉不等于「恢复原样」：头名会退回全小写，且 user-agent/accept-encoding
                  会被前置到队首，比开着更不像官方客户端——这个开关只用于出问题时二分。
                </span>
              </>
            }
          />

          <Toggle
            k="cache_scope_global"
            label="缓存 scope=global"
            desc={
              <>
                给最长的静态 <code className="font-mono">system</code> 块标{' '}
                <code className="font-mono">cache_control.scope = &quot;global&quot;</code>，
                提升跨会话缓存复用。抓包显示官方订阅模式自己就带这个标记、API-key 模式不带，
                所以它既贴形态也真省钱——<span className="text-warn">关掉会掉缓存命中率</span>，
                只在追求「零改写」时才需要关。
              </>
            }
          />
        </DialogBody>
      </DialogContent>
    </Dialog>
  )
}

/** 单个开关：读写都走 ['settings']，改完让账号列表也失效（形态影响缓存命中与计费）。 */
function Toggle({
  k, label, desc,
}: {
  k: ForwardingKey
  label: string
  desc: React.ReactNode
}) {
  const qc = useQueryClient()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const enabled = data?.[k] ?? true

  const save = useMutation({
    mutationFn: (next: boolean) => setForwarding(k, next),
    onSuccess: (s: Settings) => {
      toast.success(`${label}：${s[k] ? '已开启' : '已关闭'}`)
      qc.invalidateQueries({ queryKey: ['settings'] })
    },
    onError: (e) => toast.error('保存失败', { description: extractError(e) }),
  })

  return (
    <div className="border-t border-border pt-4">
      <div className="label-eyebrow mb-1.5">{label}</div>
      <div className="flex items-center gap-3">
        <Switch
          variant="success"
          checked={enabled}
          disabled={save.isPending}
          onCheckedChange={(next) => save.mutate(next)}
        />
        <span className="text-sm">{enabled ? '已开启' : '已关闭（原样透传）'}</span>
      </div>
      <p className="mt-1.5 text-xs leading-5 text-muted-foreground">{desc}</p>
    </div>
  )
}
