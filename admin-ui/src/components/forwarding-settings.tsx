import { createContext, useContext, useEffect, useId, useMemo, useState, type ReactNode } from 'react'
import { useMutation, useQuery, useQueryClient } from '@tanstack/react-query'
import {
  ActivityIcon,
  BadgeCheckIcon,
  BracesIcon,
  BrainIcon,
  ChevronDownIcon,
  DatabaseIcon,
  FilterIcon,
  FingerprintIcon,
  KeyRoundIcon,
  LayersIcon,
  RefreshCwIcon,
  RotateCcwIcon,
  RouteIcon,
  SaveIcon,
  SearchIcon,
  ServerIcon,
  ShieldBanIcon,
  SlidersHorizontalIcon,
  TerminalIcon,
  Trash2Icon,
  type LucideIcon,
} from 'lucide-react'
import {
  clearLearnedRejections,
  forgetLearnedGroup,
  forgetLearnedRejection,
  listLearnedRejections,
  setForwarding,
  setForwardingMany,
  setOauthScopes,
  setQuotaPausePct,
  setRateLimitRetryMax,
  type ForwardingKey,
  type LearnedRejection,
  type Settings,
} from '@/api/settings'
import { useI18n } from '@/lib/i18n'
import { cn, extractError, formatFullTime, relativeTime } from '@/lib/utils'
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
import { Radio, RadioGroup } from '@/components/ui/radio-group'
import { Switch } from '@/components/ui/switch'
import { Textarea } from '@/components/ui/textarea'
import { Toggle } from '@/components/ui/toggle'
import { Hint, Tooltip, TooltipPopup, TooltipTrigger } from '@/components/ui/tooltip'
import { toastManager } from '@/components/ui/toast'
import { ClampedDescription, DetailsBody, DetailsTrigger, SettingsGroup, SettingsRow } from '@/components/settings-group'
import { useSettingsQuery, useSettingsSave } from '@/components/setting-controls'
import { ErrorState, LoadingState } from '@/components/state-placeholders'
import { ACTIVE_FILTER_CLASS, ToolbarSearch } from '@/components/toolbar-controls'
import { InfoHint } from '@/components/info-hint'

