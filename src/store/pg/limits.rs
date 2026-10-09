//! 进程内限流（PG 版），对应 `store::limits`。
//!
//! 滑动窗口计数（[`RateWindow`]）、上游 429 冷却表（[`RateLimitCooldown`]）与各拒绝错误都是
//! 纯内存的，直接用 `store::limits` 里的；这里只有挂在 [`PgStore`] 上的几个入口，全部不碰库、
//! 保持同步。

use std::time::Duration;

use super::super::{DEVICE_RATE_MAX_KEYS, RPM_WINDOW_SECS, SESSION_RATE_MAX_KEYS};
use super::PgStore;

impl PgStore {
    /// 给这台设备记一条转发；名额已满时不记，返回**建议等待的秒数**（窗口里最早那条滚出去
    /// 的时刻）。上限未配置时恒为 `None`（不限，且一条都不记）。
    ///
    /// 与账号 RPM 刻意不同的两点：
    /// - **不参与选号**。账号打满可以换个号发，设备打满换哪个号都是同一台机器在刷，故这道闸
    ///   在代理入口独立判定、直接 429，不进 [`Self::select_for_device`]（那里换号是为了绕开
    ///   一个满了的号，对设备维度没有意义，只会白白改绑设备）。
    /// - **问与记在同一把锁里**（[`RateWindow::try_take`]）。选号那两道窗口靠选号的串行化写
    ///   事务（[`PgStore::begin_write`]）串行，这里没有那把锁，同一台设备的并发请求必须自己
    ///   防住「都看到最后一个名额」。
    ///
    /// 口径与账号 RPM 一致：**含失败的、含 `count_tokens`**，两个数才比得了。代价同样一致——
    /// 记在这里的是「获准转发」的条数，上游若把它拒了也照算。
    ///
    /// [`RateWindow`]: super::super::RateWindow
    /// [`RateWindow::try_take`]: super::super::RateWindow::try_take
    pub fn take_device_rpm_slot(&self, device_id: &str) -> Option<i64> {
        let limit = self.device_rpm_limit();
        if limit <= 0 {
            return None;
        }
        let window = Duration::from_secs(RPM_WINDOW_SECS as u64);
        // device_id 是客户端自报的，乱编 id 的脚本能把 map 撑大——超过阈值就清掉空窗口。
        // 阈值远高于任何真实设备数：清扫要遍历全表，不该在正常规模下发生。
        self.device_rate.sweep_if_crowded(window, DEVICE_RATE_MAX_KEYS);
        if self.device_rate.try_take(device_id.to_string(), limit, window) {
            return None;
        }
        Some(self.device_rate.retry_after_secs(&device_id.to_string(), window))
    }

    /// 给这个会话记一条转发；名额已满时不记，返回建议等待的秒数。口径、锁的粒度、以及
    /// 「不参与选号」这三点都与 [`Self::take_device_rpm_slot`] 完全一致——差别只在分桶的键。
    ///
    /// 键的清扫比设备维度更要紧：会话 id 每 `/clear`、每个新窗口都是一个新值，故阈值单列
    /// （`SESSION_RATE_MAX_KEYS`）而不是复用设备那个。
    pub fn take_session_rpm_slot(&self, session_id: &str) -> Option<i64> {
        let limit = self.session_rpm_limit();
        if limit <= 0 {
            return None;
        }
        let window = Duration::from_secs(RPM_WINDOW_SECS as u64);
        self.session_rate.sweep_if_crowded(window, SESSION_RATE_MAX_KEYS);
        if self.session_rate.try_take(session_id.to_string(), limit, window) {
            return None;
        }
        Some(self.session_rate.retry_after_secs(&session_id.to_string(), window))
    }

    /// 给凭证打上「被上游限流」的冷却，见 `RateLimitCooldown`。时长与作用域都由调用方
    /// 从上游响应头算出（`crate::proxy::rate_limit_scope`）：`model` 为 `None` 即账号级
    /// （额度真耗尽），`Some(m)` 即只冷却该模型（窗口没跑满却被拒，多半是模型容量限制）。
    pub fn mark_rate_limited(&self, cred_id: i64, model: Option<&str>, dur: Duration) {
        self.cooldown.mark(cred_id, model, dur);
    }

    /// 该凭证**账号级**冷却的剩余秒数（未冷却为 0）。注意正常路径上账号级限流走的是落库的
    /// `resume_at`，这一档只反映落库失败的兜底状态。
    pub fn rate_limited_secs(&self, cred_id: i64) -> i64 {
        self.cooldown.remaining_secs(cred_id)
    }

    /// 该凭证**模型级**冷却的明细 `(模型名, 剩余秒数, 是否挡选号)`，未冷却为空。
    ///
    /// 这一档都不影响账号整体调度：其余模型照常可用。第三项（gated）现在总为 `true`——额度池
    /// 满和瞬时限速两档都走门禁，区别在于持续时间。
    pub fn rate_limited_models(&self, cred_id: i64) -> Vec<(String, i64, bool)> {
        self.cooldown.model_remaining(cred_id)
    }

