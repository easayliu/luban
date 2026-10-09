//! `settings` 表的 PG 版，对应 `store::settings`：各项设置的取值口径与读写。
//!
//! 键名常量、[`ForwardFlags`]、三态换算等纯函数直接用 `store::settings` 的。读一律走
//! [`PgStore`] 的内存缓存（零查询），写只有 [`PgStore::set_setting`] /
//! [`PgStore::delete_setting`] 两处，都是先落库再更新缓存。整表装进缓存的
//! `load_settings` 在 `pg/mod.rs`。
//!
//! **多进程共享同一个库时缓存会读到陈旧值**：别的进程改了设置，本进程要重启才看得到。

use anyhow::Result;

use super::super::settings::setting_is_on;
use super::super::*;
use super::PgStore;

impl PgStore {
    /// 全局默认设备数上限：`<= 0` 表示显式不限。未设置或解析失败时使用
    /// [`DEFAULT_DEVICE_LIMIT_VALUE`]。
    pub fn default_device_limit(&self) -> i64 {
        self.get_setting(DEFAULT_DEVICE_LIMIT)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(DEFAULT_DEVICE_LIMIT_VALUE)
            .max(0)
    }

    /// 全局默认模拟会话数上限；未设置或解析失败时用 [`DEFAULT_SESSION_LIMIT_VALUE`]。
    pub fn default_session_limit(&self) -> i64 {
        self.get_setting(DEFAULT_SESSION_LIMIT)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(DEFAULT_SESSION_LIMIT_VALUE)
            .max(0)
    }

