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
import { toastManager } from '@/components/ui/toast'
import { SettingsGroup } from '@/components/settings-group'

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
      <DialogPopup className="sm:max-w-3xl">
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
                '官方客户端每条请求都带设备身份，缺失本身就是一处差异。常见于模仿 Claude Code 的第三方客户端。补出的身份与「身份一致性」用同一套取值，会话标识优先沿用请求自带的。',
                'The official client includes a device identity with every request, so a missing identity is itself a discrepancy. This is common in third-party clients that imitate Claude Code. The generated identity uses the same values as “Identity consistency,” while a session identifier already present in the request takes precedence.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="api_telemetry"
          label={t('逐请求遥测', 'Per-request telemetry')}
          summary={t(
            '替每条转发出去的请求上报官方客户端会发的那串遥测：事件链、Datadog 日志、用量指标；失败的请求报错误事件。',
            'Report the telemetry the official client sends for every forwarded request: the event chain, Datadog logs, and usage metrics — with an error event for the ones that fail.',
          )}
          description={
            <>
              {t(
                '官方客户端每发一条请求都会上报 tengu_api_query → tengu_api_success → tengu_turn_end 这一串事件（带上游 request-id、逐项 token 与花费），以及 Datadog 日志和 OTel 用量指标。此前 luban 只有每 30 分钟一次的保活遥测，上游看到的是「有大量 API 用量、遥测里却一条 API 调用都没有」。开启后按 2.1.260 抓包的字段与节奏（事件 30 秒、日志 10 秒、指标 5 分钟攒批）替每张账号补上，身份取实际发往上游的那份，与请求两侧一致。失败的请求同样上报——官方客户端对它们发的是 tengu_api_error 加 tengu_feature_bad，只报成功那些一样是个可对照出来的差异。关闭即只剩保活遥测。',
                'The official client reports a chain of events for every request it sends (tengu_api_query → tengu_api_success → tengu_turn_end, carrying the upstream request-id, per-type token counts and cost), plus Datadog logs and OTel usage metrics. Until now luban only sent the 30-minute keepalive telemetry, so upstream saw an account with heavy API usage and not a single API call in its telemetry. When enabled, luban fills this in for every account following the fields and cadence captured from 2.1.260 (events batched every 30s, logs every 10s, metrics every 5min), using the identity actually sent upstream so both sides agree. Failed requests are reported too — the official client sends tengu_api_error plus tengu_feature_bad for those, so reporting only the successes is itself a detectable discrepancy. Turn it off to keep only the keepalive telemetry.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="keepalive_telemetry"
          label={t('保活遥测', 'Keepalive telemetry')}
          summary={t(
            '每 30 分钟替每张账号发一组空闲版本检查事件与 Datadog 日志，每 6 小时一次画像上报。',
            'Every 30 minutes send a set of idle version-check events and Datadog logs for each account, plus a profile report every 6 hours.',
          )}
          description={
            <>
              {t(
                '模拟一个开着但空闲的 Claude Code 进程。账号近 3 小时内有真实会话时，事件挂在那个会话的身份上（同一 session_id、设备标识、客户端版本），和真实客户端挂着终端没人说话时的行为一致；没有近期会话的账号才用按账号派生的空闲身份。关闭只停遥测这一半，token 刷新、启动握手（bootstrap / policy_limits / settings）与 401/403 探测照常。与「逐请求遥测」互不影响。',
                'Simulates an open but idle Claude Code process. When the account has had a real session in the last 3 hours, the events are attached to that session (same session_id, device identifier and client version), matching what a real client does when a terminal is left open; only accounts with no recent session fall back to an account-derived idle identity. Turning it off stops only the telemetry half: token refresh, the startup handshake (bootstrap / policy_limits / settings) and 401/403 detection continue. Independent of “Per-request telemetry”.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="spoof_device_id"
          label={t('改写设备标识', 'Rewrite device identifier')}
          summary={t(
            '请求自带设备标识时，换成当前账号派生的那个；关闭则原样沿用。',
            'Replace a device identifier sent by the client with one derived from the current account; when disabled, pass it through unchanged.',
          )}
          requires={{ key: 'spoof_identity', label: t('身份一致性', 'Identity consistency') }}
          description={
            <>
              {t(
                '官方客户端的设备标识是「机器标识」，同一台机器用哪个账号都发同一个；官方在 API key 与订阅两种模式下发的也完全相同，两者真正的差别只在账号标识那一段。所以换掉它不是形态需要，而是一道防关联措施：开启后每个账号在同一台机器上各有各的设备标识，账号之间不会因共用一个标识而被关联；代价是「一台机器多个账号」这种真实用户里很常见的情形，在经由本代理的流量里一次都不会出现。关闭后与官方逐字节一致，但同机多账号可被上游关联。请求未携带设备标识时一律派生，不受本开关影响。',
                'The official client’s device identifier is a machine identifier: the same machine sends the same value no matter which account is used, and it is identical in both API-key and subscription modes — the only real difference between those modes is the account identifier. Replacing it is therefore not a shape requirement but an unlinking measure. When enabled, each account gets its own device identifier on the same machine, so accounts cannot be linked through a shared value; the cost is that “one machine, several accounts” — common among real users — never appears in traffic through this proxy. When disabled, the value matches the official client byte for byte, but several accounts on one machine can be linked upstream. Requests that carry no device identifier always get a derived one, regardless of this toggle.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="normalize_device_fp"
          label={t('设备指纹归一化', 'Normalize device fingerprint')}
          summary={t(
            '同平台且同客户端版本的客户端收敛为同一个设备标识。',
            'Converge clients that share a platform and a client version into one device identifier.',
          )}
          requires={{ key: 'spoof_device_id', label: t('改写设备标识', 'Rewrite device identifier') }}
          description={
            <>
              {t(
                '设备指纹用于派生每个账号的伪装设备标识。开启后指纹只取平台信息（CPU 架构与操作系统），不含客户端原始设备标识——同一平台上的客户端会收敛成同一个伪装设备标识，符合真实用户一人多设备的使用模式。关闭后指纹包含客户端原始设备标识，每个（账号、客户端设备）组合都是一个独立的上游设备标识，客户端越多、上游看到该账号的设备数就越多。无论开关如何，指纹都包含实际发往上游的客户端版本：一台设备只能有一个版本，否则上游会看到同一个设备标识在同一秒里自报好几个版本（封号复盘里最刺眼的一条）。代价是客户端升级会换一个设备标识，上游看来像是这台机器重装了一次——比版本反复横跳安全得多。',
                'The device fingerprint is used to derive a spoofed device identifier per account. When enabled, the fingerprint uses only platform information (CPU architecture and operating system) and excludes the client’s original device identifier, so clients on the same platform converge onto one spoofed device identifier, matching the usage pattern of a real user with multiple devices. When disabled, the fingerprint includes the client’s original device identifier, making each (account, client device) combination a separate upstream device identifier — the more clients there are, the more devices upstream sees for that account. Either way the fingerprint also includes the client version actually sent upstream: one device may only ever report one version, otherwise upstream sees a single device identifier claiming several versions within the same second (the most damning signal in the ban post-mortem). The cost is that a client upgrade rotates the device identifier, which upstream reads as a machine being reinstalled — far safer than a version that flips back and forth.',
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
            '补齐缺失的版本、编码和请求标识，不覆盖已有值。',
            'Fill in missing version, encoding, and request identifiers without overwriting existing values.',
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
                '官方客户端的对话请求一律是流式的，非流式请求转发出去就是一处稳定特征。开启后 luban 只改请求里的 stream 字段，收到的流式响应会在本地拼回完整内容，再按客户端原本期待的格式返回，请求头与返回格式都不变。上游中途报错时，错误原文会照非流式该有的状态码返回，客户端的错误处理不受影响。代价：响应要等上游全部生成完才发出（与非流式本来的行为一致），且整段内容要在内存里暂存；请求明细里这类记录会标注「非流转流」，因为它的首字耗时记的是上游首字节，与客户端的感知不同。仅作用于对话请求，token 计数接口不受影响。',
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
                '工具名是上游判断第三方应用的一个已验证判据，命中就返回「Third-party apps now draw from your extra usage」并改扣超额用量，即使订阅用量充足。实测三个业务工具名就足以触发，而 `mcp__` 开头的名字会被豁免。开启后 luban 把这些名字换成 `mcp__luban__*` 下的稳定假名发出，响应里再换回真名，客户端从头到尾看到的都是自己的工具名。官方自带的工具、来访本就是 MCP 的工具和服务端工具都保留原名不动，所以对真实官方客户端没有任何影响。代价：响应内容要多做一次字符串替换；客户端中途增删工具会让假名整体重算，上游的提示词缓存会失效一次。',
                'Tool names are one verified signal the upstream uses to classify third-party apps; a match returns “Third-party apps now draw from your extra usage” and bills against extra usage even when plan usage remains. Names beginning with `mcp__` are exempt in testing. When enabled, luban forwards affected names as stable aliases under `mcp__luban__*` and restores them in responses, so the client only sees its own names. Official tools, tools that already use MCP names, and server tools remain unchanged, making this a no-op for the genuine official client. Costs: one extra string replacement pass over responses, and adding or removing tools mid-session recomputes aliases and invalidates the upstream prompt cache once.',
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
                '官方客户端的对话请求字段是固定一套，多出来的字段就是一处稳定特征，可能导致请求被判为第三方应用而改扣超额用量。开启后 luban 会删掉两样：一是语义等于默认值的 tool_choice（客户端强制指定工具或关闭并行调用时不动）；二是 thinking 里的 display 字段。代价：删掉 display 后上游不再返回思考摘要，客户端的「思考过程」会是空的，功能本身不受影响。真实官方客户端本来就不发这两样，开启对它没有任何影响。',
                'The official client sends a fixed set of fields on conversation requests; anything extra is a stable tell and can get the request classified as a third-party app, drawing from extra usage instead of plan limits. When enabled, luban removes two things: a tool_choice whose meaning equals the default (a forced tool choice or disabled parallel calls is left alone), and the display field inside thinking. Cost: without display the upstream no longer returns reasoning summaries, so the client shows an empty thinking section — functionality is otherwise unaffected. The real official client never sends either field, so enabling this is a no-op for it.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="inject_thinking"
          label={t('注入 Thinking', 'Inject thinking')}
          summary={t(
            '模拟路径下自动补 thinking 和 context_management，与官方形态一致；同时强制 temperature=1。',
            'Inject thinking and context_management in simulation mode to match the official shape; also forces temperature=1.',
          )}
          description={
            <>
              {t(
                '官方客户端的对话请求恒带 thinking 字段，缺了可能被上游判为第三方应用。开启后，模拟路径下客户端没发 thinking 时自动补上 {type:"enabled", budget_tokens: max_tokens-1}（max_tokens < 1024 的探测级请求不补），context_management 随之自动补上。同时 thinking 开启时上游要求 temperature 必须为 1，客户端若设了其他值会被自动剥掉。代价：注入 thinking 会改变模型行为（输出可能更长、thinking token 按输出计费）。不想要这些副作用就关掉，代价是模拟形态少一个与官方对齐的信号。',
                'The official client always includes a thinking field on conversation requests; omitting it may cause the upstream to classify the request as third-party. When enabled, if the client did not send thinking, luban injects {type:"enabled", budget_tokens: max_tokens-1} in simulation mode (skipped for probe-level requests with max_tokens < 1024), and context_management is added automatically. Since thinking requires temperature=1, any other value the client set is stripped. Cost: injected thinking changes model behaviour (outputs may be longer, thinking tokens are billed as output). Turn it off to avoid these side effects at the cost of one less signal aligning with the official shape.',
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
                '只调整分块，不改变提示词文本（缓存时长另由「缓存时长对齐 1h」那项管）。它不只是缓存优化：官方客户端的系统提示词恒为 4 块，超出的会被上游判成第三方应用，改从超额用量（extra usage）扣费而不是订阅用量，所以多出来的块会被并回第 4 块。无法识别切点时原样转发。',
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
                '基座是按模型族固定的官方提示词，全网同一份，标记后跨账号命中同一份缓存，省下重复的写入。官方客户端总是把 scope 和 ttl:1h 一起发，所以这项与「缓存时长对齐 1h」同时开着才是官方形态；单独关掉其中一项，发出去的就是官方不产生的组合。该标记需要上游的 prompt-caching-scope beta，故依赖「Beta 标记」开关。',
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
                '官方订阅客户端的三个缓存断点全带 ttl:"1h"，而 API key 模式发的是不带时长的裸断点——这一项正是两种模式之间的真实差别之一，不写就等于每条请求都留一处固定差异。代价要知道：1h 的缓存写入单价是默认 5 分钟的 2 倍。是省是亏取决于使用节奏——长会话里 1h 往往更省（5 分钟内没接上话，下一轮就得按写入价把整段前缀重写一遍），零散的一次性请求则是纯多付。关闭后 luban 一个字节都不改，客户端传什么时长就用什么。客户端自己写了时长的，任何情况下都照发不覆盖。该字段需要上游的 extended-cache-ttl beta，故依赖「Beta 标记」开关。',
                'All three cache breakpoints from the official subscription client carry ttl:"1h", whereas API-key mode sends bare breakpoints with no duration — this is one of the real differences between the two modes, so omitting it leaves a fixed discrepancy on every request. Know the cost: a 1h cache write is priced at twice the default 5-minute write. Whether that saves or costs money depends on your usage rhythm — in long sessions 1h usually saves (if you do not reply within five minutes, the next turn rewrites the whole prefix at write price), while scattered one-off requests simply pay more. When disabled, luban changes nothing and whatever duration the client sent is used as-is. A duration written by the client itself is always forwarded untouched. The field requires the upstream extended-cache-ttl beta, hence the dependency on “Beta flags”.',
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
                '仅改写非 Claude Code 请求。开启后会增加系统提示词和客户端请求头，可能提高 Token 成本并改变输出风格。此类请求通常没有设备身份，需先关闭「设备身份校验」。',
                'Only non-Claude Code requests are rewritten. Enabling this adds a system prompt and client request headers, which may increase Token costs and change the output style. These requests usually have no device identity, so disable “Device identity checks” first.',
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
                '上游 API 不支持 JSON Schema 的 allOf / oneOf / anyOf 组合关键字出现在 input_schema 顶层，直接 400。开启后自动将它们合并为一个普通 object schema 后转发。',
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
                '上游要求 text 内容块的 text 字段非空，部分第三方客户端会发送 {"type":"text","text":""} 的空块导致 400。开启后自动剥除空 text 块（若消息仅含空 text 块则保留原样）。',
                'The upstream API requires text content blocks to be non-empty. Some third-party clients send {"type":"text","text":""} which causes a 400. When enabled, empty text blocks are automatically stripped (if a message contains only empty text blocks, it is left unchanged).',
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
                '经 litellm、one-api、claude-code-router 等从 OpenAI 格式转过来的请求，到这里已是 Anthropic 形态，只能靠残留识别：messages 里的 role:"system" / "tool"、消息上的 name / tool_calls、call_ 前缀的工具调用 id、字串形态或 type:"function" 的 tool_choice、OpenAI function 形态的 tools、n / stop / user / response_format 等 OpenAI 专属顶层字段、image_url 等 OpenAI 内容块。命中任一即本地 400，错误消息指出位置与 Anthropic 的对应写法。关掉后退回下面的 System Role 提升等修补路径。不影响模拟路径：它只接管本来就是 Anthropic 形态的非 CC 请求。',
                'Requests converted from the OpenAI format by litellm, one-api, claude-code-router and the like arrive already in Anthropic shape; the only way to tell is the residue they leave: role:"system" / "tool" in messages, name / tool_calls on a message, tool call ids prefixed call_, a string or type:"function" tool_choice, tools in the OpenAI function shape, OpenAI-only top-level fields such as n / stop / user / response_format, and OpenAI content blocks such as image_url. Any hit is rejected locally with 400 and a message naming the location and the Anthropic equivalent. Turn it off to fall back to the repair paths below (system role hoisting etc.). The simulation path is unaffected: it only takes over non-CC requests that are already in Anthropic shape.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="fable_refusal_fallback"
          label={t('Fable 拒答换模型重跑', 'Fable refusal fallback')}
          summary={t(
            'fable 主线程请求带上官方那份服务端 fallback：安全分类器拒答时由上游在同一次调用里换 opus-5 重跑。形态逐字同官方 2.1.260，默认关。',
            'Fable main-thread requests carry the official server-side fallback: when the safety classifier refuses, upstream reruns the same call on opus-5. Byte-for-byte the official 2.1.260 shape; off by default.',
          )}
          description={
            <>
              {t(
                'Fable 5.1 / Fable 5 带安全分类器，命中（多为 cyber 类，良性安全工作也会误伤）时回 200 加 stop_reason refusal、正文为空。官方 Claude Code 2.1.260 在 fable 上自带 fallbacks: [{"model":"claude-opus-5"}] 和 server-side-fallback beta，拒答会由上游换 Opus 5 重跑，用户看不到拒答。开启后 luban 给 fable 主线程补上与官方逐字相同的这份，出站头一并带 server-side-fallback-2026-06-01——有抓包依据，补上更像 2.1.260 的官方形态；但它替用户决定了拒答后由 Opus 5 作答、按 Opus 计价、同一对话约一小时粘在 Opus 上，用户还看不到拒答本身，所以默认关、由用户自己拨开。关着时模拟出的 fable 请求比 2.1.260 少这一个字段、回到裸拒答——头上的 beta 仍按版本补，「有 beta 没字段」正是 2.1.260 之前的官方形态；客户端自带的 fallbacks 照样保留。客户端自己带了数组形态的不动；helper / 标题 / 安全分类 / 额度探测这些辅助请求官方都不发，不补。上游以 400 拒掉 fallback 目标时，剥掉重发一次并记进「从上游学到的规则」，之后该模型不再补。落到 fallback 的回复按实际服务的模型计价，同一对话约一小时内会粘在 fallback 模型上。输出前就被拒的请求上游不计费，流水里花费记 0。opus-5 那条自定链是另一个开关，见下。Sonnet 5 与 Opus 4.7/4.8 同样带网络安全分类器、同样会以 200 加 stop_reason refusal 拒答，luban 只解析并记录它们的拒答、不替它们补 fallbacks——官方客户端在这些模型上不发该字段，这是有意的产品限制，不是遗漏。',
                'Fable 5.1 / Fable 5 run safety classifiers; a hit (mostly the cyber category, and benign security work gets caught too) returns 200 with stop_reason refusal and empty content. Official Claude Code 2.1.260 sends fallbacks: [{"model":"claude-opus-5"}] plus the server-side-fallback beta on fable, so upstream reruns a refused call on Opus 5 and the user never sees the refusal. When enabled, luban adds that exact field to fable main-thread requests, with server-side-fallback-2026-06-01 in the outbound header; this is backed by a capture, so adding it matches the 2.1.260 official shape. But it also decides for the user that a refused request is answered by Opus 5, billed at Opus rates, with the conversation stuck to Opus for about an hour, and the user never sees the refusal, so it is off by default and left for the user to switch on. Turned off, simulated fable requests lack that one field and you get the raw refusal; the beta header is still added per version, and "beta present, field absent" is exactly the official shape before 2.1.260. A client-supplied fallbacks field is kept as is. A client-supplied array form is left alone; helper / title / classifier / quota-probe requests are not touched, as the official client never sends it there. If upstream rejects the fallback target with a 400, the field is stripped and the request resent once, and the rule lands under Rules learned from upstream so that model is not padded again. A reply served by a fallback is priced at the model that actually served it, and the conversation sticks to the fallback model for about an hour. Requests refused before any output are not billed upstream, so their cost is recorded as 0. The luban-defined opus-5 chain is a separate switch below. Sonnet 5 and Opus 4.7/4.8 also run cybersecurity classifiers and refuse with 200 plus stop_reason refusal; luban parses and records those refusals but does not add fallbacks for them, because the official client never sends the field on those models. That is a deliberate product limit, not an omission.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="opus_refusal_fallback"
          label={t('Opus 拒答换模型重跑（实验）', 'Opus refusal fallback (experimental)')}
          summary={t(
            'opus-5 主线程请求带上 luban 自定的 fallback 链：拒答时上游落 4.8 再落 4.6。官方 opus 客户端不发这个字段，默认关。',
            'Opus-5 main-thread requests carry a luban-defined fallback chain: on refusal upstream falls to 4.8, then 4.6. The official opus client never sends this field; off by default.',
          )}
          description={
            <>
              {t(
                '官方 Claude Code 2.1.260 的 opus 客户端只带 server-side-fallback beta、不发 fallbacks 字段——「有 beta 没字段」就是官方形态。开启后 luban 给 opus-5 主线程补上自定的 fallbacks: [{"model":"claude-opus-4-8"},{"model":"claude-opus-4-6"}]（cyber 类拒答官方推荐的 fallback 正是 4.8），这是一份官方客户端从不产生的请求形态：封号复盘里查不出它导致了 account_on_hold，但作为风控层面的自证风险，它只该是独立的实验开关，默认关、保持官方 opus 请求形态。其余行为同上一条：只补主线程、客户端自带的不动、上游 400 拒掉目标后学下来不再补、落到 fallback 的回复按实际作答模型计价。若日后要重新启用，更稳妥的做法是发字符串 "default" 让上游按当前推荐模型路由，或先读 /v1/models 的 allowed_fallback_models、全部目标获允许时再发自定链。',
                'The official Claude Code 2.1.260 opus client sends only the server-side-fallback beta and no fallbacks field; "beta present, field absent" is the official shape. When enabled, luban adds a self-defined fallbacks: [{"model":"claude-opus-4-8"},{"model":"claude-opus-4-6"}] to opus-5 main-thread requests (4.8 is the fallback officially recommended for cyber refusals). That is a request shape the official client never produces: the ban post-mortem does not show it caused account_on_hold, but as a fingerprint risk it belongs behind a separate experimental switch, off by default, keeping the official opus request shape. Everything else matches the switch above: main thread only, client-supplied arrays left alone, a 400 on a fallback target is learned and the model is not padded again, replies served by a fallback are priced at the model that answered. If you re-enable it later, the safer options are sending the string "default" so upstream routes to its current recommended model, or reading allowed_fallback_models from /v1/models first and only sending the custom chain when every target is allowed.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="reject_probes"
          label={t('拒绝探针请求', 'Reject probe requests')}
          summary={t(
            '下游中转拿账号做探活/测活的那类请求，本地直接 403，不到上游。只管形态判据；从响应学来的两类规则各有自己的开关，见下两条。',
            'Health-check / channel-test requests that downstream relays send to probe the account are rejected locally with 403 and never reach upstream. Shape signatures only; the two rule kinds learned from responses have their own switches below.',
          )}
          description={
            <>
              {t(
                '探活脚本借 Claude Code 的 UA 发一条无 tools 的单句小请求，每条在上游侧都是「一台设备开一个一次性会话只问一句话」，是封号复盘里最显眼的判据。三条强特征，命中任一即拒，只对自报 claude-cli UA 的请求生效：带 system、没有 tools、只有一条消息、max_tokens 在 2 到 16 之间；带 system、没有 tools、只有一条消息、不是官方那两种无 tools 形态，且来自一台从没见过的设备；system 里的 CC 身份句出现在不止一块里。「没有 tools」按值算，缺失、null、[] 都是没有，加空字段绕不过。官方 Claude Code 的三种无 tools 请求（cache 预热、haiku Helper、安全分类）按取值逐项对、都在判据之外。身份字段写错的（device_id 不是 64 位 hex、session_id 不是 UUID，如 channel-test）不算探针、不在这里拒，而是不当官方客户端、走模拟路径重建身份。只看形态、一条就判，不做计数。边界：把官方 haiku Helper 的取值逐字抄全的探针，形态上就是官方请求，这里分不出来。这个开关只管上面三条形态判据；从响应学来的「已拒答的提示词」与「零输出请求类」两类规则各有自己的开关（见下两条），0.3.93 之前三者共用这一个键，关探针会把学到的规则一起放行。',
                "Probe scripts borrow the Claude Code UA to send a single tool-less one-liner; upstream sees each one as \"a device opening a throwaway session to ask one question\", the most conspicuous pattern in the ban post-mortem. Three strong signatures, any one of which rejects, applied only to requests claiming a claude-cli UA: a system prompt with no tools, a single message and max_tokens between 2 and 16; a system prompt with no tools, a single message, not one of the two official tool-less shapes, and a never-seen device; the Claude Code identity sentence in more than one system block. \"No tools\" is judged by value: missing, null and [] all count, so padding with empty fields does not help. The three tool-less shapes official Claude Code does send (cache prewarm, the haiku helper, the security classifier) are matched value by value and fall outside all three. Malformed identities (a device_id that is not 64-hex, a session_id that is not a UUID, e.g. channel-test) are not probes and are not rejected here; they are simply not treated as an official client and go through the simulation path, which rebuilds the identity. Shape only, decided per request, no counting. Limit: a probe that copies the official haiku helper's values verbatim is, by shape, an official request and cannot be told apart here. This switch governs only the three shape signatures above; the two rule kinds learned from responses (refused prompts, empty-reply request classes) have their own switches below. Before 0.3.93 all three shared this one key, so turning probes off also let the learned rules through.",
              )}
            </>
          }
        />
        <ForwardingToggle
          k="reject_refusals"
          label={t('拒绝已拒答的提示词', 'Reject refused prompts')}
          summary={t(
            '上游分类器拒答过的那条提示词，逐字相同的重发本地直接 403；出站带 fallbacks 的请求不拦，交给上游换模型重跑。',
            'A prompt the upstream classifier has already refused is rejected locally with 403 when resent verbatim; requests that go out with fallbacks are not blocked, so upstream can rerun them on another model.',
          )}
          description={
            <>
              {t(
                '上游拒答（200 加 stop_reason refusal）不按形态学，只按那条提示词学：同一模型、system + messages + tools + tool_choice 逐字相同才算同一条，改一个字、换个模型、换个工具集都不命中，其他请求一律不拦。且只学分类器的判决（stop_details 带 category，如 cyber）——category 为空的按官方口径可能是模型自己拒的、也可能是不带类别的分类器判决，带 recommended_model 的是 fallback 没跑成，这两种重发都可能就答，只记流水不学。出站会带 fallbacks 的请求（客户端自带，或上面两个开关让 luban 补的）不在这里拦：带 fallback 的请求拒答后上游会换模型重跑，那正是拒答该走的路；此前不看这一点，fallback 关着时学到的一条拒答，之后即便把 fallback 打开也永远走不到上游。规则记在「从上游学到的规则」里、7 天到期（进程内每小时按库重建一次，不再依赖重启才过期）、可手动删。规则不分凭证、对全池生效：分类器判决对同一条提示词是确定性的，换个号重发结果一样。',
                "An upstream refusal (200 with stop_reason refusal) is never learned by shape, only by that exact prompt: the same model with system + messages + tools + tool_choice byte-for-byte identical; changing a word, the model, or the tool set misses, and nothing else is ever blocked. Only classifier verdicts are learned (stop_details carries a category such as cyber). A refusal with no category may, per the official docs, be the model's own or an uncategorised classifier verdict, and one carrying recommended_model means the fallback could not run; both may succeed on a resend, so they are logged but not learned. Requests that will go out with fallbacks (supplied by the client, or added by luban under the two switches above) are not blocked here: upstream reruns a refused request on another model, which is exactly the path a refusal should take. Previously this was not checked, so a refusal learned while fallbacks were off kept being rejected locally even after fallbacks were turned on. Rules live under Rules learned from upstream, expire after 7 days (the in-memory table is rebuilt from the store hourly, so expiry no longer waits for a restart) and can be removed by hand. Rules are not scoped to a credential and apply to the whole pool: the classifier verdict for a given prompt is deterministic, and resending it from another account gets the same answer.",
              )}
            </>
          }
        />
        <ForwardingToggle
          k="reject_empty_replies"
          label={t('拒绝零输出请求类', 'Reject empty-reply request classes')}
          summary={t(
            '某模型对「无 tools 单条消息 + 某个 max_tokens」回过 200 却零输出之后，同类请求本地直接 403，不限 UA。',
            'Once a model has answered a tool-less single-message request with a given max_tokens with 200 and zero output, that request class is rejected locally with 403, regardless of UA.',
          )}
          description={
            <>
              {t(
                '这条规则从响应学来、不限 UA：某模型对「无 tools 的单条消息 + 某个 max_tokens」回过 200 却零输出（有 usage、output_tokens 为 0，上游收了输入的钱一个字没回）之后，同类请求本地 403，上游当时的回复开头记在流水的这条记录与「从上游学到的规则」里；带 tools、多轮或换了 max_tokens 的不受影响。类取得很窄，宁可多放一条。封号复盘里这样的记录 13 小时里每 37 秒一条，每条在上游侧都是「一台设备只问一句话、什么都没得到」的探活式痕迹；模拟路径重建的是身份，改不了「问一句、上游一个字不回」这件事，所以不限 UA。规则 7 天到期、可手动删。',
                'This rule is learned from responses, regardless of UA: once a model has answered a tool-less single-message request with a given max_tokens with 200 and zero output tokens (usage present, output_tokens 0: upstream billed the input and returned nothing), that request class is rejected locally with 403; the start of that upstream reply is kept on the usage record and under Rules learned from upstream. Requests with tools, multi-turn conversations, or a different max_tokens are unaffected; the class is deliberately narrow. The ban post-mortem had one such record every 37 seconds for 13 hours, each one a probe-like trace of "one device asking one question and getting nothing" on the upstream side; the simulation path rebuilds identity but cannot change that, hence no UA limit. Rules expire after 7 days and can be removed by hand.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="reject_session_conflict"
          label={t('拒绝会话 id 冲突', 'Reject session id conflicts')}
          summary={t(
            '请求头与 metadata 里的会话 id 不一致时本地直接 400，不替客户端挑一个。',
            'When the session id in the header and in metadata disagree, reject locally with 400 instead of picking one.',
          )}
          description={
            <>
              {t(
                '官方 Claude Code 在 X-Claude-Code-Session-Id 与 metadata.user_id 两处发的是同一个值，逐字相同。两处给出两个都合法却不同的 UUID，是官方从不产生的形态；而 luban 用会话 id 作为会话链（cc_prompt_id / cc_prev_req / diagnostics.previous_message_id）的键，挑错一个就把两条链接到了一起，事后再也看不出来。开启后这类请求本地 400，错误消息里列出两个值。关掉后退回「取请求头那个 + 记一条 warn 日志」。只有一处合法时不算冲突——那是客户端只给对了一个，照常取合法的那个。',
                'Official Claude Code sends the same value in X-Claude-Code-Session-Id and in metadata.user_id, byte for byte. Two different but individually valid UUIDs is a shape the official client never produces, and luban keys the session chain (cc_prompt_id / cc_prev_req / diagnostics.previous_message_id) on the session id — picking the wrong one splices two chains together with no way to notice afterwards. When enabled such requests are rejected locally with 400, naming both values. Turn it off to fall back to using the header value and logging a warning. If only one of the two is a valid UUID it is not a conflict: the client simply got one of them right, and that one is used.',
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
                '上游 API 不支持 messages 数组里的 role:"system"（直接 400）。litellm 等采用 OpenAI 格式的客户端会把 system 指令放在 messages 里。开启后自动将这些消息的内容提升到顶层 system 字段，再从 messages 中移除。仅在「拒绝 OpenAI 转换残留」关闭时才有机会生效：那个开关开着，这类请求在入口就被拒了。',
                'The upstream API does not support role:"system" in the messages array (returns 400). Clients using OpenAI format (e.g. litellm) place system instructions in messages. When enabled, their content is automatically hoisted to the top-level system field and removed from messages. Only takes effect while "Reject OpenAI-format residue" is off: with that on, such requests are rejected at the door.',
              )}
            </>
          }
        />
      </SettingsGroup>

      <LearnedRejections />

      <SettingsGroup icon={RefreshCwIcon} title={t('限流与错误恢复', 'Rate limits & error recovery')}>
        <ForwardingToggle
          k="rate_limit_retry"
          label={t('429 自动换号', '429 automatic account switching')}
          summary={t(
            '遇到限流后，冷却受限账号或模型，并换用其他账号重试。',
            'After a rate limit, cool down the affected account or model and retry with another account.',
          )}
          description={
            <>
              {t(
                '账号用量耗尽时冷却整个账号；只有当前模型受限时仅冷却该模型。默认分别冷却 60 / 30 秒，并优先采用上游等待时间。换号会改绑有设备身份的请求，也可能降低缓存命中率；达到重试上限或没有其他账号时返回',
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
    <Field className="p-5">
      <div className="w-full space-y-3">
        <div className="min-w-0 space-y-1">
          <FieldLabel htmlFor="oauth-scopes">{t('申请的 scope', 'Requested scopes')}</FieldLabel>
          <FieldDescription className="max-w-2xl leading-5">
            {t(
              '空格分隔，填什么发什么——这里不校验，认不认由 Claude 的同意页说（例如整个不带 scope 会被回 Missing scope parameter）。留空恢复官方默认那一整套，与官方客户端逐字一致，scope 集合也是指纹的一部分。精简那一档只留 Luban 真正用得上的三项：user:inference 转发要用（去掉这个号就只能登进来看额度）、user:profile 决定邮箱与等级读不读得到、user:file_upload 管走 Files API 的上传。',
              'Space separated, sent verbatim — nothing is validated here; Claude\u2019s consent page decides what it accepts (omitting scope entirely, for instance, comes back as Missing scope parameter). Leave empty to restore the full official set, which is byte-for-byte what the official client requests, and the scope set is part of the fingerprint. The minimal preset keeps the three Luban actually uses: user:inference for forwarding (without it an account can only sign in and show quota), user:profile for the email and tier, user:file_upload for uploads through the Files API.',
            )}
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
    <Field className="p-5">
      <div className="flex w-full items-start justify-between gap-4">
        <div className="min-w-0 space-y-1">
          <FieldLabel htmlFor={id}>{label}</FieldLabel>
          <FieldDescription className="leading-5">{summary}</FieldDescription>
        </div>
        <select
          id={id}
          className="h-8 min-w-28 rounded-lg border border-input bg-background px-2 text-sm shadow-xs/5 outline-none focus-visible:border-ring focus-visible:ring-[3px] focus-visible:ring-ring/24 disabled:opacity-64"
          value={value}
          disabled={save.isPending}
          onChange={(e) => save.mutate(e.target.value as PolicyValue)}
        >
          {(Object.keys(POLICY_LABELS) as PolicyValue[]).map((k) => (
            <option key={k} value={k}>
              {t(POLICY_LABELS[k][0], POLICY_LABELS[k][1])}
            </option>
          ))}
        </select>
      </div>
    </Field>
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
              '429 将直接透传，不冷却、不换号。',
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
    <Field className="p-5">
      <div className="flex w-full flex-wrap items-end justify-between gap-3">
        <div className="min-w-0 space-y-1">
          <FieldLabel>{t('追加重试账号数', 'Additional retry accounts')}</FieldLabel>
          <FieldDescription>
            {t(
              '填 2 时最多尝试 3 个账号（含首次）。',
              'Set this to 2 to try up to 3 accounts in total, including the first.',
            )}
          </FieldDescription>
        </div>
        <div className="flex items-center gap-2">
          <NumberField
            className="w-32"
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
            size="sm"
            loading={save.isPending}
            disabled={!enabled || count === (data?.rate_limit_retry_max ?? 2)}
            onClick={() => save.mutate(count)}
          >
            <SaveIcon />
            {t('保存', 'Save')}
          </Button>
        </div>
      </div>
    </Field>
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
              '两档都已关闭：账号会一直参与调度，直到真的收到 429。',
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
    <Field className="p-5">
      <div className="w-full space-y-3">
        <div className="min-w-0 space-y-1">
          <FieldLabel>{t('提前停调度阈值', 'Early pause threshold')}</FieldLabel>
          <FieldDescription className="max-w-xl leading-5">
            {t(
              '上游每条响应都带着账号的用量限制使用率；到达阈值就把账号挪出调度池，不必等下一条请求去撞 429（那一发必定失败）。两个窗口各配一档，别混用：5 小时窗口停号最多歇几小时就自己回来，7 天窗口停号是歇到下个周重置——一个周用量偏高的号会被整段挪出池子，哪怕它这 5 小时一点没用。故 7 天那档默认关（周额度真用光时上游会回 429，账号级冷却照常接手）；要开建议配得比 5 小时那档更高。超额池快满不算在内。停用后按触发的那个窗口的重置时刻自动恢复，也可手动启用或用连通性测试放回。填 0 = 该档不停号。这里是全局值；单个账号可在账号菜单「提前停调度阈值」里逐档覆盖（跟随全局 / 这一档不停 / 独立阈值）。',
              'Every upstream response reports the account’s usage-limit utilization; once it reaches the threshold the account leaves the scheduling pool, instead of waiting for the next request to hit a 429 (which is bound to fail). Each window gets its own threshold — do not treat them as one: a pause from the 5h window lasts a few hours at most, while a pause from the 7d window lasts until the weekly reset, so an account with heavy weekly usage would sit out entirely even when its 5h window is untouched. That is why the 7d threshold is off by default (when the weekly quota really runs out, upstream returns a 429 and the account-level cooldown takes over); if you do enable it, set it higher than the 5h one. A nearly full overage pool never counts. A paused account comes back automatically when the window that triggered it resets, and can also be re-enabled by hand or by a passing connectivity test. 0 turns that threshold off. These are the global values; each account can override either window from its menu under Early pause threshold (use global / off for this account / custom threshold).',
            )}
          </FieldDescription>
        </div>
        <div className="flex flex-wrap items-end gap-4">
          <div className="space-y-1.5">
            <FieldDescription>{t('5 小时窗口', '5h window')}</FieldDescription>
            <NumberField
              className="w-32"
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
          <div className="space-y-1.5">
            <FieldDescription>
              {t('7 天窗口（0 = 不停）', '7d window (0 = off)')}
            </FieldDescription>
            <NumberField
              className="w-32"
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
          <Button
            size="sm"
            loading={save.isPending}
            disabled={!enabled || unchanged}
            onClick={() => save.mutate({ pct, week })}
          >
            <SaveIcon />
            {t('保存', 'Save')}
          </Button>
        </div>
      </div>
    </Field>
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
    <Field className="p-5" disabled={blocked}>
      <div className="flex w-full items-start justify-between gap-4">
        <div className="min-w-0 space-y-1">
          <FieldLabel htmlFor={id}>{label}</FieldLabel>
          <FieldDescription className="leading-5">
            {blocked
              ? t(`需先开启「${requires.label}」`, `Enable “${requires.label}” first`)
              : summary}
          </FieldDescription>
        </div>
        <Switch
          id={id}
          checked={enabled && !blocked}
          disabled={save.isPending || blocked}
          onCheckedChange={(next) => save.mutate(next)}
        />
      </div>
      {description && (
        <details className="group text-xs text-muted-foreground">
          <summary className="flex w-fit cursor-pointer list-none items-center gap-1.5 rounded-sm font-medium transition-colors hover:text-foreground focus-visible:outline-none focus-visible:ring-2 focus-visible:ring-ring [&::-webkit-details-marker]:hidden">
            {t('影响与限制', 'Impact & limitations')}
            <ChevronDownIcon
              aria-hidden="true"
              className="size-3 transition-transform group-open:rotate-180"
            />
          </summary>
          <div className="mt-2 border-l-2 border-border pl-3 leading-5 [&_code]:rounded-sm [&_code]:bg-muted [&_code]:px-1 [&_code]:py-0.5 [&_code]:text-foreground">
            {description}
          </div>
        </details>
      )}
    </Field>
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
}
const RULE_KIND_ORDER = ['refusal', 'empty_reply', 'shape', 'deprecated']

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
    <li className="px-4 py-3">
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
          {group.tag && <span className="rounded bg-muted px-1 py-px font-mono text-[10px]">{group.tag}</span>}
          <span className="text-xs text-muted-foreground tabular-nums">
            {t(`${total} 条提示词`, `${total} prompt${total === 1 ? '' : 's'}`)}
          </span>
          <span className="text-[11px] text-muted-foreground" title={formatFullTime(group.latest, language)}>
            {t('最近学到于', 'Latest')} {relativeTime(group.latest, undefined, language)}
          </span>
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
                <li key={key} className="px-3 py-1.5 text-[11px] transition-colors hover:bg-muted/40">
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
                    <span className="shrink-0 whitespace-nowrap text-muted-foreground tabular-nums" title={formatFullTime(row.learned_at, language)}>
                      {relativeTime(row.learned_at, undefined, language)}
                    </span>
                    <span className="hidden shrink-0 whitespace-nowrap text-muted-foreground tabular-nums sm:inline" title={formatFullTime(row.expires_at, language)}>
                      {expiresIn(row.expires_at)}
                    </span>
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
              <div className="row-start-1 flex items-center gap-2 justify-self-end sm:col-start-3">
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
                  <SelectTrigger size="sm" aria-label={t('每页条数', 'Rows per page')}>
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
            ? t('拒答过的提示词，本地拒绝', 'Refused prompt, rejected locally')
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
      <li className="flex items-start gap-3 px-4 py-3 transition-colors hover:bg-muted/40">
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
            <code className="inline-flex min-w-0 items-center gap-1 rounded border bg-muted/60 px-1.5 py-0.5 font-mono text-[11px]">
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
                  <span className="shrink-0 rounded bg-muted px-1 py-px font-mono text-[10px]">
                    {tag}
                  </span>
                )}
                <span className={cn('min-w-0 flex-1 font-mono text-[11px]', !open && 'truncate')}>
                  {open ? t('上游当时的回复', 'Upstream reply') : body}
                </span>
              </button>
              {open && (
                <pre className="max-h-52 overflow-auto whitespace-pre-wrap rounded-md border bg-muted/50 p-2 font-mono text-[11px] leading-5 text-muted-foreground [overflow-wrap:anywhere]">
                  {body}
                </pre>
              )}
            </div>
          )}
          <div className="flex flex-wrap items-center gap-x-2 gap-y-0.5 text-[11px] text-muted-foreground tabular-nums">
            <span title={formatFullTime(row.learned_at, language)}>
              {t('学到于', 'Learned')} {relativeTime(row.learned_at, undefined, language)}
            </span>
            <span aria-hidden="true" className="opacity-40">·</span>
            <span title={formatFullTime(row.expires_at, language)}>{expiresIn(row.expires_at)}</span>
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
        '上游点名过的组合会被记下来：某模型不收某取值的（400），下次本地直接拒；某模型已废弃某参数的（400），下次转发前剥掉；某模型对「无 tools 的单条消息 + 某个 max_tokens」回过 200 却零输出的，同类下次本地直接拒；某模型被分类器拒答过（stop_reason refusal 且 stop_details 带 category，如 cyber）的那条提示词，逐字相同的重发本地直接拒——只拦那一条内容，同形态的其他请求不受影响；模型自己拒的（无 category）或 fallback 没跑成的（带 recommended_model）不学（两条都是 403，各随「拒绝已拒答的提示词」/「拒绝零输出请求类」开关；出站带 fallbacks 的请求不拦），规则文案里是「[类别] + 上游 stop_details 原样」——零输出规则的文案才是上游回复的开头。规则落库、重启保留，7 天后自动丢弃重新验证。拒答规则不设上限、按「模型 + 类别」折叠成一组，点开分页看每条、可整组删除；顶部可按模型 / 哈希 / 原话搜索，筛选到某一类时可只清空那一类。上游放开了而本地还在拦时，在这里删掉即可。',
        'Combinations upstream has called out are remembered: a value a model refuses (400) is rejected locally next time; a parameter a model deprecated (400) is stripped before forwarding; a tool-less single-message request class (model + max_tokens) that upstream answered with 200 and zero output tokens is rejected locally next time; a prompt the upstream classifier refused (stop_reason refusal with a stop_details category such as cyber) is rejected locally when resent verbatim, and only that one prompt, never other requests of the same shape; refusals the model made on its own (no category) and refusals whose fallback could not run (recommended_model present) are not learned (both 403, governed by "Reject refused prompts" / "Reject empty-reply request classes" respectively; requests going out with fallbacks are not blocked), with "[category] " plus the upstream stop_details verbatim kept as the rule text; only empty-reply rules keep the start of the upstream reply. Rules persist across restarts and expire after 7 days. Refused prompts are unbounded and folded into one group per model and category; expand a group to page through its prompts or remove the whole group, search by model / hash / text at the top, and with a kind filter active you can clear just that kind. If upstream has since allowed something, remove the rule here.',
      )}
    >
      {query.isPending ? (
        <div className="flex items-center gap-2 p-5 text-sm text-muted-foreground">
          <Spinner />
          {t('正在加载', 'Loading')}
        </div>
      ) : query.isError ? (
        <div className="flex flex-wrap items-center justify-between gap-2 p-5 text-sm">
          <span className="text-destructive">{extractError(query.error, language)}</span>
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
          <div className="flex flex-wrap items-center gap-x-3 gap-y-2 px-4 py-2.5">
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
            <div className="flex flex-wrap items-center justify-between gap-2 px-4 py-6 text-sm text-muted-foreground">
              {needle ? t('没有匹配的规则。', 'No matching rules.') : t('这一类下没有规则。', 'No rules of this kind.')}
              <Button size="xs" variant="outline" onClick={() => { setKindFilter('all'); setSearch('') }}>
                {t('看全部', 'Show all')}
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
