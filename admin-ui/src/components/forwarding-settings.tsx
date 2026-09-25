import { useEffect, useId, useState, type ReactNode } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  BadgeCheckIcon,
  BrainIcon,
  ChevronDownIcon,
  DatabaseIcon,
  SearchIcon,
  XIcon,
  InfoIcon,
  KeyRoundIcon,
  RefreshCwIcon,
  SaveIcon,
  ServerIcon,
  SlidersHorizontalIcon,
  TerminalIcon,
  Trash2Icon,
} from 'lucide-react'
import {
  clearLearnedRejections,
  forgetLearnedGroup,
  forgetLearnedRejection,
  getSettings,
  listLearnedRejections,
  setForwarding,
  setOauthScopes,
  setPrefillPolicy,
  setQuotaPausePct,
  setRateLimitRetryMax,
  setSamplingPolicy,
  type ForwardingKey,
  type LearnedRejection,
  type PolicyValue,
  type Settings,
} from '@/api/settings'
import { useI18n } from '@/lib/i18n'
import { cn, extractError, formatFullTime, relativeTime } from '@/lib/utils'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
import {
  AlertDialog, AlertDialogClose, AlertDialogDescription, AlertDialogFooter,
  AlertDialogHeader, AlertDialogPopup, AlertDialogTitle,
} from '@/components/ui/alert-dialog'
import { Badge } from '@/components/ui/badge'
import { Button } from '@/components/ui/button'
import {
  Dialog,
  DialogDescription,
  DialogHeader,
  DialogPanel,
  DialogPopup,
  DialogTitle,
} from '@/components/ui/dialog'
import { Empty, EmptyDescription, EmptyHeader, EmptyMedia, EmptyTitle } from '@/components/ui/empty'
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import { Input } from '@/components/ui/input'
import {
  NumberField,
  NumberFieldDecrement,
  NumberFieldGroup,
  NumberFieldIncrement,
  NumberFieldInput,
} from '@/components/ui/number-field'
import {
  Pagination, PaginationContent, PaginationItem, PaginationNext, PaginationPrevious,
} from '@/components/ui/pagination'
import {
  Select, SelectItem, SelectPopup, SelectTrigger, SelectValue,
} from '@/components/ui/select'
import { Spinner } from '@/components/ui/spinner'
import { Switch } from '@/components/ui/switch'
import { Textarea } from '@/components/ui/textarea'
import { Tooltip, TooltipPopup, TooltipTrigger } from '@/components/ui/tooltip'
import { toastManager } from '@/components/ui/toast'
import { ClampedDescription, SettingsGroup, SettingsRow } from '@/components/settings-group'

/**
 * 转发形态开关。
 *
 * 这些改动都不是「能不能用」的必需项。每一项都可以单独关闭，用于排查上游兼容性。
 */
export function ForwardingSettings({
  open,
  onOpenChange,
}: {
  open: boolean
  onOpenChange: (open: boolean) => void
}) {
  const { t } = useI18n()

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogPopup size="lg">
        <DialogHeader>
          <DialogTitle className="flex items-center gap-2">
            <SlidersHorizontalIcon aria-hidden="true" />
            {t('转发策略', 'Forwarding policy')}
          </DialogTitle>
          <DialogDescription>
            {t(
              '配置兼容、缓存、限流和错误恢复策略。',
              'Configure compatibility, caching, rate limiting, and error recovery policies.',
            )}
          </DialogDescription>
        </DialogHeader>
        <DialogPanel>
          <ForwardingSettingsContent />
        </DialogPanel>
      </DialogPopup>
    </Dialog>
  )
}