    /// 全局默认账号 RPM 上限：`<= 0` 表示默认不限（默认即不限，与加入本机制前一致）。
    pub fn default_rpm_limit(&self) -> i64 {
        self.get_setting(DEFAULT_RPM_LIMIT)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0)
            .max(0)
    }

    /// 每设备 RPM 上限：单台设备在最近 [`RPM_WINDOW_SECS`] 秒内最多转发多少条；
    /// `<= 0`（含未设置）表示不限，即加入本机制前的行为。
    pub fn device_rpm_limit(&self) -> i64 {
        self.get_setting(DEVICE_RPM_LIMIT)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0)
            .max(0)
    }

    /// 每会话 RPM 上限：单个会话在最近 [`RPM_WINDOW_SECS`] 秒内最多转发多少条；
    /// `<= 0`（含未设置）表示不限。语义与配套的设备闸见 [`SESSION_RPM_LIMIT`]。
    pub fn session_rpm_limit(&self) -> i64 {
        self.get_setting(SESSION_RPM_LIMIT)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0)
            .max(0)
    }

    /// 每会话并发在途上限：单个会话最多同时有多少条请求在飞；`<= 0` 表示不限。
    /// 未设置时默认 [`DEFAULT_SESSION_CONCURRENCY_LIMIT`]。
    pub fn session_concurrency_limit(&self) -> i64 {
        self.get_setting(SESSION_CONCURRENCY_LIMIT)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(DEFAULT_SESSION_CONCURRENCY_LIMIT)
            .max(0)
    }

    /// 上游 429 时最多换几个号重试；`0` 表示不重试（原样透传 429）。
    /// 未设置时默认 [`DEFAULT_RATE_LIMIT_RETRY_MAX`]，上限 10——再多也只是把一次失败的
    /// 请求拖成十几秒，不如早点把 429 交回给客户端。
    pub fn rate_limit_retry_max(&self) -> usize {
        self.get_setting(RATE_LIMIT_RETRY_MAX)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(DEFAULT_RATE_LIMIT_RETRY_MAX)
            .clamp(0, 10) as usize
    }

    /// **5h 窗口**的使用率到多少百分比就提前把这个号挪出调度池（`0` 表示关闭，只在真收到
    /// 429 时才停）。
    ///
    /// 判定与停用都在 `crate::proxy::park_if_quota_nearly_exhausted`：上游**每一条**响应都
    /// 报基础额度窗口的使用率，越过这个数就当额度已耗尽，不必等下一发请求去撞 429。
    /// 未设置时用 [`DEFAULT_QUOTA_PAUSE_PCT`]（90），取值夹在 `0..=100`（100 即「满了才停」，
    /// 与不开本机制的差别只剩「不用等 429」）。
    ///
    /// **只管小时级窗口**：7d 那种天级窗口另配一档 [`Self::quota_pause_pct_7d`]，理由见
    /// [`QUOTA_PAUSE_PCT_7D`]。
    pub fn quota_pause_pct(&self) -> i64 {
        self.get_setting(QUOTA_PAUSE_PCT)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(DEFAULT_QUOTA_PAUSE_PCT)
            .clamp(0, 100)
    }

    /// **7d（天级）窗口**的提前停调度阈值；`0`（含未设置，即默认）= 不按这个窗口停号。
    ///
    /// 与 [`Self::quota_pause_pct`] 是两档、各算各的，别指望一个数字管两边——同一个 90%
    /// 在 5h 上是「歇几小时」，在 7d 上是「歇到几天后」。默认关，见 [`QUOTA_PAUSE_PCT_7D`]。
    pub fn quota_pause_pct_7d(&self) -> i64 {
        self.get_setting(QUOTA_PAUSE_PCT_7D)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(DEFAULT_QUOTA_PAUSE_PCT_7D)
            .clamp(0, 100)
    }

    /// 裸请求速率上限：单个凭证在 [`Self::bare_rate_window_secs`] 的窗口内最多接多少条
    /// **无设备身份**的请求。`<= 0`（含未设置）表示不限——默认即不限，与加入本机制前一致。
    ///
    /// 只卡裸请求：带 `metadata.user_id` 的那些由设备绑定 + `device_limit` 管着，而裸请求
    /// 不写绑定、不占名额，`device_limit` 对它们不生效。注意客户端只要自己编一个
    /// `metadata.user_id` 就能从这条限制里出去（那时它转而受设备上限约束），这不是漏洞而是
    /// 分工——本项限的是「没有任何身份可依据」的那部分流量。
    pub fn bare_rate_limit(&self) -> i64 {
        self.get_setting(BARE_RATE_LIMIT)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0)
            .max(0)
    }

    /// 裸请求速率窗口（秒），默认 60。取值 `<= 0` 时退回默认，避免除零/永久封锁那类配置。
    pub fn bare_rate_window_secs(&self) -> i64 {
        self.get_setting(BARE_RATE_WINDOW_SECS)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .filter(|v| *v > 0)
            .unwrap_or(DEFAULT_BARE_RATE_WINDOW_SECS)
    }

    /// 读取设置项；不存在返回 None。**走内存缓存，不查库**（见 `settings` 字段）。
    ///
    /// 返回值仍是 `Result` 是为了不动调用方：这条路径现在不会失败，但签名一改就要改十几处。
    pub fn get_setting(&self, key: &str) -> Result<Option<String>> {
        Ok(self.settings.read().get(key).cloned())
    }

    /// 写入设置项（upsert）：先落库，成功后再更新缓存——反过来的话写库失败就会留下一份
    /// 库里没有、内存里却生效的设置，重启即凭空回滚。
    pub async fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO settings (key, value) VALUES ($1, $2) \
             ON CONFLICT (key) DO UPDATE SET value = EXCLUDED.value",
        )
        .bind(key)
        .bind(value)
        .execute(&self.pool)
        .await?;
        self.settings.write().insert(key.to_string(), value.to_string());
        Ok(())
    }

    /// 设备绑定有效期（秒）；未设置或解析失败时用默认值。`<= 0` 表示永不过期。
    pub fn device_binding_ttl(&self) -> i64 {
        self.get_setting(DEVICE_BINDING_TTL)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(DEFAULT_DEVICE_BINDING_TTL_SECS)
    }

    /// 软绑定保留期（秒）；未设置或解析失败时用默认值。`<= 0` 表示永久保留。
    ///
    /// 与 [`Self::device_binding_ttl`] 的分工：TTL 管「还占不占名额」，这个管「还记不记得
    /// 这台设备上次用的哪个号」。见 [`effective_retention`]。
    pub fn device_binding_retention(&self) -> i64 {
        self.get_setting(DEVICE_BINDING_RETENTION)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(DEFAULT_DEVICE_BINDING_RETENTION_SECS)
    }

    /// 模拟会话绑定有效期（秒）；未设置或解析失败时用默认值。`<= 0` 表示永不过期。
    pub fn session_binding_ttl(&self) -> i64 {
        self.get_setting(SESSION_BINDING_TTL)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(DEFAULT_SESSION_BINDING_TTL_SECS)
    }

    /// 模拟会话软绑定保留期（秒）；未设置或解析失败时用默认值。`<= 0` 表示永久保留。
    pub fn session_binding_retention(&self) -> i64 {
        self.get_setting(SESSION_BINDING_RETENTION)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(DEFAULT_SESSION_BINDING_RETENTION_SECS)
    }

    /// 一次读齐全部转发形态开关（[`ForwardFlags`]）。
    ///
    /// 走内存缓存（见 `settings` 字段），零查询。任何读不出来的键都退回默认值（= 开启），
    /// 故设置表是空的时候也不会挡住转发。
    ///
    /// [`SYSTEM_SHAPE`] 缺省时沿用旧键 [`CACHE_SCOPE_GLOBAL`]（新键存在则以新键为准）。
    pub fn forward_flags(&self) -> ForwardFlags {
        let mut flags = ForwardFlags::default();
        let settings = self.settings.read();
        let on = |key: &str| settings.get(key).map(|v| setting_is_on(v));
        if let Some(v) = on(SPOOF_IDENTITY_ENABLED) {
            flags.spoof_identity = v;
        }
        if let Some(v) = on(SPOOF_DEVICE_ID) {
            flags.spoof_device_id = v;
        }
        if let Some(v) = on(NORMALIZE_DEVICE_FP) {
            flags.normalize_device_fp = v;
        }
        if let Some(v) = on(SPOOF_BILLING_CCH) {
            flags.billing_cch = v;
        }
        if let Some(v) = on(CCH_REAL_RECOMPUTE) {
            flags.cch_real_recompute = v;
        }
        if let Some(v) = on(CCH_SIM_COMPUTE) {
            flags.cch_sim_compute = v;
        }
        if let Some(v) = on(FILL_CLIENT_HEADERS) {
            flags.fill_client_headers = v;
        }
        if let Some(v) = on(MERGE_BETA) {
            flags.merge_beta = v;
        }
        if let Some(v) = on(ORIG_HEADER_CASE) {
            flags.orig_header_case = v;
        }
        if let Some(v) = on(THINKING_SIGNATURE_RETRY) {
            flags.thinking_signature_retry = v;
        }
        if let Some(v) = on(REDACTED_THINKING_RETRY) {
            flags.redacted_thinking_retry = v;
        }
        if let Some(v) = on(SIMULATE_CC) {
            flags.simulate_cc = v;
        }
        if let Some(v) = on(SIMULATE_FULL_SYSTEM) {
            flags.simulate_full_system = v;
        }
        if let Some(v) = on(FILL_ABSENT_TOOLS) {
            flags.fill_absent_tools = v;
        }
        if let Some(v) = on(SIM_TRIM_TOOLS) {
            flags.sim_trim_tools = v;
        }
        if let Some(v) = on(SIM_BILLING_ONLY) {
            flags.sim_billing_only = v;
        }
        if let Some(v) = on(SIM_BILLING_KEEP_USER_ID) {
            flags.sim_billing_keep_user_id = v;
        }
        if let Some(v) = on(REAL_BILLING_KEEP_USER_ID) {
            flags.real_billing_keep_user_id = v;
        }
        if let Some(v) = on(SIM_MESSAGE_THREADS) {
            flags.sim_message_threads = v;
        }
        if let Some(v) = on(FILL_METADATA) {
            flags.fill_metadata = v;
        }
        if let Some(v) = on(RATE_LIMIT_RETRY) {
            flags.rate_limit_retry = v;
        }
        if let Some(v) = on(SYSTEM_CACHE_SCOPE) {
            flags.cache_scope_global = v;
        }
        if let Some(v) = on(SYSTEM_CACHE_TTL) {
            flags.cache_ttl_1h = v;
        }
        if let Some(v) = on(NONSTREAM_AS_SSE) {
            flags.nonstream_as_sse = v;
        }
        if let Some(v) = on(EAGER_TOOL_STREAMING) {
            flags.eager_tool_streaming = v;
        }
        if let Some(v) = on(STRIP_EXTRA_FIELDS) {
            flags.strip_extra_fields = v;
        }
        if let Some(v) = on(TOOL_NAME_MIMIC) {
            flags.tool_name_mimic = v;
        }
        if let Some(v) = on(INJECT_THINKING) {
            flags.inject_thinking = v;
        }
        if let Some(v) = on(REJECT_OPENAI_SHAPE) {
            flags.reject_openai_shape = v;
        }
        if let Some(v) = on(REJECT_SESSION_CONFLICT) {
            flags.reject_session_conflict = v;
        }
        if let Some(v) = on(REJECT_PROBES) {
            flags.reject_probes = v;
        }
        if let Some(v) = on(REJECT_PROBES_STRICT) {
            flags.reject_probes_strict = v;
        }
        // 拆分前三件事共用 `reject_probes`：旧库只写过它的，两条新键沿用它的取值（关过 =
        // 用户当时把学到的规则也一起关了，升级不能悄悄开回来）；新键一旦写了就以新键为准。
        if let Some(v) = on(REJECT_REFUSALS).or_else(|| on(REJECT_PROBES)) {
            flags.reject_refusals = v;
        }
        if let Some(v) = on(REJECT_EMPTY_REPLIES).or_else(|| on(REJECT_PROBES)) {
            flags.reject_empty_replies = v;
        }
        if let Some(v) = on(REJECT_LEARNED_SHAPES) {
            flags.reject_learned_shapes = v;
        }
        if let Some(v) = on(API_TELEMETRY) {
            flags.api_telemetry = v;
        }
        if let Some(v) = on(KEEPALIVE_TELEMETRY) {
            flags.keepalive_telemetry = v;
        }
        // fable 那档沿用 v0.3.91 的单一旧键；opus 那档默认关，旧键不算数。
        if let Some(v) = on(FABLE_REFUSAL_FALLBACK).or_else(|| on(REFUSAL_FALLBACK_LEGACY)) {
            flags.fable_refusal_fallback = v;
        }
        if let Some(v) = on(OPUS_REFUSAL_FALLBACK) {
            flags.opus_refusal_fallback = v;
        }
        // 新键存在就以它为准，否则沿用旧键——旧库里若把旧键关过，语义就是「别动 system」。
        if let Some(v) = on(SYSTEM_SHAPE).or_else(|| on(CACHE_SCOPE_GLOBAL)) {
            flags.system_shape = v;
        }
        flags
    }

    /// 是否要求请求携带有效设备身份（`metadata.user_id`）；未设置时默认要求（保持严格）。
    /// 仅 `"0"`/`"false"`（忽略大小写与首尾空白）视为关闭。
    pub fn require_device_id(&self) -> bool {
        match self.get_setting(REQUIRE_DEVICE_ID).ok().flatten() {
            Some(v) => setting_is_on(&v),
            None => true,
        }
    }

    /// 允许接入的最低 Claude Code 客户端版本（形如 `2.1.220`）；未设置或空串表示不限。
    ///
    /// 只影响 `User-Agent` 里自报了 `claude-cli/<版本>` 的请求，别的客户端一律放行——见
    /// [`crate::proxy::below_min_client_version`]。
    pub fn min_client_version(&self) -> Option<String> {
        self.get_setting(MIN_CLIENT_VERSION)
            .ok()
            .flatten()
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }

    /// 登录时实际申请的 OAuth scope（单空格分隔）；未配置或配了个空串就是官方那一整套
    /// [`crate::config::SCOPES`]。
    ///
    /// 读出来再规整一遍而不是信库里的原样：这一项可能是从别的机器 import 进来的，
    /// 那边的写入校验未必和这边同一个版本。
    pub fn oauth_scopes(&self) -> String {
        self.get_setting(OAUTH_SCOPES)
            .ok()
            .flatten()
            .map(|v| crate::config::normalize_scopes(&v))
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| crate::config::SCOPES.to_string())
    }

    /// 删除设置项（顺序同 [`Self::set_setting`]：先落库再更新缓存）。
    pub async fn delete_setting(&self, key: &str) -> Result<()> {
        sqlx::query("DELETE FROM settings WHERE key = $1").bind(key).execute(&self.pool).await?;
        self.settings.write().remove(key);
        Ok(())
    }
}

