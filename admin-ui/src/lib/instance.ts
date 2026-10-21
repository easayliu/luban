import { useMe } from '@/lib/role'

/** 本实例的名称（系统设置 → 客户端接入 → 实例名称）；未命名或未登录为 null。 */
export function useInstanceName(): string | null {
  return useMe().data?.instance_name || null
}

/** 取字母数字、其余折成连字符，用作 MCP 服务名的一段；一个都不剩回空串。 */
function slug(text: string): string {
  return text
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, '-')
    .replace(/^-+|-+$/g, '')
}

/** 原文的短指纹（FNV-1a 32 位，base36 补足 6 位），同一原文恒得同一值。 */
function shortHash(text: string): string {
  let h = 0x811c9dc5
  for (const b of new TextEncoder().encode(text)) {
    h ^= b
    h = Math.imul(h, 0x01000193) >>> 0
  }
  return h.toString(36).padStart(6, '0').slice(-6)
}

/** 实例名称能否用作服务名：只含英文字母、数字、空格、`-`、`_`。 */
const ASCII_NAME = /^[A-Za-z0-9 _-]+$/

/**
 * 接入 Claude Code 时给本实例起的 MCP 服务名，多个 luban 同时接入时据此区分（工具名形如
 * `mcp__<服务名>__get_request`）。服务名只收字母、数字与连字符：
 * - 实例名称只含英文字母、数字、空格、`-`、`_` 时以它为底（`billingonly` → `luban-billingonly`）；
 *   含中文等其他字符或未命名时，以站点域名为底；
 * - 折算会丢信息时（大小写、空格、`_`、域名里的点都会折成小写与连字符），在末尾接原文的短指纹：
 *   `hk.example.com` 与 `hk-example.com` 都折成 `hk-example-com`，不加区分就会撞名，后一条接入
 *   命令因同名而失败。原文本就合规的（`billingonly`）不加，保持好认。
 */
export function mcpServerName(instanceName: string | null, host: string): string {
  const name = instanceName?.trim() ?? ''
  // 名称合规却折不出字母数字（`---`）的，与未命名一样以域名为底。
  const source = ASCII_NAME.test(name) && slug(name) ? name : host.trim().toLowerCase()
  let part = slug(source)
  if (!part) return 'luban'
  if (part !== source) part = `${part}-${shortHash(source)}`
  return part === 'luban' || part.startsWith('luban-') ? part : `luban-${part}`
}