export function ForwardingSettingsContent() {
  const { t } = useI18n()
  const settingsQuery = useQuery({ queryKey: ['settings'], queryFn: getSettings })

  if (settingsQuery.isPending) {
    return (
      <div className="flex min-h-40 items-center justify-center gap-2 text-sm text-muted-foreground" role="status">
        <Spinner className="size-4" />
        {t('正在加载设置', 'Loading settings')}
      </div>
    )
  }

  if (settingsQuery.isError) {
    return (
      <div className="flex min-h-40 flex-col items-center justify-center gap-3 text-center" role="alert">
        <p className="text-sm font-medium">
          {t('无法读取当前设置', 'Unable to load current settings')}
        </p>
        <Button
          size="sm"
          variant="outline"
          loading={settingsQuery.isFetching}
          onClick={() => settingsQuery.refetch()}
        >
          {t('重试', 'Retry')}
        </Button>
      </div>
    )
  }

  return (
    <div className="space-y-4">
      <Alert variant="info">
        <InfoIcon aria-hidden="true" />
        <AlertTitle>{t('必要请求头不受影响', 'Required headers are unaffected')}</AlertTitle>
        <AlertDescription>
          <p>
            {t('下列开关不影响必要的', 'The following toggles do not affect required')}{' '}
            <code className="font-mono text-foreground">Authorization</code>{' '}
            {t('注入。', 'injection.')}
          </p>
        </AlertDescription>
      </Alert>

      <SettingsGroup icon={BadgeCheckIcon} title={t('身份与订阅', 'Identity & subscription')}>
        <ForwardingToggle
          k="spoof_identity"
          label={t('身份一致性', 'Identity consistency')}
          summary={t(
            '让客户端身份与当前账号、设备保持一致；关闭后原样转发。',
            'Keep the client identity consistent with the current account and device; when disabled, forward it unchanged.',
          )}
          description={
            <>
              {t(
                '把请求里的账号 ID 与设备 ID 改写为当前账号的值，并把客户端的会话 ID 按账号派生为另一个稳定值：同一条会话在同一个账号上始终得到同一个值，多轮对话与缓存照常接续；换到另一个账号就是另一个值。若不这样做，设备因账号停用或限流被改绑到其他账号时，同一个会话 ID 会带着两个设备 ID 先后出现在两个账号下。而官方客户端的一条会话只属于一个账号、一台设备；封号复盘中，两个账号的会话正是这样在半分钟内先后出现在两个组织下。请求头与 metadata 两处始终写入同一个值。',
                'Rewrites the account ID and device ID in the request to the current account’s values, and derives a stable per-account value from the client’s session ID: the same session always maps to the same value on the same account, so multi-turn conversations and caching continue as usual, while another account gets a different value. Without this, when a device is rebound to another account because its account was disabled or rate-limited, the same session ID would appear under two accounts with two different device IDs. The official client’s session belongs to exactly one account and one device; in the ban review, sessions from two accounts surfaced in two organizations within half a minute in exactly this way. The header and metadata always carry the same value.',
              )}
            </>
          }
        />
        {/* 紧跟「身份一致性」：它是本项的前置开关，挨着放才好一起改。 */}
        <ForwardingToggle
          k="fill_metadata"
          label={t('补齐设备身份', 'Fill missing device identity')}
          summary={t(
            '请求未携带设备身份时，按当前账号补一份；已携带的不改动。',
            'When a request lacks a device identity, add one for the current account; leave existing values unchanged.',
          )}
          requires={{ key: 'spoof_identity', label: t('身份一致性', 'Identity consistency') }}
          description={
            <>
              {t(
                '官方客户端的每条请求都带设备身份，缺失本身就是一处差异，常见于模仿 Claude Code 的第三方客户端。补出的身份与「身份一致性」使用同一套取值；会话 ID 优先沿用请求自带的。',
                'The official client includes a device identity with every request, so a missing identity is itself a discrepancy; this is common in third-party clients that imitate Claude Code. The generated identity uses the same values as “Identity consistency”, while a session ID already present in the request takes precedence.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="api_telemetry"
          label={t('逐请求遥测', 'Per-request telemetry')}
          summary={t(
            '为每条转发的请求上报官方客户端会发送的整套遥测：事件链、Datadog 日志与用量指标；失败的请求上报错误事件。',
            'Report the telemetry the official client sends for every forwarded request: the event chain, Datadog logs, and usage metrics — with an error event for the ones that fail.',
          )}
          description={
            <>
              {t(
                '官方客户端每发一条请求，都会上报 tengu_api_query → tengu_api_success → tengu_turn_end 这一串事件（带上游 request-id、逐项 token 与花费），以及 Datadog 日志和 OTel 用量指标。此前 luban 只有每 30 分钟一次的保活遥测，上游看到的是「有大量 API 用量，遥测里却没有一条 API 调用」。开启后，按 2.1.260 抓包的字段与节奏（事件每 30 秒、日志每 10 秒、指标每 5 分钟攒批发送）为每个账号补上这些遥测；身份取实际发往上游的那一份，与请求两侧保持一致。失败的请求同样上报：官方客户端对失败请求发送的是 tengu_api_error 加 tengu_feature_bad，只上报成功的请求同样是一处可被对照出来的差异。关闭后只保留保活遥测。',
                'The official client reports a chain of events for every request it sends (tengu_api_query → tengu_api_success → tengu_turn_end, carrying the upstream request-id, per-type token counts and cost), plus Datadog logs and OTel usage metrics. Previously luban only sent the 30-minute keepalive telemetry, so upstream saw an account with heavy API usage and not a single API call in its telemetry. When enabled, luban fills this in for every account following the fields and cadence captured from 2.1.260 (events batched every 30s, logs every 10s, metrics every 5min), using the identity actually sent upstream so both sides agree. Failed requests are reported too — the official client sends tengu_api_error plus tengu_feature_bad for those, so reporting only the successes is itself a detectable discrepancy. Turn it off to keep only the keepalive telemetry.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="keepalive_telemetry"
          label={t('保活遥测', 'Keepalive telemetry')}
          summary={t(
            '每 30 分钟为每个账号发送一组空闲版本检查事件与 Datadog 日志，每 6 小时上报一次画像。',
            'Every 30 minutes send a set of idle version-check events and Datadog logs for each account, plus a profile report every 6 hours.',
          )}
          description={
            <>
              {t(
                '模拟一个开着但空闲的 Claude Code 进程。账号近 3 小时内有真实会话时，事件挂在该会话的身份上（相同的 session_id、设备 ID 与客户端版本），与真实客户端开着终端却无人输入时的行为一致；近期没有会话的账号才使用按账号派生的空闲身份。关闭后只停止遥测这一部分，token 刷新、启动握手（bootstrap / policy_limits / settings）与 401/403 探测保持不变。与「逐请求遥测」互不影响。',
                'Simulates an open but idle Claude Code process. When the account has had a real session in the last 3 hours, the events are attached to that session (same session_id, device ID and client version), matching what a real client does when a terminal is left open with no input; only accounts with no recent session fall back to an account-derived idle identity. Turning it off stops only the telemetry part: token refresh, the startup handshake (bootstrap / policy_limits / settings) and 401/403 detection continue. Independent of “Per-request telemetry”.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="spoof_device_id"
          label={t('改写设备 ID', 'Rewrite device ID')}
          summary={t(
            '请求自带设备 ID 时，替换为按当前账号派生的值；关闭则原样沿用。',
            'Replace a device ID sent by the client with one derived from the current account; when disabled, pass it through unchanged.',
          )}
          requires={{ key: 'spoof_identity', label: t('身份一致性', 'Identity consistency') }}
          description={
            <>
              {t(
                '官方客户端的设备 ID 是「机器标识」：同一台机器无论使用哪个账号，发送的都是同一个值；官方在 API key 与订阅两种模式下发送的也完全相同，两种模式真正的差别只在账号 ID 那一段。因此替换它并非形态需要，而是一项防关联措施：开启后，每个账号在同一台机器上各有独立的设备 ID，账号之间不会因共用一个 ID 而被关联。代价：「一台机器多个账号」在真实用户中很常见，但在经由本代理的流量里一次都不会出现。关闭后与官方逐字节一致，但同一台机器上的多个账号可被上游关联。请求未携带设备 ID 时一律派生，不受本开关影响。',
                'The official client’s device ID is a machine identifier: the same machine sends the same value no matter which account is used, and it is identical in both API-key and subscription modes; the only real difference between those modes is the account ID. Replacing it is therefore not a shape requirement but an unlinking measure. When enabled, each account gets its own device ID on the same machine, so accounts cannot be linked through a shared value. Cost: “one machine, several accounts”, common among real users, never appears in traffic through this proxy. When disabled, the value matches the official client byte for byte, but several accounts on one machine can be linked upstream. Requests that carry no device ID always get a derived one, regardless of this toggle.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="normalize_device_fp"
          label={t('设备指纹归一化', 'Normalize device fingerprint')}
          summary={t(
            '同平台且同客户端版本的客户端收敛为同一个设备 ID。',
            'Converge clients that share a platform and a client version into one device ID.',
          )}
          requires={{ key: 'spoof_device_id', label: t('改写设备 ID', 'Rewrite device ID') }}
          description={
            <>
              {t(
                '设备指纹用于派生每个账号的伪装设备 ID。开启后，指纹只取平台信息（CPU 架构与操作系统），不含客户端原始设备 ID，同一平台上的客户端会收敛成同一个伪装设备 ID，符合真实用户一人多设备的使用模式。关闭后，指纹包含客户端原始设备 ID，每个（账号、客户端设备）组合都对应一个独立的上游设备 ID；客户端越多，上游看到该账号的设备数就越多。无论开关如何，指纹都包含实际发往上游的客户端版本：一台设备只能有一个版本，否则上游会看到同一个设备 ID 在同一秒里自报多个版本（这是封号复盘里最明显的一条）。代价：客户端升级后设备 ID 会随之更换，在上游看来像是这台机器重装了一次，但这比版本来回切换安全得多。',
                'The device fingerprint is used to derive a spoofed device ID per account. When enabled, the fingerprint uses only platform information (CPU architecture and operating system) and excludes the client’s original device ID, so clients on the same platform converge onto one spoofed device ID, matching the usage pattern of a real user with multiple devices. When disabled, the fingerprint includes the client’s original device ID, making each (account, client device) combination a separate upstream device ID; the more clients there are, the more devices upstream sees for that account. Either way the fingerprint also includes the client version actually sent upstream: one device may only ever report one version, otherwise upstream sees a single device ID claiming several versions within the same second (the most obvious signal in the ban post-mortem). Cost: a client upgrade rotates the device ID, which upstream reads as the machine being reinstalled, but that is far safer than a version that flips back and forth.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="billing_cch"
          label={t('订阅计费标识', 'Subscription billing identifier')}
          summary={t(
            '补齐订阅客户端所需的计费标识。',
            'Add the billing identifier required by subscription clients.',
          )}
        />
      </SettingsGroup>

      <SettingsGroup
        icon={KeyRoundIcon}
        title={t('登录授权范围', 'Login authorization scopes')}
        description={t(
          '添加账号时向 Claude 申请哪些权限。只影响之后新登录的账号，已添加的不受影响。',
          'Which permissions are requested from Claude when adding an account. Only affects accounts added from now on; existing ones are unchanged.',
        )}
      >
        <OAuthScopes />
      </SettingsGroup>

      <SettingsGroup icon={ServerIcon} title={t('协议与请求头', 'Protocol & request headers')}>
        <ForwardingToggle
          k="merge_beta"
          label={t('Beta 标记', 'Beta flags')}
          summary={t(
            '合并客户端 Beta 标记并补齐订阅所需标记。',
            'Merge the client’s Beta flags and add the flags required for subscriptions.',
          )}
        />
        <ForwardingToggle
          k="fill_client_headers"
          label={t('客户端请求头', 'Client request headers')}
          summary={t(
            '补齐缺失的版本、编码和请求 ID，不覆盖已有值。',
            'Fill in missing version, encoding, and request ID headers without overwriting existing values.',
          )}
        />
        <ForwardingToggle
          k="orig_header_case"
          label={t('请求头形态', 'Request header shape')}
          summary={t(
            '还原官方客户端的请求头拼写与顺序；仅在排查兼容问题时关闭。',
            'Restore the official client’s request header casing and order; disable only when troubleshooting compatibility issues.',
          )}
        />
        <ForwardingToggle
          k="nonstream_as_sse"
          label={t('非流式请求流式化', 'Upgrade non-streaming requests')}
          summary={t(
            '把非流式请求改成流式发给上游，响应仍按非流式整段返回，客户端无感。',
            'Send non-streaming requests upstream as streaming ones and return the response as a single non-streaming body, transparently to the client.',
          )}
          description={
            <>
              {t(
                '官方客户端的对话请求一律是流式的，原样转发非流式请求就是一处稳定特征。开启后，luban 只改请求里的 stream 字段，收到的流式响应在本地拼回完整内容，再按客户端原本期待的格式返回，请求头与返回格式都不变。上游中途报错时，错误原文按非流式请求应有的状态码返回，客户端的错误处理不受影响。代价：响应要等上游全部生成完才发出（与非流式本来的行为一致），且整段内容需在内存中暂存。请求明细里这类记录会标注「非流转流」，因为其首字耗时记录的是上游首字节，与客户端的感知不同。仅作用于对话请求，token 计数接口不受影响。',
                'The official client always sends conversation requests as streaming ones, so a non-streaming request forwarded as-is is a stable tell. When enabled, luban only flips the stream field in the request, reassembles the streamed response locally, and returns it in the format the client already expected — request headers and response format are unchanged. If the upstream errors mid-stream, the raw error is returned with the status code a non-streaming request would have received, so client error handling is unaffected. Costs: the response is sent only after the upstream finishes generating (same as non-streaming behaviour anyway) and the whole body is buffered in memory; such records are tagged “stream-upgraded” in the request log, because their TTFT is the upstream first byte rather than what the client perceived. Applies to conversation requests only; the token-counting endpoint is untouched.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="tool_name_mimic"
          label={t('工具名混淆', 'Tool name obfuscation')}
          summary={t(
            '把会被上游判成第三方应用的工具名换成 MCP 形态假名转发，返回时自动还原，客户端无感。',
            'Forward tool names that would flag the request as a third-party app under generated MCP-shaped aliases, restored transparently on the way back.',
          )}
          description={
            <>
              {t(
                '工具名是上游判断第三方应用的一项已验证判据：命中后上游返回「Third-party apps now draw from your extra usage」，即使订阅用量充足也改扣超额用量。实测 3 个业务工具名就足以触发，而以 `mcp__` 开头的名字会被豁免。开启后，luban 把这些名字替换成 `mcp__luban__*` 下的稳定假名再发出，响应中再换回真名，客户端自始至终看到的都是自己的工具名。官方自带的工具、客户端本就以 MCP 命名的工具和服务端工具都保留原名，因此对真实的官方客户端没有任何影响。代价：响应内容要多做一次字符串替换；客户端在会话中途增删工具会让假名整体重算，上游的提示词缓存会失效一次。',
                'Tool names are one verified signal the upstream uses to classify third-party apps; a match returns “Third-party apps now draw from your extra usage” and bills against extra usage even when plan usage remains. In testing, three business tool names were enough to trigger it, while names beginning with `mcp__` are exempt. When enabled, luban forwards affected names as stable aliases under `mcp__luban__*` and restores them in responses, so the client only sees its own names. Official tools, tools that already use MCP names, and server tools remain unchanged, making this a no-op for the genuine official client. Costs: one extra string replacement pass over responses, and adding or removing tools mid-session recomputes aliases and invalidates the upstream prompt cache once.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="strip_extra_fields"
          label={t('剥除多余字段', 'Strip extra fields')}
          summary={t(
            '删掉官方客户端从不发送的请求字段，只删语义等价于缺省值的那些。',
            'Remove request fields the official client never sends, limited to those equivalent to their defaults.',
          )}
          description={
            <>
              {t(
                '官方客户端的对话请求字段是固定的一套，多出来的字段就是一处稳定特征，可能导致请求被判为第三方应用而改扣超额用量。开启后，luban 会删掉两项：一是语义等于默认值的 tool_choice（客户端强制指定工具或关闭并行调用时保持不变）；二是 thinking 里的 display 字段。代价：删掉 display 后上游不再返回思考摘要，客户端的「思考过程」会是空的，但功能本身不受影响。真实的官方客户端本来就不发送这两项，开启对它没有任何影响。',
                'The official client sends a fixed set of fields on conversation requests; anything extra is a stable tell and can get the request classified as a third-party app, drawing from extra usage instead of plan limits. When enabled, luban removes two things: a tool_choice whose meaning equals the default (a forced tool choice or disabled parallel calls is left alone), and the display field inside thinking. Cost: without display the upstream no longer returns reasoning summaries, so the client shows an empty thinking section — functionality is otherwise unaffected. The real official client never sends either field, so enabling this is a no-op for it.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="inject_thinking"
          label={t('注入 Thinking', 'Inject thinking')}
          summary={t(
            '在模拟路径下自动补上 thinking 和 context_management，与官方形态一致；同时强制 temperature=1。',
            'Inject thinking and context_management in simulation mode to match the official shape; also forces temperature=1.',
          )}
          description={
            <>
              {t(
                '官方客户端的对话请求始终带 thinking 字段，缺少它可能被上游判为第三方应用。开启后，在模拟路径下，客户端未发送 thinking 时自动补上 {type:"enabled", budget_tokens: max_tokens-1}（max_tokens < 1024 的探测级请求不补），并随之自动补上 context_management。另外，thinking 开启时上游要求 temperature 必须为 1，客户端若设置了其他值会被自动剥掉。代价：注入 thinking 会改变模型行为（输出可能更长，thinking token 按输出计费）。不想要这些副作用可以关闭，代价是模拟形态少一个与官方对齐的信号。',
                'The official client always includes a thinking field on conversation requests; omitting it may cause the upstream to classify the request as third-party. When enabled, if the client did not send thinking, luban injects {type:"enabled", budget_tokens: max_tokens-1} in simulation mode (skipped for probe-level requests with max_tokens < 1024), and context_management is added automatically. Since upstream requires temperature=1 when thinking is on, any other value the client set is stripped. Cost: injected thinking changes model behaviour (outputs may be longer, thinking tokens are billed as output). Turn it off to avoid these side effects, at the cost of one fewer signal aligning with the official shape.',
              )}
            </>
          }
        />
      </SettingsGroup>

      <SettingsGroup icon={DatabaseIcon} title={t('系统提示词', 'System prompt')}>
        <ForwardingToggle
          k="system_shape"
          label={t('分块与缓存形态', 'Block shape & caching')}
          summary={t(
            '按官方客户端对齐系统提示词的分块与缓存断点；同时把块数封顶在 4 块。',
            'Align system prompt blocks and cache breakpoints with the official client, and cap the block count at 4.',
          )}
          description={
            <>
              {t(
                '只调整分块，不改变提示词文本（缓存时长由「缓存时长对齐 1h」单独控制）。这不只是缓存优化：官方客户端的系统提示词始终是 4 块，超出 4 块会被上游判为第三方应用，改从超额用量（extra usage）而不是订阅用量扣费，所以多出来的块会被并回第 4 块。无法识别切分点时原样转发。',
                'Only block boundaries are adjusted; the prompt text is unchanged (cache duration is governed separately by “Match official cache duration”). This is not merely a cache optimization: the official client always sends exactly 4 system blocks, and anything beyond that is treated upstream as a third-party app and billed to extra usage instead of your plan, so surplus blocks are merged back into the fourth. Requests are forwarded unchanged when no split point can be identified.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="cache_scope_global"
          label={t('基座缓存跨账号共享', 'Share base-prompt cache across accounts')}
          summary={t(
            '给官方基座那块标 scope:"global"，让所有账号共用同一份基座缓存。',
            'Mark the official base prompt block with scope:"global" so every account shares one cached copy.',
          )}
          requires={{
            key: 'merge_beta',
            label: t('协议与请求头 · Beta 标记', 'Protocol & request headers · Beta flags'),
          }}
          description={
            <>
              {t(
                '基座是按模型族固定的官方提示词，全网只有一份；标记后各账号命中同一份缓存，省去重复写入。官方客户端总是把 scope 和 ttl:1h 一起发送，所以本项与「缓存时长对齐 1h」同时开启才是官方形态；只关掉其中一项，发出的就是官方不会产生的组合。该标记需要上游的 prompt-caching-scope beta，因此依赖「Beta 标记」开关。',
                'The base prompt is a fixed official block per model family — identical everywhere — so marking it lets all accounts hit one cached copy instead of each paying its own cache write. The official client always sends scope together with ttl:1h, so this and “Match official cache duration” form the official shape only when both are on; turning off just one emits a combination the official client never produces. The marker requires the upstream prompt-caching-scope beta, hence the dependency on “Beta flags”.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="cache_ttl_1h"
          label={t('缓存时长对齐 1h', 'Match official cache duration')}
          summary={t(
            '给缓存断点写 ttl:"1h"，与官方一致；关闭则沿用客户端自己传的时长。',
            'Write ttl:"1h" on cache breakpoints to match the official client; when disabled, keep whatever duration the client sent.',
          )}
          requires={{
            key: 'merge_beta',
            label: t('协议与请求头 · Beta 标记', 'Protocol & request headers · Beta flags'),
          }}
          description={
            <>
              {t(
                '官方订阅客户端的 3 个缓存断点都带 ttl:"1h"，而 API key 模式发送的是不带时长的裸断点。这正是两种模式之间的真实差别之一，不写就等于每条请求都留下一处固定差异。代价：1h 缓存的写入单价是默认 5 分钟缓存的 2 倍。是省是亏取决于使用节奏：长会话里 1h 往往更省（5 分钟内没有接着对话，下一轮就要按写入价把整段前缀重写一遍），零散的一次性请求则纯属多付。关闭后 luban 不改动任何字节，客户端传什么时长就用什么。客户端自己写了时长的，任何情况下都原样发出、不覆盖。该字段需要上游的 extended-cache-ttl beta，因此依赖「Beta 标记」开关。',
                'All three cache breakpoints from the official subscription client carry ttl:"1h", whereas API-key mode sends bare breakpoints with no duration. This is one of the real differences between the two modes, so omitting it leaves a fixed discrepancy on every request. Cost: a 1h cache write is priced at twice the default 5-minute write. Whether that saves or costs money depends on your usage rhythm: in long sessions 1h usually saves (if you do not reply within five minutes, the next turn rewrites the whole prefix at write price), while scattered one-off requests simply pay more. When disabled, luban changes nothing and whatever duration the client sent is used as-is. A duration written by the client itself is always forwarded untouched. The field requires the upstream extended-cache-ttl beta, hence the dependency on “Beta flags”.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="eager_tool_streaming"
          label={t('工具声明对齐 eager 流式', 'Match official eager tool streaming')}
          summary={t(
            '给工具声明补 eager_input_streaming:true，只补抓包证实过的版本、模型与用途组合。',
            'Add eager_input_streaming:true to tool declarations, only for version, model and purpose combinations confirmed by captures.',
          )}
          requires={{
            key: 'merge_beta',
            label: t('协议与请求头 · Beta 标记', 'Protocol & request headers · Beta flags'),
          }}
          description={
            <>
              {t(
                '官方订阅客户端主线程请求里的每个内建工具都带 eager_input_streaming:true，API key 模式一个都不带，这是两种模式之间在每个工具上重复出现的一处固定差异。只对抓包证实过的组合补齐：2.1.258 的四个模型族、2.1.260 的 opus、2.1.270 的 sonnet 会补；2.1.260 的 fable 已证实不带，不补；没有样本的组合不做猜测。真实的 Claude Code 请求按客户端自报的版本、模型与请求用途判断；模拟请求按实际出站的模拟 profile 判断。客户端自己写了这个字段的（无论 true 还是 false）不覆盖；MCP 工具、延迟加载占位与服务端工具没有样本，保持不变。该字段与上游的 advanced-tool-use beta 同时出现，因此依赖「Beta 标记」开关。收益是缩小声明差异，对封号率的影响幅度尚未测量。',
                'Every built-in tool in a main-thread request from the official subscription client carries eager_input_streaming:true, while API-key mode sends none: a fixed per-tool difference between the two modes. The fill only covers combinations confirmed by captures: all four model families on 2.1.258, opus on 2.1.260 and sonnet on 2.1.270 are filled; fable on 2.1.260 is confirmed absent and left alone; combinations without a sample are not guessed. Real Claude Code requests are judged by the client’s reported version, model and request purpose; emulated requests by the emulated profile actually sent upstream. A value the client wrote itself (true or false) is never overwritten; MCP tools, deferred-loading placeholders and server tools have no samples and are left untouched. The field appears together with the upstream advanced-tool-use beta, hence the dependency on “Beta flags”. The benefit is a smaller declaration gap; the effect on ban rates has not been measured.',
              )}
            </>
          }
        />
      </SettingsGroup>

      <SettingsGroup icon={TerminalIcon} title={t('非官方客户端', 'Third-party clients')}>
        <ForwardingToggle
          k="simulate_cc"
          label={t('模拟 Claude Code', 'Emulate Claude Code')}
          summary={t(
            '让 SDK 和第三方客户端按 Claude Code 请求形态转发。',
            'Forward SDK and third-party client requests in the Claude Code request format.',
          )}
          requires={{
            key: 'merge_beta',
            label: t('协议与请求头 · Beta 标记', 'Protocol & request headers · Beta flags'),
          }}
          description={
            <>
              {t(
                '仅改写非 Claude Code 请求。开启后会增加系统提示词和客户端请求头，可能提高 token 成本并改变输出风格。此类请求通常没有设备身份，需先关闭「设备身份校验」。官方客户端自己发出的两种不带基座提示词的请求原样放行：桌面端的缓存预热，以及 WebSearch 工具另外发出的搜索子调用（一条用户消息，只带 web_search 这一个服务端工具且强制调用，系统提示词只有一句搜索助手说明）。此前后者会被重建成主线程请求，结果同一台机器在几秒内以另一个版本、另一台设备、另一条会话的身份出现，发出一条搜索。',
                'Only non-Claude Code requests are rewritten. Enabling this adds a system prompt and client request headers, which may increase token costs and change the output style. These requests usually have no device identity, so disable “Device identity checks” first. Two requests the official client itself sends without the base prompt are passed through unchanged: the desktop app’s cache warm-up, and the separate search sub-call made by the WebSearch tool (one user message, a single forced web_search server tool, and a one-line search-assistant system prompt). Previously the latter was rebuilt into a main-thread request, so the same machine showed up seconds later as another version, another device and another session sending a search.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="simulate_full_system"
          label={t('补齐官方 system 第四块', 'Fill the official fourth system block')}
          summary={t(
            '模拟请求在基座之后再补上官方的 harness 提示词（约 6,700 字节）；客户端自己的 system 仍单独占最后一块，但回答会受这段官方提示词影响而偏离。',
            'Emulated requests get the official harness prompt, roughly 6.7 KB, after the base block. The client’s own system prompt still occupies the last block on its own, but the official prompt pulls the model’s answers towards its own style.',
          )}
          requires={{
            key: 'simulate_cc',
            label: t('非官方客户端 · 模拟 Claude Code', 'Third-party clients · Emulate Claude Code'),
          }}
          description={
            <>
              {t(
                '官方 2.1.280 主线程的 system 由四块组成：billing、身份句、基座，以及基座之后约 7,000 字节的「其余」段（会话指引、记忆说明、模型清单、上下文管理）。末尾断点就标在这一块上，四个模型族用的是同一份。开启后，按 2.1.280 抓包的原文填充这一块：其中唯一随机器变化的是记忆目录，优先使用客户端自己写的工作目录，客户端没写时才按账号加设备派生一个固定的假路径；模板去掉了依赖 ToolSearch 的那段指令，因为模拟不注入 ToolSearch。客户端自己的 system 单独占最后一块（超过 1,900 字节时移入首条消息，原位置留一行占位；约合 633 个汉字），因此开启时出站是五块，比官方多一块。注意：回答也会受影响。这一块会提示模型自己是 Claude Code，实测客户端 system 要求「只回复某个标记」时，模型回复了那个标记，但后面还会再加一句自我介绍。代价：每条模拟请求多出约 1,700 token 的前缀；它带 1h 断点、在同一会话内稳定，基本按缓存读价计费。关闭即不发送这一块：system 由 billing、身份句、基座加客户端那块组成，恰好四块，客户端那块落在官方「其余」段的位置上，也不注入任何环境信息。',
                'The official 2.1.280 main-thread system prompt has four blocks: billing, identity, base, and a roughly 7 KB “rest” section after the base (session guidance, memory instructions, model list, context management), which carries the final cache breakpoint; all four model families share the same text. When enabled, this block is filled verbatim from the 2.1.280 captures: the only machine-specific part, the memory directory, follows the working directory the client wrote itself, falling back to a fixed fake path derived per account and device only when the client wrote none; the paragraph that depends on ToolSearch is removed, since emulation does not inject ToolSearch. The client’s own system prompt occupies the last block on its own (moved into the first message above 1,900 bytes, roughly 633 CJK characters, leaving a one-line placeholder), so with this on the request has five blocks, one more than the official shape. Note: answers are affected too. This block tells the model it is Claude Code; in testing, when the client’s system prompt asked for a single marker only, the model returned the marker but still added a line introducing itself. Cost: roughly 1,700 extra prefix tokens per emulated request, behind a 1h breakpoint and stable within a session, so mostly billed at cache-read price. Disable to drop this block: the system prompt is then billing, identity and base plus the client’s block, exactly four blocks, with the client’s block in the official “rest” position, and no environment details are injected.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="fill_absent_tools"
          label={t('不带工具的请求也补官方工具', 'Add official tools to tool-less requests')}
          summary={t(
            '客户端请求完全没带 tools 时，也补上官方主线程的 14 个工具；模型可能会调用它们，而这类客户端通常处理不了。',
            'When a request carries no tools at all, add the 14 official main-thread tools anyway; the model may call them, and such clients usually cannot handle that.',
          )}
          requires={{
            key: 'simulate_cc',
            label: t('非官方客户端 · 模拟 Claude Code', 'Third-party clients · Emulate Claude Code'),
          }}
          description={
            <>
              {t(
                '模拟请求一律伪装成官方主线程请求，而官方主线程的每条请求都带工具（2.1.280 抓包中始终是 19 或 20 个），「主线程的 beta 与 system、零个工具」是官方不会产生的组合。开启后，不带工具的客户端请求（没有 tools、tools 为 null 或空数组）也会补上 Agent、Bash、Read 等 14 个官方工具；客户端请求的 tool_choice 要求必须调用工具（any 或指定工具）时不补。自己带了工具的请求，无论本开关开启还是关闭，都照常补齐缺少的工具。风险：不带工具的大多是纯聊天客户端，没有工具循环；模型一旦调用注入的工具，客户端收到的是一个它处理不了的 tool_use，这次回答就失败了。补了工具的请求会在流水的改写列标注 tools_filled，模型确实调用了注入工具时再标注 injected_tool_called，两者相比即为命中率。成本：工具声明约 80,000 字节（约 20,000 token），每个新会话的首轮按写入价付一次，之后按缓存读价计费。关闭后这类请求一个工具都不注入，空数组也原样发出。',
                'Emulated requests are always shaped as official main-thread requests, and every official main-thread request carries tools (always 19 or 20 in the 2.1.280 captures); main-thread betas and system prompt with zero tools is a combination the official client never produces. When enabled, requests without tools (no tools field, tools set to null, or an empty array) also get the 14 official tools such as Agent, Bash and Read, unless the request’s tool_choice demands a tool call (any or a named tool). Requests that carry their own tools have missing ones added whether this is on or off. Risk: tool-less clients are usually plain chat clients without a tool loop, so if the model calls an injected tool the client receives a tool_use it cannot handle and that answer is broken. Requests that got the tools are tagged tools_filled in the rewrites column of the request log, and injected_tool_called is added when the model actually called one, so comparing the two gives the hit rate. Cost: the tool declarations are about 80 KB (roughly 20K tokens), paid at write price on the first turn of each new session and at cache-read price afterwards. Disable to inject no tools into such requests; an empty array is sent as is.',
              )}
            </>
          }
        />
      </SettingsGroup>

      <SettingsGroup icon={SlidersHorizontalIcon} title={t('请求兼容性', 'Request compatibility')}>
        <PolicySelect
          label={t('Assistant Prefill', 'Assistant Prefill')}
          summary={t(
            '4.6+ 模型不支持 assistant message prefill（末尾 assistant 轮）。',
            'Claude 4.6+ models do not support assistant message prefill (trailing assistant turns).',
          )}
          value={settingsQuery.data.prefill_policy as PolicyValue}
          settingKey="prefill"
        />
        <PolicySelect
          label={t('Sampling 参数', 'Sampling parameters')}
          summary={t(
            '4.7+ 模型不支持 temperature / top_p / top_k。',
            'Claude 4.7+ models do not support temperature / top_p / top_k.',
          )}
          value={settingsQuery.data.sampling_policy as PolicyValue}
          settingKey="sampling"
        />
        <ForwardingToggle
          k="flatten_tool_schemas"
          label={t('Schema 展平', 'Schema flattening')}
          summary={t(
            '展平 tool input_schema 顶层的 allOf / oneOf / anyOf。',
            'Flatten top-level allOf / oneOf / anyOf in tool input_schema.',
          )}
          description={
            <>
              {t(
                '上游 API 不支持 JSON Schema 的 allOf / oneOf / anyOf 组合关键字出现在 input_schema 顶层，会直接返回 400。开启后自动将它们合并为一个普通 object schema 再转发。',
                'The upstream API rejects allOf / oneOf / anyOf at the top level of tool input_schema with a 400 error. When enabled, these are automatically merged into a plain object schema before forwarding.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="strip_empty_text"
          label={t('空 text 块剥除', 'Strip empty text blocks')}
          summary={t(
            '剥除 messages 中的空 text 内容块。',
            'Strip empty text content blocks from messages.',
          )}
          description={
            <>
              {t(
                '上游要求 text 内容块的 text 字段非空，部分第三方客户端会发送 {"type":"text","text":""} 这样的空块，导致 400。开启后自动剥除空 text 块（若消息只含空 text 块，则保留原样）。',
                'The upstream API requires text content blocks to be non-empty. Some third-party clients send {"type":"text","text":""}, which causes a 400. When enabled, empty text blocks are automatically stripped (if a message contains only empty text blocks, it is left unchanged).',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="reject_openai_shape"
          label={t('拒绝 OpenAI 转换残留', 'Reject OpenAI-format residue')}
          summary={t(
            '带 OpenAI 格式转换痕迹的请求本地直接 400，不修补、不转发。',
            'Requests carrying traces of OpenAI-format conversion are rejected locally with 400, never repaired or forwarded.',
          )}
          description={
            <>
              {t(
                '经 litellm、one-api、claude-code-router 等工具从 OpenAI 格式转换过来的请求，到达这里时已是 Anthropic 形态，只能靠残留特征识别：messages 里的 role:"system" / "tool"、消息上的 name / tool_calls、以 call_ 为前缀的工具调用 ID、字符串形态或 type:"function" 的 tool_choice、OpenAI function 形态的 tools、n / stop / user / response_format 等 OpenAI 专属顶层字段，以及 image_url 等 OpenAI 内容块。命中任意一项即在本地返回 400，错误消息会指出位置与 Anthropic 的对应写法。关闭后退回下方「System Role 提升」等修补路径。不影响模拟路径：模拟路径只接管本来就是 Anthropic 形态的非 CC 请求。',
                'Requests converted from the OpenAI format by litellm, one-api, claude-code-router and the like arrive already in Anthropic shape; the only way to tell is the residue they leave: role:"system" / "tool" in messages, name / tool_calls on a message, tool call IDs prefixed call_, a string or type:"function" tool_choice, tools in the OpenAI function shape, OpenAI-only top-level fields such as n / stop / user / response_format, and OpenAI content blocks such as image_url. Any hit is rejected locally with 400 and a message naming the location and the Anthropic equivalent. Turn it off to fall back to the repair paths below (“System role hoisting” and others). The simulation path is unaffected: it only takes over non-CC requests that are already in Anthropic shape.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="fable_refusal_fallback"
          label={t('Fable 拒答换模型重跑', 'Fable refusal fallback')}
          summary={t(
            'fable 主线程请求带上官方的服务端 fallback：安全分类器拒答时，由上游在同一次调用里换 opus-5 重跑。形态与官方 2.1.260 逐字相同，默认关闭。',
            'Fable main-thread requests carry the official server-side fallback: when the safety classifier refuses, upstream reruns the same call on opus-5. Byte-for-byte the official 2.1.260 shape; off by default.',
          )}
          description={
            <>
              {t(
                'Fable 5.1 / Fable 5 带安全分类器，命中时（多为 cyber 类，良性的安全工作也会被误伤）返回 200 加 stop_reason refusal，正文为空。官方 Claude Code 2.1.260 在 fable 上自带 fallbacks: [{"model":"claude-opus-5"}] 和 server-side-fallback beta，拒答后由上游换 Opus 5 重跑，用户看不到拒答。开启后，luban 为 fable 主线程请求补上与官方逐字相同的这一字段，出站请求头一并带上 server-side-fallback-2026-06-01。这有抓包依据，补上后更接近 2.1.260 的官方形态。但它替用户做了决定：拒答后由 Opus 5 作答、按 Opus 计价、同一对话约一小时内固定在 Opus 上，用户也看不到拒答本身，所以默认关闭，由用户自行开启。关闭时，模拟出的 fable 请求比 2.1.260 少这一个字段，拒答直接返回、不换模型重跑；请求头里的 beta 仍按版本补上，「有 beta 没字段」正是 2.1.260 之前的官方形态；客户端自带的 fallbacks 照样保留。客户端自己带了数组形态的 fallbacks 时不改动；helper、标题、安全分类、额度探测这些辅助请求官方都不发送该字段，也不补。上游以 400 拒绝 fallback 目标时，剥掉该字段重发一次并记入「从上游学到的规则」，之后该模型不再补。落到 fallback 的回复按实际作答的模型计价，同一对话约一小时内会固定在 fallback 模型上。输出前就被拒的请求上游不计费，流水里花费记为 0。opus-5 的自定 fallback 链是另一个开关，见下一条。Sonnet 5 与 Opus 4.7/4.8 同样带网络安全分类器，同样会以 200 加 stop_reason refusal 拒答；luban 只解析并记录它们的拒答，不为它们补 fallbacks。官方客户端在这些模型上不发送该字段，这是有意的产品限制，不是遗漏。',
                'Fable 5.1 / Fable 5 run safety classifiers; a hit (mostly the cyber category, and benign security work gets caught too) returns 200 with stop_reason refusal and empty content. Official Claude Code 2.1.260 sends fallbacks: [{"model":"claude-opus-5"}] plus the server-side-fallback beta on fable, so upstream reruns a refused call on Opus 5 and the user never sees the refusal. When enabled, luban adds that exact field to fable main-thread requests, with server-side-fallback-2026-06-01 in the outbound header; this is backed by a capture, so adding it brings the request closer to the 2.1.260 official shape. But it also decides for the user that a refused request is answered by Opus 5, billed at Opus rates, with the conversation stuck to Opus for about an hour, and the user never sees the refusal itself, so it is off by default and left for the user to switch on. Turned off, simulated fable requests lack that one field and the refusal is returned as is, without a rerun; the beta header is still added per version, and “beta present, field absent” is exactly the official shape before 2.1.260. A client-supplied fallbacks field is kept as is. A client-supplied array form is left alone; helper / title / classifier / quota-probe requests are not touched, as the official client never sends the field there. If upstream rejects the fallback target with a 400, the field is stripped and the request resent once, and the rule lands under “Rules learned from upstream” so the field is not added for that model again. A reply served by a fallback is priced at the model that actually served it, and the conversation sticks to the fallback model for about an hour. Requests refused before any output are not billed upstream, so their cost is recorded as 0. The luban-defined opus-5 fallback chain is a separate switch, described next. Sonnet 5 and Opus 4.7/4.8 also run cybersecurity classifiers and refuse with 200 plus stop_reason refusal; luban parses and records those refusals but does not add fallbacks for them, because the official client never sends the field on those models. That is a deliberate product limit, not an omission.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="opus_refusal_fallback"
          label={t('Opus 拒答换模型重跑（实验）', 'Opus refusal fallback (experimental)')}
          summary={t(
            'opus-5 主线程请求带上 luban 自定的 fallback 链：拒答时上游先回退到 4.8，再回退到 4.6。官方 opus 客户端不发送这个字段，默认关闭。',
            'Opus-5 main-thread requests carry a luban-defined fallback chain: on refusal upstream falls to 4.8, then 4.6. The official opus client never sends this field; off by default.',
          )}
          description={
            <>
              {t(
                '官方 Claude Code 2.1.260 的 opus 客户端只带 server-side-fallback beta，不发送 fallbacks 字段，「有 beta 没字段」就是官方形态。开启后，luban 为 opus-5 主线程请求补上自定的 fallbacks: [{"model":"claude-opus-4-8"},{"model":"claude-opus-4-6"}]（官方为 cyber 类拒答推荐的 fallback 正是 4.8）。这是官方客户端从不产生的请求形态：封号复盘中查不出它导致了 account_on_hold，但作为风控层面的自证风险，它只应作为独立的实验开关，默认关闭，保持官方的 opus 请求形态。其余行为同上一条：只补主线程；客户端自带的不改动；上游以 400 拒绝目标后学成规则、不再补；落到 fallback 的回复按实际作答的模型计价。若日后要重新启用，更稳妥的做法是发送字符串 "default"，让上游按当前推荐的模型路由；或先读取 /v1/models 的 allowed_fallback_models，所有目标都获允许时再发送自定链。',
                'The official Claude Code 2.1.260 opus client sends only the server-side-fallback beta and no fallbacks field; “beta present, field absent” is the official shape. When enabled, luban adds a self-defined fallbacks: [{"model":"claude-opus-4-8"},{"model":"claude-opus-4-6"}] to opus-5 main-thread requests (4.8 is the fallback officially recommended for cyber refusals). That is a request shape the official client never produces: the ban post-mortem does not show it caused account_on_hold, but as a fingerprint risk it belongs behind a separate experimental switch, off by default, keeping the official opus request shape. Everything else matches the switch above: main thread only, client-supplied arrays left alone, a 400 on a fallback target is learned and the field is not added for that model again, and replies served by a fallback are priced at the model that answered. If you re-enable it later, the safer options are sending the string "default" so upstream routes to its current recommended model, or reading allowed_fallback_models from /v1/models first and only sending the custom chain when every target is allowed.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="reject_probes"
          label={t('拒绝探针请求', 'Reject probe requests')}
          summary={t(
            '下游中转用账号做探活、测活的请求，由本地直接回复一条最小的正常响应（200 加一句「OK」，响应头标注 x-luban-local: probe_reply），不发往上游，不限 UA。本开关只管形态判据；从响应学到的两类规则各有自己的开关，见下面两条。',
            'Health-check / channel-test requests that downstream relays send to probe the account are answered locally with a minimal 200 (a one-word "OK", marked with the x-luban-local: probe_reply header) and never reach upstream, regardless of UA. Shape signatures only; the two rule kinds learned from responses have their own switches below.',
          )}
          description={
            <>
              {t(
                '探活脚本发送的是不带 tools 的单句小请求，每条在上游看来都是「一台设备开一个一次性会话，只问一句话」，这是封号复盘里最显眼的判据。本开关按三条强特征判断，命中任意一条即拒绝，不限 UA：一是没有 tools、只有一条消息、max_tokens 在 2 到 16 之间（不要求带 system：官方没有这种形态，而 Go-http-client 的探活通常不带 system）；二是带 system、没有 tools、只有一条消息、不属于官方那两种无 tools 形态，且来自一台从未见过的设备；三是 system 里的 CC 身份句出现在不止一块中。0.3.99 之前只判断自报 claude-cli UA 的请求，Go-http-client 的探活反而走了模拟路径，被伪装成官方形态，每 3 到 6 秒一批发往上游。「没有 tools」按取值判断：缺失、null、[] 都算没有，加空字段也绕不过。官方 Claude Code 的三种无 tools 请求（cache 预热、haiku Helper、安全分类）按取值逐项比对，都不在判据之内。身份字段写错的请求（device_id 不是 64 位 hex、session_id 不是 UUID，如 channel-test）不算探针，不在这里拒绝，而是不被当作官方客户端，走模拟路径重建身份。只看形态，单条即判定，不做计数。边界：逐字照抄官方 haiku Helper 全部取值的探针，在形态上就是官方请求，这里无法区分。不带 metadata.user_id 的探针也在这里应答：这道判据排在设备身份校验之前，否则设备身份校验先返回的 403 在下游看来与「账号被封」一样，整个 key 同样会被摘除。命中后返回给客户端的是一条最小的正常回复：200、一个文本块「OK」、stop_reason end_turn；客户端请求流式时按 SSE 返回，model 原样返回客户端声明的那个，正文里没有任何被拦截过的痕迹。0.3.101 之前返回的是 403 permission_error，而探活恰恰是下游中转判断「这个账号还能不能用」的那条请求：luban 的 403 在下游看来与「账号被封」一样，整个 key 被摘除，真实流量随之中断，而这条请求根本没有到达上游，账号完全没有问题。这类由 luban 本地应答、未到达上游的请求可从三处识别：响应头 x-luban-local: probe_reply 与 x-luban-probe-kind（命中的是哪条判据）；Message id 以 msg_luban 开头（流式响应在 message_start 里同样带着）；请求查询中带 probe_reply 标签且花费记为 0。这些标记都在正文语义之外，探活读到的仍是一条正常回复，而在抓包与下游面板里可以认出这条请求，不会把它当成上游真正回答过的一次。与其他开关的关系：本开关只管上面三条形态判据；从响应学到的「已拒答的提示词」与「零输出请求类」两类规则各有自己的开关（见下面两条）。0.3.93 之前三者共用这一个键，关闭探针拒绝会把学到的规则一并放行。',
                "Probe scripts send a single tool-less one-liner; upstream sees each one as \"a device opening a throwaway session to ask one question\", the most conspicuous pattern in the ban post-mortem. This switch checks three strong signatures, any one of which matches, regardless of UA: no tools, a single message and max_tokens between 2 and 16 (no system prompt required: official Claude Code has no such shape, and Go-http-client health checks usually carry none); a system prompt with no tools, a single message, not one of the two official tool-less shapes, and a never-seen device; the Claude Code identity sentence in more than one system block. Before 0.3.99 only requests claiming a claude-cli UA were judged, so Go-http-client health checks went through the simulation path instead and were sent upstream dressed as official traffic, in batches every 3 to 6 seconds. \"No tools\" is judged by value: missing, null and [] all count, so padding with empty fields does not help. The three tool-less shapes official Claude Code does send (cache prewarm, the haiku helper, the security classifier) are matched value by value and fall outside the signatures. Malformed identities (a device_id that is not 64-hex, a session_id that is not a UUID, e.g. channel-test) are not probes and are not rejected here; they are simply not treated as an official client and go through the simulation path, which rebuilds the identity. Shape only, decided per request, no counting. Limit: a probe that copies every value of the official haiku helper verbatim is, by shape, an official request and cannot be told apart here. Probes without metadata.user_id are answered here as well: this check runs before the device-identity check, which would otherwise return a 403 that a downstream relay cannot tell apart from a banned account, so the whole key would be pulled anyway. A hit is answered locally with a minimal normal reply: 200, one text block \"OK\", stop_reason end_turn, streamed as SSE if the client asked for a stream, echoing the model the client declared, with nothing in the body hinting it was intercepted. Before 0.3.101 the answer was a 403 permission_error, but a health check is exactly how a downstream relay decides whether the account still works: luban's 403 looks the same to it as a banned account, so the whole key gets pulled and real traffic stops with it, even though the request never reached upstream and the account is fine. Requests answered locally by luban without reaching upstream can be identified in three places: the response headers x-luban-local: probe_reply and x-luban-probe-kind (which signature matched), a message id starting with msg_luban (also carried in message_start when streaming), and the probe_reply tag with zero cost in request lookup. These markers sit outside the body's semantics, so a health check still reads a normal reply, while packet captures and downstream panels can recognize the request and never mistake it for a real upstream answer. Relation to other switches: this switch governs only the three shape signatures above; the two rule kinds learned from responses (refused prompts, empty-reply request classes) have their own switches below. Before 0.3.93 all three shared this one key, so turning probe rejection off also let the learned rules through.",
              )}
            </>
          }
        />
        <ForwardingToggle
          k="reject_probes_strict"
          label={t('探针拒绝：严格模式', 'Probe rejection: strict mode')}
          summary={t(
            '在上面三条判据之外再增加两条：带 tools 但 max_tokens 为 2 到 16 的请求也算 ping；无 system、无 tools、只有一条不超过 32 字节（中文约 10 个字）用户消息的请求也算探活。会误伤真人的第一句「你好」，默认关闭。',
            'Two more signatures on top of the three above: max_tokens 2 to 16 counts as a ping even with tools; a single user message of 32 bytes or fewer (about ten CJK characters) with no system prompt and no tools counts as a probe. It also catches a real user\'s first "hello", so it is off by default.',
          )}
          description={
            <>
              {t(
                '封号复盘（ban-37 / 38 / 42）中，Go-http-client 的探活有四种形态，默认判据只拦住「无 tools、单条消息、max_tokens 2 到 16」这一种。另外三种被放行后走模拟路径，被伪装成带基座的官方形态发往上游，每个账号上仍有一两百条：4 个 tools 配 max_tokens 16；无 system、只问一句「hi」，max_tokens 为 50、1024 或 32000。本开关增加两条判据来覆盖它们。第一条有硬依据：16 个 token 装不下一次 tool_use 调用，带着工具却只给 16 个 token，只可能是测活。第二条有代价：真人用纯聊天客户端经中转站发出的第一句「你好」同样是无 system、无 tools、一条短消息，会收到一条 200 的「OK」（与探活收到的是同一条最小回复），下一句正常长度的消息照常通过。带附件、多轮、带 system 的请求，以及 max_tokens 为 1 的预热，都不算短开场。需与「拒绝探针请求」一起开启才生效。',
                'The ban post-mortems (ban-37 / 38 / 42) show four shapes of Go-http-client health checks; the default signatures catch only "no tools, single message, max_tokens 2 to 16". The other three were simulated into an official-looking body with a base prompt and sent upstream, still one or two hundred per account: 4 tools with max_tokens 16; no system prompt, a single "hi", and max_tokens 50, 1024 or 32000. This switch adds two signatures that cover them. The first rests on hard evidence: 16 tokens cannot hold a tool_use call, so tools plus a 16-token budget can only be a health check. The second has a cost: a real user\'s first "hello" sent through a relay from a plain chat client is also a single short message with no system prompt and no tools, so it gets the same minimal 200 \"OK\" a probe would, and the next normal-length message goes through. Attachments, multi-turn conversations, a system prompt, or a max_tokens of 1 (cache warm-up) never count as a short opener. Takes effect only together with "Reject probe requests".',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="reject_refusals"
          label={t('拒绝已拒答的提示词', 'Reject refused prompts')}
          summary={t(
            '上游分类器拒答过的提示词，逐字相同地重发时不再发往上游，由本地原样回放上游那次的响应（200 加同一段 stop_reason refusal 响应体）；无法识别会话的客户端（既不带会话 ID 也不带 device_id 的中转）另按「模型 + system」学习，拒答至少 3 条且占该应用请求 30% 以上才学成规则，之后同一应用的请求一律回放；出站带 fallbacks 的请求不拦截，交给上游换模型重跑。',
            'A prompt the upstream classifier has already refused is not forwarded when resent verbatim; luban replays upstream\'s own refusal (200 with the same stop_reason refusal body). Session-less clients (relays sending neither a session ID nor a device_id) are additionally learned per model + system prompt once at least 3 refusals make up 30% or more of that app\'s requests, and every further request of that app is replayed. Requests that go out with fallbacks are not blocked, so upstream can rerun them on another model.',
          )}
          description={
            <>
              {t(
                '上游拒答（200 加 stop_reason refusal）不按形态学习，只按那条提示词学习：同一模型下，system + messages + tools + tool_choice 逐字相同才算同一条；改一个字、换一个模型或换一套工具集都不会命中，其他请求一律不拦截。命中时返回给客户端的不是 luban 自己构造的 403，而是学习规则时上游那次响应的原样响应体（200、同一段 stop_reason refusal 与 stop_details），并按本次客户端请求的形态返回：学习时是 SSE、本次也要求流式，就逐字节原样发出；形态不一致时才在 SSE 与整段 JSON 之间转换。客户端看到的与上游亲自再拒一次完全相同。只学习输出前就被拒的请求（正文为空）：流式输出到一半才被截断的，没有确定性的判决可以回放，照常发往上游。并且只学习分类器的判决（stop_details 带 category，如 cyber）。按官方说法，category 为空的可能是模型自己拒答，也可能是不带类别的分类器判决；带 recommended_model 的表示 fallback 没有执行成功。这两种情况重发都可能得到回答，因此只记入流水、不学习。出站会带 fallbacks 的请求（客户端自带，或由上面两个开关让 luban 补上）不在这里拦截：带 fallback 的请求被拒答后，上游会换模型重跑，这正是拒答应走的路径。此前没有考虑这一点，fallback 关闭时学到的拒答规则，之后即使打开 fallback，相应请求也永远到不了上游。规则记录在「从上游学到的规则」中，7 天后到期（进程内每小时按数据库重建一次，不再依赖重启才过期），也可手动删除。规则不区分账号，对整个调度池生效：分类器对同一条提示词的判决是确定性的，换一个账号重发结果也一样。无法识别会话的客户端另有一档「按应用学习」：封号复盘（ban-37 / 38 / 42）中，Go-http-client 的那场 reasoning_extraction 风暴共 568 条请求，正文各不相同，按提示词学习的规则一条都命中不了，而它只用了 4 种 system。对既不带会话 ID 也不带 device_id 的中转流量来说，system 就代表「哪个应用在说话」。这类客户端按「模型 + system 哈希」统计到达上游的请求数与被分类器拒答的条数，拒答至少 3 条且占比 30% 以上才学成 app_refusal 规则；之后同一模型、同一份 system 的每条请求都回放最近那条拒答，不论正文。之所以按比例而不是一条就学，是因为同一批中转站上还有固定 18 字节 system、几十到两百多轮的真人 agent 会话，拒答率只有 1% 到 2%，一条就学会让整个应用受牵连 7 天；而风暴应用的拒答率为 35% 到 63%，三五条就能学到。计数只保存在进程内，重启后归零。带会话 ID 或 device_id 的客户端不走这一档：它们的 system 每轮都在变，真人对话偶尔出现一次拒答，不应让整个会话受牵连。这一档同样 7 天到期、可删除、可按种类清空，回放记入流水并标注 app_refusal_replay。',
                "An upstream refusal (200 with stop_reason refusal) is never learned by shape, only by that exact prompt: the same model with system + messages + tools + tool_choice byte-for-byte identical; changing a word, the model, or the tool set misses, and nothing else is ever blocked. On a hit the client does not get a luban-made 403 but the body upstream returned when the rule was learned (200, the same stop_reason refusal and stop_details), in the shape this request asked for: learned as SSE and streaming again, the bytes are replayed verbatim; only a shape mismatch converts between SSE and a single JSON message. The client sees exactly what a fresh upstream refusal looks like. Only refusals issued before any output (empty content) are learned; a refusal that cut a stream mid-way has no deterministic verdict to replay and goes upstream as usual. Only classifier verdicts are learned (stop_details carries a category such as cyber). A refusal with no category may, per the official docs, be the model's own or an uncategorised classifier verdict, and one carrying recommended_model means the fallback could not run; both may succeed on a resend, so they are logged but not learned. Requests that will go out with fallbacks (supplied by the client, or added by luban under the two switches above) are not blocked here: upstream reruns a refused request on another model, which is exactly the path a refusal should take. Previously this was not checked, so a refusal learned while fallbacks were off kept being rejected locally even after fallbacks were turned on. Rules live under “Rules learned from upstream”, expire after 7 days (the in-memory table is rebuilt from the store hourly, so expiry no longer waits for a restart) and can be removed by hand. Rules are not scoped to an account and apply to the whole scheduling pool: the classifier verdict for a given prompt is deterministic, and resending it from another account gets the same answer. Session-less clients get one more tier, learned per app: in the ban post-mortems (ban-37 / 38 / 42) the Go-http-client reasoning_extraction storm sent 568 requests with different bodies every time, so no prompt rule ever matched, yet it used only 4 system prompts; for relay traffic carrying neither a session ID nor a device_id, the system prompt is what identifies the app. For such clients luban counts, per model + system hash, the requests that reached upstream and how many were classifier refusals; once at least 3 refusals make up 30% or more, an app_refusal rule is learned and every further request with that model and system is replayed regardless of its body. Ratio rather than a single hit, because the same relays also carry real agent sessions with a fixed 18-byte system prompt and dozens to hundreds of turns, refused 1 to 2% of the time; a single-hit rule would block that whole app for 7 days, while the storm apps run at 35 to 63% and trip within a handful of requests. Counters live in process only and reset on restart. Clients with a session ID or device_id never enter this tier: their system prompt changes every turn, and one refusal in a real conversation must not block the whole session. Same 7-day expiry, deletable, clearable by kind; replays are logged with the app_refusal_replay tag.",
              )}
            </>
          }
        />
        <ForwardingToggle
          k="reject_empty_replies"
          label={t('拒绝零输出请求类', 'Reject empty-reply request classes')}
          summary={t(
            '某模型对「无 tools 单条消息 + 某个 max_tokens」返回过 200 却零输出之后，同类请求在本地直接返回 403，不限 UA。',
            'Once a model has answered a tool-less single-message request with a given max_tokens with 200 and zero output, that request class is rejected locally with 403, regardless of UA.',
          )}
          description={
            <>
              {t(
                '这条规则从响应中学习，不限 UA：某模型对「无 tools 的单条消息 + 某个 max_tokens」返回过 200 却零输出（有 usage、output_tokens 为 0，即上游收了输入的费用却一个字都没返回）之后，同类请求在本地返回 403；上游当时回复的开头记录在流水的对应记录与「从上游学到的规则」中。带 tools、多轮或换了 max_tokens 的请求不受影响。类别划得很窄，宁可多放行一条。封号复盘中，这样的记录在 13 小时里每 37 秒出现一条，每条在上游看来都是「一台设备只问一句话、什么都没得到」的探活式痕迹。模拟路径重建的是身份，改变不了「问一句、上游一个字不回」这一事实，所以不限 UA。规则 7 天后到期，可手动删除。',
                'This rule is learned from responses, regardless of UA: once a model has answered a tool-less single-message request with a given max_tokens with 200 and zero output tokens (usage present, output_tokens 0: upstream billed the input and returned nothing), that request class is rejected locally with 403; the start of that upstream reply is kept on the usage record and under “Rules learned from upstream”. Requests with tools, multi-turn conversations, or a different max_tokens are unaffected; the class is deliberately narrow, erring on the side of letting requests through. The ban post-mortem had one such record every 37 seconds for 13 hours, each one a probe-like trace of "one device asking one question and getting nothing" on the upstream side; the simulation path rebuilds identity but cannot change that, hence no UA limit. Rules expire after 7 days and can be removed by hand.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="reject_session_conflict"
          label={t('拒绝会话 ID 冲突', 'Reject session ID conflicts')}
          summary={t(
            '请求头与 metadata 里的会话 ID 不一致时，在本地直接返回 400，不替客户端选一个。',
            'When the session ID in the header and in metadata disagree, reject locally with 400 instead of picking one.',
          )}
          description={
            <>
              {t(
                '官方 Claude Code 在 X-Claude-Code-Session-Id 与 metadata.user_id 两处发送的是同一个值，逐字相同。两处给出两个各自合法却不同的 UUID，是官方从不产生的形态；而 luban 以会话 ID 作为会话链（cc_prompt_id / cc_prev_req / diagnostics.previous_message_id）的键，选错一个就会把两条链接到一起，事后再也无法察觉。开启后，这类请求在本地返回 400，错误消息中列出两个值。关闭后退回「取请求头里的值 + 记一条 warn 日志」。只有一处合法时不算冲突：那是客户端只给对了一个，照常取合法的那个。',
                'Official Claude Code sends the same value in X-Claude-Code-Session-Id and in metadata.user_id, byte for byte. Two different but individually valid UUIDs is a shape the official client never produces, and luban keys the session chain (cc_prompt_id / cc_prev_req / diagnostics.previous_message_id) on the session ID; picking the wrong one splices two chains together with no way to notice afterwards. When enabled such requests are rejected locally with 400, naming both values. Turn it off to fall back to using the header value and logging a warning. If only one of the two is a valid UUID it is not a conflict: the client simply got one of them right, and that one is used.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="hoist_system_role"
          label={t('System Role 提升', 'System role hoisting')}
          summary={t(
            '将 messages 里的 role:"system" 消息提升到顶层 system 字段。',
            'Hoist role:"system" messages to the top-level system field.',
          )}
          description={
            <>
              {t(
                '上游 API 不支持 messages 数组里的 role:"system"（会直接返回 400）。litellm 等采用 OpenAI 格式的客户端会把 system 指令放在 messages 里。开启后，自动将这些消息的内容提升到顶层 system 字段，再从 messages 中移除。仅在「拒绝 OpenAI 转换残留」关闭时才有机会生效：那个开关开启时，这类请求在入口就被拒绝了。官方 Claude Code 自己也会在 messages 里合法使用 role:"system"（deferred tools），所以 CC 形态的请求整体跳过提升，以免破坏形态。唯一的例外是空壳 role:"system" 消息，即 content 为空数组、空字符串、null、字段缺失，或整条只有空 text 块：无论本开关开启与否、也无论是否为 CC 形态，一律在出站前丢弃。上游对它始终返回 400（messages.N: system content must contain at least one block），而它没有任何内容块，丢弃不会损失语义。实际运行中遇到这种情况的，是一个 agent-sdk 的 VS Code 扩展发来的正常 CC 请求。',
                'The upstream API does not support role:"system" in the messages array (returns 400). Clients using OpenAI format (e.g. litellm) place system instructions in messages. When enabled, their content is automatically hoisted to the top-level system field and removed from messages. Only takes effect while “Reject OpenAI-format residue” is off: with that on, such requests are rejected at the door. Official Claude Code also uses role:"system" inside messages legitimately (deferred tools), so CC-shaped requests skip hoisting entirely rather than have their shape broken. The one exception is an empty shell: a role:"system" message whose content is an empty array, an empty string, null, missing, or nothing but empty text blocks is dropped before the request goes out — whatever this switch is set to, and whether or not the request is CC-shaped. Upstream always rejects it (messages.N: system content must contain at least one block) and it carries no content to lose. This was found on a real request from an agent-sdk VS Code extension.',
              )}
            </>
          }
        />
      </SettingsGroup>

      <LearnedRejections />

      <SettingsGroup icon={RefreshCwIcon} title={t('限流与错误恢复', 'Rate limits & error recovery')}>
        <ForwardingToggle
          k="rate_limit_retry"
          label={t('429 自动换账号', '429 automatic account switching')}
          summary={t(
            '遇到限流后，冷却受限账号或模型，并换用其他账号重试。',
            'After a rate limit, cool down the affected account or model and retry with another account.',
          )}
          description={
            <>
              {t(
                '账号用量耗尽时冷却整个账号；只有当前模型受限时仅冷却该模型。默认分别冷却 60 / 30 秒，并优先采用上游等待时间。换账号会改绑有设备身份的请求，也可能降低缓存命中率；达到重试上限或没有其他账号时返回',
                'When an account’s usage is exhausted, the entire account is cooled down; when only the current model is limited, only that model is cooled down. The defaults are 60 / 30 seconds respectively, with the upstream wait time taking precedence. Switching accounts rebinds requests that carry a device identity and may also reduce the cache hit rate. When the retry limit is reached or no other account is available, return',
              )}{' '}
              <code className="font-mono tabular-nums">429</code>{t('。', '.')}
            </>
          }
        />
        <RetryMax />
        <QuotaPausePct />
        <ForwardingToggle
          k="thinking_signature_retry"
          label={t('thinking 签名兜底', 'thinking signature fallback')}
          summary={t(
            '账号切换导致历史 thinking 签名失效时，自动降级并重试一次。',
            'When switching accounts invalidates a historical thinking signature, automatically downgrade it and retry once.',
          )}
          description={
            <>
              {t('无法验证的历史', 'Unverifiable historical')}{' '}
              <code className="font-mono">thinking</code>{' '}
              {t(
                '会转为普通文本，并用同一账号重试一次，不会删除原内容。工具续跑仍可能失败；重试会增加一次请求成本，失败时返回原始 400。',
                'content is converted to plain text and retried once with the same account; the original content is not deleted. Tool continuation may still fail. The retry adds the cost of one request, and a failure returns the original 400 response.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="thinking_modified_retry"
          label={t('thinking 修改兜底', 'thinking modification fallback')}
          summary={t(
            '上游检测到 thinking 块被修改时，自动降级并重试一次。',
            'When the upstream detects modified thinking blocks, automatically downgrade them and retry once.',
          )}
          description={
            <>
              {t(
                '成因通常是 JSON 序列化/反序列化改变了 thinking 块的编码（如 Unicode 转义、数字格式）。处理方式与签名兜底相同：把历史 thinking 块降级成普通文本后重试。',
                'Usually caused by JSON serialization changing the encoding of thinking blocks (e.g. Unicode escapes, number formatting). Handled the same way as the signature fallback: historical thinking blocks are downgraded to plain text and retried.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="redacted_thinking_retry"
          label={t('redacted thinking 兜底', 'redacted thinking fallback')}
          summary={t(
            '上游拒绝 redacted_thinking 块的密文时，自动降级并重试一次。',
            'When the upstream rejects the ciphertext of a redacted_thinking block, automatically downgrade it and retry once.',
          )}
          description={
            <>
              {t('上游回', 'The upstream returns')}{' '}
              <code className="font-mono">
                Invalid `data` in `redacted_thinking` block
              </code>{' '}
              {t(
                '时触发。这段密文由上游签发，校验不通过通常是因为会话中途换了账号，或该轮 assistant 消息在转发时被改写过（如工具名混淆）。处理方式与另外两种兜底相同：历史 thinking 降级成普通文本、redacted_thinking 整块删除后重试一次；命中时日志会输出这一块在收到与发出的两份请求体中的对照，据此可判断是哪一种成因。',
                '. The ciphertext is issued by the upstream, so a failure usually means the session switched accounts midway, or that assistant turn was rewritten on forwarding (for example by tool name obfuscation). Handled like the other two fallbacks: historical thinking is downgraded to plain text, redacted_thinking blocks are dropped, and the request is retried once. On a hit, the log compares that block between the received and the forwarded request body so the cause can be told apart.',
              )}
            </>
          }
        />
      </SettingsGroup>
    </div>
  )
}

/**
 * 登录时申请的 OAuth scope。
 *
 * 默认与官方 Claude Code 逐字一致（scope 集合也是指纹的一部分）；想少授权就切到精简那一档，
 * 代价是授权请求与官方不再完全相同。改动只对之后的新登录生效——已有凭证的范围在授权那一刻
 * 就定了，刷新 token 不带 scope，改这里不会追溯。
 *
 * **不校验**：填什么存什么，前后端都不判合法性。这个框就是拿来试上游认哪些 scope 的，
 * 拦一道就等于把它唯一的用途拦掉；认不认由同意页说。
 */
function OAuthScopes() {
  const { language, t } = useI18n()
  const qc = useQueryClient()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const [draft, setDraft] = useState('')

  useEffect(() => {
    if (data) setDraft(data.oauth_scopes)
  }, [data?.oauth_scopes])

  const save = useMutation({
    mutationFn: (scopes: string) => setOauthScopes(scopes),
    onSuccess: (settings: Settings) => {
      toastManager.add({
        title: t('登录授权范围已更新', 'Login authorization scopes updated'),
        description: settings.oauth_scopes === settings.oauth_scopes_default
          ? t(
              '已恢复官方默认范围；下次添加账号时生效。',
              'Restored the official default scopes; effective the next time an account is added.',
            )
          : t(
              `下次添加账号时按这 ${settings.oauth_scopes.split(' ').length} 项申请。`,
              `The next account added will request these ${settings.oauth_scopes.split(' ').length} scopes.`,
            ),
        type: 'success',
      })
      qc.setQueryData(['settings'], settings)
    },
    onError: (error) => {
      toastManager.add({
        title: t('保存失败', 'Save failed'),
        description: extractError(error, language),
        type: 'error',
      })
    },
  })

  const current = data?.oauth_scopes ?? ''
  const official = data?.oauth_scopes_default ?? ''
  const minimal = data?.oauth_scopes_minimal ?? ''
  // 与后端同一口径：压空白、按输入顺序去重（不排序——顺序也是指纹的一部分）。
  const items = Array.from(new Set(draft.split(/\s+/).filter(Boolean)))
  const value = items.join(' ')
  const preset = value === official
    ? t('官方默认', 'Official default')
    : value === minimal
      ? t('精简', 'Minimal')
      : t('自定义', 'Custom')

  return (
    <Field className="p-4 sm:p-5">
      <div className="w-full space-y-3">
        <div className="min-w-0 space-y-1">
          <FieldLabel htmlFor="oauth-scopes">{t('申请的 scope', 'Requested scopes')}</FieldLabel>
          <FieldDescription className="max-w-xl leading-5">
            <ClampedDescription text={t(
              '以空格分隔，填什么就发什么。这里不做校验，是否接受由 Claude 的同意页决定（例如完全不带 scope 会返回 Missing scope parameter）。留空则恢复官方默认的整套 scope，与官方客户端逐字一致，scope 集合也是指纹的一部分。「精简」档只保留 Luban 真正用得上的三项：user:inference 用于转发（去掉后账号只能登录查看额度）、user:profile 决定能否读取邮箱与等级、user:file_upload 负责经 Files API 的上传。',
              'Space separated, sent verbatim — nothing is validated here; Claude\u2019s consent page decides what it accepts (omitting scope entirely, for instance, comes back as Missing scope parameter). Leave empty to restore the full official set, which is byte-for-byte what the official client requests, and the scope set is part of the fingerprint. The minimal preset keeps the three Luban actually uses: user:inference for forwarding (without it an account can only sign in and show quota), user:profile for the email and tier, user:file_upload for uploads through the Files API.',
            )} />
          </FieldDescription>
        </div>
        <Textarea
          id="oauth-scopes"
          className="font-mono"
          size="sm"
          placeholder={official}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
        />
        <div className="flex flex-wrap items-center justify-between gap-2">
          <div className="flex flex-wrap items-center gap-2">
            <Badge variant="secondary" size="sm">
              {items.length > 0
                ? t(`${preset} · ${items.length} 项`, `${preset} · ${items.length} scopes`)
                : t('留空 = 官方默认', 'Empty = official default')}
            </Badge>
            <Button size="sm" variant="ghost" onClick={() => setDraft(official)}>
              {t('官方默认', 'Official default')}
            </Button>
            <Button size="sm" variant="ghost" onClick={() => setDraft(minimal)}>
              {t('精简', 'Minimal')}
            </Button>
          </div>
          <Button
            size="sm"
            loading={save.isPending}
            disabled={value === current}
            onClick={() => save.mutate(value)}
          >
            <SaveIcon />
            {t('保存', 'Save')}
          </Button>
        </div>
      </div>
    </Field>
  )
}

type PolicyKey = 'prefill' | 'sampling'

const POLICY_LABELS: Record<PolicyValue, [string, string]> = {
  strip: ['剥离后转发', 'Strip & forward'],
  reject: ['本地拒绝', 'Reject locally'],
  off: ['不处理', 'Off'],
}

function PolicySelect({
  label,
  summary,
  value,
  settingKey,
}: {
  label: string
  summary: string
  value: PolicyValue
  settingKey: PolicyKey
}) {
  const { language, t } = useI18n()
  const id = useId()
  const qc = useQueryClient()

  const items = (Object.keys(POLICY_LABELS) as PolicyValue[]).map((k) => ({
    label: t(POLICY_LABELS[k][0], POLICY_LABELS[k][1]),
    value: k,
  }))

  const save = useMutation({
    mutationFn: (next: PolicyValue) =>
      settingKey === 'prefill' ? setPrefillPolicy(next) : setSamplingPolicy(next),
    onSuccess: (settings: Settings) => {
      const v = settingKey === 'prefill' ? settings.prefill_policy : settings.sampling_policy
      const [zh, en] = POLICY_LABELS[v as PolicyValue] ?? ['', '']
      toastManager.add({
        title: t(`${label}：${zh}`, `${label}: ${en}`),
        description: summary,
        type: 'success',
      })
      qc.setQueryData(['settings'], settings)
    },
    onError: (error) => {
      toastManager.add({
        title: t('保存失败', 'Save failed'),
        description: extractError(error, language),
        type: 'error',
      })
    },
  })

  return (
    <SettingsRow
      htmlFor={id}
      label={label}
      description={<ClampedDescription text={summary} />}
    >
      {/* 用全站那套 Select，不再手搓原生 <select>：原来这里是一个自己抄了一遍边框样式的
          原生下拉，高度 h-8 写死，弹出层还是操作系统那一套，和同一页别处的下拉长得是两个东西。 */}
      <Select
        items={items}
        value={value}
        disabled={save.isPending}
        onValueChange={(next) => next && save.mutate(next as PolicyValue)}
      >
        <SelectTrigger id={id} className="sm:w-40" aria-label={label}>
          <SelectValue />
        </SelectTrigger>
        <SelectPopup>
          {items.map((item) => (
            <SelectItem key={item.value} value={item.value}>
              {item.label}
            </SelectItem>
          ))}
        </SelectPopup>
      </Select>
    </SettingsRow>
  )
}

/** 429 后追加尝试的账号数（不含首次请求；0 = 不重试；后端限制在 0~10）。 */
function RetryMax() {
  const { language, t } = useI18n()
  const qc = useQueryClient()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const [draft, setDraft] = useState<number | null>(null)

  useEffect(() => {
    if (data) setDraft(data.rate_limit_retry_max)
  }, [data?.rate_limit_retry_max])

  const save = useMutation({
    mutationFn: (count: number) => setRateLimitRetryMax(count),
    onSuccess: (settings: Settings) => {
      toastManager.add({
        title: t('429 重试策略已更新', '429 retry policy updated'),
        description: settings.rate_limit_retry_max > 0
          ? t(
              `最多追加尝试 ${settings.rate_limit_retry_max} 个账号。`,
              `Try up to ${settings.rate_limit_retry_max} additional ${settings.rate_limit_retry_max === 1 ? 'account' : 'accounts'}.`,
            )
          : t(
              '429 将直接透传，不冷却、不换账号。',
              '429 responses will pass through unchanged, without cooldown or account switching.',
            ),
        type: 'success',
      })
      qc.setQueryData(['settings'], settings)
    },
    onError: (error) => {
      toastManager.add({
        title: t('保存失败', 'Save failed'),
        description: extractError(error, language),
        type: 'error',
      })
    },
  })

  const count = Math.min(10, Math.max(0, Math.floor(draft ?? 0)))
  const enabled = data?.rate_limit_retry ?? true

  return (
    <SettingsRow
      label={t('追加重试账号数', 'Additional retry accounts')}
      description={t(
        '填 2 时最多尝试 3 个账号（含首次）。',
        'Set this to 2 to try up to 3 accounts in total, including the first.',
      )}
    >
      <NumberField
        className="min-w-0 flex-1 sm:w-32 sm:flex-none"
        disabled={!enabled}
        max={10}
        min={0}
        value={draft}
        onValueChange={setDraft}
      >
        <NumberFieldGroup>
          <NumberFieldDecrement
            aria-label={t('减少 429 追加重试账号数', 'Decrease additional accounts retried after 429')}
          />
          <NumberFieldInput
            aria-label={t('429 追加重试账号数', 'Additional accounts to retry after 429')}
          />
          <NumberFieldIncrement
            aria-label={t('增加 429 追加重试账号数', 'Increase additional accounts retried after 429')}
          />
        </NumberFieldGroup>
      </NumberField>
      <Button
        loading={save.isPending}
        disabled={!enabled || count === (data?.rate_limit_retry_max ?? 2)}
        onClick={() => save.mutate(count)}
      >
        <SaveIcon />
        {t('保存', 'Save')}
      </Button>
    </SettingsRow>
  )
}

/**
 * 额度用到多少就提前把账号挪出调度池（0 = 关闭，等真收到 429 才停；后端限制在 0~100）。
 *
 * 判定用的是上游每条响应都带的基础额度窗口使用率，只看 5h/7d 这类基础窗口，不看超额池。
 */
function QuotaPausePct() {
  const { language, t } = useI18n()
  const qc = useQueryClient()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const [draft, setDraft] = useState<number | null>(null)
  const [weekDraft, setWeekDraft] = useState<number | null>(null)

  useEffect(() => {
    if (data) {
      setDraft(data.quota_pause_pct)
      setWeekDraft(data.quota_pause_pct_7d)
    }
  }, [data?.quota_pause_pct, data?.quota_pause_pct_7d])

  const save = useMutation({
    mutationFn: ({ pct, week }: { pct: number; week: number }) => setQuotaPausePct(pct, week),
    onSuccess: (settings: Settings) => {
      const parts = [
        settings.quota_pause_pct > 0
          ? t(`5 小时窗口 ${settings.quota_pause_pct}%`, `5h window ${settings.quota_pause_pct}%`)
          : t('5 小时窗口不停', '5h window off'),
        settings.quota_pause_pct_7d > 0
          ? t(
              `7 天窗口 ${settings.quota_pause_pct_7d}%`,
              `7d window ${settings.quota_pause_pct_7d}%`,
            )
          : t('7 天窗口不停', '7d window off'),
      ]
      toastManager.add({
        title: t('提前停调度阈值已更新', 'Early pause threshold updated'),
        description: settings.quota_pause_pct > 0 || settings.quota_pause_pct_7d > 0
          ? t(`${parts.join(' · ')}。`, `${parts.join(' · ')}.`)
          : t(
              '两档都已关闭：账号会一直参与调度，直到真正收到 429。',
              'Both thresholds are off: accounts keep taking traffic until they actually get a 429.',
            ),
        type: 'success',
      })
      qc.setQueryData(['settings'], settings)
      qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (error) => {
      toastManager.add({
        title: t('保存失败', 'Save failed'),
        description: extractError(error, language),
        type: 'error',
      })
    },
  })

  const clamp = (v: number | null) => Math.min(100, Math.max(0, Math.floor(v ?? 0)))
  const pct = clamp(draft)
  const week = clamp(weekDraft)
  const enabled = data?.rate_limit_retry ?? true
  const unchanged =
    pct === (data?.quota_pause_pct ?? 90) && week === (data?.quota_pause_pct_7d ?? 0)

  return (
    <SettingsRow
      label={t('提前停调度阈值', 'Early pause threshold')}
      description={
        <ClampedDescription text={t(
          '上游每条响应都带有账号的用量窗口使用率；达到阈值就把账号移出调度池，不必等下一条请求触发 429（那一条请求必定失败）。两个窗口各设一档，不要混为一谈：因 5 小时窗口暂停调度的账号最多暂停几小时就会自动恢复，因 7 天窗口暂停的则要暂停到下一次周重置。一个周用量偏高的账号会因此被整段移出调度池，哪怕它这 5 小时内完全没用。所以 7 天那档默认关闭（周额度真正用完时上游会返回 429，账号级冷却照常接手）；如需开启，建议设得比 5 小时那档更高。超额用量（extra usage）快满不计入。暂停后按触发的那个窗口的重置时刻自动恢复，也可手动启用或通过连通性测试放回。填 0 表示该档不暂停调度。这里是全局值；单个账号可在账号菜单「提前停调度阈值」中逐档覆盖（跟随全局 / 该窗口不提前停 / 独立阈值）。',
          'Every upstream response reports the account’s usage window utilization; once it reaches the threshold the account leaves the scheduling pool, instead of waiting for the next request to hit a 429 (which is bound to fail). Each window gets its own threshold; do not treat them as one: a pause from the 5h window lasts a few hours at most, while a pause from the 7d window lasts until the weekly reset, so an account with heavy weekly usage would sit out entirely even when its 5h window is untouched. That is why the 7d threshold is off by default (when the weekly quota really runs out, upstream returns a 429 and the account-level cooldown takes over); if you do enable it, set it higher than the 5h one. Extra usage nearing its limit never counts. A paused account comes back automatically when the window that triggered it resets, and can also be re-enabled by hand or by a passing connectivity test. 0 turns that threshold off. These are the global values; each account can override either window from its menu under Early pause threshold (Use global / Off for this window / Custom threshold).',
        )} />
      }
    >
      {/* 两行的网格：第一行是两枚标签，第二行是两个数字框与保存按钮。
          按钮显式落在第二行第三列（`col-start-3 row-start-2`，列也必须钉——只写行的话，
          CSS 网格会把「行确定」的项**先于**纯自动项放置，按钮就抢到第二行第一列、跑到数字框左边），
          于是它和数字框是**同一行带里的兄弟**，
          上下居中由网格保证——不再取决于「标签 + 输入框」那摞东西有多高。
          先前用 flex + `items-end` 对不齐：那种排法下按钮的位置要跟着旁边那摞的底边走，
          差一点点就歪，而这一行恰恰差了几个像素。 */}
      {/* 手机上两列走 `minmax(0,1fr)` 而不是 `auto`：`auto` 不肯收缩，
          两个 w-32（128px）加保存按钮要 366px，而 375px 的屏上这一行只有 311px，会横向溢出。
          ≥640px 退回内容宽（`sm:grid-cols-[auto_auto_auto]`）。与接入页「无身份请求上限」同一套。 */}
      <div className="grid w-full grid-cols-[minmax(0,1fr)_minmax(0,1fr)_auto] grid-rows-[auto_auto] items-center gap-x-3 gap-y-1.5 sm:w-auto sm:grid-cols-[auto_auto_auto]">
        <div className="row-span-2 grid grid-rows-subgrid gap-y-1.5">
          <FieldDescription>{t('5 小时窗口', '5h window')}</FieldDescription>
          <NumberField
            className="w-full sm:w-32"
            disabled={!enabled}
            max={100}
            min={0}
            value={draft}
            onValueChange={setDraft}
          >
            <NumberFieldGroup>
              <NumberFieldDecrement
                aria-label={t('降低 5 小时窗口阈值', 'Decrease 5h window threshold')}
              />
              <NumberFieldInput
                aria-label={t('5 小时窗口提前停调度阈值（%）', '5h window early pause threshold (%)')}
              />
              <NumberFieldIncrement
                aria-label={t('提高 5 小时窗口阈值', 'Increase 5h window threshold')}
              />
            </NumberFieldGroup>
          </NumberField>
        </div>
        <div className="row-span-2 grid grid-rows-subgrid gap-y-1.5">
          <FieldDescription>
            {t('7 天窗口（0 = 不停）', '7d window (0 = off)')}
          </FieldDescription>
          <NumberField
            className="w-full sm:w-32"
            disabled={!enabled}
            max={100}
            min={0}
            value={weekDraft}
            onValueChange={setWeekDraft}
          >
            <NumberFieldGroup>
              <NumberFieldDecrement
                aria-label={t('降低 7 天窗口阈值', 'Decrease 7d window threshold')}
              />
              <NumberFieldInput
                aria-label={t('7 天窗口提前停调度阈值（%）', '7d window early pause threshold (%)')}
              />
              <NumberFieldIncrement
                aria-label={t('提高 7 天窗口阈值', 'Increase 7d window threshold')}
              />
            </NumberFieldGroup>
          </NumberField>
        </div>
        {/* 保存按钮用**默认尺寸**而不是 `sm`：默认高度（h-9 / sm:h-8）与 NumberField 那一组
            完全相同，两者的边框与字才落在同一条线上；`sm` 矮 4px，就是截图里那个歪。
            设置页所有「输入框 + 保存」的行现在都是这一档。 */}
        <Button
          className="col-start-3 row-start-2 max-sm:size-9 max-sm:px-0"
          loading={save.isPending}
          disabled={!enabled || unchanged}
          onClick={() => save.mutate({ pct, week })}
        >
          <SaveIcon />
          <span className="max-sm:sr-only">{t('保存', 'Save')}</span>
        </Button>
      </div>
    </SettingsRow>
  )
}

/** 单个开关：读写都走 ['settings']，改完让账号列表也失效。 */
function ForwardingToggle({
  k,
  label,
  summary,
  description,
  requires,
}: {
  k: ForwardingKey
  label: string
  summary: string
  description?: ReactNode
  /**
   * 依赖的前置开关：它关着时本项即便存着「开」也不会生效（后端同样这么判），
   * 故置灰并改写副标题，把这层依赖摆到界面上——否则就是个拨得动、却一动不动的开关。
   * 存储值不动，前置开关一开回来，本项还是原来那个状态。
   */
  requires?: { key: ForwardingKey; label: string }
}) {
  const { language, t } = useI18n()
  const id = useId()
  const qc = useQueryClient()
  const { data } = useQuery({ queryKey: ['settings'], queryFn: getSettings })
  const enabled = data?.[k] ?? true
  const blocked = requires != null && data?.[requires.key] === false

  const save = useMutation({
    mutationFn: (next: boolean) => setForwarding(k, next),
    onSuccess: (settings: Settings) => {
      toastManager.add({
        title: settings[k]
          ? t(`${label}已开启`, `${label} enabled`)
          : t(`${label}已关闭`, `${label} disabled`),
        description: summary,
        type: 'success',
      })
      qc.setQueryData(['settings'], settings)
      qc.invalidateQueries({ queryKey: ['credentials'] })
    },
    onError: (error) => {
      toastManager.add({
        title: t('保存失败', 'Save failed'),
        description: extractError(error, language),
        type: 'error',
      })
    },
  })

  return (
    <SettingsRow
      disabled={blocked}
      htmlFor={id}
      label={label}
      description={
        blocked
          ? t(`需先开启「${requires.label}」`, `Enable “${requires.label}” first`)
          : description
            // 这一行底下已经挂了「影响与限制」，摘要就不再自带第二个展开器：同一个标签下面
            // 一枚文字按钮「了解更多」加一个 details「影响与限制」是两套交互、两种长相。
            // 全部 38 条摘要里只有一条超过收起阈值，多出的那一两行铺开就是了。
            ? summary
            : <ClampedDescription text={summary} />
      }
      footer={description && (
        <details className="group text-xs text-muted-foreground">
          <summary className="flex w-fit cursor-pointer list-none items-center gap-1.5 rounded-sm font-medium transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring [&::-webkit-details-marker]:hidden">
            {t('影响与限制', 'Impact & limitations')}
            <ChevronDownIcon
              aria-hidden="true"
              className="size-3 transition-transform group-open:rotate-180"
            />
          </summary>
          <div className="mt-2 max-w-xl border-l-2 border-border pl-3 leading-5 [&_code]:rounded-sm [&_code]:bg-muted [&_code]:px-1 [&_code]:py-0.5 [&_code]:text-foreground">
            {description}
          </div>
        </details>
      )}
    >
      <Switch
        id={id}
        checked={enabled && !blocked}
        disabled={save.isPending || blocked}
        onCheckedChange={(next) => save.mutate(next)}
      />
    </SettingsRow>
  )
}

/**
 * 从上游 400 学到的规则：列表 + 单条删除 + 全部清空。
 *
 * 这些规则是从一条报错里学来的推断，上游放开了本地没有信号能知道；后端给了 7 天保鲜期，
 * 这里是等不及 7 天时的逃生口。删错的代价只是同一组合再撞一次 400。
 */
/** 规则种类 → 徽章色调、状态点颜色；未知种类退回中性灰。 */
const RULE_TONES: Record<string, { badge: 'error' | 'info' | 'warning'; dot: string }> = {
  shape: { badge: 'error', dot: 'bg-destructive' },
  deprecated: { badge: 'info', dot: 'bg-info' },
  empty_reply: { badge: 'warning', dot: 'bg-warning' },
  refusal: { badge: 'error', dot: 'bg-destructive' },
  app_refusal: { badge: 'error', dot: 'bg-destructive' },
}
const RULE_KIND_ORDER = ['app_refusal', 'refusal', 'empty_reply', 'shape', 'deprecated']

const ruleKey = (row: Pick<LearnedRejection, 'kind' | 'model' | 'field' | 'value'>) =>
  `${row.kind}:${row.model}:${row.field}:${row.value}`

/** 规则文案开头的「[类别]」拆出来单独当标签显示，剩下的才是上游原话。 */
function splitRuleMessage(message: string): { tag: string | null; body: string } {
  const m = /^\[([^\]\n]{1,48})\]\s*/.exec(message)
  return m ? { tag: m[1], body: message.slice(m[0].length) } : { tag: null, body: message }
}

function FilterChip({
  active,
  count,
  dot,
  label,
  onClick,
}: {
  active: boolean
  count: number
  dot?: string
  label: string
  onClick: () => void
}) {
  return (
    <button
      aria-pressed={active}
      className={cn(
        'inline-flex items-center gap-1.5 rounded-full border px-2.5 py-1 text-xs transition-colors',
        'focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring focus-visible:ring-offset-1 focus-visible:ring-offset-background',
        active
          ? 'border-border bg-muted font-medium text-foreground'
          : 'border-transparent text-muted-foreground hover:bg-muted/60 hover:text-foreground',
      )}
      type="button"
      onClick={onClick}
    >
      {dot && <span aria-hidden="true" className={cn('size-1.5 rounded-full', dot)} />}
      {label}
      <span className="tabular-nums opacity-60">{count}</span>
    </button>
  )
}

/** 拒答组内每页条数可选值。 */
const GROUP_PAGE_SIZES = [25, 50, 100] as const

/**
 * 一组拒答规则（同模型 + 同类别）：组头一行，展开后是紧凑的一行一条（模型与类别已在组头，
 * 每条只剩哈希、时间、原话与删除），分页翻看；整组可一键删。一个下游被分类器盯上几小时就是
 * 几百条，逐条三行平铺既翻不完也删不完。
 */
function RefusalGroup({
  group,
  open,
  onToggle,
  onDeleteGroup,
  onForget,
  forgetPending,
  kindLabel,
  expiresIn,
}: {
  group: { key: string; model: string; tag: string | null; rows: LearnedRejection[]; latest: number }
  open: boolean
  onToggle: () => void
  onDeleteGroup: () => void
  onForget: (row: LearnedRejection) => void
  forgetPending: boolean
  kindLabel: (kind: string) => string
  expiresIn: (unixSecs: number) => string
}) {
  const { t, language } = useI18n()
  const [page, setPage] = useState(0)
  const [pageSize, setPageSize] = useState<number>(GROUP_PAGE_SIZES[0])
  const [openMessages, setOpenMessages] = useState<ReadonlySet<string>>(() => new Set())
  const total = group.rows.length
  const totalPages = Math.max(1, Math.ceil(total / pageSize))
  // 删到页码越界（最后一页删空了）时退回最后一页。
  const currentPage = Math.min(page, totalPages - 1)
  const pageRows = group.rows.slice(currentPage * pageSize, currentPage * pageSize + pageSize)
  const firstIndex = currentPage * pageSize + 1
  const lastIndex = currentPage * pageSize + pageRows.length
  const toggleMessage = (key: string) =>
    setOpenMessages((prev) => {
      const next = new Set(prev)
      if (!next.delete(key)) next.add(key)
      return next
    })
  return (
    <li className="px-4 py-3 sm:px-5">
      <div className="flex items-start gap-3">
        <span aria-hidden="true" className={cn('mt-2 size-1.5 shrink-0 rounded-full', RULE_TONES.refusal.dot)} />
        <button
          aria-expanded={open}
          className="flex min-w-0 flex-1 flex-wrap items-center gap-x-2 gap-y-1 text-left"
          type="button"
          onClick={onToggle}
        >
          <ChevronDownIcon
            aria-hidden="true"
            className={cn('size-3.5 shrink-0 text-muted-foreground transition-transform', !open && '-rotate-90')}
          />
          <span className="text-sm font-medium [overflow-wrap:anywhere]">{group.model}</span>
          <Badge size="sm" variant={RULE_TONES.refusal.badge}>
            {kindLabel('refusal')}
          </Badge>
          {group.tag && <span className="rounded bg-muted px-1 py-px font-mono text-xs">{group.tag}</span>}
          <span className="text-xs text-muted-foreground tabular-nums">
            {t(`${total} 条提示词`, `${total} prompt${total === 1 ? '' : 's'}`)}
          </span>
          <Tooltip>
            <TooltipTrigger render={<span />} delay={0} className="cursor-help text-xs text-muted-foreground">
              {t('最近学到于', 'Latest')} {relativeTime(group.latest, undefined, language)}
            </TooltipTrigger>
            <TooltipPopup>{formatFullTime(group.latest, language)}</TooltipPopup>
          </Tooltip>
        </button>
        <Button
          aria-label={t('删除这一组规则', 'Remove this rule group')}
          title={t('删除这一组规则', 'Remove this rule group')}
          size="icon"
          variant="ghost"
          className="shrink-0 text-muted-foreground hover:text-foreground"
          onClick={onDeleteGroup}
        >
          <Trash2Icon />
        </Button>
      </div>
      {open && (
        <div className="mt-2 ms-4 overflow-hidden rounded-md border">
          <ul className="divide-y" role="list">
            {pageRows.map((row) => {
              const key = ruleKey(row)
              const { body } = splitRuleMessage(row.message ?? '')
              const showing = openMessages.has(key)
              return (
                <li key={key} className="px-3 py-1.5 text-xs transition-colors hover:bg-muted/40">
                  <div className="flex items-center gap-2">
                    <button
                      aria-expanded={showing}
                      aria-label={t('上游当时的判决', 'Upstream verdict')}
                      className="shrink-0 text-muted-foreground transition-colors hover:text-foreground disabled:opacity-30"
                      disabled={!body}
                      type="button"
                      onClick={() => toggleMessage(key)}
                    >
                      <ChevronDownIcon
                        aria-hidden="true"
                        className={cn('size-3.5 transition-transform', !showing && '-rotate-90')}
                      />
                    </button>
                    <code className="min-w-0 shrink-0 font-mono [overflow-wrap:anywhere]">{row.value}</code>
                    <span className="min-w-0 flex-1 truncate font-mono text-muted-foreground">{body}</span>
                    <Tooltip>
                      <TooltipTrigger
                        render={<span />}
                        delay={0}
                        className="shrink-0 cursor-help whitespace-nowrap text-muted-foreground tabular-nums"
                      >
                        {relativeTime(row.learned_at, undefined, language)}
                      </TooltipTrigger>
                      <TooltipPopup>{formatFullTime(row.learned_at, language)}</TooltipPopup>
                    </Tooltip>
                    <Tooltip>
                      <TooltipTrigger
                        render={<span />}
                        delay={0}
                        className="hidden shrink-0 cursor-help whitespace-nowrap text-muted-foreground tabular-nums sm:inline"
                      >
                        {expiresIn(row.expires_at)}
                      </TooltipTrigger>
                      <TooltipPopup>{formatFullTime(row.expires_at, language)}</TooltipPopup>
                    </Tooltip>
                    <Button
                      aria-label={t('删除这条规则', 'Remove this rule')}
                      title={t('删除这条规则', 'Remove this rule')}
                      size="icon-xs"
                      variant="ghost"
                      className="shrink-0 text-muted-foreground hover:text-foreground"
                      disabled={forgetPending}
                      onClick={() => onForget(row)}
                    >
                      <Trash2Icon />
                    </Button>
                  </div>
                  {showing && body && (
                    <pre className="mt-1.5 max-h-52 overflow-auto whitespace-pre-wrap rounded-md border bg-muted/50 p-2 font-mono leading-5 text-muted-foreground [overflow-wrap:anywhere]">
                      {body}
                    </pre>
                  )}
                </li>
              )
            })}
          </ul>
          {(total > GROUP_PAGE_SIZES[0] || totalPages > 1) && (
            <div className="grid grid-cols-[minmax(0,1fr)_auto] items-center gap-3 border-t bg-muted/30 px-3 py-2 text-xs sm:grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)]">
              <p className="min-w-0 text-muted-foreground tabular-nums">
                {t(`第 ${firstIndex}–${lastIndex} 条，共 ${total} 条`, `${firstIndex}–${lastIndex} of ${total}`)}
              </p>
              {/* `col-start-2` 不能省：这一格是「行确定、列自动」，而 CSS 网格会把这类项**先于**纯自动项
                放置（放置算法第 2 步早于第 4 步），不钉列它就会抢到第 1 列、和左边那句计数调个个儿。
                sm 起三列时它本来就有 `col-start-3`，只有窄屏这一档踩坑。 */}
              <div className="col-start-2 row-start-1 flex items-center gap-2 justify-self-end sm:col-start-3">
                <span className="whitespace-nowrap text-muted-foreground">{t('每页', 'Per page')}</span>
                <Select
                  items={GROUP_PAGE_SIZES.map((size) => ({ value: size, label: String(size) }))}
                  value={pageSize}
                  onValueChange={(value) => {
                    if (value == null) return
                    setPageSize(Number(value))
                    setPage(0)
                  }}
                >
                  <SelectTrigger size="sm" className="w-auto min-w-20" aria-label={t('每页条数', 'Rows per page')}>
                    <SelectValue />
                  </SelectTrigger>
                  <SelectPopup>
                    {GROUP_PAGE_SIZES.map((size) => (
                      <SelectItem key={size} value={size}>{size}</SelectItem>
                    ))}
                  </SelectPopup>
                </Select>
              </div>
              {totalPages > 1 && (
                <Pagination className="col-span-2 row-start-2 justify-center sm:col-span-1 sm:col-start-2 sm:row-start-1">
                  <PaginationContent>
                    <PaginationItem>
                      <PaginationPrevious
                        render={<Button variant="ghost" disabled={currentPage === 0} />}
                        aria-disabled={currentPage === 0}
                        onClick={() => setPage((current) => Math.max(0, current - 1))}
                      />
                    </PaginationItem>
                    <PaginationItem>
                      <span className="whitespace-nowrap px-2 text-xs text-foreground tabular-nums" aria-live="polite">
                        {t(`第 ${currentPage + 1} / ${totalPages} 页`, `Page ${currentPage + 1} of ${totalPages}`)}
                      </span>
                    </PaginationItem>
                    <PaginationItem>
                      <PaginationNext
                        render={<Button variant="ghost" disabled={currentPage >= totalPages - 1} />}
                        aria-disabled={currentPage >= totalPages - 1}
                        onClick={() => setPage((current) => Math.min(totalPages - 1, current + 1))}
                      />
                    </PaginationItem>
                  </PaginationContent>
                </Pagination>
              )}
            </div>
          )}
        </div>
      )}
    </li>
  )
}

function LearnedRejections() {
  const { t, language } = useI18n()
  const qc = useQueryClient()
  /** 清空确认框：关着 / 清全部 / 清一种类 / 删一组（同模型同类别）。 */
  type ClearTarget =
    | { type: 'all' }
    | { type: 'kind'; kind: string }
    | { type: 'group'; kind: string; model: string; category: string | null; count: number }
  const [confirmClear, setConfirmClear] = useState<ClearTarget | null>(null)
  const [kindFilter, setKindFilter] = useState<string>('all')
  /** 搜索框：按模型 / 字段取值（哈希）/ 规则文案子串过滤，几百条拒答里找一条 403 报出的哈希用。 */
  const [search, setSearch] = useState('')
  const [expanded, setExpanded] = useState<ReadonlySet<string>>(() => new Set())
  const query = useQuery({ queryKey: ['learned-rejections'], queryFn: listLearnedRejections })
  const failure = (title: string, error: unknown) =>
    toastManager.add({ title, description: extractError(error, language), type: 'error' })

  const forget = useMutation({
    mutationFn: (row: LearnedRejection) => forgetLearnedRejection(row),
    onSuccess: (rows) => {
      qc.setQueryData(['learned-rejections'], rows)
      toastManager.add({ title: t('已删除规则', 'Rule removed'), type: 'success' })
    },
    onError: (e) => failure(t('删除失败', 'Failed to remove'), e),
  })
  const clear = useMutation({
    // `kind` 为空清全部，否则只清那一种类（拒答提示词几百条时不必连别的规则一起清）。
    mutationFn: (kind: string | null) => clearLearnedRejections(kind ?? undefined),
    onSuccess: (deleted, kind) => {
      setConfirmClear(null)
      qc.setQueryData<LearnedRejection[]>(['learned-rejections'], (prev) =>
        kind ? (prev ?? []).filter((row) => row.kind !== kind) : [],
      )
      toastManager.add({
        title: t(`已清空 ${deleted} 条规则`, `Cleared ${deleted} rule${deleted === 1 ? '' : 's'}`),
        type: 'success',
      })
    },
    onError: (e) => { setConfirmClear(null); failure(t('清空失败', 'Failed to clear'), e) },
  })
  const forgetGroup = useMutation({
    mutationFn: (group: { kind: string; model: string; category: string | null }) => forgetLearnedGroup(group),
    onSuccess: (rows) => {
      setConfirmClear(null)
      qc.setQueryData(['learned-rejections'], rows)
      toastManager.add({ title: t('已删除这一组规则', 'Rule group removed'), type: 'success' })
    },
    onError: (e) => { setConfirmClear(null); failure(t('删除失败', 'Failed to remove'), e) },
  })

  const rows = query.data ?? []
  const kindLabel = (kind: string) =>
    kind === 'shape'
      ? t('本地拒绝', 'Rejected locally')
      : kind === 'deprecated'
        ? t('剥掉字段', 'Field stripped')
        : kind === 'empty_reply'
          ? t('零输出，本地拒绝', 'Empty reply, rejected locally')
          : kind === 'refusal'
            ? t('拒答过的提示词，本地回放上游的拒答', 'Refused prompt, upstream refusal replayed locally')
            : kind === 'app_refusal'
              ? t('拒答过的应用（模型 + system），本地回放上游的拒答', 'Refused app (model + system), upstream refusal replayed locally')
              : kind

  /** 到期还剩多久——比一个绝对时间戳更能一眼看出这条规则还要拦多久。 */
  const expiresIn = (unixSecs: number) => {
    const diff = unixSecs - Math.floor(Date.now() / 1000)
    if (diff <= 0) return t('已到期', 'Expired')
    const hours = Math.floor(diff / 3600)
    if (hours < 1) {
      const min = Math.max(1, Math.floor(diff / 60))
      return t(`${min} 分钟后到期`, `Expires in ${min}m`)
    }
    if (hours < 24) return t(`${hours} 小时后到期`, `Expires in ${hours}h`)
    const days = Math.floor(hours / 24)
    return t(`${days} 天后到期`, `Expires in ${days}d`)
  }

  const counts = rows.reduce<Record<string, number>>((acc, row) => {
    acc[row.kind] = (acc[row.kind] ?? 0) + 1
    return acc
  }, {})
  const kinds = [
    ...RULE_KIND_ORDER.filter((k) => counts[k]),
    ...Object.keys(counts).filter((k) => !RULE_KIND_ORDER.includes(k)).sort(),
  ]
  const byKind = kindFilter === 'all' ? rows : rows.filter((row) => row.kind === kindFilter)
  const needle = search.trim().toLowerCase()
  const visible = needle
    ? byKind.filter((row) =>
        [row.model, row.field, row.value, row.message ?? ''].some((s) => s.toLowerCase().includes(needle)),
      )
    : byKind
  const toggleMessage = (key: string) =>
    setExpanded((prev) => {
      const next = new Set(prev)
      if (!next.delete(key)) next.add(key)
      return next
    })

  /**
   * 拒答规则按「模型 + 类别」折叠成一组：每条只拦逐字相同的一条提示词，一个下游被分类器盯上
   * 几小时就是几百条，平铺会把其他几条有用的规则淹掉。别的种类照常一行一条。行本来按学到
   * 时间倒序，组按首次出现排、组内保持原序。
   */
  type Entry =
    | { type: 'row'; row: LearnedRejection }
    | { type: 'group'; key: string; model: string; tag: string | null; rows: LearnedRejection[]; latest: number }
  const entries: Entry[] = []
  const groups = new Map<string, Extract<Entry, { type: 'group' }>>()
  for (const row of visible) {
    if (row.kind !== 'refusal') {
      entries.push({ type: 'row', row })
      continue
    }
    const { tag } = splitRuleMessage(row.message ?? '')
    const key = `group:refusal:${row.model}:${tag ?? ''}`
    let group = groups.get(key)
    if (!group) {
      group = { type: 'group', key, model: row.model, tag, rows: [], latest: row.learned_at }
      groups.set(key, group)
      entries.push(group)
    }
    group.rows.push(row)
    group.latest = Math.max(group.latest, row.learned_at)
  }

  /** 一条规则：模型、种类、字段取值、可展开的上游原话、时间、删除。 */
  function RuleItem({ row }: { row: LearnedRejection }) {
    const key = ruleKey(row)
    const tone = RULE_TONES[row.kind]
    const { tag, body } = splitRuleMessage(row.message ?? '')
    const open = expanded.has(key)
    return (
      <li className="flex items-start gap-3 px-4 py-3 transition-colors sm:px-5 hover:bg-muted/40">
        <span
          aria-hidden="true"
          className={cn('mt-2 size-1.5 shrink-0 rounded-full', tone?.dot ?? 'bg-muted-foreground')}
        />
        <div className="min-w-0 flex-1 space-y-1.5">
          <div className="flex flex-wrap items-center gap-x-2 gap-y-1">
            <span className="text-sm font-medium [overflow-wrap:anywhere]">{row.model}</span>
            <Badge size="sm" variant={tone?.badge ?? 'secondary'}>
              {kindLabel(row.kind)}
            </Badge>
            <code className="inline-flex min-w-0 items-center gap-1 rounded border bg-muted/60 px-1.5 py-0.5 font-mono text-xs">
              <span className="text-muted-foreground">{row.field}</span>
              {row.value && (
                <>
                  <span className="text-muted-foreground/60">=</span>
                  <span className="[overflow-wrap:anywhere]">{row.value}</span>
                </>
              )}
            </code>
          </div>
          {body && (
            <div className="space-y-1.5">
              <button
                aria-expanded={open}
                className="flex w-full items-center gap-1.5 text-left text-muted-foreground transition-colors hover:text-foreground"
                type="button"
                onClick={() => toggleMessage(key)}
              >
                <ChevronDownIcon
                  aria-hidden="true"
                  className={cn('size-3.5 shrink-0 transition-transform', !open && '-rotate-90')}
                />
                {tag && (
                  <span className="shrink-0 rounded bg-muted px-1 py-px font-mono text-xs">
                    {tag}
                  </span>
                )}
                <span className={cn('min-w-0 flex-1 font-mono text-xs', !open && 'truncate')}>
                  {open ? t('上游当时的回复', 'Upstream reply') : body}
                </span>
              </button>
              {open && (
                <pre className="max-h-52 overflow-auto whitespace-pre-wrap rounded-md border bg-muted/50 p-2 font-mono text-xs leading-5 text-muted-foreground [overflow-wrap:anywhere]">
                  {body}
                </pre>
              )}
            </div>
          )}
          <div className="flex flex-wrap items-center gap-x-2 gap-y-0.5 text-xs text-muted-foreground tabular-nums">
            <Tooltip>
              <TooltipTrigger render={<span />} delay={0} className="cursor-help">
                {t('学到于', 'Learned')} {relativeTime(row.learned_at, undefined, language)}
              </TooltipTrigger>
              <TooltipPopup>{formatFullTime(row.learned_at, language)}</TooltipPopup>
            </Tooltip>
            <span aria-hidden="true" className="opacity-40">·</span>
            <Tooltip>
              <TooltipTrigger render={<span />} delay={0} className="cursor-help">
                {expiresIn(row.expires_at)}
              </TooltipTrigger>
              <TooltipPopup>{formatFullTime(row.expires_at, language)}</TooltipPopup>
            </Tooltip>
          </div>
        </div>
        <Button
          aria-label={t('删除这条规则', 'Remove this rule')}
          title={t('删除这条规则', 'Remove this rule')}
          size="icon"
          variant="ghost"
          className="shrink-0 text-muted-foreground hover:text-foreground"
          disabled={forget.isPending}
          onClick={() => forget.mutate(row)}
        >
          <Trash2Icon />
        </Button>
      </li>
    )
  }

  return (
    <SettingsGroup
      icon={BrainIcon}
      title={t('从上游学到的规则', 'Rules learned from upstream')}
      description={t(
        '上游明确拒绝过的组合会被记录下来：某模型不接受的取值（400，目前识别 effort 档位、messages 里的 role、tools 里的工具类型三种），下次在本地直接拒绝；某模型已废弃的参数（400），下次转发前剥掉；某模型对「无 tools 的单条消息 + 某个 max_tokens」返回过 200 却零输出的，同类请求下次在本地直接拒绝；某模型的分类器拒答过（stop_reason refusal 且 stop_details 带 category，如 cyber）的提示词，逐字相同地重发时在本地直接拒绝，只拦截那一条内容，同形态的其他请求不受影响。模型自己拒答的（无 category）或 fallback 没有执行成功的（带 recommended_model）不学习。命中拒答规则时本地回放上游那次的拒答（200 加同一段 stop_reason refusal），命中零输出规则时本地返回 403，两类分别受「拒绝已拒答的提示词」与「拒绝零输出请求类」开关控制；出站带 fallbacks 的请求不拦截。拒答规则的文案是「[类别] + 上游 stop_details 原文」，零输出规则的文案才是上游回复的开头。规则存入数据库、重启后保留，7 天后自动丢弃并重新验证。拒答规则不设上限，按「模型 + 类别」折叠成一组，展开后可分页查看每条，也可整组删除；顶部可按模型 / 哈希 / 原话搜索，筛选到某一类时可只清空那一类。上游已放开而本地仍在拦截时，在这里删除即可。',
        'Combinations upstream has called out are remembered: a value a model refuses (400; currently the effort level, a role in messages, and a tool type in tools) is rejected locally next time; a parameter a model deprecated (400) is stripped before forwarding; a tool-less single-message request class (model + max_tokens) that upstream answered with 200 and zero output tokens is rejected locally next time; a prompt the upstream classifier refused (stop_reason refusal with a stop_details category such as cyber) is rejected locally when resent verbatim, and only that one prompt, never other requests of the same shape. Refusals the model made on its own (no category) and refusals whose fallback could not run (recommended_model present) are not learned. A refused-prompt hit replays the original upstream refusal locally (200 with the same stop_reason refusal body), while an empty-reply hit answers 403; the two are governed by “Reject refused prompts” and “Reject empty-reply request classes” respectively; requests going out with fallbacks are not blocked. Refusal rules keep “[category] ” plus the upstream stop_details verbatim as their text; only empty-reply rules keep the start of the upstream reply. Rules persist across restarts and expire after 7 days. Refused prompts are unbounded and folded into one group per model and category; expand a group to page through its prompts or remove the whole group, search by model / hash / text at the top, and with a kind filter active you can clear just that kind. If upstream has since allowed something, remove the rule here.',
      )}
    >
      {query.isPending ? (
        <div className="flex items-center gap-2 p-4 text-sm text-muted-foreground sm:p-5">
          <Spinner />
          {t('正在加载', 'Loading')}
        </div>
      ) : query.isError ? (
        <div className="flex flex-wrap items-center justify-between gap-2 p-4 text-sm sm:p-5">
          <span className="text-destructive-foreground">{extractError(query.error, language)}</span>
          <Button size="xs" variant="outline" onClick={() => query.refetch()}>
            {t('重试', 'Retry')}
          </Button>
        </div>
      ) : rows.length === 0 ? (
        <Empty className="py-10 md:py-12">
          <EmptyHeader>
            <EmptyMedia variant="icon">
              <BrainIcon />
            </EmptyMedia>
            <EmptyTitle className="text-base sm:text-base">
              {t('还没有学到任何规则', 'No rules learned yet')}
            </EmptyTitle>
            <EmptyDescription className="text-xs leading-5">
              {t(
                '上游拒掉某个取值、废弃某个参数，或对某条提示词拒答之后，规则会自动出现在这里。',
                'Rules appear here on their own once upstream rejects a value, deprecates a parameter, or refuses a prompt.',
              )}
            </EmptyDescription>
          </EmptyHeader>
        </Empty>
      ) : (
        <>
          {/* 工具条：总数 + 按种类筛选 + 清空，动作放顶部，列表本身保持干净 */}
          <div className="flex flex-wrap items-center gap-x-3 gap-y-2 px-4 py-2.5 sm:px-5">
            <div className="flex flex-wrap items-center gap-1">
              <FilterChip
                active={kindFilter === 'all'}
                count={rows.length}
                label={t('全部', 'All')}
                onClick={() => setKindFilter('all')}
              />
              {kinds.map((kind) => (
                <FilterChip
                  key={kind}
                  active={kindFilter === kind}
                  count={counts[kind]}
                  dot={RULE_TONES[kind]?.dot}
                  label={kindLabel(kind)}
                  onClick={() => setKindFilter(kind)}
                />
              ))}
            </div>
            <div className="ms-auto flex flex-wrap items-center gap-1.5">
              <div className="relative">
                <SearchIcon aria-hidden="true" className="pointer-events-none absolute start-2 top-1/2 size-3.5 -translate-y-1/2 text-muted-foreground" />
                <Input
                  aria-label={t('搜索规则', 'Search rules')}
                  className="h-7 w-52 ps-7 pe-7 text-xs"
                  placeholder={t('模型 / 哈希 / 原话', 'Model / hash / text')}
                  value={search}
                  onChange={(e) => setSearch(e.target.value)}
                />
                {search && (
                  <button
                    aria-label={t('清除搜索', 'Clear search')}
                    className="absolute end-1.5 top-1/2 -translate-y-1/2 rounded p-0.5 text-muted-foreground hover:text-foreground"
                    type="button"
                    onClick={() => setSearch('')}
                  >
                    <XIcon className="size-3.5" />
                  </button>
                )}
              </div>
              {kindFilter !== 'all' && (counts[kindFilter] ?? 0) > 0 && (
                <Button size="xs" variant="outline" onClick={() => setConfirmClear({ type: 'kind', kind: kindFilter })}>
                  <Trash2Icon />
                  {t(`清空这一类 ${counts[kindFilter]}`, `Clear this kind ${counts[kindFilter]}`)}
                </Button>
              )}
              <Button size="xs" variant="outline" onClick={() => setConfirmClear({ type: 'all' })}>
                <Trash2Icon />
                {t('全部清空', 'Clear all')}
              </Button>
            </div>
          </div>
          {visible.length === 0 ? (
            <div className="flex flex-wrap items-center justify-between gap-2 px-4 py-6 text-sm sm:px-5 text-muted-foreground">
              {needle ? t('没有匹配的规则。', 'No matching rules.') : t('这一类下没有规则。', 'No rules of this kind.')}
              <Button size="xs" variant="outline" onClick={() => { setKindFilter('all'); setSearch('') }}>
                {t('查看全部', 'Show all')}
              </Button>
            </div>
          ) : (
            <ul className="divide-y" role="list">
              {entries.map((entry) =>
                entry.type === 'row' ? (
                  <RuleItem key={ruleKey(entry.row)} row={entry.row} />
                ) : (
                  <RefusalGroup
                    key={entry.key}
                    group={entry}
                    open={expanded.has(entry.key)}
                    onToggle={() => toggleMessage(entry.key)}
                    onDeleteGroup={() =>
                      setConfirmClear({
                        type: 'group',
                        kind: 'refusal',
                        model: entry.model,
                        category: entry.tag,
                        count: entry.rows.length,
                      })
                    }
                    onForget={(row) => forget.mutate(row)}
                    forgetPending={forget.isPending}
                    kindLabel={kindLabel}
                    expiresIn={expiresIn}
                  />
                ),
              )}
            </ul>
          )}
          <AlertDialog open={confirmClear !== null} onOpenChange={(open) => { if (!open) setConfirmClear(null) }}>
            <AlertDialogPopup>
              <AlertDialogHeader>
                <AlertDialogTitle>
                  {confirmClear?.type === 'kind'
                    ? t(`清空「${kindLabel(confirmClear.kind)}」`, `Clear "${kindLabel(confirmClear.kind)}"`)
                    : confirmClear?.type === 'group'
                      ? t('删除这一组规则', 'Remove this rule group')
                      : t('清空学到的规则', 'Clear learned rules')}
                </AlertDialogTitle>
                <AlertDialogDescription>
                  {confirmClear?.type === 'kind'
                    ? t(
                        `将删除这一类的 ${counts[confirmClear.kind] ?? 0} 条规则，其他种类不动。之后同样的组合会再向上游发一次，被拒的话会重新学到。`,
                        `${counts[confirmClear.kind] ?? 0} rules of this kind will be removed; other kinds are untouched. The same combinations will be sent upstream once more and re-learned if rejected.`,
                      )
                    : confirmClear?.type === 'group'
                      ? t(
                          `将删除 ${confirmClear.model}${confirmClear.category ? ` 的 ${confirmClear.category} 类` : ''} 共 ${confirmClear.count} 条拒答提示词规则，其他模型、其他类别不动。之后这些提示词逐字重发会再送到上游一次，被拒的话会重新学到。`,
                          `${confirmClear.count} refused-prompt rules for ${confirmClear.model}${confirmClear.category ? ` (${confirmClear.category})` : ''} will be removed; other models and categories are untouched. Resending those prompts verbatim will reach upstream once more and be re-learned if refused.`,
                        )
                      : t(
                          `将删除全部 ${rows.length} 条规则。之后同样的组合会再向上游发一次，被拒的话会重新学到。`,
                          `All ${rows.length} rules will be removed. The same combinations will be sent upstream once more and re-learned if rejected.`,
                        )}
                </AlertDialogDescription>
              </AlertDialogHeader>
              <AlertDialogFooter>
                <AlertDialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</AlertDialogClose>
                <Button
                  variant="destructive"
                  loading={clear.isPending || forgetGroup.isPending}
                  onClick={() => {
                    if (!confirmClear) return
                    if (confirmClear.type === 'group') {
                      forgetGroup.mutate({ kind: confirmClear.kind, model: confirmClear.model, category: confirmClear.category })
                    } else {
                      clear.mutate(confirmClear.type === 'kind' ? confirmClear.kind : null)
                    }
                  }}
                >
                  {confirmClear?.type === 'group' ? t('删除', 'Remove') : t('清空', 'Clear')}
                </Button>
              </AlertDialogFooter>
            </AlertDialogPopup>
          </AlertDialog>
        </>
      )}
    </SettingsGroup>
  )
}
