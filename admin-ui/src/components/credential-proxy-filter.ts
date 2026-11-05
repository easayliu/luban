import type { Credential } from '@/api/credentials'
import type { SavedProxy } from '@/api/proxies'

type Translate = (zh: string, en: string) => string

/**
 * 账号池的「代理」筛选：`all`、`direct`（直连）、`custom`（自定义地址，不在代理池里）、
 * `proxy:<id>`（代理池里的某一条）。存进链接和 localStorage 的就是这个串。
 */
export type CredentialProxyFilter = string

export function parseProxyFilter(raw: string): CredentialProxyFilter | null {
  return raw === 'all' || raw === 'direct' || raw === 'custom' || /^proxy:\d+$/.test(raw) ? raw : null
}

/** 一个号落在哪一档：与筛选值同一套串，`all` 之外的各项都由它算出。 */
export function proxyBucket(credential: Pick<Credential, 'proxy' | 'proxy_id'>): CredentialProxyFilter {
  if (!credential.proxy) return 'direct'
  return credential.proxy_id == null ? 'custom' : `proxy:${credential.proxy_id}`
}

export interface ProxyFacet {
  value: CredentialProxyFilter
  label: string
  count: number
}

/**
 * 下拉里的各组：直连与自定义一组，代理池里的各条按 id 一组。只列有号在用的；池里没有号时
 * 返回空。名称先查自己的代理池，查不到退回号上带的 `proxy_label`
 * （代理看下属的号：那条在下属的池里），再不行只写 id。
 */
export function proxyFacets(
  pool: readonly Credential[],
  proxies: readonly SavedProxy[] | undefined,
  t: Translate,
): ProxyFacet[][] {
  const counts = new Map<CredentialProxyFilter, number>()
  const labels = new Map<number, string>()
  for (const c of pool) {
    const bucket = proxyBucket(c)
    counts.set(bucket, (counts.get(bucket) ?? 0) + 1)
    if (c.proxy_id != null && c.proxy_label && !labels.has(c.proxy_id)) labels.set(c.proxy_id, c.proxy_label)
  }
  // 只有一档时也照常出：藏起来会让人以为没有这个筛选，号多了再找不到入口。
  if (counts.size === 0) return []
  for (const p of proxies ?? []) labels.set(p.id, p.label)

  const head: ProxyFacet[] = []
  for (const [value, label] of [['direct', t('直连', 'Direct')], ['custom', t('自定义代理', 'Custom proxy')]] as const) {
    const count = counts.get(value) ?? 0
    if (count > 0) head.push({ value, label, count })
  }
  const saved = [...counts]
    .filter(([value]) => value.startsWith('proxy:'))
    .map(([value, count]) => {
      const id = Number(value.slice('proxy:'.length))
      const name = labels.get(id)?.trim()
      return { id, facet: { value, label: name ? `#${id} ${name}` : `#${id}`, count } }
    })
    .sort((a, b) => a.id - b.id)
    .map(({ facet }) => facet)
  return [head, saved].filter((group) => group.length > 0)
}
