import { useEffect, useId, useState, type ReactNode } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  BadgeCheckIcon,
  ChevronDownIcon,
  DatabaseIcon,
  InfoIcon,
  KeyRoundIcon,
  RefreshCwIcon,
  SaveIcon,
  ServerIcon,
  SlidersHorizontalIcon,
  TerminalIcon,
} from 'lucide-react'
import {
  getSettings,
  setForwarding,
  setOauthScopes,
  setQuotaPausePct,
  setRateLimitRetryMax,
  type ForwardingKey,
  type Settings,
} from '@/api/settings'
import { useI18n } from '@/lib/i18n'
import { extractError } from '@/lib/utils'
import { Alert, AlertDescription, AlertTitle } from '@/components/ui/alert'
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
import { Field, FieldDescription, FieldLabel } from '@/components/ui/field'
import {
  NumberField,
  NumberFieldDecrement,
  NumberFieldGroup,
  NumberFieldIncrement,
  NumberFieldInput,
} from '@/components/ui/number-field'
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
            '同平台的所有客户端收敛为同一个设备标识，每个账号最多 2–3 个设备。',
            'Converge all clients on the same platform into one device identifier, limiting each account to 2–3 devices.',
          )}
          requires={{ key: 'spoof_device_id', label: t('改写设备标识', 'Rewrite device identifier') }}
          description={
            <>
              {t(
                '设备指纹用于派生每个账号的伪装设备标识。开启后指纹只取平台信息（CPU 架构与操作系统），不含客户端原始设备标识——同一平台上的所有客户端都会得到同一个伪装设备标识，每个账号最多只有 2–3 个设备（如 macOS/arm64、Linux/x86_64），符合真实用户一人多设备的使用模式。关闭后指纹包含客户端原始设备标识，每个（账号、客户端设备）组合都是一个独立的上游设备标识，客户端越多、上游看到该账号的设备数就越多。',
                'The device fingerprint is used to derive a spoofed device identifier per account. When enabled, the fingerprint uses only platform information (CPU architecture and operating system) and excludes the client’s original device identifier — all clients on the same platform share one spoofed device identifier, limiting each account to at most 2–3 devices (e.g. macOS/arm64, Linux/x86_64), which matches the usage pattern of a real user with multiple devices. When disabled, the fingerprint includes the client’s original device identifier, making each (account, client device) combination a separate upstream device identifier — the more clients there are, the more devices upstream sees for that account.',
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
              '上游每条响应都带着账号的用量限制使用率；到达阈值就把账号挪出调度池，不必等下一条请求去撞 429（那一发必定失败）。两个窗口各配一档，别混用：5 小时窗口停号最多歇几小时就自己回来，7 天窗口停号是歇到下个周重置——一个周用量偏高的号会被整段挪出池子，哪怕它这 5 小时一点没用。故 7 天那档默认关（周额度真用光时上游会回 429，账号级冷却照常接手）；要开建议配得比 5 小时那档更高。超额池快满不算在内。停用后按触发的那个窗口的重置时刻自动恢复，也可手动启用或用连通性测试放回。填 0 = 该档不停号。',
              'Every upstream response reports the account’s usage-limit utilization; once it reaches the threshold the account leaves the scheduling pool, instead of waiting for the next request to hit a 429 (which is bound to fail). Each window gets its own threshold — do not treat them as one: a pause from the 5h window lasts a few hours at most, while a pause from the 7d window lasts until the weekly reset, so an account with heavy weekly usage would sit out entirely even when its 5h window is untouched. That is why the 7d threshold is off by default (when the weekly quota really runs out, upstream returns a 429 and the account-level cooldown takes over); if you do enable it, set it higher than the 5h one. A nearly full overage pool never counts. A paused account comes back automatically when the window that triggered it resets, and can also be re-enabled by hand or by a passing connectivity test. 0 turns that threshold off.',
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
