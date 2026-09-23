import { api } from './client'

export interface Metrics {
  /** 全局 RPM：最近 window_secs 秒转发的请求总数，恒等于各账号 RPM 之和。 */
  rpm: number
  /** 在途请求数：已进入转发入口、响应尚未走完的那些（流式回复整段传输期间都算）。 */
  in_flight: number
  /** RPM 的统计窗口（秒），当前固定 60。 */
  window_secs: number
}

/**
 * 读取实时指标。
 *
 * 与账号列表分开的一个便宜接口：这两个数几秒就变一次，值得单独高频轮询，而账号列表那个
 * 响应要跑十几条聚合查询，按同样频率拉只是白烧数据库。
 */
export async function getMetrics(): Promise<Metrics> {
  const { data } = await api.get<Metrics>('/metrics')
  return data
}

// ---------- 趋势接口共用 ----------

/** 本地时区相对 UTC 的偏移（秒）：按天分桶时后端要按本地零点切桶边界。 */
export function localTzOffsetSecs(): number {
  return -new Date().getTimezoneOffset() * 60
}

/** 一次趋势查询：回看多少小时、一格多少秒（逐小时 3600 / 逐天 86400）。 */
export interface SeriesParams {
  hours: number
  bucketSecs: number
}

// ---------- 缓存命中率趋势 ----------

export interface CacheSeriesPoint {
  ts: number
  /** 全部输入 token（含命中与写入）。 */
  input_tokens: number
  /** 其中命中缓存的。 */
  cached_tokens: number
  /** 其中写进缓存的。命中率一个数分不出「没命中」和「没写入」，拆开才知道该查什么。 */
  written_tokens: number
}

export interface CacheSeries {
  since: number
  bucket_secs: number
  points: CacheSeriesPoint[]
  /** 整个窗口的合计。 */
  summary: CacheSeriesPoint
  /** 近 60 分钟的合计。 */
  recent: CacheSeriesPoint
}

export async function getCacheSeries({ hours, bucketSecs }: SeriesParams): Promise<CacheSeries> {
  const { data } = await api.get('/metrics/cache-series', {
    params: { hours, bucket_secs: bucketSecs, tz_offset_secs: localTzOffsetSecs() },
  })
  return data
}

// ---------- TTFT 趋势 ----------

export interface TtftSeriesPoint {
  ts: number
  /** 算术平均，留作对照；看 p50 / p95。 */
  avg_ms: number
  p50_ms: number
  p95_ms: number
  /** 参与统计的成功请求数。 */
  count: number
  /** 输出吞吐（token / 秒），没有可算的请求时为 null。 */
  tokens_per_sec: number | null
}

export interface TtftSeries {
  since: number
  bucket_secs: number
  points: TtftSeriesPoint[]
  /** 整个窗口的分位与吞吐（对整窗口的原始值算，不是各桶的平均）。 */
  summary: TtftSeriesPoint
  /** 近 60 分钟。 */
  recent: TtftSeriesPoint
}

export async function getTtftSeries({ hours, bucketSecs }: SeriesParams): Promise<TtftSeries> {
  const { data } = await api.get('/metrics/ttft-series', {
    params: { hours, bucket_secs: bucketSecs, tz_offset_secs: localTzOffsetSecs() },
  })
  return data
}

// ---------- 按模型 / 按账号拆分 ----------

export type BreakdownBy = 'model' | 'account'

export interface BreakdownRow {
  /** 模型名，或凭证 id 的十进制串。 */
  key: string
  /** 模型名，或凭证 label（已删的号是 `#<id>`）。 */
  label: string
  /** 按账号拆时该号的套餐；按模型拆或号已删为 null。 */
  tier: string | null
  /** 这段时间里的全部请求数（含失败的）。 */
  requests: number
  /** 缓存给这一组省下的钱（USD）：命中省的减去写入多付的，可能为负。 */
  cache_saved_usd: number
  /** 延迟（只算成功且记了 TTFT 的请求）。 */
  latency: TtftSeriesPoint
  /** 缓存三段 token（所有请求）。 */
  cache: CacheSeriesPoint
}

export interface Breakdown {
  since: number
  by: BreakdownBy
  rows: BreakdownRow[]
  /** 全部分组（不止前 12 行）缓存省下的钱合计（USD）。 */
  cache_saved_usd_total: number
}

// ---------- 本地拒绝 ----------

export interface RejectionKind {
  /** device-limit / session-limit / account-rpm / device-rpm / session-rpm / session-concurrency / bare-rate-limit / all-cooling-down / no-device-id / model-min-version / model-unsupported / unavailable / other */
  kind: string
  count: number
}

export interface Rejections {
  since: number
  total: number
  rows: RejectionKind[]
}

/** 近几小时 luban 自己拒掉（没转发）的请求数，按原因分类，按条数降序。 */
export async function getRejections(hours: number): Promise<Rejections> {
  const { data } = await api.get<Rejections>('/metrics/rejections', { params: { hours } })
  return data
}

/** 这段时间按模型或按账号拆开的延迟与缓存，按请求数降序、最多 12 行。 */
export async function getUsageBreakdown(hours: number, by: BreakdownBy): Promise<Breakdown> {
  const { data } = await api.get('/metrics/breakdown', { params: { hours, by } })
  return data
}