/// 测试用：开一个存储并按 `labels` 依次上号（号主 admin），回存储与各号 id。对应旧测试里的
/// `store_with`；A 的几个模块的测试共用。
#[cfg(test)]
pub(super) async fn store_with_local(pool: sqlx::PgPool, labels: &[&str]) -> (PgStore, Vec<i64>) {
    let store = PgStore::for_test(pool).await;
    let mut ids = Vec::with_capacity(labels.len());
    for l in labels {
        // refresh_token 有唯一约束，按 label 取值保证互不相同。
        let c = store
            .insert(l, None, &format!("tok-{l}"), &format!("refresh-{l}"), 0, None, None, 1)
            .await
            .unwrap();
        ids.push(c.id);
    }
    (store, ids)
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sqlx::PgPool;

    use super::*;

    /// 裸请求速率上限：单号发满后自动分流到下一个号，全部发满才 429（[`BareRateLimited`]）。
    /// 带 device_id 的请求不受此限——那条路由设备绑定 + `device_limit` 管着。
    #[sqlx::test]
    async fn bare_rate_limit_spills_to_next_credential_then_rejects(pool: PgPool) {
        let (store, ids) = store_with_local(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        store.set_setting(BARE_RATE_LIMIT, "2").await.unwrap();
        let bare =
            || Select { ttl_secs: 0, rate_limited: true, exclude: &[], ..Default::default() };

        // 前两条落在 a（同优先级、设备数都是 0 时 id 小者先中），第 3、4 条 a 已满 → 溢到 b。
        let mut picked = Vec::new();
        for _ in 0..4 {
            picked.push(store.select_for_device(bare()).await.unwrap().id);
        }
        assert_eq!(picked, vec![a, a, b, b], "满了应换号而不是直接拒");

        // 两个号都满 → 拒绝，且带得出重试间隔（默认窗口 60s）。
        let err = store.select_for_device(bare()).await.unwrap_err();
        let rl = err.downcast_ref::<BareRateLimited>().expect("应是裸请求限流错误");
        assert_eq!(rl.retry_after_secs, DEFAULT_BARE_RATE_WINDOW_SECS);

        // 带设备身份的请求照常放行：它受的是设备上限，不是这条。
        assert!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .is_ok()
        );

        // 上限设回 0（不限）即刻恢复，计数不再拦。
        store.set_setting(BARE_RATE_LIMIT, "0").await.unwrap();
        assert!(store.select_for_device(bare()).await.is_ok());
    }

    /// 导出的设置快照与导入都**绕开管理密码**：那是目标机器自己的门锁，不该被一次导入换掉。
    #[sqlx::test]
    async fn admin_password_never_travels_with_settings(pool: PgPool) {
        // 源与目标是两套存储：`#[sqlx::test]` 只给一个库，目标另开一个临时库太重，这里改为
        // 先在同一个库上做源、导出快照后清掉设置再当目标——导入只看快照，不看库里原有什么。
        let (store, _) = store_with_local(pool.clone(), &["a"]).await;
        store.set_setting(ADMIN_PASSWORD, "hash-of-source-box").await.unwrap();
        store.set_setting(CLIENT_API_KEY, "key-from-source").await.unwrap();

        let snapshot = store.settings_snapshot();
        assert!(!snapshot.contains_key(ADMIN_PASSWORD), "导出不带管理密码");
        assert_eq!(
            snapshot.get(CLIENT_API_KEY).map(String::as_str),
            Some("key-from-source"),
            "接入 key 要带上：不跟着走的话所有客户端都得重配"
        );

        // 手工把管理密码塞回文件里也不认。
        store.delete_setting(CLIENT_API_KEY).await.unwrap();
        let target = PgStore::for_test(pool).await;
        target.set_setting(ADMIN_PASSWORD, "hash-of-target-box").await.unwrap();
        let mut incoming = snapshot.clone();
        incoming.insert(ADMIN_PASSWORD.into(), "hash-of-source-box".into());
        target.import_settings(&incoming).await.unwrap();
        assert_eq!(
            target.get_setting(ADMIN_PASSWORD).unwrap().as_deref(),
            Some("hash-of-target-box"),
            "目标机器的管理密码不该被导入改掉"
        );
        // 旧版文件里的全局接入 Key 导进来变成一把不绑定分组的接入 Key，设置项本身不再落库。
        assert_eq!(target.get_setting(CLIENT_API_KEY).unwrap(), None);
        let access =
            target.api_key_access("key-from-source").await.unwrap().expect("转成了接入 Key");
        assert!(access.groups.is_none(), "不绑定分组，用全部号");
    }

    /// 设备上限三态：账号独立值覆盖全局，0 跟随全局，负值明确不限。
    #[test]
    fn effective_device_limit_tri_state() {
        assert_eq!(effective_device_limit(3, 5), 3, "账号独立上限覆盖全局");
        assert_eq!(effective_device_limit(0, 5), 5, "未配置则跟随全局默认");
        assert_eq!(effective_device_limit(0, 0), 0, "全局也不限时不限");
        assert_eq!(effective_device_limit(-1, 5), 0, "账号明确不限，忽略全局默认");
    }

    #[test]
    fn effective_retention_tri_state() {
        assert_eq!(effective_retention(60, 3600), Some(3600), "正常配置按保留期删");
        assert_eq!(effective_retention(60, 30), Some(60), "保留期短于 TTL 时按 TTL 兜底");
        assert_eq!(effective_retention(60, 0), None, "保留期为 0 = 永久保留");
        assert_eq!(effective_retention(0, 3600), None, "绑定永不过期时不删任何行");
    }

    /// 生效的 RPM 上限与设备上限共用一套三态语义，改了一处另一处不能悄悄漂开。
    #[test]
    fn effective_rpm_limit_matches_the_device_limit_tri_state() {
        assert_eq!(effective_rpm_limit(30, 60), 30, "账号独立上限覆盖全局");
        assert_eq!(effective_rpm_limit(0, 60), 60, "未配置则跟随全局默认");
        assert_eq!(effective_rpm_limit(0, 0), 0, "全局也不限时不限");
        assert_eq!(effective_rpm_limit(-1, 60), 0, "账号明确不限，忽略全局默认");
    }

    /// 最低客户端版本：未设置、空串、纯空白都等于「不限」（`None`），其余去掉首尾空白后原样返回。
    /// 空白不归一成 `None` 的话，代理侧会拿一个空串去 `parse_version`，虽然也放行，但网页上
    /// 会显示成「已配置」——两边说法不一致比闸本身更难查。
    #[sqlx::test]
    async fn blank_min_client_version_means_no_limit(pool: PgPool) {
        let store = PgStore::for_test(pool).await;

        assert_eq!(store.min_client_version(), None, "没配就是不限");
        store.set_setting(MIN_CLIENT_VERSION, "2.1.220").await.unwrap();
        assert_eq!(store.min_client_version().as_deref(), Some("2.1.220"));
        store.set_setting(MIN_CLIENT_VERSION, "  2.1  ").await.unwrap();
        assert_eq!(store.min_client_version().as_deref(), Some("2.1"), "首尾空白不带进判定");
        store.set_setting(MIN_CLIENT_VERSION, "   ").await.unwrap();
        assert_eq!(store.min_client_version(), None, "只剩空白等于没配");
        store.delete_setting(MIN_CLIENT_VERSION).await.unwrap();
        assert_eq!(store.min_client_version(), None);
    }

    /// 登录 scope：没配 / 配了空白都退回官方默认那一串；配了就按规整后的形态原样发出去。
    /// 库里的值可能来自另一台机器的 import，故读出来还要再规整一遍（顺序不动、只去重与压空白）。
    #[sqlx::test]
    async fn oauth_scopes_fall_back_to_the_official_set(pool: PgPool) {
        let store = PgStore::for_test(pool).await;

        assert_eq!(store.oauth_scopes(), crate::config::SCOPES, "没配就是官方那一整套");
        store.set_setting(OAUTH_SCOPES, crate::config::SCOPES_MINIMAL).await.unwrap();
        assert_eq!(store.oauth_scopes(), crate::config::SCOPES_MINIMAL);
        store
            .set_setting(OAUTH_SCOPES, "  user:inference   user:profile  user:inference ")
            .await
            .unwrap();
        assert_eq!(
            store.oauth_scopes(),
            "user:inference user:profile",
            "压成单空格、按输入顺序去重"
        );
        store.set_setting(OAUTH_SCOPES, "   ").await.unwrap();
        assert_eq!(store.oauth_scopes(), crate::config::SCOPES, "只剩空白等于没配");
        store.delete_setting(OAUTH_SCOPES).await.unwrap();
        assert_eq!(store.oauth_scopes(), crate::config::SCOPES);
    }

    /// 设置项走内存缓存后，读写口径必须与直接查库一致（含删除与重开库）。
    #[sqlx::test]
    async fn settings_cache_matches_the_database(pool: PgPool) {
        let store = PgStore::for_test(pool).await;

        assert_eq!(store.get_setting(REQUIRE_DEVICE_ID).unwrap(), None);
        store.set_setting(REQUIRE_DEVICE_ID, "false").await.unwrap();
        assert_eq!(store.get_setting(REQUIRE_DEVICE_ID).unwrap().as_deref(), Some("false"));
        assert!(!store.require_device_id(), "缓存值要真的参与判定");

        // 缓存和库不能漂：直接查库应看到同一个值。
        let in_db: String = sqlx::query_scalar("SELECT value FROM settings WHERE key = $1")
            .bind(REQUIRE_DEVICE_ID)
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(in_db, "false");

        store.set_setting(REQUIRE_DEVICE_ID, "true").await.unwrap();
        assert!(store.require_device_id(), "覆盖写要立刻生效");
        store.delete_setting(REQUIRE_DEVICE_ID).await.unwrap();
        assert_eq!(store.get_setting(REQUIRE_DEVICE_ID).unwrap(), None);
        assert!(store.require_device_id(), "删除后退回默认值（要求设备身份）");

        // 转发开关同样走缓存，且新键优先于旧键。
        store.set_setting(CACHE_SCOPE_GLOBAL, "false").await.unwrap();
        assert!(!store.forward_flags().system_shape, "旧键应在新键缺省时生效");
        store.set_setting(SYSTEM_SHAPE, "true").await.unwrap();
        assert!(store.forward_flags().system_shape, "新键存在就以新键为准");
    }

    /// 转发形态开关：**未设置时必须全开**——否则升级到带开关的版本会让既有部署的转发形态
    /// 悄悄变样。只有 `"0"`/`"false"`（忽略大小写与首尾空白）算关，其余取值一律视为开。
    #[sqlx::test]
    async fn forward_flags_default_on_and_parse_off(pool: PgPool) {
        let store = PgStore::for_test(pool).await;

        assert_eq!(store.forward_flags(), ForwardFlags::default(), "空库应等于默认值");
        assert!(ForwardFlags::default().spoof_identity, "默认必须是开");
        assert!(ForwardFlags::default().system_shape);
        assert!(
            !ForwardFlags::default().fable_refusal_fallback,
            "fable 那档替用户决定换模型作答，默认必须是关"
        );
        assert!(
            !ForwardFlags::default().opus_refusal_fallback,
            "opus 那档是官方不产生的形态，默认必须是关"
        );
        assert!(
            !ForwardFlags::default().sim_billing_only,
            "仅注 billing header 是实验性形态，默认必须是关"
        );

        // 每个键各用一种「关」的写法，确认逐项独立且解析口径一致。
        for (key, off) in [
            (SPOOF_IDENTITY_ENABLED, "0"),
            (SPOOF_DEVICE_ID, "0"),
            (NORMALIZE_DEVICE_FP, "0"),
            (SPOOF_BILLING_CCH, "false"),
            (CCH_REAL_RECOMPUTE, "0"),
            (CCH_SIM_COMPUTE, "0"),
            (FILL_CLIENT_HEADERS, " FALSE "),
            (MERGE_BETA, "False"),
            (SYSTEM_SHAPE, "0"),
            (ORIG_HEADER_CASE, "0"),
            (THINKING_SIGNATURE_RETRY, "0"),
            (SIMULATE_CC, "0"),
            (SIMULATE_FULL_SYSTEM, "0"),
            (FILL_ABSENT_TOOLS, "0"),
            (SIM_TRIM_TOOLS, "0"),
            (SIM_BILLING_ONLY, "0"),
            (SIM_BILLING_KEEP_USER_ID, "0"),
            (REAL_BILLING_KEEP_USER_ID, "0"),
            (SIM_MESSAGE_THREADS, "0"),
            (FILL_METADATA, "0"),
            (RATE_LIMIT_RETRY, "0"),
            (SYSTEM_CACHE_SCOPE, "0"),
            (SYSTEM_CACHE_TTL, "0"),
            (EAGER_TOOL_STREAMING, "0"),
            (NONSTREAM_AS_SSE, "0"),
            (STRIP_EXTRA_FIELDS, "0"),
            (TOOL_NAME_MIMIC, "0"),
            (INJECT_THINKING, "0"),
            (REDACTED_THINKING_RETRY, "0"),
            (REJECT_OPENAI_SHAPE, "0"),
            (REJECT_SESSION_CONFLICT, "0"),
            (REJECT_PROBES, "0"),
            (REJECT_REFUSALS, "0"),
            (REJECT_EMPTY_REPLIES, "0"),
            (REJECT_LEARNED_SHAPES, "0"),
            (REJECT_PROBES_STRICT, "0"),
            (API_TELEMETRY, "0"),
            (KEEPALIVE_TELEMETRY, "0"),
            (FABLE_REFUSAL_FALLBACK, "0"),
            (OPUS_REFUSAL_FALLBACK, "0"),
        ] {
            store.set_setting(key, off).await.unwrap();
        }
        let f = store.forward_flags();
        assert_eq!(
            f,
            ForwardFlags {
                spoof_identity: false,
                spoof_device_id: false,
                normalize_device_fp: false,
                billing_cch: false,
                cch_real_recompute: false,
                cch_sim_compute: false,
                fill_client_headers: false,
                merge_beta: false,
                system_shape: false,
                orig_header_case: false,
                thinking_signature_retry: false,
                redacted_thinking_retry: false,
                simulate_cc: false,
                simulate_full_system: false,
                fill_absent_tools: false,
                sim_trim_tools: false,
                sim_billing_only: false,
                sim_billing_keep_user_id: false,
                real_billing_keep_user_id: false,
                sim_message_threads: false,
                fill_metadata: false,
                rate_limit_retry: false,
                cache_scope_global: false,
                cache_ttl_1h: false,
                eager_tool_streaming: false,
                nonstream_as_sse: false,
                strip_extra_fields: false,
                tool_name_mimic: false,
                inject_thinking: false,
                reject_openai_shape: false,
                reject_session_conflict: false,
                reject_probes: false,
                reject_probes_strict: false,
                reject_refusals: false,
                reject_empty_replies: false,
                reject_learned_shapes: false,
                api_telemetry: false,
                keepalive_telemetry: false,
                fable_refusal_fallback: false,
                opus_refusal_fallback: false,
            }
        );

        // 只开回一项，其余保持关闭：开关之间不得互相影响。
        store.set_setting(MERGE_BETA, "true").await.unwrap();
        let f = store.forward_flags();
        assert!(f.merge_beta);
        assert!(!f.spoof_identity && !f.billing_cch && !f.fill_client_headers);
        assert!(!f.orig_header_case);

        // 无法识别的取值算「开」，不能因为写错字把形态悄悄关掉。
        store.set_setting(SPOOF_IDENTITY_ENABLED, "yes").await.unwrap();
        assert!(store.forward_flags().spoof_identity);
    }

    /// 0.3.93 把 `reject_probes` 拆成三个键：旧库只写过 `reject_probes=0` 的，升级后两条新键
    /// 沿用它（用户当时关掉的是整套，不能悄悄开回来）；旧键开着的新键也开；新键一旦写了
    /// 就以新键为准，与旧键互不影响。
    #[sqlx::test]
    async fn forward_flags_reject_probes_legacy_value_seeds_split_switches(pool: PgPool) {
        let store = PgStore::for_test(pool).await;

        // 全新库：三个都默认开。
        let f = store.forward_flags();
        assert!(f.reject_probes && f.reject_refusals && f.reject_empty_replies);

        // 旧库只关过 reject_probes：两条新键跟着关。
        store.set_setting(REJECT_PROBES, "0").await.unwrap();
        let f = store.forward_flags();
        assert!(!f.reject_probes);
        assert!(!f.reject_refusals, "旧键关过 = 学到的拒答规则也别拦");
        assert!(!f.reject_empty_replies, "旧键关过 = 零输出规则也别拦");

        // 新键显式开：压过旧键；另一条没写的仍跟旧键。
        store.set_setting(REJECT_REFUSALS, "1").await.unwrap();
        let f = store.forward_flags();
        assert!(!f.reject_probes && f.reject_refusals && !f.reject_empty_replies);

        // 旧键开、新键显式关：新键为准。
        store.set_setting(REJECT_PROBES, "1").await.unwrap();
        store.set_setting(REJECT_EMPTY_REPLIES, "0").await.unwrap();
        let f = store.forward_flags();
        assert!(f.reject_probes && f.reject_refusals && !f.reject_empty_replies);
    }

    /// v0.3.91 的单一 `refusal_fallback` 旧键拆成 fable / opus 两档后：旧键只沿用到 fable
    /// （关过的库升级后 fable 也别补），对 opus 不算数（旧库里开着，opus 仍按默认关）；
    /// 新键一旦写了就以新键为准。
    #[sqlx::test]
    async fn forward_flags_split_refusal_fallback_keys_migrate_legacy_to_fable_only(pool: PgPool) {
        let store = PgStore::for_test(pool).await;

        // 旧库把总开关关了：fable 跟着关，opus 本来就关。
        store.set_setting(REFUSAL_FALLBACK_LEGACY, "0").await.unwrap();
        let f = store.forward_flags();
        assert!(!f.fable_refusal_fallback, "旧键关过 = fable 也别补");
        assert!(!f.opus_refusal_fallback);

        // 旧库把总开关明确开着：fable 开，opus **不**跟着开——默认关正是拆分的目的。
        store.set_setting(REFUSAL_FALLBACK_LEGACY, "1").await.unwrap();
        let f = store.forward_flags();
        assert!(f.fable_refusal_fallback);
        assert!(!f.opus_refusal_fallback, "旧键开着也不能把 opus 那条实验开关带开");

        // 新键存在就以新键为准，旧键不再作数；两档互不影响。
        store.set_setting(FABLE_REFUSAL_FALLBACK, "0").await.unwrap();
        store.set_setting(OPUS_REFUSAL_FALLBACK, "1").await.unwrap();
        let f = store.forward_flags();
        assert!(!f.fable_refusal_fallback, "fable 新键 0 压过旧键 1");
        assert!(f.opus_refusal_fallback, "opus 显式开才开");
    }

    /// 设备窗口表的清扫：device_id 是客户端自报的，乱编 id 能把 map 撑大；超过阈值时清掉
    /// 空窗口，但**窗口内还有记录的键一个都不能丢**——丢了等于给那台设备白送一轮名额。
    #[sqlx::test]
    async fn crowded_device_windows_are_swept_without_losing_live_ones(pool: PgPool) {
        let store = PgStore::for_test(pool).await;
        store.set_setting(DEVICE_RPM_LIMIT, "1").await.unwrap();

        let window = Duration::from_secs(RPM_WINDOW_SECS as u64);
        for i in 0..(DEVICE_RATE_MAX_KEYS + 10) {
            store.device_rate.try_take(format!("dev-{i}"), 1, window);
        }
        // 除了一台仍在窗口内的，其余全部推到过期。
        {
            let mut hits = store.device_rate.hits.lock();
            for (k, q) in hits.iter_mut() {
                if k != "dev-0" {
                    for t in q.iter_mut() {
                        *t -= Duration::from_secs(RPM_WINDOW_SECS as u64 + 1);
                    }
                }
            }
        }
        store.device_rate.sweep_if_crowded(window, DEVICE_RATE_MAX_KEYS);
        let hits = store.device_rate.hits.lock();
        assert_eq!(hits.len(), 1, "过期的键该被清掉");
        assert!(hits.contains_key("dev-0"), "还在窗口内的键不能被清掉");
    }

    /// 重开同一个库时，缓存要从库里重新装载（否则重启后设置全部凭空回到默认值）。
    #[sqlx::test]
    async fn settings_cache_is_reloaded_on_open(pool: PgPool) {
        {
            let store = PgStore::for_test(pool.clone()).await;
            store.set_setting(BARE_RATE_LIMIT, "42").await.unwrap();
        }
        let store = PgStore::for_test(pool).await;
        assert_eq!(store.bare_rate_limit(), 42, "重开库后设置应从库里装回来");
    }
}
