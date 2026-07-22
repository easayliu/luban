import { useQuery } from '@tanstack/react-query'
import { Activity } from 'lucide-react'
import { listUsage, type UsageLog } from '@/api/usage'
import { cn, formatUsd, relativeTime } from '@/lib/utils'
import { Card, CardContent, CardDescription, CardHeader, CardTitle } from '@/components/ui/card'

/** 用量面板：最近请求的 token 用量明细（额度已在凭证卡片展示）。 */
export function UsagePanel() {
  const { data: logs, isLoading } = useQuery({
    queryKey: ['usage'],
    queryFn: () => listUsage(200),
    refetchInterval: 15_000,
  })

  return (
    <Card>
      <CardHeader>
        <CardTitle className="flex items-center gap-2">
          <Activity className="size-4" />
          最近请求
        </CardTitle>
        <CardDescription>各次转发的 token 用量与耗时（额度见各账号卡片）。</CardDescription>
      </CardHeader>
      <CardContent>
        <div>
          {isLoading ? (
            <div className="py-8 text-center text-sm text-muted-foreground">加载中…</div>
          ) : (logs?.length ?? 0) === 0 ? (
            <div className="py-8 text-center text-sm text-muted-foreground">
              还没有请求记录，接入 Claude Code 发起一次请求后这里会显示。
            </div>
          ) : (
            <div className="overflow-x-auto">
              <table className="w-full text-xs">
                <thead className="text-muted-foreground">
                  <tr className="border-b border-border text-left">
                    <th className="py-1.5 pr-3 font-medium">时间</th>
                    <th className="py-1.5 pr-3 font-medium">账号</th>
                    <th className="py-1.5 pr-3 font-medium">模型</th>
                    <th className="py-1.5 pr-3 font-medium text-right">输入</th>
                    <th className="py-1.5 pr-3 font-medium text-right">输出</th>
                    <th className="py-1.5 pr-3 font-medium text-right">缓存</th>
                    <th className="py-1.5 pr-3 font-medium text-right">费用</th>
                    <th className="py-1.5 pr-3 font-medium text-right">状态</th>
                    <th className="py-1.5 font-medium text-right">耗时</th>
                  </tr>
                </thead>
                <tbody>
                  {logs!.map((l) => (
                    <tr key={l.id} className="border-b border-border/50 last:border-0">
                      <td className="py-1.5 pr-3 text-muted-foreground whitespace-nowrap">
                        {relativeTime(l.ts)}
                      </td>
                      <td className="py-1.5 pr-3 max-w-[10rem] truncate" title={l.cred_label}>
                        {l.cred_label}
                      </td>
                      <td className="py-1.5 pr-3 text-muted-foreground whitespace-nowrap">
                        {shortModel(l.model)}
                      </td>
                      <td className="py-1.5 pr-3 text-right tabular-nums">{fmt(l.input_tokens)}</td>
                      <td className="py-1.5 pr-3 text-right tabular-nums">{fmt(l.output_tokens)}</td>
                      <td className="py-1.5 pr-3 text-right tabular-nums text-muted-foreground">
                        {fmt(cacheTotal(l))}
                      </td>
                      <td className="py-1.5 pr-3 text-right tabular-nums">
                        {l.cost_usd != null ? formatUsd(l.cost_usd) : '-'}
                      </td>
                      <td className="py-1.5 pr-3 text-right">
                        <span className={cn(l.status >= 400 ? 'text-bad' : 'text-muted-foreground')}>
                          {l.status}
                        </span>
                      </td>
                      <td className="py-1.5 text-right tabular-nums text-muted-foreground whitespace-nowrap">
                        {l.total_ms != null ? `${(l.total_ms / 1000).toFixed(1)}s` : '-'}
                      </td>
                    </tr>
                  ))}
                </tbody>
                <tfoot>
                  <tr className="border-t border-border font-medium">
                    <td className="py-1.5 pr-3 text-muted-foreground" colSpan={3}>
                      共 {logs!.length} 条
                    </td>
                    <td className="py-1.5 pr-3 text-right tabular-nums">{fmt(sum(logs!, (l) => l.input_tokens))}</td>
                    <td className="py-1.5 pr-3 text-right tabular-nums">{fmt(sum(logs!, (l) => l.output_tokens))}</td>
                    <td className="py-1.5 pr-3 text-right tabular-nums text-muted-foreground">{fmt(sum(logs!, cacheTotal))}</td>
                    <td className="py-1.5 pr-3 text-right tabular-nums">{formatUsd(sum(logs!, (l) => l.cost_usd))}</td>
                    <td className="py-1.5" colSpan={2} />
                  </tr>
                </tfoot>
              </table>
            </div>
          )}
        </div>
      </CardContent>
    </Card>
  )
}

/** 对日志列表按选择器求和（null 视为 0）。 */
function sum(logs: UsageLog[], pick: (l: UsageLog) => number | null): number {
  return logs.reduce((acc, l) => acc + (pick(l) ?? 0), 0)
}

function cacheTotal(l: UsageLog): number | null {
  const c = l.cache_creation_tokens
  const r = l.cache_read_tokens
  if (c == null && r == null) return null
  return (c ?? 0) + (r ?? 0)
}

function fmt(n: number | null): string {
  if (n == null) return '-'
  return n.toLocaleString('en-US')
}

/** 去掉常见前缀，缩短模型名显示。 */
function shortModel(model: string | null): string {
  if (!model) return '-'
  return model.replace(/^claude-/, '')
}