/**
 * 转发策略。
 *
 * 页首是改写模式（完整对齐 / 仅 billing 标识）与搜索、只看已改动；其下按请求经过的环节分组：
 * 身份与计费标识 → 请求头 → 请求体与系统提示词 → 非官方客户端模拟 → 遥测 → 本地拦截 →
 * 拒答换模型 → 限流与错误恢复，最后是登录授权范围。
 *
 * 这些改动都不是「能不能用」的必需项，每一项都可以单独停用，用于排查上游兼容性。客户端自己
 * 写出的参数错误一律不修补，由上游原样返回官方报错。
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
              '配置身份、请求形态、本地拦截、限流与错误恢复策略。',
              'Configure identity, request shape, local interception, rate limiting and error recovery policies.',
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
  const settingsQuery = useSettingsQuery()
  const [query, setQuery] = useState('')
  const [changedOnly, setChangedOnly] = useState(false)
  const filter = useMemo(() => ({ query, changedOnly }), [query, changedOnly])

  if (settingsQuery.isPending) {
    return <LoadingState label={t('正在加载设置', 'Loading settings')} />
  }

  if (settingsQuery.isError) {
    return (
      <ErrorState
        error={settingsQuery.error}
        title={t('无法读取当前设置', 'Unable to load current settings')}
        onRetry={() => settingsQuery.refetch()}
        retrying={settingsQuery.isFetching}
      />
    )
  }

  // 模拟的各子项：「模拟 Claude Code」自己又依赖 Beta 标记（后端 merge_beta 关着即不模拟），
  // 两层都得列上，否则 Beta 一关，子项仍拨得动却不生效。
  const simulateRequires = [
    {
      key: 'simulate_cc' as const,
      label: t('模拟 Claude Code', 'Emulate Claude Code'),
    },
    {
      key: 'merge_beta' as const,
      label: t('请求头 · Beta 标记', 'Request headers · Beta flags'),
    },
  ]

  return (
    <ForwardingFilterContext.Provider value={filter}>
    <div className="space-y-4" data-forwarding-list>
      <ForwardingToolbar
        query={query}
        onQueryChange={setQuery}
        changedOnly={changedOnly}
        onChangedOnlyChange={setChangedOnly}
      />

      <Empty className="rounded-xl border border-dashed py-10" data-forwarding-empty>
        <EmptyHeader>
          <EmptyMedia variant="icon"><SearchIcon /></EmptyMedia>
          <EmptyTitle>{changedOnly && !query.trim()
            ? t('所有开关均为默认值', 'Every switch is at its default')
            : t('没有匹配的设置项', 'No matching settings')}</EmptyTitle>
          <EmptyDescription>
            {changedOnly && !query.trim()
              ? t('当前没有改动过的开关。', 'No switch has been changed.')
              : t('换个关键词，或清除筛选后再看。', 'Try another keyword, or clear the filters.')}
          </EmptyDescription>
        </EmptyHeader>
      </Empty>

      <RewriteModeGroup
        simulateRequires={simulateRequires}
        userIdToggles={
          <>

            <ForwardingToggle
              k="sim_billing_keep_user_id"
              label={t('模拟请求带 user_id', 'Send user_id on emulated requests')}
              summary={t(
                '自带者依身份规则改写，缺失者按官方形态补齐；停用后一并移除。',
                'An existing one is rewritten by the identity rules, a missing one is added in the official shape; when disabled it is removed.',
              )}
              description={
                <>
                  {t(
                    '启用（默认）：客户端携带的 user_id 予以保留，并照常按「身份一致性」「改写设备 ID」「设备指纹归一化」的规则处理——account_uuid 替换为池中账号，device_id 按设备指纹派生，会话段与出站会话 ID 请求头对齐；「身份一致性」停用时原样发送。客户端未携带时，按官方形态补一份（账号为池中账号、设备 ID 按设备指纹派生、会话段即出站会话 ID），需「身份一致性」启用。官方每条请求都带 user_id，抓包中「仅 billing 标识、无身份句」的续轮也不例外。停用：user_id 整体剥离（metadata 剥离后为空则一并移除）。只作用于模拟请求（第三方客户端，以及形态未被认作官方的请求），真实 Claude Code 客户端由「真实客户端带 user_id」单独控制。',
                    'Enabled (default): a user_id sent by the client is kept and handled by the usual “Identity consistency”, “Rewrite device ID” and “Normalize device fingerprint” rules: account_uuid is replaced with the pool account, device_id is derived from the device fingerprint, and the session segment matches the outgoing session ID header; with “Identity consistency” disabled it is sent unchanged. When the client sends none, one in the official shape is added (pool account, device ID derived from the device fingerprint, session segment equal to the outgoing session ID), which requires “Identity consistency” to be enabled. The official client sends user_id on every request, including the billing-only continuation turns without an identity line in the captures. Disabled: user_id is stripped entirely (metadata is removed too if left empty). Applies only to emulated requests (third-party clients and requests whose shape is not recognized as official); real Claude Code clients are controlled separately by “Send user_id on real clients”.',
                  )}
                </>
              }
            />
            <ForwardingToggle
              k="real_billing_keep_user_id"
              label={t('真实客户端带 user_id', 'Send user_id on real clients')}
              summary={t(
                '自带者依身份规则改写，缺失者按官方形态补齐；停用后一并移除。',
                'An existing one is rewritten by the identity rules, a missing one is added in the official shape; when disabled it is removed.',
              )}
              description={
                <>
                  {t(
                    '启用（默认）：客户端携带的 user_id 予以保留，并照常按「身份一致性」「改写设备 ID」「设备指纹归一化」的规则处理——account_uuid 替换为池中账号，device_id 按设备指纹派生，会话段与出站会话 ID 请求头对齐；「身份一致性」停用时原样发送。客户端未携带时，按官方形态补一份（账号为池中账号、设备 ID 按设备指纹派生、会话段即出站会话 ID），需「身份一致性」启用。官方每条请求都带 user_id，抓包中「仅 billing 标识、无身份句」的续轮也不例外。停用：user_id 整体剥离（metadata 剥离后为空则一并移除）。只作用于真实 Claude Code 客户端，启用期间代替「补齐设备身份」。官方本就有不带 user_id 的形态（Claude Desktop 不带，Claude Code 也可通过环境变量不带），剥离同样是官方会出现的形态。',
                    'Enabled (default): a user_id sent by the client is kept and handled by the usual “Identity consistency”, “Rewrite device ID” and “Normalize device fingerprint” rules: account_uuid is replaced with the pool account, device_id is derived from the device fingerprint, and the session segment matches the outgoing session ID header; with “Identity consistency” disabled it is sent unchanged. When the client sends none, one in the official shape is added (pool account, device ID derived from the device fingerprint, session segment equal to the outgoing session ID), which requires “Identity consistency” to be enabled. The official client sends user_id on every request, including the billing-only continuation turns without an identity line in the captures. Disabled: user_id is stripped entirely (metadata is removed too if left empty). Applies only to real Claude Code clients, and replaces “Fill missing device identity” while billing-only is enabled. Official clients already send requests without user_id (Claude Desktop never does, and Claude Code can omit it via an environment variable), so stripping is also a shape the official client produces.',
                  )}
                </>
              }
            />
          </>
        }
        description={
          <>
                  {t(
                    '上游放行须满足两项条件之一：请求 system 中带有身份句，或 system 首块中带有合法的 billing 标识（含 cc_version 与 cc_entrypoint，cch 可选）。本开关使模拟请求仅满足后者——在 system 首块注入一条 billing 标识（与完整模拟同一份：cc_version、cc_entrypoint、cch，另带会话关联的 cc_prompt_id、cc_turn_origin、轮次，续轮再带 cc_prev_req，与请求头的 prompt id 同值；cch 跟随「模拟请求计算计费校验值」：启用时按出站请求体算真值，停用时填随机值），其余注入一律跳过：不补身份句、不补官方基座与第四块、不注官方工具、不写 thread/diagnostics/output_config、不重排顶层键；metadata.user_id 由「模拟请求带 user_id」决定。客户端自带的 system 块、工具与参数原样透传，客户端自带的 fallbacks 字符串仍归一为官方数组；换头（官方 UA 等）照常进行。本开关对真实 Claude Code 客户端（含 VSCode 扩展、agent-sdk、子代理）同样生效：其自带的 billing 标识照原样发送（cch 按「计费校验值」开关照旧重算），缺失时只补 billing 标识、不补身份句；metadata.user_id 由「真实客户端带 user_id」决定（代替「补齐设备身份」）；会话关联字段（cc_prompt_id、diagnostics 等）、system 分块与消息断点整形、thinking.display、eager_input_streaming、多余字段剥除一律跳过；「工具名混淆」照常生效，范围由其子开关「改写全部非官方工具名」决定。收益：第三方客户端借用订阅额度时可保留自身的提示词与行为，模型依据客户端的 system 正常作答，不会被赋予 Claude Code 的角色设定。权衡：请求头按官方主线程发送（请求类别 main、带 prompt id），billing 标识随之采用主线程形态——官方主线程首条带 cc_prompt_id 起诸段、续轮再带 cc_prev_req，从不只发三段（2.1.303～2.1.306 抓包核对），字段、次序与版本后缀算法均与官方一致；system 其余部分为客户端自有内容，不带身份句、基座与 thread，故形态近于官方而非逐项相同。实测（2026-10-10）：客户端 system 长达 12KB、system 多达 9 块，均照常计入订阅额度，未被判为第三方应用。建议按需启用。与完整模拟并存，默认停用，可随时回退。',
                    'Upstream admits a request that meets either of two conditions: an identity line in the request system, or a valid billing identifier in the first system block (with cc_version and cc_entrypoint; cch optional). This switch makes emulated requests meet only the second — it injects a billing identifier as the first system block (the same one full emulation uses: cc_version, cc_entrypoint and cch, plus the session-chain fields cc_prompt_id, cc_turn_origin and the turn indices, with cc_prev_req added on follow-up turns, carrying the same prompt id as the request header; cch follows “Compute billing checksum for emulated requests”: computed from the outgoing body when enabled, a random value when disabled) and skips every other injection: no identity line, no official base prompt or fourth block, no official tools, no thread/diagnostics/output_config, and no top-level key reordering; metadata.user_id is governed by “Send user_id on emulated requests”. The client’s own system blocks, tools and parameters pass through unchanged; a client-supplied fallbacks string is still normalized to the official array, and header rewriting (official UA, etc.) still applies. The switch applies equally to real Claude Code clients (including the VSCode extension, agent-sdk and subagents): their own billing identifier is sent as-is (cch is still recomputed per the billing checksum switch), and when it is missing only a billing identifier is added, without the identity line; metadata.user_id is governed by “Send user_id on real clients” (in place of “Fill missing device identity”); session-chain fields (cc_prompt_id, diagnostics, etc.), system block and message breakpoint shaping, thinking.display, eager_input_streaming and stripping extra fields are all skipped; tool-name obfuscation still applies, with its scope set by the sub-switch “Obfuscate all non-official tool names”. Benefit: a third-party client using subscription quota keeps its own prompt and behavior, and the model answers according to the client’s system prompt without taking on the Claude Code persona. Trade-off: request headers are sent as an official main-thread request (request class main, with a prompt id), so the billing identifier takes the main-thread shape as well — an official main thread’s first request carries cc_prompt_id and the fields after it, follow-up turns add cc_prev_req, and it never sends only three fields (checked against the 2.1.303–2.1.306 captures); the fields, their order and the version-suffix algorithm all match the official client. The rest of the system is the client’s own content, with no identity line, base prompt or thread, so the shape is close to the official one rather than identical. Tested on 2026-10-10: client system prompts up to 12KB and up to 9 system blocks were billed against plan limits as usual and were not classified as third-party apps. Enable it only as needed. It coexists with full emulation, is disabled by default, and can be reverted at any time.',
                  )}
          </>
        }
      />

      <ForwardingGroup
        icon={FingerprintIcon}
        title={t('设备与会话身份', 'Device & session identity')}
        description={t('请求中的账号、设备与会话 ID 如何改写，使上游看到的身份与当前账号一致。', 'How the account, device and session IDs in a request are rewritten so upstream sees an identity consistent with the current account.')}
      >
        <ForwardingToggle
          k="spoof_identity"
          label={t('身份一致性', 'Identity consistency')}
          summary={t(
            '让客户端身份与当前账号、设备保持一致；停用后原样转发。',
            'Keep the client identity consistent with the current account and device; when disabled, forward it unchanged.',
          )}
          description={
            <>
              {t(
                '把请求里的账号 ID 与设备 ID 改写为当前账号的值，并把客户端的会话 ID 按账号派生为另一个稳定值：同一条会话在同一个账号上始终得到同一个值，多轮对话与缓存照常接续；换到其他账号则得到不同的值。若不改写，设备因账号停用或限流被改绑到其他账号时，同一个会话 ID 会带着两个设备 ID 先后出现在两个账号下。而官方客户端的一条会话只属于一个账号、一台设备；封号复盘中，两个账号的会话正是以这种方式在半分钟内先后出现在两个组织下。请求头与 metadata 两处始终写入同一个值。',
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
                '官方客户端的每条请求都带设备身份，缺失本身即构成一处差异，常见于模仿 Claude Code 的第三方客户端。补齐的身份与「身份一致性」使用同一套取值；请求已带会话 ID 时优先沿用。',
                'The official client includes a device identity with every request, so a missing identity is itself a discrepancy; this is common in third-party clients that imitate Claude Code. The generated identity uses the same values as “Identity consistency”, while a session ID already present in the request takes precedence.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="spoof_device_id"
          label={t('改写设备 ID', 'Rewrite device ID')}
          summary={t(
            '请求自带设备 ID 时，替换为按当前账号派生的值；停用后原样保留。',
            'Replace a device ID sent by the client with one derived from the current account; when disabled, pass it through unchanged.',
          )}
          requires={{ key: 'spoof_identity', label: t('身份一致性', 'Identity consistency') }}
          description={
            <>
              {t(
                '官方客户端的设备 ID 是「机器标识」：同一台机器无论使用哪个账号，发送的都是同一个值；官方在 API key 与订阅两种模式下发送的也完全相同，两种模式真正的差别只在账号 ID 那一段。因此替换设备 ID 并非形态上的需要，而是一项防关联措施：启用后，每个账号在同一台机器上各有独立的设备 ID，账号之间不会因共用一个 ID 而被关联。代价：真实用户中常见的「一台机器多个账号」，在经由本代理的流量中将完全不会出现。停用后与官方逐字节一致，但同一台机器上的多个账号可被上游关联。请求未携带设备 ID 时一律派生，不受本开关影响。',
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
                '设备指纹用于派生每个账号的伪装设备 ID。启用后，指纹只取平台信息（CPU 架构与操作系统），不含客户端原始设备 ID，同一平台上的客户端会收敛成同一个伪装设备 ID，符合真实用户一人多设备的使用模式。停用后，指纹包含客户端原始设备 ID，每个（账号、客户端设备）组合都对应一个独立的上游设备 ID；客户端越多，上游看到该账号的设备数就越多。无论开关如何，指纹都包含实际发往上游的客户端版本：一台设备只能有一个版本，否则上游会看到同一个设备 ID 在同一秒里自报多个版本（这是封号复盘中最明显的特征）。代价：客户端升级后设备 ID 会随之更换，在上游看来相当于这台机器重装了一次，但远比版本来回切换安全。启用后，账号名额改按会话计算，设备上限不再生效：上游已看不出绑定了几台真实设备，能看出的是同时活跃的会话数。',
                'The device fingerprint is used to derive a spoofed device ID per account. When enabled, the fingerprint uses only platform information (CPU architecture and operating system) and excludes the client’s original device ID, so clients on the same platform converge onto one spoofed device ID, matching the usage pattern of a real user with multiple devices. When disabled, the fingerprint includes the client’s original device ID, making each (account, client device) combination a separate upstream device ID; the more clients there are, the more devices upstream sees for that account. Either way the fingerprint also includes the client version actually sent upstream: one device may only ever report one version, otherwise upstream sees a single device ID claiming several versions within the same second (the most obvious signal in the ban post-mortem). Cost: a client upgrade rotates the device ID, which upstream reads as the machine being reinstalled, but that is far safer than a version that flips back and forth. While enabled, account slots are counted per session and the device limit no longer applies: upstream can no longer tell how many real devices are bound, only how many sessions are active at once.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="reject_session_conflict"
          label={t('拒绝会话 ID 冲突', 'Reject session ID conflicts')}
          summary={t(
            '请求头与 metadata 中的会话 ID 不一致时，于本地返回 400，不代为取舍。',
            'When the session IDs in the header and metadata disagree, reject locally with 400 rather than choosing one.',
          )}
          description={
            <>
              {t(
                '官方 Claude Code 在 X-Claude-Code-Session-Id 与 metadata.user_id 两处发送的是同一个值，逐字相同。两处给出两个各自合法却不同的 UUID，是官方从不产生的形态；而 luban 以会话 ID 作为会话链（cc_prompt_id / cc_prev_req / diagnostics.previous_message_id）的键，一旦选错，两条会话链会被错误地拼接在一起，且事后无从察觉。启用后，这类请求在本地返回 400，错误消息中列出两个值。停用后退回「取请求头里的值 + 记一条 warn 日志」。只有一处合法时不算冲突：这表示客户端只有一处填写正确，照常采用合法的那个值。',
                'Official Claude Code sends the same value in X-Claude-Code-Session-Id and in metadata.user_id, byte for byte. Two different but individually valid UUIDs is a shape the official client never produces, and luban keys the session chain (cc_prompt_id / cc_prev_req / diagnostics.previous_message_id) on the session ID; picking the wrong one splices two chains together with no way to notice afterwards. When enabled such requests are rejected locally with 400, naming both values. Turn it off to fall back to using the header value and logging a warning. If only one of the two is a valid UUID it is not a conflict: the client simply got one of them right, and that one is used.',
              )}
            </>
          }
        />
      </ForwardingGroup>

      <ForwardingGroup
        icon={BadgeCheckIcon}
        title={t('计费标识', 'Billing identifier')}
        description={t('订阅请求 system 首块中的计费标识及其校验值。', 'The billing identifier in the first system block of subscription requests, and its checksum.')}
      >
        <ForwardingToggle
          k="billing_cch"
          label={t('订阅计费标识', 'Subscription billing identifier')}
          summary={t(
            '官方客户端以 API key 模式接入时，计费标识不含 cch，按订阅形态予以补齐。',
            'In API-key mode the official client’s billing identifier lacks cch; add it to match the subscription shape.',
          )}
        />
        <ForwardingToggle
          k="cch_real_recompute"
          label={t('官方客户端 · 重算计费校验值', 'Official clients · Recompute billing checksum')}
          summary={t(
            '官方客户端的请求体被改写后，按最终发出的请求体重新计算计费标识中的 cch。',
            'When an official client’s request body has been rewritten, recompute the cch in the billing identifier from the body actually sent.',
          )}
          description={
            <>
              {t(
                'cch 是官方客户端在发出请求前，对整条请求体计算的校验值（xxHash64 取低 20 位），请求体改动一个字节它就随之改变。官方客户端自带的 cch 对应的是它自己发出的那份请求体；luban 一旦改写请求体（身份一致性、工具补齐、工具名混淆等），这个值就与实际发往上游的请求体对不上。启用后，请求体被改写过的请求会按最终发出的字节重新计算 cch，luban 替客户端补上的计费标识同样按此计算；未经改写的请求原样转发，自带值本来就是对的。停用后，客户端自带的 cch 原样保留（改写后与请求体不符），luban 补上的那条填入随机值。',
                'cch is a checksum the official client computes over the whole request body just before sending (the low 20 bits of xxHash64), so it changes whenever a single byte of the body does. The cch an official client sends matches the body it produced; once luban rewrites the body (identity consistency, tool filling, tool name mimicry and so on) that value no longer matches what is actually sent upstream. When enabled, rewritten requests get their cch recomputed from the final outgoing bytes, and the billing identifier luban adds for a client is computed the same way; requests that were not rewritten are forwarded as is, since their own value is already correct. When disabled, the client’s own cch is kept unchanged (no longer matching the rewritten body), and the one luban adds is filled with a random value.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="cch_sim_compute"
          label={t('模拟请求 · 计算计费校验值', 'Emulated requests · Compute billing checksum')}
          summary={t(
            '依据实际发出的请求体计算 cch，与官方算法一致；停用后改填随机值。',
            'Compute cch from the body actually sent, as the official client does; when disabled, a random value is used.',
          )}
          requires={simulateRequires}
        />
      </ForwardingGroup>

      <ForwardingGroup
        icon={ServerIcon}
        title={t('请求头', 'Request headers')}
        description={t('出站请求头的取值、拼写与顺序。', 'Values, spelling and order of outbound request headers.')}
      >
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
            '还原官方客户端的请求头拼写与顺序；仅在排查兼容问题时停用。',
            'Restore the official client’s request header casing and order; disable only when troubleshooting compatibility issues.',
          )}
        />
      </ForwardingGroup>

      <ForwardingGroup
        icon={BracesIcon}
        title={t('请求体形态', 'Request body shape')}
        description={t('请求体中与官方客户端存在差异的字段与写法。', 'Request body fields and forms that differ from the official client.')}
      >
        <ForwardingToggle
          k="nonstream_as_sse"
          label={t('非流式请求流式化', 'Upgrade non-streaming requests')}
          summary={t(
            '将非流式请求改为流式发往上游，响应仍以非流式整段返回，对客户端透明。',
            'Send non-streaming requests upstream as streaming ones and return the response as a single non-streaming body, transparently to the client.',
          )}
          description={
            <>
              {t(
                '官方客户端的对话请求一律是流式的，原样转发非流式请求会形成一处稳定特征。启用后，luban 仅修改请求中的 stream 字段，将收到的流式响应在本地拼接为完整内容，再按客户端原本期待的格式返回，请求头与返回格式都不变。上游中途报错时，错误原文按非流式请求应有的状态码返回，客户端的错误处理不受影响。代价：响应须等上游全部生成完毕后才返回（与非流式原本的行为一致），且整段内容需在内存中暂存。请求明细里这类记录会标注「非流转流」，因为其首字耗时记录的是上游首字节，与客户端的感知不同。仅作用于对话请求，token 计数接口不受影响。',
                'The official client always sends conversation requests as streaming ones, so a non-streaming request forwarded as-is is a stable tell. When enabled, luban only flips the stream field in the request, reassembles the streamed response locally, and returns it in the format the client already expected — request headers and response format are unchanged. If the upstream errors mid-stream, the raw error is returned with the status code a non-streaming request would have received, so client error handling is unaffected. Costs: the response is sent only after the upstream finishes generating (same as non-streaming behaviour anyway) and the whole body is buffered in memory; such records are tagged “stream-upgraded” in the request log, because their TTFT is the upstream first byte rather than what the client perceived. Applies to conversation requests only; the token-counting endpoint is untouched.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="strip_extra_fields"
          label={t('移除多余字段', 'Strip extra fields')}
          summary={t(
            '删除官方客户端从不发送的请求字段；参数错误不做修补，由上游原样返回。',
            'Remove request fields the official client never sends; parameter errors are not repaired and upstream returns them as is.',
          )}
          description={
            <>
              {t(
                '官方客户端的对话请求字段是固定的一套，多出的字段会构成一处稳定特征，可能导致请求被判为第三方应用而改扣超额用量。启用后，luban 只删除语义等于默认值的 tool_choice（客户端强制指定工具或关闭并行调用时保持不变）。thinking.display 不再删除：它是官方请求的一部分，客户端写了 summarized 就照常拿到思考摘要，没写的按官方默认补 updates。客户端自己写出的参数错误（如与 thinking 冲突的 temperature、budget_tokens 不足 1024）不做修补，原样发出，由上游返回官方的 400。真实的官方客户端本来就不发送多余字段，启用本项对其基本没有影响。',
                'The official client sends a fixed set of fields on conversation requests; anything extra is a stable tell and can get the request classified as a third-party app, drawing from extra usage instead of plan limits. When enabled, luban removes only a tool_choice whose meaning equals the default (a forced tool choice or disabled parallel calls is left alone). thinking.display is no longer removed: it is part of the official request, a client that writes summarized gets its reasoning summaries as usual, and one that writes nothing gets the official default of updates. Parameter errors the client makes itself (such as a temperature that conflicts with thinking, or a budget_tokens below 1024) are not repaired: they go out as is and upstream answers with its own 400. The real official client never sends extra fields, so enabling this is essentially a no-op for it.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="tool_name_mimic"
          label={t('工具名混淆', 'Tool name obfuscation')}
          summary={t(
            '易被判为第三方应用的工具名改以 MCP 别名转发，响应时自动还原。',
            'Tool names that would flag a third-party app are forwarded as MCP aliases and restored in responses.',
          )}
          description={
            <>
              {t(
                '工具名是上游判断第三方应用的一项已验证判据：命中后上游返回「Third-party apps now draw from your extra usage」，即使订阅用量充足也改扣超额用量。经测试，3 个业务工具名即足以触发，而以 `mcp__` 开头的名字会被豁免。启用后，luban 把这些名字替换成 `mcp__luban__*` 下的稳定别名再发出，并在响应中换回真名，客户端自始至终看到的都是自己的工具名。官方自带的工具、客户端本就以 MCP 命名的工具和服务端工具都保留原名，因此对真实的官方客户端没有任何影响。代价：响应内容需额外进行一次字符串替换；客户端在会话中途增删工具时，别名会整体重新计算，上游的提示词缓存随之失效一次。「仅 billing 标识」模式下本项照常生效，范围由子开关「改写全部非官方工具名」决定。',
                'Tool names are one verified signal the upstream uses to classify third-party apps; a match returns “Third-party apps now draw from your extra usage” and bills against extra usage even when plan usage remains. In testing, three business tool names were enough to trigger it, while names beginning with `mcp__` are exempt. When enabled, luban forwards affected names as stable aliases under `mcp__luban__*` and restores them in responses, so the client only sees its own names. Official tools, tools that already use MCP names, and server tools remain unchanged, making this a no-op for the genuine official client. Costs: one extra string replacement pass over responses, and adding or removing tools mid-session recomputes aliases and invalidates the upstream prompt cache once. In “billing identifier only” mode this switch still applies; its scope is set by the sub-switch “Obfuscate all non-official tool names”.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="sim_billing_mimic_all"
          label={t('改写全部非官方工具名', 'Obfuscate all non-official tool names')}
          summary={t(
            '仅 billing 标识模式下，工具名混淆与完整对齐同一套规则；停用后只改以 mcp_ 开头的工具名。',
            'In billing-identifier-only mode, tool name obfuscation follows the same rules as full alignment; when disabled only names beginning with mcp_ are renamed.',
          )}
          requires={{ key: 'tool_name_mimic', label: t('工具名混淆', 'Tool name obfuscation') }}
          description={
            <>
              {t(
                '只在「仅 billing 标识」模式下有意义。启用（默认）：与完整对齐同一套规则——不在官方工具名集合内、也不以 mcp__ 开头的客户端工具一律改为 mcp__luban__ 下的稳定别名，响应中换回真名。实测（2026-10-16）：同一天另外几款第三方产品的工具名集合，在描述与 system 全为中性文字的情况下，7 组中有 4 组仅凭名字即被判为第三方应用，判据不止一条。停用：只把以 mcp_（单下划线）开头的工具名改为 mcp__luban__ 加原名，其余客户端工具原样透传。这类名字单独即触发第三方判定，与描述文字、模型无关；别名直接拼原名，增删其他工具不影响它，改动最小，但其他产品的名字判据不在覆盖之内。',
                'Only meaningful in “billing identifier only” mode. Enabled (default): the same rules as full alignment: every client tool whose name is neither an official tool name nor prefixed with mcp__ is forwarded as a stable alias under mcp__luban__ and restored in responses. Tested 2026-10-16: with neutral descriptions and a neutral system prompt, 4 of 7 tool-name sets from other third-party products seen the same day were classified as third-party apps by their names alone, so there is more than one rule. Disabled: only names beginning with mcp_ (single underscore) are renamed to mcp__luban__ plus the original name, and other client tools pass through unchanged. Such a name alone triggers the classification regardless of description or model; the alias is the original name, so adding or removing other tools does not affect it. This is the smallest change, but the name rules of other products are not covered.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="sim_billing_relocate"
          label={t('仅 billing 标识下也挪客户端 system', 'Also move client system in billing-only mode')}
          summary={t(
            '仅 billing 标识模式下，客户端 system 合计超过 1900 字节时挪进首条用户消息；默认停用。',
            'In billing-identifier-only mode, move the client system into the first user message when it exceeds 1900 bytes in total; disabled by default.',
          )}
          description={
            <>
              {t(
                '只在「仅 billing 标识」模式下有意义。上游对 system 正文的内容级检测在这一模式下同样生效：实测（2026-10-16）同一段 OpenClaw 提示词放在 system 中返回第三方应用 400，挪进用户消息则正常计入订阅用量。停用（默认）：客户端 system 各块原样透传，与本模式「只补 billing 标识」的口径一致。启用：billing 标识之后的客户端各块正文按空行拼成一段，裹成 <system-reminder> 块插在首条用户消息最前，system 只剩 billing 标识加一行占位，占位继承原末块的缓存断点；请求明细的 rewrites 列标注 sys_relocated。代价与完整对齐下的同名开关相同。',
                'Only meaningful in “billing identifier only” mode. The upstream content check on the system prompt applies here as well: tested 2026-10-16, the same OpenClaw prompt in system returned the third-party 400, while moved into the user message it was billed against the plan as usual. Disabled (default): the client system blocks pass through unchanged, consistent with this mode only adding the billing identifier. Enabled: the client blocks after the billing identifier are joined with blank lines, wrapped as a <system-reminder> block at the start of the first user message, and system keeps only the billing identifier plus a one-line placeholder that inherits the last block’s cache breakpoint; such records are tagged sys_relocated in the request log. Costs are the same as the switch of the same name under full alignment.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="reject_known_third_party"
          label={t('拦截已知第三方特征', 'Block known third-party signatures')}
          summary={t(
            '出站请求命中已验证必被判为第三方应用的特征时，由 luban 直接返回同样的 400，不再发往上游。',
            'When an outbound request carries a signature verified to be classified as a third-party app, luban returns the same 400 itself instead of sending it upstream.',
          )}
          description={
            <>
              {t(
                '判据来自线上流水与本地对拍（2026-10-16），每条都已验证为必定触发：一是 OpenClaw 系统提示词中的两个原文片段「[[reply_to_current]]」与「stripped before rendering」同时出现在 system 中，二是以 mcp_（单下划线）开头的工具名。判定看的是发往上游的请求：被「超长客户端 system 挪入首条消息」挪走的、被「工具名混淆」改名的都不再命中，本项只拦那两步覆盖不到的情形，如不足 1900 字节的短提示词或混淆停用时。启用（默认）：命中即在本地返回与上游逐字相同的 400，请求明细标注 rejected_locally:third-party，账号不再为这类请求反复留下第三方记录。停用：照常发往上游，由上游拒绝。完整对齐与「仅 billing 标识」两种模式下均生效。',
                'The rules come from production logs and local comparison (2026-10-16), each verified to trigger every time: two verbatim fragments of the OpenClaw system prompt, “[[reply_to_current]]” and “stripped before rendering”, appearing together in system, and any tool name beginning with mcp_ (single underscore). The check runs on the request as it would be sent upstream: a system moved by “Move long client system into the first message” or a name rewritten by “Tool name obfuscation” no longer matches, so this only catches what those two cannot, such as prompts under 1900 bytes or obfuscation being disabled. Enabled (default): a match returns the same 400 as the upstream, verbatim, from luban itself; such records are tagged rejected_locally:third-party and the account stops accumulating third-party rejections for them. Disabled: the request is sent upstream and rejected there. Applies in both full alignment and “billing identifier only” modes.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="eager_tool_streaming"
          label={t('工具声明对齐 eager 流式', 'Match official eager tool streaming')}
          summary={t(
            '为工具声明补充 eager_input_streaming，仅限抓包证实过的组合。',
            'Add eager_input_streaming to tool declarations, only for combinations confirmed by captures.',
          )}
          requires={{
            key: 'merge_beta',
            label: t('请求头 · Beta 标记', 'Request headers · Beta flags'),
          }}
          description={
            <>
              {t(
                '官方订阅客户端主线程请求里的每个内建工具都带 eager_input_streaming:true，API key 模式一个都不带，这是两种模式之间在每个工具上重复出现的一处固定差异。只对抓包证实过的组合补齐：2.1.258 的四个模型族、2.1.260 的 opus、2.1.270 的 sonnet 予以补齐；2.1.260 的 fable 已证实不带该字段，不予补齐；没有样本的组合不做推测。真实的 Claude Code 请求按客户端自报的版本、模型与请求用途判断；模拟请求按实际出站的模拟 profile 判断。客户端自行写入该字段时（无论 true 还是 false）不予覆盖；MCP 工具、延迟加载占位与服务端工具没有样本，保持不变。该字段与上游的 advanced-tool-use beta 同时出现，因此依赖「Beta 标记」开关。收益是缩小声明差异，对封号率的影响幅度尚未测量。',
                'Every built-in tool in a main-thread request from the official subscription client carries eager_input_streaming:true, while API-key mode sends none: a fixed per-tool difference between the two modes. The fill only covers combinations confirmed by captures: all four model families on 2.1.258, opus on 2.1.260 and sonnet on 2.1.270 are filled; fable on 2.1.260 is confirmed absent and left alone; combinations without a sample are not guessed. Real Claude Code requests are judged by the client’s reported version, model and request purpose; emulated requests by the emulated profile actually sent upstream. A value the client wrote itself (true or false) is never overwritten; MCP tools, deferred-loading placeholders and server tools have no samples and are left untouched. The field appears together with the upstream advanced-tool-use beta, hence the dependency on “Beta flags”. The benefit is a smaller declaration gap; the effect on ban rates has not been measured.',
              )}
            </>
          }
        />
      </ForwardingGroup>

      <ForwardingGroup
        icon={DatabaseIcon}
        title={t('系统提示词与缓存', 'System prompt & caching')}
        description={t('系统提示词的分块方式与缓存断点。', 'How the system prompt is split into blocks, and its cache breakpoints.')}
      >
        <ForwardingToggle
          k="system_shape"
          label={t('分块与缓存形态', 'Block shape & caching')}
          summary={t(
            '按官方客户端对齐系统提示词的分块与缓存断点；同时将块数上限设为 4 块。',
            'Align system prompt blocks and cache breakpoints with the official client, and cap the block count at 4.',
          )}
          description={
            <>
              {t(
                '只调整分块，不改变提示词文本（缓存时长由「缓存时长对齐 1h」单独控制）。这不只是缓存优化：官方客户端的系统提示词始终是 4 块，超出 4 块会被上游判为第三方应用，改从超额用量（extra usage）而不是订阅用量扣费，因此超出的块会合并到第 4 块。无法识别切分点时原样转发。',
                'Only block boundaries are adjusted; the prompt text is unchanged (cache duration is governed separately by “Match official cache duration”). This is not merely a cache optimization: the official client always sends exactly 4 system blocks, and anything beyond that is treated upstream as a third-party app and billed to extra usage instead of your plan, so surplus blocks are merged back into the fourth. Requests are forwarded unchanged when no split point can be identified.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="cache_scope_global"
          label={t('基座缓存跨账号共享', 'Share base-prompt cache across accounts')}
          summary={t(
            '为官方基座块标记 scope:"global"，使所有账号共用同一份基座缓存。',
            'Mark the official base prompt block with scope:"global" so every account shares one cached copy.',
          )}
          requires={{
            key: 'merge_beta',
            label: t('请求头 · Beta 标记', 'Request headers · Beta flags'),
          }}
          description={
            <>
              {t(
                '基座是按模型族固定的官方提示词，全网只有一份；标记后各账号命中同一份缓存，省去重复写入。官方客户端总是把 scope 和 ttl:1h 一起发送，所以本项与「缓存时长对齐 1h」同时启用才是官方形态；只停用其中一项，发出的将是官方不会产生的组合。该标记需要上游的 prompt-caching-scope beta，因此依赖「Beta 标记」开关。',
                'The base prompt is a fixed official block per model family — identical everywhere — so marking it lets all accounts hit one cached copy instead of each paying its own cache write. The official client always sends scope together with ttl:1h, so this and “Match official cache duration” form the official shape only when both are on; turning off just one emits a combination the official client never produces. The marker requires the upstream prompt-caching-scope beta, hence the dependency on “Beta flags”.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="cache_ttl_1h"
          label={t('缓存时长对齐 1h', 'Match official cache duration')}
          summary={t(
            '为缓存断点写入 ttl:"1h"，与官方一致；停用后沿用客户端自带的时长。',
            'Write ttl:"1h" on cache breakpoints to match the official client; when disabled, keep whatever duration the client sent.',
          )}
          requires={{
            key: 'merge_beta',
            label: t('请求头 · Beta 标记', 'Request headers · Beta flags'),
          }}
          description={
            <>
              {t(
                '官方订阅客户端的 3 个缓存断点都带 ttl:"1h"，而 API key 模式发送的是不带时长的裸断点。这正是两种模式之间的真实差别之一，不写入则每条请求都会带有一处固定差异。代价：1h 缓存的写入单价是默认 5 分钟缓存的 2 倍。是否更划算取决于使用频率：长会话里 1h 往往更省（若 5 分钟内未继续对话，下一轮需按写入价重新写入整段前缀），零散的一次性请求只会增加费用。与官方一致，只有主对话（含「猜下一句」）写入 1h，子代理、分叉与预热请求保持不带时长的断点；是否写入取决于请求类别，与系统提示词能否分块无关，桌面端等基座不同的客户端同样生效。启用时同一请求内客户端自带的 5m 会一并升为 1h，因为上游要求时长按 tools → system → messages 顺序不增。停用后 luban 不改动任何字节，沿用客户端传入的时长。该字段需要上游的 extended-cache-ttl beta，因此依赖「Beta 标记」开关。',
                'All three cache breakpoints from the official subscription client carry ttl:"1h", whereas API-key mode sends bare breakpoints with no duration. This is one of the real differences between the two modes, so omitting it leaves a fixed discrepancy on every request. Cost: a 1h cache write is priced at twice the default 5-minute write. Whether that saves or costs money depends on your usage rhythm: in long sessions 1h usually saves (if you do not reply within five minutes, the next turn rewrites the whole prefix at write price), while scattered one-off requests simply pay more. As in the official client, only main conversation requests (including next-prompt suggestions) get 1h; subagent, fork and prewarm requests keep breakpoints without a duration. Whether 1h is written depends on the request kind, not on whether the system prompt can be split, so clients with a different base prompt such as the desktop app are covered too. When enabled, any 5m the client set in the same request is raised to 1h as well, because upstream requires durations not to increase in tools → system → messages order. When disabled, luban changes nothing and whatever duration the client sent is used as-is. The field requires the upstream extended-cache-ttl beta, hence the dependency on “Beta flags”.',
              )}
            </>
          }
        />
      </ForwardingGroup>

      <ForwardingGroup
        icon={TerminalIcon}
        title={t('非官方客户端模拟', 'Third-party client emulation')}
        description={t('SDK 与第三方客户端的请求按 Claude Code 形态重建后再转发。', 'Requests from SDKs and third-party clients are rebuilt in the Claude Code shape before forwarding.')}
      >
        <ForwardingToggle
          k="simulate_cc"
          label={t('模拟 Claude Code', 'Emulate Claude Code')}
          summary={t(
            '让 SDK 和第三方客户端按 Claude Code 请求形态转发。',
            'Forward SDK and third-party client requests in the Claude Code request format.',
          )}
          requires={{
            key: 'merge_beta',
            label: t('请求头 · Beta 标记', 'Request headers · Beta flags'),
          }}
          description={
            <>
              {t(
                '仅改写非 Claude Code 请求。启用后会增加系统提示词和客户端请求头，可能提高 token 成本并改变输出风格。此类请求通常没有设备身份，需先停用「设备身份校验」。官方客户端自己发出的两种不带基座提示词的请求原样放行：桌面端的缓存预热，以及 WebSearch 工具另外发出的搜索子调用（一条用户消息，只带 web_search 这一个服务端工具且强制调用，系统提示词只有一句搜索助手说明）。此前后者会被重建为主线程请求，导致同一台机器在几秒内以另一个版本、另一台设备、另一条会话的身份发出一条搜索请求。',
                'Only non-Claude Code requests are rewritten. Enabling this adds a system prompt and client request headers, which may increase token costs and change the output style. These requests usually have no device identity, so disable “Device identity checks” first. Two requests the official client itself sends without the base prompt are passed through unchanged: the desktop app’s cache warm-up, and the separate search sub-call made by the WebSearch tool (one user message, a single forced web_search server tool, and a one-line search-assistant system prompt). Previously the latter was rebuilt into a main-thread request, so the same machine showed up seconds later as another version, another device and another session sending a search.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="inject_thinking"
          label={t('注入 Thinking', 'Inject thinking')}
          requires={simulateRequires}
          summary={t(
            '补齐 thinking 与 context_management，并将 temperature 定为 1。',
            'Add thinking and context_management, with temperature fixed at 1.',
          )}
          description={
            <>
              {t(
                '官方客户端的对话请求始终带 thinking 字段，缺少它可能被上游判为第三方应用。启用后，在模拟路径下，客户端未发送 thinking 时自动补充 {type:"enabled", budget_tokens: max_tokens-1}（max_tokens < 1024 的探测级请求不补充），并随之补充 context_management。thinking 启用时上游要求 temperature 必须为 1，这一冲突由 luban 注入引起，因此由注入这一步自行处理：客户端设置的其他 temperature 值会被移除。代价：注入 thinking 会改变模型行为（输出可能更长，thinking token 按输出计费）。如需避免这些副作用，可停用此项，代价是模拟形态少了一项与官方对齐的特征。',
                'The official client always includes a thinking field on conversation requests; omitting it may cause the upstream to classify the request as third-party. When enabled, if the client did not send thinking, luban injects {type:"enabled", budget_tokens: max_tokens-1} in simulation mode (skipped for probe-level requests with max_tokens < 1024), and context_management is added automatically. Upstream requires temperature=1 when thinking is on; since that conflict is caused by luban’s own injection, the injection step handles it itself and strips any other temperature the client set. Cost: injected thinking changes model behaviour (outputs may be longer, thinking tokens are billed as output). Turn it off to avoid these side effects, at the cost of one fewer signal aligning with the official shape.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="simulate_full_system"
          label={t('补齐官方 system 第四块', 'Fill the official fourth system block')}
          summary={t(
            '补上官方 harness 提示词（约 4.7–16.7 KB），回答风格将随之趋近 Claude Code。',
            'Add the official harness prompt (about 4.7–16.7 KB); answers will lean towards the Claude Code style.',
          )}
          requires={simulateRequires}
          description={
            <>
              {t(
                '官方 2.1.291 主线程的 system 由四块组成：billing、身份句、基座，以及基座之后的「其余」段（会话指引、记忆说明、模型清单、上下文管理）。末尾的缓存断点即位于这一块。opus 与 sonnet 共用一份，fable、haiku 各有一份，haiku 的基座也是单独的一份，且篇幅更长。启用后，按 2.1.291 抓包的原文填充这一块：其中唯一随机器变化的是记忆目录，优先使用客户端自己写的工作目录，客户端未提供时才按账号与设备派生一个固定的虚拟路径；模板已删除依赖 ToolSearch 与 WebSearch 的段落，因为模拟不注入这两个工具。客户端自己的 system 单独占最后一块（超过 1,900 字节时移入首条消息，原位置留一行占位；约合 633 个汉字），因此启用时出站是五块，比官方多一块。注意：回答也会受影响。这一块会提示模型自己是 Claude Code，经测试，当客户端 system 要求「只回复某个标记」时，模型虽回复了该标记，但仍会附加一句自我介绍。代价：每条模拟请求多出约 1,200 至 4,000 token 的前缀（视模型族而定）；它带 1h 断点、在同一会话内稳定，基本按缓存读价计费。停用后不发送这一块：system 由 billing、身份句、基座加客户端那块组成，恰好四块，客户端那块落在官方「其余」段的位置上，记忆目录也随这一块一并不发。随首条消息补上的环境说明（工作目录、平台、模型、Agent 类型、技能清单，约 10 KB）不受本开关影响，模拟主线程照常补上。',
                'The official 2.1.291 main-thread system prompt has four blocks: billing, identity, base, and a “rest” section after the base (session guidance, memory instructions, model list, context management), which carries the final cache breakpoint. Opus and sonnet share one text, fable and haiku each have their own, and haiku also uses a separate, longer base. When enabled, this block is filled verbatim from the 2.1.291 captures: the only machine-specific part, the memory directory, follows the client’s own working directory, falling back to a fixed placeholder path derived from the account and device only when the client provides none; the paragraphs that depend on ToolSearch and WebSearch are removed, since emulation does not inject those tools. The client’s own system prompt occupies the last block on its own (moved into the first message above 1,900 bytes, roughly 633 CJK characters, leaving a one-line placeholder), so with this on the request has five blocks, one more than the official shape. Note: answers are affected too. This block tells the model it is Claude Code; in testing, when the client’s system prompt asked for a single marker only, the model returned the marker but still added a line introducing itself. Cost: roughly 1,200 to 4,000 extra prefix tokens per emulated request depending on the model family, behind a 1h breakpoint and stable within a session, so mostly billed at cache-read price. Disable to drop this block: the system prompt is then billing, identity and base plus the client’s block, exactly four blocks, with the client’s block in the official “rest” position, and the memory directory goes with it. The environment note sent with the first message (working directory, platform, model, agent types and skill list, about 10 KB) is not affected by this switch and is still added to emulated main-thread requests.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="relocate_client_system"
          label={t('超长客户端 system 挪入首条消息', 'Move long client system into the first message')}
          summary={t(
            '客户端 system 超过 1900 字节时整段挪进首条用户消息，system 末块只留一行占位。',
            'When the client system exceeds 1900 bytes it is moved into the first user message, leaving a one-line placeholder as the last system block.',
          )}
          requires={simulateRequires}
          description={
            <>
              {t(
                '上游对 system 正文有内容级检测：命中第三方产品提示词里的特征词组合即判为第三方应用，与长度、工具描述、模型无关（2026-10-16 实测 OpenClaw 的提示词仅 46 字节即触发），而同样的文字放在用户消息里不触发。启用（默认）：超过 1900 字节的客户端 system 裹成官方首条用户消息里的 <system-reminder> 块插在最前，末块原地留一行占位，断点与块数形态不变；请求明细的 rewrites 列标注 sys_relocated。1900 字节是「短到装不下一份产品提示词」的经验线，不是上游的阈值。代价：客户端那段指令降级为用户轮文本，少一层 system 优先级。停用：不论多长都整段留在 system 末块，这正是 v0.3.243 至 v0.3.249 的行为——上线当天线上第三方 400 由每天个位数涨到每小时约 70 条。',
                'The upstream inspects system prompt content: a match on the characteristic phrases of a third-party product prompt classifies the request as a third-party app, regardless of length, tool descriptions or model (tested 2026-10-16: 46 bytes of the OpenClaw prompt were enough), while the same text in a user message does not trigger it. Enabled (default): a client system over 1900 bytes is wrapped as the official <system-reminder> block at the start of the first user message and a one-line placeholder stays as the last system block, keeping breakpoints and block count unchanged; such records are tagged sys_relocated in the request log. 1900 bytes is a rule of thumb for “too short to hold a product prompt”, not an upstream threshold. Cost: the client instructions become user-turn text and lose one level of system priority. Disabled: the system stays in the last block whatever its length, the behaviour of v0.3.243 to v0.3.249, which raised third-party 400s from a handful per day to about 70 per hour on the day it shipped.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="fill_absent_tools"
          label={t('无工具请求也补齐官方工具', 'Add official tools to tool-less requests')}
          summary={t(
            '未携带 tools 的请求亦注入官方工具；模型若调用，此类客户端多半无法承接。',
            'Inject official tools even when a request has none; if the model calls one, such clients usually cannot handle it.',
          )}
          requires={simulateRequires}
          description={
            <>
              {t(
                '模拟请求一律伪装成官方主线程请求，而官方主线程的每条请求都带工具（2.1.280 抓包中始终是 19 或 20 个），「主线程的 beta 与 system、零个工具」是官方不会产生的组合。启用后，不带工具的客户端请求（没有 tools、tools 为 null 或空数组）也会补齐 Agent、Bash、Read 等官方工具（14 个，启用「精简注入的官方工具」时 11 个）；客户端请求的 tool_choice 要求必须调用工具（any 或指定工具）时不补。自己带了工具的请求，无论本开关启用还是停用，都照常补齐缺少的工具。风险：不带工具的大多是纯聊天客户端，没有工具循环；模型一旦调用注入的工具，客户端收到的是一个它处理不了的 tool_use，本次回答将会失败。补了工具的请求会在流水的改写列标注 tools_filled，模型确实调用了注入工具时再标注 injected_tool_called，两者之比即为命中率。成本：工具声明约 71,000 字节（约 2.5 万 token；启用「精简注入的官方工具」后约 29,000 字节），每个新会话的首轮按写入价付一次，之后按缓存读价计费。停用后此类请求不注入任何工具，空数组也原样发出。',
                'Emulated requests are always shaped as official main-thread requests, and every official main-thread request carries tools (always 19 or 20 in the 2.1.280 captures); main-thread betas and system prompt with zero tools is a combination the official client never produces. When enabled, requests without tools (no tools field, tools set to null, or an empty array) also get the official tools such as Agent, Bash and Read (14, or 11 with “Trim injected official tools” enabled), unless the request’s tool_choice demands a tool call (any or a named tool). Requests that carry their own tools have missing ones added whether this is on or off. Risk: tool-less clients are usually plain chat clients without a tool loop, so if the model calls an injected tool the client receives a tool_use it cannot handle and that answer is broken. Requests that got the tools are tagged tools_filled in the rewrites column of the request log, and injected_tool_called is added when the model actually called one, so comparing the two gives the hit rate. Cost: the tool declarations are about 71 KB (roughly 25K tokens, or about 29 KB with “Trim injected official tools” enabled), paid at write price on the first turn of each new session and at cache-read price afterwards. Disable to inject no tools into such requests; an empty array is sent as is.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="sim_trim_tools"
          label={t('精简注入的官方工具', 'Trim injected official tools')}
          summary={t(
            '注入的官方工具略去 Artifact、ListAgents、SendFeedback，仅保留 11 个。',
            'Leave Artifact, ListAgents and SendFeedback out of the injected official tools, keeping 11.',
          )}
          requires={simulateRequires}
          description={
            <>
              {t(
                '官方客户端里这三个工具都能由用户关闭：Artifact 用环境变量 CLAUDE_CODE_DISABLE_ARTIFACT=1 或设置 enableArtifact: false，ListAgents 用 CLAUDE_CODE_HARBOR_KITE=0，SendFeedback 用 CLAUDE_CODE_SEND_FEEDBACK=0 或设置 feedbackDrafts: "off"。按 2.1.291 四个模型族的抓包，关闭后主线程请求正好少这三个工具，其余 11 个工具与 system 逐字节不变。启用后，模拟请求按这个形态注入；遥测也按关闭后的样子上报：启动遥测里列出这三个环境变量名，多一条 Artifact 已停用的事件，少几条 Artifact 与跨会话消息相关的事件，工具数量与字符数随之变化。客户端自己声明了其中某个工具时（包括延迟加载的声明，Artifact 还包括 ArtifactComments、ArtifactData），该工具照常发出，遥测也只按实际缺少的那几个上报关闭。收益：工具声明从约 71 KB 降到约 29 KB（Artifact 一个就约 34 KB），每个新会话首轮少写入约 1.4 万 token。ReportFindings、ShareOnboardingGuide、TaskStop 是否出现由官方服务端按账号决定，无法模拟，因此不在精简之列。默认启用。停用时注入完整的 14 个工具，与官方默认配置相同。',
                'Official users can turn all three off: Artifact with the environment variable CLAUDE_CODE_DISABLE_ARTIFACT=1 or the setting enableArtifact: false, ListAgents with CLAUDE_CODE_HARBOR_KITE=0, and SendFeedback with CLAUDE_CODE_SEND_FEEDBACK=0 or the setting feedbackDrafts: "off". In the 2.1.291 captures for all four model families, turning them off removes exactly these three tools from the main-thread request, while the other 11 tools and the system prompt stay byte-for-byte the same. When enabled, emulated requests are injected in that shape, and telemetry reports the same setting: the startup telemetry lists the three environment variable names, an Artifact-disabled event is added, a few Artifact and cross-session messaging events are dropped, and the tool counts and sizes change accordingly. If the client declares one of these tools itself (deferred declarations included, and for Artifact also ArtifactComments and ArtifactData), that tool is still sent and telemetry reports only the tools actually missing as turned off. Benefit: the tool declarations shrink from about 71 KB to about 29 KB (Artifact alone is about 34 KB), saving roughly 14K written tokens on the first turn of each new session. Whether ReportFindings, ShareOnboardingGuide and TaskStop appear is decided per account by the official server and cannot be emulated, so they are not trimmed. Enabled by default. When disabled, all 14 tools are injected, matching the official default.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="sim_message_threads"
          label={t('按官方 message threads 形态续轮', 'Follow the official message-threads shape')}
          summary={t(
            '按官方 thread 形态续轮，仅发送新增消息，并附上 total_tokens 余量提醒。',
            'Continue turns in the official thread shape, sending only new messages, with the total_tokens reminder appended.',
          )}
          requires={simulateRequires}
          description={
            <>
              {t(
                '官方 2.1.291 的 opus、sonnet、fable、haiku 主线程每条请求都带 thread：会话首条为 create 并携带完整上下文；此后无论工具续轮还是新的用户输入，都是 continue，只发送新增消息，system 仅保留 billing 一块、不带 tools，由上游按上一条回复的 message id 接续。只有切换模型或 effort、压缩上下文、中断后重发、恢复会话时才重新 create。启用后，模拟请求按同样的规则发送：客户端这一轮的历史恰好是「上一轮 + 上游那条回复 + 新消息」时发 continue，只发新增部分；客户端改动了历史、重新生成、自行裁剪上下文，或模型、effort、system、tools 有变化，以及上一条失败或被取消时，一律重新 create。2.1.285 的 fable-5-1 官方请求不携带 thread，2.1.291 起也携带，模拟路径按 2.1.291 发送。注意：continue 时模型看到的是上游保存的那条回复，而非客户端手中的版本，因此衔接判断会逐条比对历史与工具调用 id，稍有不符即退回 create。此外，官方每条主线程请求末尾都有一条「<total_tokens>N tokens left</total_tokens>」提醒：新的用户输入时 N 为 15,000,000，工具续轮时减去本轮以来上下文的增长量（按上一条回复的用量计算，只减不增）；opus、sonnet、fable 以独立的 system 消息发送，haiku 写成 system-reminder。启用后按同一算法补上这条提醒。停用后不写 thread、不补提醒，每轮都发送完整上下文。',
                'In the official 2.1.291 client every opus, sonnet, fable and haiku main-thread request carries a thread field: the first request of a session is create with the full context; after that every request, whether a tool round or a new user turn, is continue with only the new messages, a system prompt reduced to the billing block and no tools, chained to the previous reply’s message id upstream. It only goes back to create after switching model or effort, compacting, retrying after an interrupt or resuming a session. When enabled, emulated requests follow the same rules: when the client’s history is exactly the previous turn plus the upstream reply plus new messages, continue is sent with only the new part; if the client changed its history, regenerated, trimmed the context itself, changed the model, effort, system prompt or tools, or the previous request failed or was cancelled, create is sent again. Official fable-5-1 requests carried no thread in 2.1.285 but do from 2.1.291, and emulated requests follow 2.1.291. Note: on continue the model sees the reply stored upstream rather than the client’s copy, so the check compares every earlier message and the tool call ids and falls back to create on any mismatch. In addition, every official main-thread request ends with a “<total_tokens>N tokens left</total_tokens>” reminder: N is 15,000,000 on a new user turn and, on tool rounds, drops by how much the context has grown since that turn began (measured from the previous reply’s usage, never going back up); opus, sonnet and fable send it as a separate system message, haiku as a system-reminder. When enabled, this reminder is added using the same calculation. Disable to omit thread and the reminder and send the full context on every turn.',
              )}
            </>
          }
        />
      </ForwardingGroup>

      <ForwardingGroup
        icon={ActivityIcon}
        title={t('遥测', 'Telemetry')}
        description={t('按官方客户端的字段与节奏补发遥测。', 'Send telemetry with the fields and cadence of the official client.')}
      >
        <ForwardingToggle
          k="api_telemetry"
          label={t('逐请求遥测', 'Per-request telemetry')}
          summary={t(
            '为每条请求补发官方客户端的整套遥测，失败的请求亦如实上报。',
            'Report the official client’s full telemetry for every request, failures included.',
          )}
          description={
            <>
              {t(
                '官方客户端每发一条请求，都会上报 tengu_api_query → tengu_api_success → tengu_turn_end 这一组事件链（带上游 request-id、逐项 token 与花费），以及 Datadog 日志和 OTel 用量指标。在此之前，luban 仅发送每 30 分钟一次的保活遥测，上游看到的是「API 用量很大，遥测中却没有任何一次 API 调用」。启用后，按 2.1.260 抓包的字段与节奏（事件每 30 秒、日志每 10 秒、指标每 5 分钟分批发送）为每个账号补发这些遥测；所用身份取自实际发往上游的请求，与请求保持一致。失败的请求同样上报：官方客户端对失败请求会发送 tengu_api_error 与 tengu_feature_bad，若只上报成功的请求，同样会留下一处可被比对出的差异。新会话的启动握手亦随此开关：首条请求发出前，按抓包顺序补发 policy_limits、settings、eval、额度探测与 bootstrap 等请求，免得账号只见对话请求、从无启动流量。额度探测仅为交互式 CLI 与模拟会话补发，-p（sdk-cli）与桌面端的官方客户端本不发送；真实客户端须「订阅计费标识」启用、改写模式为「完整对齐」方有会话链，握手方才进行。停用后只保留保活遥测，新会话亦不再握手。',
                'The official client reports a chain of events for every request it sends (tengu_api_query → tengu_api_success → tengu_turn_end, carrying the upstream request-id, per-type token counts and cost), plus Datadog logs and OTel usage metrics. Previously luban only sent the 30-minute keepalive telemetry, so upstream saw an account with heavy API usage and not a single API call in its telemetry. When enabled, luban fills this in for every account following the fields and cadence captured from 2.1.260 (events batched every 30s, logs every 10s, metrics every 5min), using the identity actually sent upstream so both sides agree. Failed requests are reported too — the official client sends tengu_api_error plus tengu_feature_bad for those, so reporting only the successes is itself a detectable discrepancy. It also drives the startup handshake of new sessions: before the first request goes out, luban sends policy_limits, settings, eval, the quota probe, bootstrap and the rest in captured order, so the account never shows conversation traffic without any startup traffic. The quota probe is sent only for interactive CLI and emulated sessions — the official client does not send it from -p (sdk-cli) or the desktop app; real clients only have a session chain, and so a handshake, while “Subscription billing identifier” is enabled and the rewrite mode is “Full alignment”. Turn it off to keep only the keepalive telemetry; new sessions then skip the handshake too.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="keepalive_telemetry"
          label={t('保活遥测', 'Keepalive telemetry')}
          summary={t(
            '每 30 分钟为各账号发送空闲遥测，每 6 小时上报一次画像。',
            'Send idle telemetry for each account every 30 minutes, and a profile report every 6 hours.',
          )}
          description={
            <>
              {t(
                '模拟一个开着但空闲的 Claude Code 进程。账号近 3 小时内有真实会话时，事件沿用该会话的身份（相同的 session_id、设备 ID 与客户端版本），与真实客户端开着终端却无人输入时的行为一致；近期没有会话的账号才使用按账号派生的空闲身份。停用后仅停止遥测部分，token 刷新、启动握手（bootstrap / policy_limits / settings）与 401/403 探测保持不变。与「逐请求遥测」互不影响。',
                'Simulates an open but idle Claude Code process. When the account has had a real session in the last 3 hours, the events are attached to that session (same session_id, device ID and client version), matching what a real client does when a terminal is left open with no input; only accounts with no recent session fall back to an account-derived idle identity. Turning it off stops only the telemetry part: token refresh, the startup handshake (bootstrap / policy_limits / settings) and 401/403 detection continue. Independent of “Per-request telemetry”.',
              )}
            </>
          }
        />
      </ForwardingGroup>

      <ForwardingGroup
        icon={ShieldBanIcon}
        title={t('本地拦截', 'Local interception')}
        description={t('命中的请求在本地应答或拒绝，或改写成官方形态后再发。', 'Matching requests are answered or rejected locally, or rewritten into an official shape before being sent.')}
      >
        <ForwardingToggle
          k="reject_probes"
          label={t('拒绝探针请求', 'Reject probe requests')}
          summary={t(
            '中转站的探活请求由本地应答「OK」，不发往上游。',
            'Health-check requests from relays are answered locally with "OK" and never reach upstream.',
          )}
          description={
            <>
              {t(
                '探活脚本发送的是不带 tools 的单句小请求，每条在上游看来都是「一台设备开一个一次性会话，只问一句话」，这是封号复盘中最显眼的特征。本开关按三条强特征判断，命中任意一条即拒绝，不限 UA：一是没有 tools、只有一条消息、max_tokens 在 2 到 16 之间（不要求带 system：官方没有这种形态，而 Go-http-client 的探活通常不带 system）；二是带 system、没有 tools、只有一条消息、不属于官方那两种无 tools 形态，且来自一台从未见过的设备；三是 system 里的 CC 身份句出现在不止一块中。0.3.99 之前仅判断自报 claude-cli UA 的请求，Go-http-client 的探活因此进入了模拟路径，被伪装成官方形态，每 3 到 6 秒一批发往上游。「没有 tools」按取值判断：缺失、null、[] 都算没有，添加空字段也无法绕过。官方 Claude Code 的三种无 tools 请求（cache 预热、haiku Helper、安全分类）按取值逐项比对，都不在判据之内。身份字段写错的请求（device_id 不是 64 位 hex、session_id 不是 UUID，如 channel-test）不算探针，不在这里拒绝，而是不被视为官方客户端，经模拟路径重建身份。只看形态，单条即判定，不做计数。边界：逐字照抄官方 haiku Helper 全部取值的探针，在形态上就是官方请求，这里无法区分。不带 metadata.user_id 的探针也在这里应答：这道判据排在设备身份校验之前，否则设备身份校验先返回的 403 在下游看来与「账号被封」一样，整个 key 同样会被摘除。命中后返回给客户端的是一条与上游预热真实回包同形态的回复：200、一个文本块「OK」（旧一代模型回上游预热真会回的那个字，如「Hi」）、stop_reason max_tokens，model（别名按上游的习惯解析成带日期的全名）与 usage 各字段按模型照上游的样子生成，id 与 request-id 按上游的规则带上当前时刻，与「探针改写为预热」发往上游后返回的是同一种回复；客户端请求流式时按 SSE 返回，model 原样返回客户端声明的那个，正文里没有任何被拦截过的痕迹。0.3.101 之前返回的是 403 permission_error，而探活请求恰恰是下游中转判断「该账号是否可用」的依据：luban 的 403 在下游看来与「账号被封」一样，整个 key 被摘除，真实流量随之中断，而该请求实际并未到达上游，账号本身完全正常。这类由 luban 本地应答、未到达上游的请求只能在请求查询中识别：带 probe_reply 标签（后接命中的判据，如 probe_reply:ping），花费记为 0。返回给客户端的响应里没有任何 luban 的标记：响应头也照上游的样子生成（request-id、traceresponse、cf-ray 及 Cloudflare 与安全相关的头，cf-ray 的机房取最近一次上游响应的那个）；限流头与组织 ID 这类与账号相关的头不生成，以免编造的额度被下游拿去调度。与其他开关的关系：本开关只负责上面三条形态判据；从响应学到的「已拒答的提示词」与「零输出请求类」两类规则各有自己的开关（见下面两条）。0.3.93 之前三者共用同一个键，停用探针拒绝会连同学到的规则一并放行。停用本开关后，命中判据的请求交由「探针改写为预热」处理；两者都停用才原样转发。',
                "Probe scripts send a single tool-less one-liner; upstream sees each one as \"a device opening a throwaway session to ask one question\", the most conspicuous pattern in the ban post-mortem. This switch checks three strong signatures, any one of which matches, regardless of UA: no tools, a single message and max_tokens between 2 and 16 (no system prompt required: official Claude Code has no such shape, and Go-http-client health checks usually carry none); a system prompt with no tools, a single message, not one of the two official tool-less shapes, and a never-seen device; the Claude Code identity sentence in more than one system block. Before 0.3.99 only requests claiming a claude-cli UA were judged, so Go-http-client health checks went through the simulation path instead and were sent upstream dressed as official traffic, in batches every 3 to 6 seconds. \"No tools\" is judged by value: missing, null and [] all count, so padding with empty fields does not help. The three tool-less shapes official Claude Code does send (cache prewarm, the haiku helper, the security classifier) are matched value by value and fall outside the signatures. Malformed identities (a device_id that is not 64-hex, a session_id that is not a UUID, e.g. channel-test) are not probes and are not rejected here; they are simply not treated as an official client and go through the simulation path, which rebuilds the identity. Shape only, decided per request, no counting. Limit: a probe that copies every value of the official haiku helper verbatim is, by shape, an official request and cannot be told apart here. Probes without metadata.user_id are answered here as well: this check runs before the device-identity check, which would otherwise return a 403 that a downstream relay cannot tell apart from a banned account, so the whole key would be pulled anyway. A hit is answered locally with a reply shaped exactly like upstream's real prewarm reply: 200, one text block \"OK\" (older models get the single token upstream really answers a prewarm with, e.g. \"Hi\"), stop_reason max_tokens, with the model (aliases resolved to the dated full name as upstream does) and every usage field generated per model the way upstream does and the id and request-id carrying the current time by upstream's own scheme, the same kind of reply \"Send probes as prewarm\" gets back from upstream; streamed as SSE if the client asked for a stream, echoing the model the client declared, with nothing in the body hinting it was intercepted. Before 0.3.101 the answer was a 403 permission_error, but a health check is exactly how a downstream relay decides whether the account still works: luban's 403 looks the same to it as a banned account, so the whole key gets pulled and real traffic stops with it, even though the request never reached upstream and the account is fine. Requests answered locally by luban without reaching upstream can be identified only in request lookup, by the probe_reply tag (followed by the signature that matched, e.g. probe_reply:ping) with zero cost. The response carries no luban marker at all: its headers are generated the way upstream sends them too (request-id, traceresponse, cf-ray and the Cloudflare and security headers, with the cf-ray colo taken from the latest upstream response); account-specific headers such as rate limits and the organization ID are not generated, so made-up quota never steers downstream scheduling. Relation to other switches: this switch governs only the three shape signatures above; the two rule kinds learned from responses (refused prompts, empty-reply request classes) have their own switches below. Before 0.3.93 all three shared this one key, so turning probe rejection off also let the learned rules through. With this switch off, matching requests are handled by “Send probes as prewarm”; only with both off are they forwarded as is.",
              )}
            </>
          }
        />
        <ForwardingToggle
          k="probe_as_prewarm"
          label={t('探针改写为预热', 'Send probes as prewarm')}
          inactiveWhileOn={{ key: 'reject_probes', label: t('拒绝探针请求', 'Reject probe requests') }}
          summary={t(
            '上一项停用时，探活请求改写为官方 /model 预热后发往上游。',
            'While the previous switch is off, health checks are rewritten into the official /model prewarm and sent upstream.',
          )}
          description={
            <>
              {t(
                '原样转发时，第三方探活会进入模拟路径，被补上基座提示词与官方工具，在上游看来是「一台新设备开一个一次性会话，只问一句话」。改写后发出的是官方 /model 预热：system 只有 billing 标识与身份句，正文「Hi」，max_tokens 为 1，请求头按官方预热的形态生成，会话使用该账号固定的会话 ID，与连通性测试发出的是同一条请求，模型沿用客户端声明的那个。客户端自带的设备与会话身份不发往上游，因此不占设备与会话名额，按无身份请求选号并受无身份请求速率上限约束；设备身份校验与每设备、每会话的 RPM 限制同样跳过。实测 opus-4-7 起的新一代模型（含 5 系）对这条预热返回空正文，旧一代返回一个字（如「Hi」「#」）；正文为空时补一句「OK」，以免下游渠道测试将空回复判为失败。状态码、响应头、用量与 model 均为上游真实返回值，响应里没有任何 luban 的标记；客户端请求流式时按 SSE 返回；请求查询中带 probe_prewarm 标签（后接命中的判据，如 probe_prewarm:short-opener），补过正文的另带 probe_reply_filled；按实际用量计费。这类请求不参与学习零输出与拒答规则，也不发送逐请求遥测。判据与「拒绝探针请求」相同，「严格模式」在此同样生效。注意：命中「新设备一次性对话」这条判据的设备不会因此登记为已知设备，这台设备之后同样形态（带 system、无 tools、单条消息）的请求会一直被改写成预热，直到它发出其他形态的请求、完成设备绑定为止；这与「拒绝探针请求」启用时一直本地应答是同一个取舍。没带 metadata.user_id 的第三方单轮调用（翻译、摘要、分类脚本常是这个形态）因此始终拿不到真实回答，有这类流量时请停用本开关。',
                'Forwarded as is, a third-party health check goes through the simulation path and gets the base prompt and official tools added, so upstream sees "a new device opening a throwaway session to ask one question". Rewritten, what goes out is the official /model prewarm: a system prompt holding only the billing identifier and the identity sentence, the body "Hi", max_tokens 1, headers generated in the official prewarm shape, and the account\'s fixed session ID. It is the same request the connectivity test sends, keeping the model the client declared. The client\'s own device and session identity never reach upstream, so no device or session slot is taken; the account is picked as for an identity-less request and the identity-less rate limit applies, while the device-identity check and the per-device and per-session RPM limits are skipped. Measured: newer models (opus-4-7 onward, including the 5 series) answer this prewarm with an empty body, older ones with a single token (e.g. "Hi", "#"); an empty body gets a one-word "OK" so downstream channel tests do not mark it as failed. Status code, response headers, usage and model are upstream\'s own, with no luban marker anywhere in the response; streamed as SSE if the client asked for a stream; request lookup shows the probe_prewarm tag followed by the signature that matched (e.g. probe_prewarm:short-opener), plus probe_reply_filled when the body was filled; billed at actual usage. These requests never feed the learned empty-reply or refusal rules and send no per-request telemetry. Same signatures as "Reject probe requests"; strict mode applies here too. Note: a device matched by the "throwaway conversation from a new device" signature is not registered as a known device as a result, so every later request of the same shape from it (a system prompt, no tools, a single message) keeps being sent as a prewarm until it sends some other shape and gets bound; the same trade-off as answering locally while "Reject probe requests" is on. Third-party single-turn calls without metadata.user_id (translation, summarisation and classification scripts often look like this) therefore never get a real answer; turn this off if you carry such traffic.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="reject_probes_strict"
          label={t('探针判据：严格模式', 'Probe signatures: strict mode')}
          requiresAny={[
            { key: 'reject_probes', label: t('拒绝探针请求', 'Reject probe requests') },
            { key: 'probe_as_prewarm', label: t('探针改写为预热', 'Send probes as prewarm') },
          ]}
          summary={t(
            '增设两条探活判据，或会误拦真实用户的首句问候，默认停用。',
            'Two more probe signatures that may catch a real user’s opening greeting; off by default.',
          )}
          description={
            <>
              {t(
                '封号复盘（ban-37 / 38 / 42）中，Go-http-client 的探活有四种形态，默认判据只能拦截「无 tools、单条消息、max_tokens 2 到 16」这一种。另外三种被放行后进入模拟路径，被伪装成带基座的官方形态发往上游，每个账号上仍有一两百条：4 个 tools 配 max_tokens 16；无 system、只问一句「hi」，max_tokens 为 50、1024 或 32000。本开关增加两条判据来覆盖它们。第一条有硬依据：16 个 token 装不下一次 tool_use 调用，带着工具却只给 16 个 token，只可能是测活。第二条有代价：真实用户使用纯聊天客户端、经中转站发出的第一句「你好」同样是无 system、无 tools、一条短消息，会收到一条 200 的「OK」（与探活收到的是同一条最小回复），下一句正常长度的消息照常通过（「探针改写为预热」生效时，这句问候会被改写成预热发出，收到的同样是一句「OK」）。带附件、多轮、带 system 的请求，以及 max_tokens 为 1 的预热，都不视为短开场白。需「拒绝探针请求」或「探针改写为预热」生效时才起作用。',
                'The ban post-mortems (ban-37 / 38 / 42) show four shapes of Go-http-client health checks; the default signatures catch only "no tools, single message, max_tokens 2 to 16". The other three were simulated into an official-looking body with a base prompt and sent upstream, still one or two hundred per account: 4 tools with max_tokens 16; no system prompt, a single "hi", and max_tokens 50, 1024 or 32000. This switch adds two signatures that cover them. The first rests on hard evidence: 16 tokens cannot hold a tool_use call, so tools plus a 16-token budget can only be a health check. The second has a cost: a real user\'s first "hello" sent through a relay from a plain chat client is also a single short message with no system prompt and no tools, so it gets the same minimal 200 \"OK\" a probe would, and the next normal-length message goes through. (With “Send probes as prewarm” in effect, that greeting is sent as a prewarm instead and likewise answered with "OK".) Attachments, multi-turn conversations, a system prompt, or a max_tokens of 1 (cache warm-up) never count as a short opener. Takes effect only while "Reject probe requests" or "Send probes as prewarm" is in effect.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="reject_refusals"
          label={t('拒绝已拒答的提示词', 'Reject refused prompts')}
          summary={t(
            '分类器拒答过的提示词再度发来时，于本地回放上次的拒答；停用后不拦截亦不学习。',
            'A prompt the classifier has refused is answered locally with the earlier refusal when resent; when disabled, nothing is blocked or learned.',
          )}
          description={
            <>
              {t(
                '上游拒答（200 加 stop_reason refusal）不按形态学习，只按那条提示词学习：同一模型下，system + messages + tools + tool_choice 逐字相同才算同一条；改一个字、换一个模型或换一套工具集都不会命中，其他请求一律不拦截。命中时返回给客户端的不是 luban 自己构造的 403，而是学习规则时上游那次响应的原样响应体（200、同一段 stop_reason refusal 与 stop_details），并按本次客户端请求的形态返回：学习时为 SSE 且本次也要求流式时，逐字节原样发出；形态不一致时才在 SSE 与整段 JSON 之间转换。客户端收到的内容与上游再次拒答时完全相同。只学习输出前就被拒的请求（正文为空）：流式输出中途才被截断的请求没有确定的判决可供回放，照常发往上游。并且只学习分类器的判决（stop_details 带 category，如 cyber）。按官方说法，category 为空的可能是模型自己拒答，也可能是不带类别的分类器判决；带 recommended_model 的表示 fallback 没有执行成功。这两种情况重发都可能得到回答，因此只记入流水、不学习。出站会带 fallbacks 的请求（客户端自带，或由上面两个开关让 luban 补上）不在这里拦截：带 fallback 的请求被拒答后，上游会换用其他模型重新生成，这正是拒答应走的路径。若不排除此类请求，fallback 停用期间学到的拒答规则会使相应请求即使在 fallback 启用后也始终无法到达上游。规则记录在「从上游学到的规则」中，7 天后到期（进程内规则每小时依据数据库重建一次，过期不再依赖重启），也可手动删除。规则不区分账号，对整个调度池生效：分类器对同一条提示词的判决是确定性的，换一个账号重发结果也一样。无法识别会话的客户端另有一档「按应用学习」：封号复盘（ban-37 / 38 / 42）中，Go-http-client 集中爆发的那批 reasoning_extraction 请求共 568 条，正文各不相同，按提示词学习的规则无一命中，而该应用只使用了 4 种 system。对既不带会话 ID 也不带 device_id 的中转流量来说，system 就代表「哪个应用在说话」。这类客户端按「模型 + system 哈希」统计到达上游的请求数与被分类器拒答的条数，拒答至少 3 条且占比 30% 以上才形成 app_refusal 规则；之后同一模型、同一份 system 的每条请求都回放最近那条拒答，不论正文。之所以按比例学习，而非一次拒答即学习，是因为同一批中转站上还有使用固定 18 字节 system、长达几十到两百多轮的真实用户 agent 会话，拒答率仅为 1% 到 2%；若一次拒答即学习，整个应用将被拦截 7 天。而集中爆发请求的那个应用，拒答率为 35% 到 63%，3 到 5 条即可形成规则。计数只保存在进程内，重启后归零。带会话 ID 或 device_id 的客户端不走这一档：它们的 system 每轮都在变，真实对话中偶发的一次拒答不应导致整个会话被拦截。这一档同样 7 天到期、可删除、可按种类清空，回放记入流水并标注 app_refusal_replay。',
                "An upstream refusal (200 with stop_reason refusal) is never learned by shape, only by that exact prompt: the same model with system + messages + tools + tool_choice byte-for-byte identical; changing a word, the model, or the tool set misses, and nothing else is ever blocked. On a hit the client does not get a luban-made 403 but the body upstream returned when the rule was learned (200, the same stop_reason refusal and stop_details), in the shape this request asked for: learned as SSE and streaming again, the bytes are replayed verbatim; only a shape mismatch converts between SSE and a single JSON message. The client sees exactly what a fresh upstream refusal looks like. Only refusals issued before any output (empty content) are learned; a refusal that cut a stream mid-way has no deterministic verdict to replay and goes upstream as usual. Only classifier verdicts are learned (stop_details carries a category such as cyber). A refusal with no category may, per the official docs, be the model's own or an uncategorised classifier verdict, and one carrying recommended_model means the fallback could not run; both may succeed on a resend, so they are logged but not learned. Requests that will go out with fallbacks (supplied by the client, or added by luban under the two switches above) are not blocked here: upstream reruns a refused request on another model, which is exactly the path a refusal should take. Previously this was not checked, so a refusal learned while fallbacks were off kept being rejected locally even after fallbacks were turned on. Rules live under “Rules learned from upstream”, expire after 7 days (the in-memory table is rebuilt from the store hourly, so expiry no longer waits for a restart) and can be removed by hand. Rules are not scoped to an account and apply to the whole scheduling pool: the classifier verdict for a given prompt is deterministic, and resending it from another account gets the same answer. Session-less clients get one more tier, learned per app: in the ban post-mortems (ban-37 / 38 / 42) the Go-http-client reasoning_extraction storm sent 568 requests with different bodies every time, so no prompt rule ever matched, yet it used only 4 system prompts; for relay traffic carrying neither a session ID nor a device_id, the system prompt is what identifies the app. For such clients luban counts, per model + system hash, the requests that reached upstream and how many were classifier refusals; once at least 3 refusals make up 30% or more, an app_refusal rule is learned and every further request with that model and system is replayed regardless of its body. Ratio rather than a single hit, because the same relays also carry real agent sessions with a fixed 18-byte system prompt and dozens to hundreds of turns, refused 1 to 2% of the time; a single-hit rule would block that whole app for 7 days, while the storm apps run at 35 to 63% and trip within a handful of requests. Counters live in process only and reset on restart. Clients with a session ID or device_id never enter this tier: their system prompt changes every turn, and one refusal in a real conversation must not block the whole session. Same 7-day expiry, deletable, clearable by kind; replays are logged with the app_refusal_replay tag.",
              )}
            </>
          }
        />
        <ForwardingToggle
          k="reject_empty_replies"
          label={t('拒绝零输出请求类', 'Reject empty-reply request classes')}
          summary={t(
            '上游零输出过的同类单句请求，于本地返回 403；停用后不拦截亦不学习。',
            'Single-message request classes upstream answered with zero output get a local 403; when disabled, nothing is blocked or learned.',
          )}
          description={
            <>
              {t(
                '这条规则从响应中学习，不限 UA：某模型对「无 tools 的单条消息 + 某个 max_tokens」返回过 200 却零输出（有 usage、output_tokens 为 0，即上游收取了输入费用但未返回任何内容）之后，同类请求在本地返回 403；上游当时回复的开头记录在流水的对应记录与「从上游学到的规则」中。带 tools、多轮或换了 max_tokens 的请求不受影响。类别划分较窄，倾向于放行。封号复盘中，这样的记录在 13 小时里每 37 秒出现一条，每条在上游看来都是「一台设备只问一句话、什么都没得到」的探活式痕迹。模拟路径重建的是身份，改变不了「单句请求、上游零输出」这一事实，所以不限 UA。规则 7 天后到期，可手动删除。',
                'This rule is learned from responses, regardless of UA: once a model has answered a tool-less single-message request with a given max_tokens with 200 and zero output tokens (usage present, output_tokens 0: upstream billed the input and returned nothing), that request class is rejected locally with 403; the start of that upstream reply is kept on the usage record and under “Rules learned from upstream”. Requests with tools, multi-turn conversations, or a different max_tokens are unaffected; the class is deliberately narrow, erring on the side of letting requests through. The ban post-mortem had one such record every 37 seconds for 13 hours, each one a probe-like trace of "one device asking one question and getting nothing" on the upstream side; the simulation path rebuilds identity but cannot change that, hence no UA limit. Rules expire after 7 days and can be removed by hand.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="reject_learned_shapes"
          label={t('拒绝已学到的形态错误', 'Reject learned shape errors')}
          summary={t(
            '上游以 400 拒绝过的「模型 + 取值」组合，于本地返回 400；停用后不拦截亦不学习。',
            'Model + value combinations upstream rejected with 400 get a local 400; when disabled, nothing is blocked or learned.',
          )}
          description={
            <>
              {t(
                '这是纯粹的请求形态错误：换哪个账号发送都是同一条 400，发往上游只会徒然占用一次请求配额，并在上游留下一条与账号状态无关的 4xx。规则由上游的 400 学习而来，本地拒绝时返回的也是上游当时的原文。规则记录在「从上游学到的规则」中，7 天后到期，也可手动删除。停用后，这类请求原样发往上游，由上游返回 400，停用期间不会积累新规则。',
                'These are pure request-shape errors: whichever account sends it gets the same 400, so forwarding it only wastes a request and leaves a 4xx upstream that has nothing to do with the account. Rules are learned from upstream 400s, and a local rejection returns upstream’s original message. Rules live under “Rules learned from upstream”, expire after 7 days and can be removed by hand. When disabled, such requests go upstream as is and upstream answers with the 400; no new rules are learned in the meantime.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="reject_openai_shape"
          label={t('拒绝 OpenAI 转换残留', 'Reject OpenAI-format residue')}
          summary={t(
            '带有 OpenAI 格式转换痕迹的请求，于本地返回 400，不作修补、不予转发。',
            'Requests carrying traces of OpenAI-format conversion are rejected locally with 400, never repaired or forwarded.',
          )}
          description={
            <>
              {t(
                '经 litellm、one-api、claude-code-router 等工具从 OpenAI 格式转换过来的请求，到达这里时已是 Anthropic 形态，只能靠残留特征识别：messages 开头（首条 user / assistant 之前）的 role:"system"、role:"tool"、消息上的 name / tool_calls、以 call_ 为前缀的工具调用 ID、字符串形态或 type:"function" 的 tool_choice、OpenAI function 形态的 tools、n / stop / user / response_format 等 OpenAI 专属顶层字段，以及 image_url 等 OpenAI 内容块。命中任意一项即在本地返回 400，错误消息会指出位置与 Anthropic 的对应写法。停用后原样转发，由上游返回官方报错。不影响模拟路径：模拟路径只接管本来就是 Anthropic 形态的非 CC 请求。',
                'Requests converted from the OpenAI format by litellm, one-api, claude-code-router and the like arrive already in Anthropic shape; the only way to tell is the residue they leave: role:"system" before the first user / assistant turn, role:"tool" in messages, name / tool_calls on a message, tool call IDs prefixed call_, a string or type:"function" tool_choice, tools in the OpenAI function shape, OpenAI-only top-level fields such as n / stop / user / response_format, and OpenAI content blocks such as image_url. Any hit is rejected locally with 400 and a message naming the location and the Anthropic equivalent. Turned off, such requests are forwarded as is and upstream returns its own error. The simulation path is unaffected: it only takes over non-CC requests that are already in Anthropic shape.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="reject_enabled_thinking"
          label={t('拒绝新模型的 enabled thinking', 'Reject enabled thinking on newer models')}
          summary={t(
            '5 系与 opus-4-7 起的模型收到客户端自行写入的 thinking.type:"enabled" 时，于本地返回 API Key 直连的同一条 400。',
            'When a client sends thinking.type:"enabled" to a Claude 5 or opus-4-7+ model, return locally the same 400 an API-key connection would.',
          )}
          description={
            <>
              {t(
                '2026-10-17 实测：API Key 直连下 opus-5-5、sonnet-5-5、haiku-5-5、opus-5、sonnet-5、fable-5-1、opus-4-8、opus-4-7 对 thinking.type:"enabled" 一律返回 400「"thinking.type.enabled" is not supported for this model」，与 budget_tokens 的取值无关；4-6 及更早各代照常接受。而订阅通道对同一请求静默放行，按 adaptive 处理，budget_tokens 与 display 均被忽略，思考正文为空。渠道测试工具正是以这一差异识别中转：上游永远不会返回这条 400，无法从响应中学到，只能写死。判定看客户端发来的原始请求：type 为 enabled 且模型属于上述集合即命中，adaptive、disabled 与未写 thinking 的请求不受影响，模型代号无法识别时放行。上游排在这条之前的校验不在此列：budget_tokens 缺失或不足 1024、display 取值非法、与 thinking 冲突的 temperature、top_k、top_p，两条通道都会返回同样的 400，照常发往上游，由上游返回各自那句原文，以保证客户端看到的文案与直连一致。官方 Claude Code 只对 haiku-4-5 发送 enabled（2.1.258 至 2.1.301 五版抓包一致），真实客户端不会命中。启用（默认）：命中即在本地返回与 API Key 直连逐字相同的 400，请求明细标注 rejected_locally:enabled-thinking。停用：照常发往上游，由订阅通道放行，客户端得到的是 adaptive 的结果。完整对齐与「仅 billing 标识」两种模式下均生效。',
                'Measured on 2026-10-17: over an API-key connection, opus-5-5, sonnet-5-5, haiku-5-5, opus-5, sonnet-5, fable-5-1, opus-4-8 and opus-4-7 all answer thinking.type:"enabled" with a 400 “"thinking.type.enabled" is not supported for this model”, whatever the budget_tokens; 4-6 and earlier accept it. The subscription channel silently accepts the same request, treating it as adaptive and ignoring budget_tokens and display, with empty thinking text. Relay-detection tools use exactly this difference: the upstream never returns this 400 to luban, so it cannot be learned from responses and has to be hard-coded. The check looks at the request as the client sent it: type enabled on one of those models is a hit; adaptive, disabled and requests without thinking are untouched, and unrecognisable model names pass through. Checks the upstream runs before this one are left to it: a missing budget_tokens or one below 1024, an invalid display value, and temperature, top_k or top_p conflicting with thinking all get the same 400 on both channels, so those requests are forwarded and the upstream returns its own wording, keeping the client-visible message identical to a direct connection. The official Claude Code only sends enabled for haiku-4-5 (consistent across five captured versions, 2.1.258 to 2.1.301), so real clients never hit it. On (default): a hit returns locally the same 400 an API-key connection would, byte for byte, and the request detail is tagged rejected_locally:enabled-thinking. Off: forwarded as is, the subscription channel accepts it and the client gets an adaptive result. Applies in both full-alignment and billing-only modes.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="reject_tampered_thinking"
          label={t('拒绝改坏的历史思考块', 'Reject tampered thinking history')}
          summary={t(
            '上游以签名或密文无效拒绝历史 thinking 时，按签发账号判断会话是否换过账号；没换过的原样返回上游 400，不降级重试。',
            'When the upstream rejects historical thinking for an invalid signature or ciphertext, decide by issuing account whether the session switched accounts; if not, return the upstream 400 as is instead of retrying.',
          )}
          description={
            <>
              {t(
                '签名由签发它的账号校验。会话确实中途换过账号时，历史 thinking 在新账号上必然验不过，「thinking 签名兜底」与「redacted thinking 兜底」把它降级后重试一次是正当的恢复；会话从未换过账号而签名仍无法验证，只可能是客户端自行改动了历史，API Key 直连此时返回 400，中转若替它救活成 200，渠道测试工具正是以此识别中转。判定依据是请求里历史 thinking 签名的签发账号：luban 转发过的每条回复都记下其中的签名由哪个账号签发，保留 90 天；有签发自其他账号的即重试，全部都有记录且均为当前账号签发则原样返回 400；重试成功后这段历史中认不出的部分记为「外来」，之后的续轮照常恢复。有认不出的签名（上线前的历史、超过保留期、非经 luban 发出，或已被改动）时再看该会话：请求流水或绑定事件里出现过其他账号则重试；此前就由当前账号在此会话应答过则视为改坏、返回 400；对该会话一无所知则按可恢复处理、照常重试。没有会话键的请求照常重试。本项与两个兜底开关相互独立：兜底停用时本来就不重试，本项无事可做；兜底启用而本项停用，则恢复为无条件降级重试的旧行为。签发账号的登记不受本项影响，始终进行。完整对齐与「仅 billing 标识」两种模式下均生效。',
                'Signatures are verified by the account that issued them. When a session really switched accounts midway, its historical thinking cannot verify on the new account, and the thinking-signature and redacted-thinking fallbacks rightly downgrade it and retry once. When the session never switched and a signature still fails, the client itself must have altered the history: an API-key connection returns 400 there, and relay-detection tools flag a relay that rescues it into a 200. The decision is based on which account issued the thinking signatures in the request: every reply luban relays has its signatures recorded against the issuing account for 90 days. Any signature issued by another account triggers the retry; when every signature is on record and all were issued by the current account, the 400 is returned as is. After a successful retry, the unrecognised part of that history is recorded as foreign so later turns keep recovering. With unknown signatures (history predating the feature, past retention, not relayed by luban, or altered) the session is consulted: another account in its request log or binding events means retry; the current account having already answered this session means the block was altered, so the 400 is returned; a session with no history at all is treated as recoverable and retried. Requests with no session key are retried as before. This setting is independent of the two fallbacks: with a fallback off nothing is retried anyway; with a fallback on and this setting off, the old unconditional retry returns. Recording issuing accounts is unaffected and always runs. Applies in both full-alignment and billing-only modes.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="api_key_parity"
          label={t('对齐 API Key 直连的请求校验', 'Match API-key request validation')}
          summary={t(
            '订阅通道放行、API Key 直连却返回 400 的几种请求形态，于本地返回直连的同一条 400。',
            'For request shapes the subscription channel accepts but an API-key connection rejects with a 400, return that same 400 locally.',
          )}
          description={
            <>
              {t(
                '2026-10-18 对照 API Key 直连实测，三类请求在直连下返回 400，经订阅通道却被放行，渠道测试工具可据此识别中转。其一，缺少 anthropic-version 请求头：直连返回「anthropic-version: header is required」，且先于其他一切校验；luban 出站时补齐了该头。其二，thinking.display 取 summarized、omitted 以外的值（包括 updates）而请求未带 thinking-display-updates beta：直连返回「thinking.<type>.display: Input should be \'summarized\', \'omitted\'」，先于缺 max_tokens、未知字段与采样参数冲突；luban 出站补了这项 beta，订阅通道因而放行 updates，并把非法值报成三选一。其三，接受 enabled 的模型（4-6 及更早）budget_tokens 不小于 max_tokens 而请求未带 interleaved-thinking beta：直连返回「`max_tokens` must be greater than `thinking.budget_tokens`」；luban 出站补了这项 beta，订阅通道因而放行。budget_tokens 不足 1024、display 非法、采样参数冲突排在这条之前，照常发往上游。判定看客户端发来的原始请求与它自带的 anthropic-beta，客户端已带相应 beta 的照常转发。后两类排在探活判定之前，改写为预热的探活同样适用；缺版本头一类排在探活判定之后，以免探活脚本被拒、下游摘除渠道。官方 Claude Code 始终带版本头与这两项 beta，不会命中。启用（默认）：命中即在本地返回与直连逐字相同的 400，请求明细分别标注 rejected_locally:anthropic-version、thinking-display、thinking-budget。停用：照常发往上游，由订阅通道放行。完整对齐与「仅 billing 标识」两种模式下均生效。',
                'Measured against an API-key connection on 2026-10-18: three kinds of request get a 400 there but are accepted through the subscription channel, which relay-detection tools can use to spot a relay. First, a missing anthropic-version header: a direct connection answers “anthropic-version: header is required” before any other check; luban fills the header in on the way out. Second, a thinking.display other than summarized or omitted (updates included) without the thinking-display-updates beta: a direct connection answers “thinking.<type>.display: Input should be \'summarized\', \'omitted\'”, ahead of a missing max_tokens, unknown fields and sampling conflicts; luban adds that beta on the way out, so the subscription channel accepts updates and lists three allowed values for invalid ones. Third, on models that accept enabled (4-6 and earlier), a budget_tokens not below max_tokens without the interleaved-thinking beta: a direct connection answers “`max_tokens` must be greater than `thinking.budget_tokens`”; luban adds that beta on the way out, so the subscription channel accepts it. A budget_tokens below 1024, an invalid display and sampling conflicts come first and are forwarded as usual. The checks look at the request as the client sent it, with its own anthropic-beta; requests that already carry the relevant beta are forwarded. The latter two run before probe detection, so probes rewritten as prewarms are covered too; the missing-header check runs after it, so probe scripts are not rejected and channels are not dropped downstream. The official Claude Code always sends the header and both betas and never hits these. On (default): a hit returns locally the same 400 a direct connection would, byte for byte, and the request detail is tagged rejected_locally:anthropic-version, thinking-display or thinking-budget. Off: forwarded, and the subscription channel accepts it. Applies in both full-alignment and billing-only modes.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="cap_messages_body"
          label={t('请求体按上游 32MB 截止', 'Cap request bodies at the upstream 32MB')}
          summary={t(
            '/v1/messages 与 count_tokens 改写后的出站请求体超过 32MB 时，于本地返回上游同一条 413，不再发往上游。',
            'When the rewritten outbound body of a /v1/messages or count_tokens request exceeds 32MB, return the upstream 413 locally instead of sending it.',
          )}
          description={
            <>
              {t(
                '上游 Messages 接口的请求体上限为 32MB，超出即返回 413 request_too_large；官方 Claude Code 2.1.306 按同一数值处理这条 413，引导用户移除图片、附件或压缩对话。判定看改写后实际要发出的那份请求体，而非客户端发来的原件：完整模拟续接 message thread 时，已提交的历史会整段剪去、只发增量，入站超过 32MB 的请求出站可能只剩几 KB，照常转发。启用（默认）：出站请求体超过 32MB 即于本地返回 413，类型同为 request_too_large，省去一次往返。停用：照常发往上游，由上游返回 413。入站请求体始终以 64MB 为限；batches、files 等上游接收更大请求体的接口不受影响。',
                'The upstream Messages endpoint caps request bodies at 32MB and answers anything larger with 413 request_too_large; the official Claude Code 2.1.306 handles that 413 at the same figure, prompting the user to remove images or attachments or to compact. The check looks at the body actually about to be sent after rewriting, not the one the client sent: when full simulation continues a message thread, the committed history is cut and only the increment is sent, so a request over 32MB inbound may be a few KB outbound and is forwarded as usual. On (default): an outbound body over 32MB gets a 413 of the same request_too_large type locally, saving a round trip. Off: forwarded, and the upstream returns its 413. Inbound bodies are always capped at 64MB; endpoints the upstream accepts larger bodies on, such as batches and files, are unaffected.',
              )}
            </>
          }
        />
      </ForwardingGroup>

      <ForwardingFilterItem
        texts={[t('从上游学到的规则', 'Rules learned from upstream'), t('拒答 零输出 形态错误', 'refusal empty reply shape error')]}
      >
        <LearnedRejections />
      </ForwardingFilterItem>

      <ForwardingGroup
        icon={RouteIcon}
        title={t('拒答换模型', 'Refusal fallback')}
        description={t('上游分类器拒答时，由上游换用其他模型重新生成。', 'When the upstream classifier refuses, upstream regenerates the answer with another model.')}
      >
        <ForwardingToggle
          k="fable_refusal_fallback"
          label={t('Fable 拒答自动换模型', 'Fable refusal fallback')}
          summary={t(
            'fable 拒答时由上游改用 opus-5 重新生成，与官方形态一致；默认停用。',
            'When fable refuses, upstream regenerates with opus-5, matching the official shape; off by default.',
          )}
          description={
            <>
              {t(
                'Fable 5.1 / Fable 5 带安全分类器，命中时（多为 cyber 类，正常的安全相关请求也可能被误判）返回 200 加 stop_reason refusal，正文为空。官方 Claude Code 2.1.260 在 fable 上自带 fallbacks: [{"model":"claude-opus-5"}] 和 server-side-fallback beta，拒答后由上游改用 Opus 5 重新生成，用户看不到拒答。启用后，luban 为 fable 主线程请求补上与官方逐字相同的这一字段，出站请求头一并带上 server-side-fallback-2026-06-01。该做法有抓包依据，补齐后更接近 2.1.260 的官方形态。但这替用户做出了决定：拒答后由 Opus 5 作答、按 Opus 计价、同一对话约一小时内固定在 Opus 上，用户也看不到拒答本身，所以默认停用，由用户自行启用。停用时，模拟出的 fable 请求比 2.1.260 少这一个字段，拒答直接返回、不换用其他模型重新生成；请求头里的 beta 仍按版本补上，「有 beta、无字段」正是 2.1.260 之前的官方形态；客户端自带的 fallbacks 仍予保留。客户端自己带了数组形态的 fallbacks 时不改动；helper、标题、安全分类、额度探测等辅助请求，官方均不发送该字段，luban 也不补充。上游以 400 拒绝 fallback 目标时，移除该字段后重发一次并记入「从上游学到的规则」，此后不再为该模型补充。落到 fallback 的回复按实际作答的模型计价，同一对话约一小时内会固定在 fallback 模型上。输出前就被拒的请求上游不计费，流水里花费记为 0。opus-5 的自定义 fallback 链由另一个开关控制，见下一条。Sonnet 5 与 Opus 4.7/4.8 同样带网络安全分类器，同样会以 200 加 stop_reason refusal 拒答；luban 只解析并记录它们的拒答，不为它们补 fallbacks。官方客户端在这些模型上不发送该字段，这是有意的产品限制，不是遗漏。',
                'Fable 5.1 / Fable 5 run safety classifiers; a hit (mostly the cyber category, and benign security work gets caught too) returns 200 with stop_reason refusal and empty content. Official Claude Code 2.1.260 sends fallbacks: [{"model":"claude-opus-5"}] plus the server-side-fallback beta on fable, so upstream reruns a refused call on Opus 5 and the user never sees the refusal. When enabled, luban adds that exact field to fable main-thread requests, with server-side-fallback-2026-06-01 in the outbound header; this is backed by a capture, so adding it brings the request closer to the 2.1.260 official shape. But it also decides for the user that a refused request is answered by Opus 5, billed at Opus rates, with the conversation stuck to Opus for about an hour, and the user never sees the refusal itself, so it is off by default and left for the user to switch on. Turned off, simulated fable requests lack that one field and the refusal is returned as is, without a rerun; the beta header is still added per version, and “beta present, field absent” is exactly the official shape before 2.1.260. A client-supplied fallbacks field is kept as is. A client-supplied array form is left alone; helper / title / classifier / quota-probe requests are not touched, as the official client never sends the field there. If upstream rejects the fallback target with a 400, the field is stripped and the request resent once, and the rule lands under “Rules learned from upstream” so the field is not added for that model again. A reply served by a fallback is priced at the model that actually served it, and the conversation sticks to the fallback model for about an hour. Requests refused before any output are not billed upstream, so their cost is recorded as 0. The luban-defined opus-5 fallback chain is a separate switch, described next. Sonnet 5 and Opus 4.7/4.8 also run cybersecurity classifiers and refuse with 200 plus stop_reason refusal; luban parses and records those refusals but does not add fallbacks for them, because the official client never sends the field on those models. That is a deliberate product limit, not an omission.',
              )}
            </>
          }
        />
        <ForwardingToggle
          k="opus_refusal_fallback"
          label={t('Opus 拒答自动换模型（实验）', 'Opus refusal fallback (experimental)')}
          summary={t(
            'opus-5 拒答时依次回退至 4.8、4.6；官方并不发送此字段，默认停用。',
            'When opus-5 refuses, fall back to 4.8 then 4.6; the official client never sends this field; off by default.',
          )}
          description={
            <>
              {t(
                '官方 Claude Code 2.1.260 的 opus 客户端只带 server-side-fallback beta，不发送 fallbacks 字段，「有 beta、无字段」即为官方形态。启用后，luban 为 opus-5 主线程请求补充自定义的 fallbacks: [{"model":"claude-opus-4-8"},{"model":"claude-opus-4-6"}]（官方为 cyber 类拒答推荐的 fallback 正是 4.8）。这是官方客户端从不产生的请求形态：封号复盘中未发现它导致 account_on_hold，但作为可被风控识别的指纹风险，它只应作为独立的实验开关，默认停用，保持官方的 opus 请求形态。其余行为同上一条：只补主线程；客户端自带的不改动；上游以 400 拒绝目标后记为规则，此后不再补充；落到 fallback 的回复按实际作答的模型计价。若日后要重新启用，更稳妥的做法是发送字符串 "default"，让上游按当前推荐的模型路由；或先读取 /v1/models 的 allowed_fallback_models，所有目标都获允许时再发送自定链。',
                'The official Claude Code 2.1.260 opus client sends only the server-side-fallback beta and no fallbacks field; “beta present, field absent” is the official shape. When enabled, luban adds a self-defined fallbacks: [{"model":"claude-opus-4-8"},{"model":"claude-opus-4-6"}] to opus-5 main-thread requests (4.8 is the fallback officially recommended for cyber refusals). That is a request shape the official client never produces: the ban post-mortem does not show it caused account_on_hold, but as a fingerprint risk it belongs behind a separate experimental switch, off by default, keeping the official opus request shape. Everything else matches the switch above: main thread only, client-supplied arrays left alone, a 400 on a fallback target is learned and the field is not added for that model again, and replies served by a fallback are priced at the model that answered. If you re-enable it later, the safer options are sending the string "default" so upstream routes to its current recommended model, or reading allowed_fallback_models from /v1/models first and only sending the custom chain when every target is allowed.',
              )}
            </>
          }
        />
      </ForwardingGroup>

      <ForwardingGroup
        icon={RefreshCwIcon}
        title={t('限流与错误恢复', 'Rate limits & error recovery')}
        description={t('限流时切换账号，以及切换账号导致 thinking 校验失败后的重试。', 'Account switching on rate limits, and retries after thinking validation fails because of a switch.')}
      >
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
                '账号用量耗尽时冷却整个账号；只有当前模型受限时仅冷却该模型。默认分别冷却 60 / 30 秒，并优先采用上游等待时间。换账号会改绑带有设备身份的请求，也可能降低缓存命中率；达到重试上限或没有其他账号时返回',
                'When an account’s usage is exhausted, the entire account is cooled down; when only the current model is limited, only that model is cooled down. The defaults are 60 / 30 seconds respectively, with the upstream wait time taking precedence. Switching accounts rebinds requests that carry a device identity and may also reduce the cache hit rate. When the retry limit is reached or no other account is available, return',
              )}{' '}
              <code className="font-mono tabular-nums">429</code>{t('。', '.')}
              {t(
                '停用时 429 原样透传：不冷却、不换账号，也不记录「套餐不含该模型」。',
                ' Turned off, 429s pass through as is: no cooldown, no account switching, and no “model not in plan” record.',
              )}
            </>
          }
        />
        <ForwardingFilterItem
          texts={[t('追加重试账号数', 'Additional retry accounts'), 'rate_limit_retry_max']}
          changed={isNumericChanged(settingsQuery.data, 'rate_limit_retry_max')}
        >
          <RetryMax />
        </ForwardingFilterItem>
        <ForwardingFilterItem
          texts={[t('提前暂停调度阈值', 'Early pause threshold'), t('用量 额度 5h 7d', 'usage quota 5h 7d'), 'quota_pause_pct']}
          changed={isNumericChanged(settingsQuery.data, 'quota_pause_pct')}
        >
          <QuotaPausePct />
        </ForwardingFilterItem>
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
                '会转为普通文本，并用同一账号重试一次，不会删除原内容。工具续跑仍可能失败；重试会增加一次请求成本，失败时返回原始 400。「拒绝改坏的历史思考块」同时启用时，只在该会话确实经过其他账号时才重试，判定规则见该项说明。',
                'content is converted to plain text and retried once with the same account; the original content is not deleted. Tool continuation may still fail. The retry adds the cost of one request, and a failure returns the original 400 response. With “Reject tampered thinking history” also on, the retry only happens when this session has actually been served by another account; see that setting for the rules.',
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
              {t('上游返回', 'The upstream returns')}{' '}
              <code className="font-mono">
                Invalid `data` in `redacted_thinking` block
              </code>{' '}
              {t(
                '时触发。这段密文由上游签发，校验不通过通常是因为会话中途换了账号，或该轮 assistant 消息在转发时被改写过（如工具名混淆）。处理方式与另外两种兜底相同：历史 thinking 降级成普通文本、redacted_thinking 整块删除后重试一次；命中时日志会输出该块在收到与发出的两份请求体中的对照，可据此判断具体成因。与「thinking 签名兜底」相同，受「拒绝改坏的历史思考块」约束。',
                '. The ciphertext is issued by the upstream, so a failure usually means the session switched accounts midway, or that assistant turn was rewritten on forwarding (for example by tool name obfuscation). Handled like the other two fallbacks: historical thinking is downgraded to plain text, redacted_thinking blocks are dropped, and the request is retried once. On a hit, the log compares that block between the received and the forwarded request body so the cause can be told apart. As with the thinking signature fallback, it is subject to “Reject tampered thinking history”.',
              )}
            </>
          }
        />
      </ForwardingGroup>

      <ForwardingGroup
        icon={KeyRoundIcon}
        title={t('登录授权范围', 'Login authorization scopes')}
        description={t(
          '添加账号时向 Claude 申请的权限范围。仅对此后新登录的账号生效，已添加的账号不受影响。',
          'Which permissions are requested from Claude when adding an account. Only affects accounts added from now on; existing ones are unchanged.',
        )}
      >
        <ForwardingFilterItem texts={[t('申请的 scope', 'Requested scopes'), 'oauth scope']}>
          <OAuthScopes />
        </ForwardingFilterItem>
      </ForwardingGroup>
    </div>
    </ForwardingFilterContext.Provider>
  )
}

/**
 * 登录时申请的 OAuth scope。
 *
 * 默认与官方 Claude Code 逐字一致（scope 集合也是指纹的一部分）；想少授权就切到精简那一档，
 * 代价是授权请求与官方不再完全相同。改动只对之后的新登录生效——已有凭证的范围在授权那一刻
 * 就定了，刷新 token 发的是官方固定那五项（扩不出时后端回退成沿用原授权），与这里无关，
 * 改这里不会追溯。
 *
 * **不校验**：填什么存什么，前后端都不判合法性。这个框就是拿来试上游认哪些 scope 的，
 * 拦一道就等于把它唯一的用途拦掉；认不认由同意页说。
 */
