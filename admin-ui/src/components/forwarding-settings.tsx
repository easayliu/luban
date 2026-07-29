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
  Dialog, DialogContent, DialogHeader, DialogBody, DialogTitle, DialogDescription,
} from '@/components/ui/dialog'
import { Badge } from '@/components/ui/badge'
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
      <DialogContent className="max-w-3xl">
        <DialogHeader>
          <DialogTitle>
            <AdjustmentsHorizontalIcon className="size-4" />
            转发形态
          </DialogTitle>
          <DialogDescription>调整身份、请求头与缓存兼容策略，修改后即时生效。</DialogDescription>
        </DialogHeader>
        <DialogBody>
          <ForwardingSettingsContent />
        </DialogBody>
      </DialogContent>
    </Dialog>
  )
}

export function ForwardingSettingsContent() {
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const switches = data
    ? [
        data.spoof_identity, data.billing_cch, data.fill_client_headers,
        data.merge_beta, data.system_shape, data.orig_header_case,
        data.thinking_signature_retry, data.simulate_cc, data.rate_limit_retry,
      ]
    : []
  const enabledCount = switches.filter(Boolean).length
  const allOff = data ? enabledCount === 0 : false

  return (
    <div className="space-y-8">
      <div className="flex flex-col gap-3 border-b border-border/80 pb-5 sm:flex-row sm:items-center sm:justify-between">
        <div className="flex items-start gap-2.5 border-l-2 border-border py-1 pl-3 text-xs leading-5 text-muted-foreground">
            <InformationCircleIcon className="size-4 shrink-0" />
            <span>仅影响兼容性改写；必要的 <code className="font-mono text-foreground">Authorization</code> 注入始终保留。</span>
        </div>
        {data && (
          <Badge variant="outline" className="w-fit shrink-0 font-normal text-muted-foreground">
            {allOff ? '零改写' : `${enabledCount} / ${switches.length} 已开启`}
          </Badge>
        )}
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
              k="system_shape"
              label="system 形态"
              summary="把 system 拆成官方订阅客户端的四块。"
              desc={
                <>
                  API-key 模式的客户端把系统提示词发成三块，官方订阅客户端发四块：基座单独一块并标
                  <code>scope = &quot;global&quot;</code>，全部缓存断点都是 <code>ttl = &quot;1h&quot;</code>。
                  开启后按官方切点拆块并对齐断点，文本本身逐字节不变。
                  <br />
                  <strong>会影响费用</strong>：1h 缓存写入单价是 5m 的两倍，换来的是缓存保留一小时、
                  以及基座那块跨账号复用。认不出切点的模型（锚点未匹配）会原样转发，不会切错。
                </>
              }
            />
          </SettingsGroup>

          <SettingsGroup
            icon={CommandLineIcon}
            title="非官方客户端"
            description="让 SDK、第三方前端这类请求也能用上订阅额度。"
          >
            <Toggle
              k="simulate_cc"
              label="模拟 Claude Code"
              summary="非 CC 请求按官方抓包形态补全 system 与请求头。"
              desc={
                <>
                  订阅凭证在上游是「只授权给 Claude Code 用」的：<code>system</code> 里缺那句
                  <code>You are Claude Code, …</code> 就用不了额度，于是各种 SDK、第三方前端、
                  <code>curl</code> 经 luban 都是死路一条。
                  <br />
                  开启后，<strong>只有认不出是 CC 的请求</strong>会被整形：<code>system</code> 前面补上
                  官方的三块（计费标识、身份句、按模型族选的官方基座，客户端自己的提示词原样留作末块），
                  请求头整套换成官方那套（<code>User-Agent</code>、<code>x-app</code>、
                  <code>x-stainless-*</code>…，客户端自带的非官方头不转发，它要的
                  <code>anthropic-beta</code> 取并集保留），<code>metadata</code> 补上与账号自洽的身份。
                  已经是 CC 形态的请求一个字节都不多改。
                  <br />
                  <strong>会影响费用与输出</strong>：每条请求多一个基座前缀（opus 族约 300 token、
                  sonnet 族约 2700 token，带 1h 全局断点，稳定后基本走缓存读价）；而且模型会被告知
                  「你是 Claude Code」，输出风格与工具偏好都会随之偏移。
                  <br />
                  这类请求通常没有 <code>metadata.user_id</code>，要先在「接入设置」里关掉
                  <strong>设备身份校验</strong>，否则它们在进门那一步就被 403 挡掉了。
                </>
              }
            />
          </SettingsGroup>

          <SettingsGroup
            icon={ArrowPathIcon}
            title="限流与错误恢复"
            description="区分额度耗尽、模型容量不足和签名异常。"
          >
            <Toggle
              k="rate_limit_retry"
              label="429 智能换号"
              summary="按账号或模型范围冷却，随后改用其它账号重发。"
              desc={
                <ul className="space-y-2">
                  <li>
                    <strong>冷却范围：</strong>额度窗口被拒或使用率达到 100% 时冷却整个账号；
                    窗口仍有余量时视为当前模型容量不足，只冷却该模型，账号仍可承接其它模型。
                  </li>
                  <li>
                    <strong>冷却时长：</strong>优先采用上游 <code>retry-after</code>；账号级其次参考整体或窗口重置时间，
                    缺失时为 60 秒；模型级缺失时为 30 秒。单次冷却最长 24 小时。
                  </li>
                  <li>
                    <strong>换号重试：</strong>每次只选择本次请求尚未尝试的账号。带设备身份的请求会同步改绑，
                    后续请求直接使用新账号；达到重试上限或没有其它账号时原样返回 <code>429</code>。
                  </li>
                  <li>
                    <strong>兜底策略：</strong>冷却只是选号提示；全部候选都在冷却时仍会继续选择，避免代理被整体锁死。
                    冷却仅保存在内存中，服务重启后自动清空。
                  </li>
                  <li>
                    <strong>成本影响：</strong>换号会失去原账号的 prompt cache，历史 <code>thinking</code> 签名也可能失效；
                    后者由下方签名兜底处理。关闭本项或将追加次数设为 0 时，不记录冷却并直接透传 <code>429</code>。
                  </li>
                </ul>
              }
            />
            <RetryMax />
            <Toggle
              k="thinking_signature_retry"
              label="thinking 签名兜底"
              summary="历史 thinking 被判签名无效时，降级后自动重试一次。"
              desc={
                <>
                  模型的 <code>thinking</code> 块带一段由签发账号校验的 <code>signature</code>。
                  会话中途换了号（设备绑定到期、原账号被停用），整段历史就验不过，
                  上游回 <code>Invalid `signature` in `thinking` block</code>，
                  而客户端自己修不了——只能 <code>/clear</code> 重开会话。
                  <br />
                  开启后遇到这条错误会把历史 <code>thinking</code> 的推理原文搬进 <code>text</code> 块
                  （裹一层 <code>&lt;previous_thinking&gt;</code>，签名丢掉），用<strong>同一个账号</strong>
                  重发一次。搬而不是删，是为了别让模型丢掉上一轮的推理链、续跑时从头再想一遍。
                  <br />
                  <strong>救不了工具续跑轮</strong>：请求末尾是 <code>tool_result</code> 时，上游另外要求
                  最后一条 assistant 消息必须以 thinking 块开头，降级完照样被拒。
                  重试失败会原样透传最初那条 400，所以开着最坏也只是多花一次往返。
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
            不含首次请求；例如填 2，单次请求最多共尝试 3 个账号。
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
      <details className="group mt-2 text-xs text-muted-foreground">
        <summary className="flex w-fit cursor-pointer list-none items-center gap-1.5 rounded-sm text-2xs font-medium text-muted-foreground transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring [&::-webkit-details-marker]:hidden">
          技术说明
          <ChevronDownIcon className="size-3 transition-transform group-open:rotate-180" />
        </summary>
        <div className="mt-2 border-l-2 border-border pl-3 leading-5 [&_code]:rounded-sm [&_code]:bg-muted [&_code]:px-1 [&_code]:py-0.5 [&_code]:font-mono [&_code]:text-foreground">
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
    <section>
      <div className="flex items-center gap-3 pb-2.5">
        <span className="grid size-8 shrink-0 place-items-center rounded-md bg-muted text-muted-foreground">
          <Icon className="size-4" />
        </span>
        <div className="min-w-0">
          <h3 className="text-sm font-semibold">{title}</h3>
          <p className="mt-0.5 text-2xs text-muted-foreground">{description}</p>
        </div>
      </div>
      <div className="divide-y divide-border border-y border-border/80">{children}</div>
    </section>
  )
}