    /// 解除该凭证的限流冷却：`Some(model)` 清账号级 + 该模型格（连通性测试成功照真实判决
    /// 恢复），`None` 清全部格（后台手动解除）。
    pub fn clear_rate_limited(&self, cred_id: i64, model: Option<&str>) {
        self.cooldown.clear(cred_id, model);
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sqlx::PgPool;

    use super::super::super::*;
    use super::super::PgStore;
    use super::super::select::tests::store_with;

    /// 每设备 RPM：各设备各算各的，打满的那台被拒并拿到 retry-after，其余设备不受影响；
    /// 窗口滚过去后名额自己回来。上限未配置时一条都不记（也就永远不拒）。
    #[sqlx::test]
    async fn device_rpm_limit_is_per_device_and_expires(pool: PgPool) {
        let (store, _) = store_with(pool, &["a"]).await;

        // 没配上限 → 恒放行，且窗口表里一条都不该有（记了只会无界增长）。
        for _ in 0..5 {
            assert_eq!(store.take_device_rpm_slot("dev-1"), None, "未配置上限就是不限");
        }
        assert!(store.device_rate.hits.lock().is_empty(), "不限时不该记账");

        store.set_setting(DEVICE_RPM_LIMIT, "2").await.unwrap();
        assert_eq!(store.take_device_rpm_slot("dev-1"), None);
        assert_eq!(store.take_device_rpm_slot("dev-1"), None);
        let retry = store.take_device_rpm_slot("dev-1").expect("第三条该被拒");
        assert!(
            (1..=RPM_WINDOW_SECS).contains(&retry),
            "retry-after 要落在窗口内且不为 0：{retry}"
        );

        // 另一台设备有自己的窗口——一台刷疯了不该连累别人，这正是这道闸的目的。
        assert_eq!(store.take_device_rpm_slot("dev-2"), None, "别的设备照常");

        // 把 dev-1 窗口里的时间戳推到过期，等价于等了一个窗口。
        {
            let mut hits = store.device_rate.hits.lock();
            for t in hits.get_mut("dev-1").expect("dev-1 该有窗口").iter_mut() {
                *t -= Duration::from_secs(RPM_WINDOW_SECS as u64 + 1);
            }
        }
        assert_eq!(store.take_device_rpm_slot("dev-1"), None, "过期后名额应回收");
    }

    /// 每会话 RPM：各会话各算各的，与设备那道闸**互不干扰**（同一台设备上两个会话各有自己的
    /// 窗口，这正是选会话粒度的目的）；窗口滚过去后名额自己回来，未配置上限时一条都不记。
    #[sqlx::test]
    async fn session_rpm_limit_is_per_session_and_independent_of_device(pool: PgPool) {
        let (store, _) = store_with(pool, &["a"]).await;

        for _ in 0..5 {
            assert_eq!(store.take_session_rpm_slot("sess-1"), None, "未配置上限就是不限");
        }
        assert!(store.session_rate.hits.lock().is_empty(), "不限时不该记账");

        store.set_setting(SESSION_RPM_LIMIT, "2").await.unwrap();
        assert_eq!(store.take_session_rpm_slot("sess-1"), None);
        assert_eq!(store.take_session_rpm_slot("sess-1"), None);
        let retry = store.take_session_rpm_slot("sess-1").expect("第三条该被拒");
        assert!(
            (1..=RPM_WINDOW_SECS).contains(&retry),
            "retry-after 要落在窗口内且不为 0：{retry}"
        );

        // 同机的另一个会话有自己的桶——按设备一刀切时它会被上面那个挤没。
        assert_eq!(store.take_session_rpm_slot("sess-2"), None, "别的会话照常");

        // 两个窗口是两份计数：会话打满不该顺带把设备的桶也算上（反之同理）。
        store.set_setting(DEVICE_RPM_LIMIT, "1").await.unwrap();
        assert_eq!(store.take_device_rpm_slot("dev-1"), None, "设备的桶此刻还是空的");

        {
            let mut hits = store.session_rate.hits.lock();
            for t in hits.get_mut("sess-1").expect("sess-1 该有窗口").iter_mut() {
                *t -= Duration::from_secs(RPM_WINDOW_SECS as u64 + 1);
            }
        }
        assert_eq!(store.take_session_rpm_slot("sess-1"), None, "过期后名额应回收");
    }

    /// 模型级冷却必须能被后台读到。
    ///
    /// 这是一处真实的观测盲区：模型级 429（fable 撞超额池就是这一档）只写进
    /// `(cred_id, 模型)` 那些格子，而控制台读的是账号级那一格，于是选号侧明明已经跳过
    /// 这个模型、界面上却一片正常，「冷却中」那套筛选与徽章形同虚设。
    #[sqlx::test]
    async fn model_level_cooldown_is_visible_to_the_console(pool: PgPool) {
        let store = PgStore::for_test(pool).await;
        let a = store.insert("a", None, "ta", "ra", 0, None, None, 1).await.unwrap().id;

        store.mark_rate_limited(a, Some("claude-fable-5"), Duration::from_secs(300));
        store.mark_rate_limited(a, Some("claude-opus-5"), Duration::from_secs(30));

        // 账号级那一格没被写过，account 档仍应是 0——模型级不等于账号被限流。
        assert_eq!(store.rate_limited_secs(a), 0, "模型级不该冒充账号级");

        let models = store.rate_limited_models(a);
        assert_eq!(models.len(), 2);
        // 剩得最久的排前面，展示顺序必须稳定（HashMap 迭代序是随机的）。
        assert_eq!(models[0].0, "claude-fable-5");
        assert!(models[0].1 > 290 && models[0].1 <= 300, "{models:?}");
        assert_eq!(models[1].0, "claude-opus-5");

        // 解除后即消失；账号级那档也照常工作（落库失败的兜底路径走它）。
        store.clear_rate_limited(a, None);
        assert!(store.rate_limited_models(a).is_empty());
        store.mark_rate_limited(a, None, Duration::from_secs(120));
        assert!(store.rate_limited_secs(a) > 110);
        assert!(store.rate_limited_models(a).is_empty(), "账号级不该混进模型级明细");
    }
}
