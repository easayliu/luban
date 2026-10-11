import type { Me } from '@/api/auth'
import type { Credential } from '@/api/credentials'
import type { ConsoleUser } from '@/api/users'

type Translate = (zh: string, en: string) => string

/**
 * 账号池的「成员」筛选：`all`、`team:<id>`（代理本人及下属用户的号，管理员那一队是本人及直属
 * 用户）、`user:<id>`（只看这一个人名下的号）。存进链接和 localStorage 的就是这个串。
 */
export type CredentialOwnerFilter = string

export function parseOwnerFilter(raw: string): CredentialOwnerFilter | null {
  return raw === 'all' || /^(team|user):\d+$/.test(raw) ? raw : null
}

export interface OwnerFacet {
  value: CredentialOwnerFilter
  label: string
  /** 队里的某一个人：在下拉里缩进一档，挂在那一队的下面。 */
  nested: boolean
  owners: ReadonlySet<number>
  count: number
}

/**
 * 下拉里的各组。没有号的人与队不列；一队里只有一个人有号时不再拆出个人那几项（选队与选人
 * 结果一样）。只有一个人有号时整个下拉都没意义，返回空。
 *
 * - 管理员：每个代理一组（管理员自己算第一组），组头是「本人及下属」，下面是有号的各人。
 * - 代理：一组，本人与各下属用户（全部就是整队，不必再有队那一项）。
 * - 访客，或成员名单还没拉回来：按号上的号主平铺。
 */
export function ownerFacets(
  pool: readonly Credential[],
  users: readonly ConsoleUser[] | undefined,
  me: Me | undefined,
  t: Translate,
): OwnerFacet[][] {
  const counts = new Map<number, number>()
  for (const c of pool) {
    if (c.owner_id != null) counts.set(c.owner_id, (counts.get(c.owner_id) ?? 0) + 1)
  }
  if (counts.size <= 1) return []
  const person = (id: number, label: string, nested: boolean): OwnerFacet => ({
    value: `user:${id}`,
    label,
    nested,
    owners: new Set([id]),
    count: counts.get(id) ?? 0,
  })
  const byName = (a: ConsoleUser, b: ConsoleUser) => a.username.localeCompare(b.username)

  if (me && users && (me.role === 'admin' || me.role === 'agent')) {
    const leaders = me.role === 'admin'
      ? [{ id: me.id, name: t('管理员', 'Admin') }, ...users.filter((u) => u.role === 'agent').sort(byName).map((u) => ({ id: u.id, name: u.username }))]
      : [{ id: me.id, name: me.username }]
    const groups: OwnerFacet[][] = []
    for (const leader of leaders) {
      const members = users.filter((u) => u.role === 'user' && u.parent_id === leader.id).sort(byName)
      const people = [
        person(leader.id, leader.id === me.id ? t('本人', 'Me') : t(`${leader.name} 本人`, `${leader.name} only`), me.role === 'admin'),
        ...members.map((u) => person(u.id, u.username, me.role === 'admin')),
      ].filter((p) => p.count > 0)
      if (people.length === 0) continue
      if (me.role === 'agent') {
        groups.push(people)
        continue
      }
      const owners = new Set(people.flatMap((p) => [...p.owners]))
      const team: OwnerFacet = {
        value: `team:${leader.id}`,
        label: leader.id === me.id ? t('管理员及直属用户', 'Admin & direct users') : t(`${leader.name} 及下属`, `${leader.name} & users`),
        nested: false,
        owners,
        count: people.reduce((sum, p) => sum + p.count, 0),
      }
      groups.push(people.length > 1 ? [team, ...people] : [team])
    }
    return groups
  }

  const names = new Map<number, string>()
  for (const c of pool) {
    if (c.owner_id != null && !names.has(c.owner_id)) names.set(c.owner_id, c.owner ?? `#${c.owner_id}`)
  }
  return [
    [...names]
      .sort(([, a], [, b]) => a.localeCompare(b))
      .map(([id, name]) => person(id, name === 'admin' ? t('管理员', 'Admin') : name, false)),
  ]
}
