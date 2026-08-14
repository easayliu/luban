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