function OAuthScopes() {
  const { t } = useI18n()
  const { data } = useSettingsQuery()
  const [draft, setDraft] = useState('')
  const [impactOpen, setImpactOpen] = useState(false)

  useEffect(() => {
    if (data) setDraft(data.oauth_scopes)
  }, [data?.oauth_scopes])

  const save = useSettingsSave((scopes: string) => setOauthScopes(scopes), {
    success: (settings) => ({
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
    }),
  })

  const current = data?.oauth_scopes ?? ''
  const official = data?.oauth_scopes_default ?? ''
  const minimal = data?.oauth_scopes_minimal ?? ''
  // 与后端同一口径：压空白、按输入顺序去重（不排序——顺序也是指纹的一部分）。
  const items = Array.from(new Set(draft.split(/\s+/).filter(Boolean)))
  const value = items.join(' ')
  // 缺 user:profile 的号读不到账号资料：重新授权核对不了是不是同一个号，新号也记不下账号 ID。
  // 留空 = 官方默认，本就含这一项。
  const missingProfile = items.length > 0 && !items.includes('user:profile')
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
            {t(
              '以空格分隔，原样发送，须包含 user:profile；留空即恢复官方默认。',
              'Space separated and sent verbatim; must include user:profile. Leave empty to restore the official default.',
            )}{' '}
            <DetailsTrigger label={t('影响与限制', 'Impact & limitations')} open={impactOpen} onToggle={() => setImpactOpen((v) => !v)} />
          </FieldDescription>
          {impactOpen && (
            <DetailsBody>
              {t(
              '以空格分隔，按填写内容原样发送。除 user:profile 外不做校验，其余是否接受由 Claude 的同意页决定（例如完全不带 scope 会返回 Missing scope parameter）。留空则恢复官方默认的整套 scope，与官方客户端逐字一致，scope 集合也是指纹的一部分。「精简」档只保留 Luban 实际需要的三项：user:inference 用于转发（移除后账号仅能登录并查看额度）；user:profile 用于读取邮箱、等级与账号 ID，重新授权时据此核对是否为同一账号，缺了它核对无从进行、刷新 token 也只能沿用原授权，故为必填；user:file_upload 负责经 Files API 的上传。',
                'Space separated, sent verbatim. Apart from user:profile nothing is validated here; Claude\u2019s consent page decides what it accepts (omitting scope entirely, for instance, comes back as Missing scope parameter). Leave empty to restore the full official set, which is byte-for-byte what the official client requests, and the scope set is part of the fingerprint. The minimal preset keeps the three Luban actually uses: user:inference for forwarding (without it an account can only sign in and show quota); user:profile for the email, tier and account ID, which reauthorization uses to confirm it is the same account — without it that check cannot run and token refresh can only keep the original grant, so it is required; user:file_upload for uploads through the Files API.',
              )}
            </DetailsBody>
          )}
        </div>
        <Textarea
          id="oauth-scopes"
          className="font-mono"
          size="sm"
          placeholder={official}
          value={draft}
          onChange={(e) => setDraft(e.target.value)}
          aria-invalid={missingProfile || undefined}
        />
        {missingProfile && (
          <p className="text-xs leading-5 text-destructive-foreground">
            {t(
              '缺少 user:profile：读取不到账号资料，重新授权时无法核对是否为同一账号。补上此项方可保存。',
              'Missing user:profile: account details cannot be read, so reauthorization cannot confirm it is the same account. Add it to save.',
            )}
          </p>
        )}
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
            disabled={value === current || missingProfile}
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
  const { t } = useI18n()
  const { data } = useSettingsQuery()
  const [draft, setDraft] = useState<number | null>(null)

  useEffect(() => {
    if (data) setDraft(data.rate_limit_retry_max)
  }, [data?.rate_limit_retry_max])

  const save = useSettingsSave((count: number) => setRateLimitRetryMax(count), {
    success: (settings) => ({
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
    }),
  })

  const count = Math.min(10, Math.max(0, Math.floor(draft ?? 0)))
  const enabled = data?.rate_limit_retry ?? true

  return (
    <SettingsRow
      label={t('追加重试账号数', 'Additional retry accounts')}
      description={t(
        '设为 2 时最多尝试 3 个账号（含首次）。',
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
        disabled={!enabled || draft === null || count === (data?.rate_limit_retry_max ?? 2)}
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
  const { t } = useI18n()
  const { data } = useSettingsQuery()
  const [draft, setDraft] = useState<number | null>(null)
  const [weekDraft, setWeekDraft] = useState<number | null>(null)

  useEffect(() => {
    if (data) {
      setDraft(data.quota_pause_pct)
      setWeekDraft(data.quota_pause_pct_7d)
    }
  }, [data?.quota_pause_pct, data?.quota_pause_pct_7d])

  const save = useSettingsSave(
    ({ pct, week }: { pct: number; week: number }) =>
      setQuotaPausePct(pct, week),
    {
      invalidateCredentials: true,
      success: (settings) => {
        const parts = [
          settings.quota_pause_pct > 0
            ? t(`5 小时窗口 ${settings.quota_pause_pct}%`, `5h window ${settings.quota_pause_pct}%`)
            : t('5 小时窗口停用', '5h window off'),
          settings.quota_pause_pct_7d > 0
            ? t(
                `7 天窗口 ${settings.quota_pause_pct_7d}%`,
                `7d window ${settings.quota_pause_pct_7d}%`,
              )
            : t('7 天窗口停用', '7d window off'),
        ]
        return {
          title: t('提前暂停调度阈值已更新', 'Early pause threshold updated'),
          description: settings.quota_pause_pct > 0 || settings.quota_pause_pct_7d > 0
            ? t(`${parts.join(' · ')}。`, `${parts.join(' · ')}.`)
            : t(
                '两档均已停用：账号将持续参与调度，直至实际收到 429。',
                'Both thresholds are off: accounts keep taking traffic until they actually get a 429.',
              ),
        }
      },
    },
  )

  const clamp = (v: number | null) => Math.min(100, Math.max(0, Math.floor(v ?? 0)))
  const pct = clamp(draft)
  const week = clamp(weekDraft)
  const enabled = data?.rate_limit_retry ?? true
  const unchanged =
    pct === (data?.quota_pause_pct ?? 90) && week === (data?.quota_pause_pct_7d ?? 0)

  return (
    <SettingsRow
      label={t('提前暂停调度阈值', 'Early pause threshold')}
      description={t(
        '用量达到阈值时提前将账号移出调度池，以免请求触发 429；两个窗口各设一档。',
        'Take an account out of scheduling once usage reaches the threshold, before a request hits 429; each window has its own threshold.',
      )}
      details={t(
        '上游每条响应都带有账号的用量窗口使用率；达到阈值即将账号移出调度池，不必等下一条请求触发 429（那一条请求必定失败）。两个窗口各设一档，二者含义不同：因 5 小时窗口暂停调度的账号最多暂停数小时即自动恢复，因 7 天窗口暂停的则要暂停到下一次周重置。因此，周用量偏高的账号会被长时间移出调度池，即使该账号近 5 小时内未被使用。所以 7 天窗口阈值默认停用（周额度真正用完时上游会返回 429，账号级冷却照常接手）；如需启用，建议设得比 5 小时窗口阈值更高。超额用量（extra usage）接近上限时不计入判定。暂停后按触发的那个窗口的重置时刻自动恢复，也可手动启用或通过连通性测试恢复调度。填 0 表示该档不暂停调度。这里是全局值；单个账号可在账号菜单「提前暂停调度阈值」中逐档覆盖（跟随全局 / 该窗口停用 / 独立阈值）。',
        'Every upstream response reports the account’s usage window utilization; once it reaches the threshold the account leaves the scheduling pool, instead of waiting for the next request to hit a 429 (which is bound to fail). Each window gets its own threshold; do not treat them as one: a pause from the 5h window lasts a few hours at most, while a pause from the 7d window lasts until the weekly reset, so an account with heavy weekly usage would sit out entirely even when its 5h window is untouched. That is why the 7d threshold is off by default (when the weekly quota really runs out, upstream returns a 429 and the account-level cooldown takes over); if you do enable it, set it higher than the 5h one. Extra usage nearing its limit never counts. A paused account comes back automatically when the window that triggered it resets, and can also be re-enabled by hand or by a passing connectivity test. 0 turns that threshold off. These are the global values; each account can override either window from its menu under Early pause threshold (Use global / Off for this window / Custom threshold).',
      )}
      detailsLabel={t('影响与限制', 'Impact & limitations')}
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
                aria-label={t('5 小时窗口提前暂停调度阈值（%）', '5h window early pause threshold (%)')}
              />
              <NumberFieldIncrement
                aria-label={t('提高 5 小时窗口阈值', 'Increase 5h window threshold')}
              />
            </NumberFieldGroup>
          </NumberField>
        </div>
        <div className="row-span-2 grid grid-rows-subgrid gap-y-1.5">
          <FieldDescription>
            {t('7 天窗口（0 = 停用）', '7d window (0 = off)')}
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
                aria-label={t('7 天窗口提前暂停调度阈值（%）', '7d window early pause threshold (%)')}
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
          disabled={!enabled || unchanged || draft === null || weekDraft === null}
          onClick={() => save.mutate({ pct, week })}
        >
          <SaveIcon />
          <span className="max-sm:sr-only">{t('保存', 'Save')}</span>
        </Button>
      </div>
    </SettingsRow>
  )
}

/**
 * 页内筛选：搜索词与「只看已改动」。各行自己判断要不要渲染（[`ForwardingFilterItem`]），
 * 分组与空状态的显隐交给 CSS（`index.css` 里的 `data-forwarding-*`）：一组里一项都没剩就整组
 * 收起，整页一项都没剩才亮出空状态——这样不必把四十个开关改写成一张数据表才能算「剩几项」。
 */
type ForwardingFilter = { query: string; changedOnly: boolean }
const ForwardingFilterContext = createContext<ForwardingFilter>({ query: '', changedOnly: false })
/** 所在分组的标题命中了搜索词：组内各项一律显示（搜「遥测」应当看到整组）。 */
const GroupMatchContext = createContext(false)

/** 空格分隔的每个词都得在某段文字里出现（不分大小写）；空搜索词恒命中。 */
function matchesQuery(query: string, texts: readonly (string | undefined)[]) {
  const terms = query.trim().toLowerCase().split(/\s+/).filter(Boolean)
  if (terms.length === 0) return true
  const haystack = texts.filter(Boolean).map((text) => text!.toLowerCase())
  return terms.every((term) => haystack.some((text) => text.includes(term)))
}

/** 存储值与出厂默认值不同。后端没给默认表或缺这一项时一律当没改。 */
function isChanged(data: Settings | undefined, k: ForwardingKey) {
  const value = data?.[k]
  const fallback = data?.forwarding_defaults?.[k]
  return value != null && fallback != null && value !== fallback
}

/** 与出厂默认值不同、且当前模式下显示着的开关，顺序按默认表。 */
function changedForwardingKeys(data: Settings | undefined): ForwardingKey[] {
  if (!data?.forwarding_defaults) return []
  return (Object.keys(data.forwarding_defaults) as ForwardingKey[]).filter(
    (k) => isChanged(data, k) && !hiddenInCurrentMode(data, k),
  )
}

/**
 * 「全部恢复默认」实际要拨回的开关：当前显示着的改动项，加上拨回之后会**重新显示**的改动项。
 *
 * 「仅 billing 标识」生效时，拨回 `sim_billing_only` 会让模式回到「完整对齐」，原先被隐藏的
 * 那些开关随之重新生效；只拨显示着的，点完它们就以「已改动」冒出来，一键恢复等于没恢复干净。
 * 一轮就够：决定模式的三项（`sim_billing_only`/`simulate_cc`/`merge_beta`）从不隐藏，第一轮
 * 已经拨回，新加进来的项不会再改变模式。
 */
function resetForwardingKeys(data: Settings | undefined): ForwardingKey[] {
  if (!data?.forwarding_defaults) return []
  const visible = changedForwardingKeys(data)
  const after: Settings = { ...data }
  for (const k of visible) after[k] = data.forwarding_defaults[k]
  return (Object.keys(data.forwarding_defaults) as ForwardingKey[]).filter(
    (k) => isChanged(data, k) && (visible.includes(k) || !hiddenInCurrentMode(after, k)),
  )
}

/** 页内的数值参数：与开关一样计入「已改动」、随「全部恢复默认」拨回，默认值取后端给的。 */
type NumericSettingKey = 'rate_limit_retry_max' | 'quota_pause_pct'

function isNumericChanged(data: Settings | undefined, k: NumericSettingKey) {
  if (!data) return false
  switch (k) {
    case 'rate_limit_retry_max':
      return data.rate_limit_retry_max_default != null
        && data.rate_limit_retry_max !== data.rate_limit_retry_max_default
    case 'quota_pause_pct':
      return data.quota_pause_pct_default != null
        && (data.quota_pause_pct !== data.quota_pause_pct_default
          || data.quota_pause_pct_7d !== data.quota_pause_pct_7d_default)
  }
}

function changedNumericKeys(data: Settings | undefined): NumericSettingKey[] {
  return (['rate_limit_retry_max', 'quota_pause_pct'] as const).filter((k) => isNumericChanged(data, k))
}

function ChangedBadge() {
  const { t } = useI18n()
  return (
    <Badge variant="info" size="sm">
      {t('已改动', 'Modified')}
    </Badge>
  )
}

/** 一行设置项的筛选外壳：不命中就不渲染；`data-forwarding-item` 供分组判断自己还剩没剩东西。 */
function ForwardingFilterItem({
  texts,
  changed = false,
  children,
}: {
  texts: readonly (string | undefined)[]
  changed?: boolean
  children: ReactNode
}) {
  const { query, changedOnly } = useContext(ForwardingFilterContext)
  const groupMatched = useContext(GroupMatchContext)
  if (changedOnly && !changed) return null
  if (!groupMatched && !matchesQuery(query, texts)) return null
  return <div data-forwarding-item>{children}</div>
}

/** 转发页的分组：在 [`SettingsGroup`] 外面挂上筛选需要的标记。 */
function ForwardingGroup({
  title,
  ...props
}: {
  icon?: LucideIcon
  title: string
  description?: string
  children: ReactNode
}) {
  const { query } = useContext(ForwardingFilterContext)
  const matched = query.trim() !== '' && matchesQuery(query, [title])
  return (
    <div data-forwarding-group>
      <GroupMatchContext.Provider value={matched}>
        <SettingsGroup title={title} {...props} />
      </GroupMatchContext.Provider>
    </div>
  )
}

/**
 * 页首工具条：搜索、只看已改动、全部恢复默认。
 *
 * 这些开关多是排查上游兼容性时临时拨的，拨完容易忘了拨回；「只看已改动」把它们一次列出来，
 * 「全部恢复默认」一次拨回出厂值。计数与列表同一口径：开关加页内的数值参数（重试账号数、
 * 暂停阈值），当前模式下隐藏的开关不算；恢复在此之外再带上拨回后会重新显示的隐藏项，
 * 见 [`resetForwardingKeys`]。
 */
function ForwardingToolbar({
  query,
  onQueryChange,
  changedOnly,
  onChangedOnlyChange,
}: {
  query: string
  onQueryChange: (query: string) => void
  changedOnly: boolean
  onChangedOnlyChange: (changedOnly: boolean) => void
}) {
  const { t } = useI18n()
  const { data } = useSettingsQuery()
  const qc = useQueryClient()
  const [confirmReset, setConfirmReset] = useState(false)
  const changedToggles = changedForwardingKeys(data)
  const changedNumeric = changedNumericKeys(data)
  const changedCount = changedToggles.length + changedNumeric.length
  // 恢复的范围比计数多出来的，是拨回后会重新显示的隐藏项；仍隐藏的改动项不动。
  const resetToggles = resetForwardingKeys(data)
  const resetCount = resetToggles.length + changedNumeric.length
  const revealedCount = resetToggles.length - changedToggles.length
  const untouchedCount =
    (data?.forwarding_defaults
      ? (Object.keys(data.forwarding_defaults) as ForwardingKey[]).filter((k) => isChanged(data, k)).length
      : 0) - resetToggles.length

  const reset = useSettingsSave(
    async ({ toggles, numeric }: { toggles: ForwardingKey[]; numeric: NumericSettingKey[] }) => {
      const d = data!
      let settings: Settings | undefined
      try {
        if (toggles.length > 0) {
          settings = await setForwardingMany(
            Object.fromEntries(toggles.map((k) => [k, d.forwarding_defaults[k]])),
          )
        }
        if (numeric.includes('rate_limit_retry_max')) {
          settings = await setRateLimitRetryMax(d.rate_limit_retry_max_default)
        }
        if (numeric.includes('quota_pause_pct')) {
          settings = await setQuotaPausePct(d.quota_pause_pct_default, d.quota_pause_pct_7d_default)
        }
      } catch (error) {
        // 三个接口依次调，前面的可能已经写进去了：重新拉一次，页面别停在旧值上。
        if (settings) qc.setQueryData(['settings'], settings)
        void qc.invalidateQueries({ queryKey: ['settings'] })
        throw error
      }
      return settings!
    },
    {
      invalidateCredentials: true,
      onSuccess: () => setConfirmReset(false),
      success: (_settings, { toggles, numeric }) => {
        const n = toggles.length + numeric.length
        return {
          title: t('已恢复默认', 'Defaults restored'),
          description: t(
            `${n} 项设置已恢复为出厂默认值。`,
            `${n} ${n === 1 ? 'setting was' : 'settings were'} restored to the default.`,
          ),
        }
      },
    },
  )

  return (
    <div className="flex flex-wrap items-center gap-2">
      <ToolbarSearch
        ariaLabel={t('搜索转发策略', 'Search forwarding policy')}
        className="min-w-0 basis-full sm:max-w-80 sm:flex-1 sm:basis-auto"
        placeholder={t('搜索名称、说明或键名', 'Search name, summary or key')}
        value={query}
        onChange={onQueryChange}
      />
      <Toggle
        variant="outline"
        className={cn(changedOnly && ACTIVE_FILTER_CLASS)}
        pressed={changedOnly}
        onPressedChange={onChangedOnlyChange}
      >
        <FilterIcon />
        {t('只看已改动', 'Modified only')}
        <span className="tabular-nums text-muted-foreground">{changedCount}</span>
      </Toggle>
      {changedCount > 0 && (
        <Button variant="ghost" className="text-muted-foreground" onClick={() => setConfirmReset(true)}>
          <RotateCcwIcon />
          {t('全部恢复默认', 'Restore all defaults')}
        </Button>
      )}
      {/* 原来是页首一整条 Alert，一句话占一张卡；收成工具条上的提示图标，悬停看。 */}
      <InfoHint label={t('必要请求头不受影响', 'Required headers are unaffected')}>
        {t(
          '必要请求头不受影响：下列开关都不影响 Authorization 的注入。',
          'Required headers are unaffected: none of these switches affect Authorization injection.',
        )}
      </InfoHint>

      <AlertDialog open={confirmReset} onOpenChange={setConfirmReset}>
        <AlertDialogPopup>
          <AlertDialogHeader>
            <AlertDialogTitle>{t('全部恢复默认', 'Restore all defaults')}</AlertDialogTitle>
            <AlertDialogDescription>
              {t(
                `将把 ${resetCount} 项改动过的设置（含重试账号数、暂停阈值等数值参数）恢复为出厂默认值，立即对之后的请求生效。`,
                `${resetCount} modified ${resetCount === 1 ? 'setting' : 'settings'} (including numeric ones such as retry accounts and pause thresholds) will return to the default, taking effect for subsequent requests.`,
              )}
              {revealedCount > 0 && t(
                `改写模式随之回到「完整对齐」，其中 ${revealedCount} 项是此前隐藏、届时重新生效的开关，一并恢复。`,
                ` The rewrite mode returns to Full alignment, so this includes ${revealedCount} previously hidden ${revealedCount === 1 ? 'switch' : 'switches'} that take effect again.`,
              )}
              {untouchedCount > 0 && t(
                `另有 ${untouchedCount} 项在当前模式下隐藏的开关保持不变。`,
                ` ${untouchedCount} hidden ${untouchedCount === 1 ? 'switch stays' : 'switches stay'} as is.`,
              )}
            </AlertDialogDescription>
          </AlertDialogHeader>
          <AlertDialogFooter>
            <AlertDialogClose render={<Button variant="outline" />}>{t('取消', 'Cancel')}</AlertDialogClose>
            <Button
              loading={reset.isPending}
              onClick={() => reset.mutate({ toggles: resetToggles, numeric: changedNumeric })}
            >
              {t('恢复默认', 'Restore defaults')}
            </Button>
          </AlertDialogFooter>
        </AlertDialogPopup>
      </AlertDialog>
    </div>
  )
}

/**
 * 改写模式：完整对齐，或只保证一条 billing 标识。
 *
 * 存的仍是 `sim_billing_only` 这一个开关，判据也与后端 `ForwardFlags::billing_only` 一致（还要
 * 「模拟 Claude Code」与「Beta 标记」开着）。之所以从「非官方客户端模拟」组里提到页首：它对
 * **所有**来访生效、一开就让十几项形态开关失效，是决定整页怎么读的总开关，不是模拟的一个子项。
 * 两项 `user_id` 子开关只在这一档生效，随模式收进这张卡，完整对齐时不显示。
 */
function RewriteModeGroup({
  simulateRequires,
  userIdToggles,
  description,
}: {
  simulateRequires: ForwardingRequire[]
  userIdToggles: ReactNode
  description: ReactNode
}) {
  const { t } = useI18n()
  const { data } = useSettingsQuery()
  const blockedBy = simulateRequires.find((r) => data?.[r.key] === false)
  const effective = billingOnlyActive(data)
  const [impactOpen, setImpactOpen] = useState(false)
  const fullLabel = t('完整对齐', 'Full alignment')
  const billingLabel = t('仅 billing 标识', 'Billing identifier only')

  const save = useSettingsSave((next: boolean) => setForwarding('sim_billing_only', next), {
    invalidateCredentials: true,
    success: (settings) => ({
      title: t(
        `改写模式已切换为「${settings.sim_billing_only ? billingLabel : fullLabel}」`,
        `Rewrite mode switched to “${settings.sim_billing_only ? billingLabel : fullLabel}”`,
      ),
    }),
  })

  return (
    <ForwardingGroup
      icon={LayersIcon}
      title={t('改写模式', 'Rewrite mode')}
      description={t(
        '决定转发时改写请求的幅度，对所有客户端生效。',
        'How far requests are rewritten on the way upstream; applies to every client.',
      )}
    >
      <ForwardingFilterItem
        texts={[fullLabel, billingLabel, 'billing', 'sim_billing_only']}
        changed={isChanged(data, 'sim_billing_only')}
      >
        <div className="space-y-3 p-4 sm:p-5">
          <RadioGroup
            aria-label={t('改写模式', 'Rewrite mode')}
            className="grid gap-3 sm:grid-cols-2"
            disabled={blockedBy != null || save.isPending}
            value={effective ? 'billing' : 'full'}
            onValueChange={(value) => save.mutate(value === 'billing')}
          >
            <ModeOption
              value="full"
              title={fullLabel}
              badge={<Badge variant="secondary" size="sm">{t('默认', 'Default')}</Badge>}
              description={t(
                '按官方客户端形态对齐每条请求，第三方客户端按 Claude Code 完整模拟。',
                'Align every request with the official client; third-party clients are fully emulated as Claude Code.',
              )}
            />
            <ModeOption
              value="billing"
              title={billingLabel}
              badge={
                <>
                  <Badge variant="warning" size="sm">{t('实验', 'Experimental')}</Badge>
                  {/* 按存储值标：选了本模式但前置开关关着、没生效时也标，与计数口径一致。 */}
                  {isChanged(data, 'sim_billing_only') && <ChangedBadge />}
                </>
              }
              description={t(
                '只补一条合法的 billing 标识，其余原样透传，保留客户端自己的提示词。',
                'Only add a valid billing identifier and pass the rest through, keeping the client’s own prompt.',
              )}
            />
          </RadioGroup>
          <p className="text-xs leading-5 text-muted-foreground">
            {blockedBy
              ? data?.sim_billing_only
                ? t(
                    `已选「${billingLabel}」，但「${blockedBy.label}」停用期间不生效，目前按「${fullLabel}」转发。`,
                    `“${billingLabel}” is selected but has no effect while “${blockedBy.label}” is disabled; requests are forwarded with “${fullLabel}” for now.`,
                  )
                : t(
                    `需先启用「${blockedBy.label}」才能切换到「${billingLabel}」。`,
                    `Enable “${blockedBy.label}” first to switch to “${billingLabel}”.`,
                  )
              : effective
                ? t(
                    '本模式下不生效的形态开关已隐藏，存储值保留，切回「完整对齐」后恢复。',
                    'Switches with no effect in this mode are hidden; saved values are kept and return after switching back to “Full alignment”.',
                  )
                : null}{' '}
            <DetailsTrigger label={t('影响与限制', 'Impact & limitations')} open={impactOpen} onToggle={() => setImpactOpen((v) => !v)} />
          </p>
          {impactOpen && <DetailsBody>{description}</DetailsBody>}
        </div>
      </ForwardingFilterItem>
      {effective && userIdToggles}
    </ForwardingGroup>
  )
}

function ModeOption({
  value,
  title,
  badge,
  description,
}: {
  value: string
  title: string
  badge?: ReactNode
  description: string
}) {
  return (
    <label className="flex cursor-pointer items-start gap-3 rounded-lg border p-3 transition-colors hover:bg-accent/40 has-data-checked:border-primary/48 has-data-checked:bg-accent/40 has-data-disabled:cursor-not-allowed has-data-disabled:opacity-64 has-data-disabled:hover:bg-transparent">
      <Radio value={value} className="mt-0.5" />
      <span className="min-w-0 space-y-1">
        <span className="flex flex-wrap items-center gap-2 text-sm font-medium">
          {title}
          {badge}
        </span>
        <span className="block text-xs leading-5 text-muted-foreground">{description}</span>
      </span>
    </label>
  )
}

/**
 * 改写模式「仅 billing 标识」实际生效时后端整个跳过的开关（按 `billing_only` 门控）：此时不显示，
 * 存储值不动，切回「完整对齐」后原样回来；一组全被跳过时整组随之收起（`data-forwarding-*`）。
 * 「只看已改动」的计数同样不算它们；「全部恢复默认」只在会把模式拨回「完整对齐」时一并拨回
 * （届时它们重新生效），见 [`resetForwardingKeys`]。
 */
const BILLING_ONLY_SKIPPED: ReadonlySet<ForwardingKey> = new Set<ForwardingKey>([
  'fill_metadata',
  'strip_extra_fields',
  'eager_tool_streaming',
  'system_shape',
  'cache_scope_global',
  'cache_ttl_1h',
  'inject_thinking',
  'simulate_full_system',
  'relocate_client_system',
  'fill_absent_tools',
  'sim_trim_tools',
  'sim_message_threads',
  'fable_refusal_fallback',
  'opus_refusal_fallback',
])

/** 只在「仅 billing 标识」实际生效时才显示的开关（改写模式卡里的两项 `user_id` 子开关，以及工具名混淆的范围子开关）。 */
const BILLING_ONLY_EXCLUSIVE: ReadonlySet<ForwardingKey> = new Set<ForwardingKey>([
  'sim_billing_keep_user_id',
  'real_billing_keep_user_id',
  'sim_billing_mimic_all',
  'sim_billing_relocate',
])

/** 当前改写模式下页面不显示这一项：「已改动」计数与「全部恢复默认」都跳过它。 */
function hiddenInCurrentMode(data: Settings | undefined, k: ForwardingKey) {
  return billingOnlyActive(data) ? BILLING_ONLY_SKIPPED.has(k) : BILLING_ONLY_EXCLUSIVE.has(k)
}

/** 开关的一项前置条件：它关着时本项不生效。 */
type ForwardingRequire = { key: ForwardingKey; label: string }

/**
 * 改写模式「仅 billing 标识」是否实际生效：开关本身开着，且模拟与 Beta 标记都开着——与后端
 * `ForwardFlags::billing_only` 同一判据（缺省值按后端默认：billing-only 停用、其余启用）。
 */
function billingOnlyActive(data: Partial<Record<ForwardingKey, boolean>> | undefined) {
  return data?.sim_billing_only === true && data?.simulate_cc !== false && data?.merge_beta !== false
}

/** 单个开关：读写都走 ['settings']，改完让账号列表也失效。 */
function ForwardingToggle({
  k,
  label,
  summary,
  description,
  requires,
  requiresAny,
  inactiveWhileOn,
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
  requires?: ForwardingRequire | ForwardingRequire[]
  /**
   * 反向依赖：这一项只在它**停用**时才生效（后端同样这么判），开着时同样置灰并说明，存储值不动。
   */
  inactiveWhileOn?: ForwardingRequire
  /**
   * 几个前置开关**任一**开着才生效（后端同样这么判）：全关时置灰并说明，存储值不动。
   */
  requiresAny?: ForwardingRequire[]
}) {
  const { t } = useI18n()
  const id = useId()
  const { data } = useSettingsQuery()
  const enabled = data?.[k] ?? true
  // 多层依赖（如模拟子项 → 模拟 → Beta 标记）逐层判，提示第一个关着的。
  const blockedBy = (Array.isArray(requires) ? requires : requires ? [requires] : []).find(
    (r) => data?.[r.key] === false,
  )
  // 「任一」依赖：一个都没开就算挡住，提示里把几项都列出来。
  const blockedByAny =
    requiresAny != null && requiresAny.length > 0 && requiresAny.every((r) => data?.[r.key] === false)
      ? requiresAny
      : null
  const blocked = blockedBy != null || blockedByAny != null
  // 缺省值按后端默认：反向依赖的那几项（如拒绝探针请求）默认启用。
  const overridden = !blocked && inactiveWhileOn != null && data?.[inactiveWhileOn.key] !== false
  // 当前改写模式下后端不读这一项（「仅 billing 标识」生效时整个跳过的那批，或只在该模式下才有意义的
  // 子开关）：拨了也没用，直接不显示（见下面的 return null），与「已改动」计数、「全部恢复默认」同一判据。
  const skipped = hiddenInCurrentMode(data, k)
  const inactive = blocked || overridden
  const changed = isChanged(data, k)

  const save = useSettingsSave((next: boolean) => setForwarding(k, next), {
    invalidateCredentials: true,
    success: (settings) => ({
      title: settings[k]
        ? t(`${label}已启用`, `${label} enabled`)
        : t(`${label}已停用`, `${label} disabled`),
      description: summary,
    }),
  })

  if (skipped) return null

  return (
    <ForwardingFilterItem texts={[label, summary, k]} changed={changed}>
    <SettingsRow
      disabled={inactive}
      inlineControl
      htmlFor={id}
      label={label}
      badge={changed && <ChangedBadge />}
      description={
          blockedBy
            ? t(`需先启用「${blockedBy.label}」`, `Enable “${blockedBy.label}” first`)
            : blockedByAny
              ? t(
                  `需先启用${blockedByAny.map((r) => `「${r.label}」`).join('或')}`,
                  `Enable ${blockedByAny.map((r) => `“${r.label}”`).join(' or ')} first`,
                )
            : overridden
              ? t(
                  `「${inactiveWhileOn.label}」启用期间不生效`,
                  `Inactive while “${inactiveWhileOn.label}” is enabled`,
                )
            : description
              // 挂着「影响与限制」的行，摘要不再自带第二个展开器（「了解更多」）：同一行两套
              // 展开交互。全部摘要里只有一条超过收起阈值，多出的一两行铺开就是了。
              ? summary
              : <ClampedDescription text={summary} />
      }
      details={description}
      detailsLabel={t('影响与限制', 'Impact & limitations')}
    >
      <Switch
        id={id}
        checked={enabled && !inactive}
        disabled={save.isPending || inactive}
        onCheckedChange={(next) => save.mutate(next)}
      />
    </SettingsRow>
    </ForwardingFilterItem>
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
      {/* 行首不再挂种类圆点：同一行里的种类徽章已经带着同一个颜色和文字。圆点只留在上面的筛选条里当图例。 */}
      <div className="flex items-start gap-3">
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
              {t('最近记录于', 'Latest')} {relativeTime(group.latest, undefined, language)}
            </TooltipTrigger>
            <TooltipPopup>{formatFullTime(group.latest, language)}</TooltipPopup>
          </Tooltip>
        </button>
        <Hint label={t('删除该组规则', 'Remove this rule group')}>
          <Button
            aria-label={t('删除该组规则', 'Remove this rule group')}
            size="icon"
            variant="ghost"
            className="shrink-0 text-muted-foreground hover:text-foreground"
            onClick={onDeleteGroup}
          >
            <Trash2Icon />
          </Button>
        </Hint>
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
                    <Hint label={t('删除这条规则', 'Remove this rule')}>
                      <Button
                        aria-label={t('删除这条规则', 'Remove this rule')}
                        size="icon-xs"
                        variant="ghost"
                        className="shrink-0 text-muted-foreground hover:text-foreground"
                        disabled={forgetPending}
                        onClick={() => onForget(row)}
                      >
                        <Trash2Icon />
                      </Button>
                    </Hint>
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
            <div className="grid grid-cols-[minmax(0,1fr)_auto_minmax(0,1fr)] items-center gap-2 sm:gap-3 border-t bg-muted/30 px-3 py-2 text-xs">
              <p className="min-w-0 text-muted-foreground tabular-nums">
                <span className="max-sm:hidden">{t(`第 ${firstIndex}–${lastIndex} 条，共 ${total} 条`, `${firstIndex}–${lastIndex} of ${total}`)}</span>
                <span className="sm:hidden">{`${firstIndex}–${lastIndex} / ${total}`}</span>
              </p>
              {/* 窄屏也排成一行：计数缩成「1–10 / 29」、翻页只写「1 / 3」、藏掉「每页」二字，
                三栏放得下，不再把翻页挤到第二行。 */}
              <div className="col-start-3 row-start-1 flex items-center gap-2 justify-self-end">
                <span className="whitespace-nowrap text-muted-foreground max-sm:hidden">{t('每页', 'Per page')}</span>
                <Select
                  items={GROUP_PAGE_SIZES.map((size) => ({ value: size, label: String(size) }))}
                  value={pageSize}
                  onValueChange={(value) => {
                    if (value == null) return
                    setPageSize(Number(value))
                    setPage(0)
                  }}
                >
                  <SelectTrigger size="sm" className="w-auto min-w-16 sm:min-w-20" aria-label={t('每页条数', 'Rows per page')}>
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
                <Pagination className="col-start-2 row-start-1 justify-center">
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
                        <span className="max-sm:hidden">{t(`第 ${currentPage + 1} / ${totalPages} 页`, `Page ${currentPage + 1} of ${totalPages}`)}</span>
                        <span className="sm:hidden">{`${currentPage + 1} / ${totalPages}`}</span>
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
      toastManager.add({ title: t('已删除该组规则', 'Rule group removed'), type: 'success' })
    },
    onError: (e) => { setConfirmClear(null); failure(t('删除失败', 'Failed to remove'), e) },
  })

  const rows = query.data ?? []
  const kindLabel = (kind: string) =>
    kind === 'shape'
      ? t('本地拒绝', 'Rejected locally')
      : kind === 'deprecated'
        ? t('不再补 fallbacks', 'Fallbacks no longer added')
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
                {t('记录于', 'Learned')} {relativeTime(row.learned_at, undefined, language)}
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
        <Hint label={t('删除这条规则', 'Remove this rule')}>
          <Button
            aria-label={t('删除这条规则', 'Remove this rule')}
            size="icon"
            variant="ghost"
            className="shrink-0 text-muted-foreground hover:text-foreground"
            disabled={forget.isPending}
            onClick={() => forget.mutate(row)}
          >
            <Trash2Icon />
          </Button>
        </Hint>
      </li>
    )
  }

  return (
    <SettingsGroup
      icon={BrainIcon}
      title={t('从上游学到的规则', 'Rules learned from upstream')}
      // 紧凑表头的说明进 ⓘ 浮层，原来那段六百字放不下；各类规则的细节在「本地拦截」里对应开关的
      // 「影响与限制」中都有，这里只留一句「这是什么、怎么处理」。
      description={t(
        '上游明确拒绝过的组合（400 形态错误、零输出请求类、分类器拒答的提示词、被拒的 fallbacks）会记在这里，下次在本地直接处理，不再发往上游。规则 7 天后自动过期；上游放开后仍在拦截的，可在此删除。',
        'Combinations upstream has explicitly rejected (400 shape errors, empty-reply request classes, classifier-refused prompts, rejected fallbacks) are recorded here and handled locally next time instead of going upstream. Rules expire after 7 days; delete one here if upstream has since allowed it.',
      )}
    >
      {query.isPending ? (
        <LoadingState className="min-h-24" label={t('正在加载规则', 'Loading rules')} />
      ) : query.isError ? (
        <ErrorState
          error={query.error}
          title={t('无法读取已学习的规则', 'Unable to load learned rules')}
          onRetry={() => query.refetch()}
          retrying={query.isFetching}
        />
      ) : rows.length === 0 ? (
        <Empty className="py-10 md:py-12">
          <EmptyHeader>
            <EmptyMedia variant="icon">
              <BrainIcon />
            </EmptyMedia>
            <EmptyTitle className="text-base sm:text-base">
              {t('尚未学到任何规则', 'No rules learned yet')}
            </EmptyTitle>
            <EmptyDescription className="text-xs leading-5">
              {t(
                '上游拒绝某个取值、拒绝 luban 补的 fallbacks，或对某条提示词拒答之后，规则会自动出现在这里。',
                'Rules appear here on their own once upstream rejects a value, rejects the fallbacks luban added, or refuses a prompt.',
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
              <ToolbarSearch
                ariaLabel={t('搜索规则', 'Search rules')}
                className="w-52"
                hotkey={false}
                placeholder={t('模型 / 哈希 / 原文', 'Model / hash / text')}
                size="sm"
                value={search}
                onChange={setSearch}
              />
              {kindFilter !== 'all' && (counts[kindFilter] ?? 0) > 0 && (
                <Button size="xs" variant="outline" onClick={() => setConfirmClear({ type: 'kind', kind: kindFilter })}>
                  <Trash2Icon />
                  {t(`清空该类 ${counts[kindFilter]}`, `Clear this category ${counts[kindFilter]}`)}
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
              {needle ? t('没有匹配的规则。', 'No matching rules.') : t('该类别下暂无规则。', 'No rules in this category.')}
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
                      ? t('删除该组规则', 'Remove this rule group')
                      : t('清空学到的规则', 'Clear learned rules')}
                </AlertDialogTitle>
                <AlertDialogDescription>
                  {confirmClear?.type === 'kind'
                    ? t(
                        `将删除该类别的 ${counts[confirmClear.kind] ?? 0} 条规则，其他种类不受影响。此后相同的组合会再次发往上游，若再被拒绝，将重新学习为规则。`,
                        `${counts[confirmClear.kind] ?? 0} rules in this category will be removed; other categories are unaffected. The same combinations will be sent upstream once more and re-learned if rejected.`,
                      )
                    : confirmClear?.type === 'group'
                      ? t(
                          `将删除 ${confirmClear.model}${confirmClear.category ? ` 的 ${confirmClear.category} 类` : ''}共 ${confirmClear.count} 条拒答提示词规则，其他模型、其他类别不受影响。此后这些提示词逐字重发时会再次发往上游，若再被拒绝，将重新学习为规则。`,
                          `${confirmClear.count} refused-prompt rules for ${confirmClear.model}${confirmClear.category ? ` (${confirmClear.category})` : ''} will be removed; other models and categories are untouched. Resending those prompts verbatim will reach upstream once more and be re-learned if refused.`,
                        )
                      : t(
                          `将删除全部 ${rows.length} 条规则。此后相同的组合会再次发往上游，若再被拒绝，将重新学习为规则。`,
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
