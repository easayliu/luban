//! `settings` 表：键名常量、读写与各项设置的取值口径。

use super::*;

impl CredentialStore {
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
    pub fn set_setting(&self, key: &str, value: &str) -> Result<()> {
        {
            let conn = self.conn.lock();
            conn.execute(
                "INSERT INTO settings (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = ?2",
                params![key, value],
            )?;
        }
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
    pub fn delete_setting(&self, key: &str) -> Result<()> {
        {
            let conn = self.conn.lock();
            conn.execute("DELETE FROM settings WHERE key = ?1", [key])?;
        }
        self.settings.write().remove(key);
        Ok(())
    }
}

/// 把 `settings` 整张表读进内存。只在打开库时调一次，见 [`CredentialStore::with_conn`]。
pub(super) fn load_settings(conn: &Connection) -> Result<HashMap<String, String>> {
    let mut stmt = conn.prepare("SELECT key, value FROM settings")?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    let mut out = HashMap::new();
    for row in rows {
        let (k, v) = row?;
        out.insert(k, v);
    }
    Ok(out)
}

/// 接入用 client api key 的 settings 键名。
pub const CLIENT_API_KEY: &str = "client_api_key";

/// 管理密码（sha256 hex）的 settings 键名。
pub const ADMIN_PASSWORD: &str = "admin_password_sha256";

/// 只读访客密码（sha256 hex）的 settings 键名。与管理密码一样属于部署本身，不随迁移走。
pub const VIEWER_PASSWORD: &str = "viewer_password_sha256";

/// 两个密码各自「规范形」（反复百分号解码到底）的 sha256 hex，判两者会不会被认混用，
/// 见 `crate::auth::canonical`。与密码本身一样不随迁移走。
pub const ADMIN_PASSWORD_CANONICAL: &str = "admin_password_canonical_sha256";

pub const VIEWER_PASSWORD_CANONICAL: &str = "viewer_password_canonical_sha256";

/// 环境变量接管的管理 / 访客密码：上一次启动时那份密码的 argon2 哈希，与它的版本号。
/// 启动时比对，环境里的密码换了或撤了版本号就加一；会话指纹只记版本号
/// （见 `crate::auth::password_tag`），库里不落环境密码任何可快速离线猜测的形态。
pub const ADMIN_ENV_PASSWORD_HASH: &str = "admin_env_password_argon2";
pub const ADMIN_ENV_PASSWORD_VERSION: &str = "admin_env_password_version";
pub const VIEWER_ENV_PASSWORD_HASH: &str = "viewer_env_password_argon2";
pub const VIEWER_ENV_PASSWORD_VERSION: &str = "viewer_env_password_version";

/// 与部署绑定、不随迁移走的 settings 键（导出不带、导入不认），除了 [`CONSOLE_AUTH_KEYS`]
/// 之外的那些。分开列是因为旧版密码键每次启动都会被清掉，这几个不能。
pub const DEPLOYMENT_ONLY_KEYS: &[&str] = &[
    ADMIN_ENV_PASSWORD_HASH,
    ADMIN_ENV_PASSWORD_VERSION,
    VIEWER_ENV_PASSWORD_HASH,
    VIEWER_ENV_PASSWORD_VERSION,
    "billing_backfilled",
];

/// 控制台登录相关的 settings 键：属于部署本身，导出不带、导入不认。
pub const CONSOLE_AUTH_KEYS: &[&str] =
    &[ADMIN_PASSWORD, VIEWER_PASSWORD, ADMIN_PASSWORD_CANONICAL, VIEWER_PASSWORD_CANONICAL];

/// 设备绑定有效期（秒）的 settings 键名；`<= 0` 表示永不过期。
pub const DEVICE_BINDING_TTL: &str = "device_binding_ttl_secs";

/// 设备绑定有效期默认值：1 小时。
pub const DEFAULT_DEVICE_BINDING_TTL_SECS: i64 = 3600;

/// 软绑定保留期（秒）的 settings 键名；`<= 0` 表示永久保留。
pub const DEVICE_BINDING_RETENTION: &str = "device_binding_retention_secs";

/// 软绑定保留期默认值：7 天。
///
/// 取得比 TTL 长得多是有意的：TTL 那一小时是「名额」的粒度（要能及时把名额还给别人），
/// 而亲和性没有名额成本——一条绑定行几十字节，多留几天换的是「同一台机器隔夜再开工还是
/// 原来那个号」，正好覆盖 thinking 签名跨天复用的场景。
pub const DEFAULT_DEVICE_BINDING_RETENTION_SECS: i64 = 7 * 24 * 3600;

/// 模拟会话绑定有效期（秒）的 settings 键名；`<= 0` 表示永不过期。与设备的那一项分开配：
/// 设备是一台机器、隔一小时再来还是它，会话是一段对话、几十分钟没动多半已经结束，两者的
/// 「还占不占名额」不该是同一个时长。
pub const SESSION_BINDING_TTL: &str = "session_binding_ttl_secs";

/// 模拟会话绑定有效期默认值：30 分钟。比设备的 1 小时短——会话名额（也就是会话 id）要及时
/// 让给下一个对话复用；代价是隔半小时以上再续的对话会换一个槽位、换一个会话 id。
pub const DEFAULT_SESSION_BINDING_TTL_SECS: i64 = 30 * 60;

/// 模拟会话软绑定保留期（秒）的 settings 键名；`<= 0` 表示永久保留。
pub const SESSION_BINDING_RETENTION: &str = "session_binding_retention_secs";

/// 模拟会话软绑定保留期默认值：1 天。对话隔夜再续仍优先回原号（thinking 签名跟着账号走），
/// 再久的对话基本不会回来，行不必留一周；会话比设备多得多，表也不该无限长。
pub const DEFAULT_SESSION_BINDING_RETENTION_SECS: i64 = 24 * 3600;

/// 绑定行真正被删除的时限：`None` 表示永不删除。
///
/// - `ttl <= 0`（绑定永不过期）：名额永远占着，删了反而丢名额语义 → 不删。
/// - `retention <= 0`：显式要求永久保留 → 不删。
/// - 否则取 `max(retention, ttl)`：保留期比 TTL 还短的配置是自相矛盾的（行会在还占着名额时
///   被删掉），按 TTL 兜底，等价于「不做软绑定」的旧行为。
pub fn effective_retention(ttl_secs: i64, retention_secs: i64) -> Option<i64> {
    if ttl_secs <= 0 || retention_secs <= 0 {
        return None;
    }
    Some(retention_secs.max(ttl_secs))
}

/// 是否改写 `metadata.user_id` 的 account_uuid/device_id；`"0"`/`"false"` 关闭，缺省视为开启。
pub const SPOOF_IDENTITY_ENABLED: &str = "spoof_identity_enabled";

/// 来访自带 `device_id` 时，要不要把它换成本凭证派生的那个。缺省视为开启（即既有行为）。
///
/// 与 [`REQUIRE_DEVICE_ID`] 无关：那个管「没带身份的请求放不放行」，这个管「带了身份的
/// 请求要不要改写其中的设备段」。
pub const SPOOF_DEVICE_ID: &str = "spoof_device_id";

/// 设备指纹是否只取平台（arch/os），不含客户端原始 `device_id`。缺省视为开启。
///
/// 开（默认）：`fingerprint = arch|os|出站 UA` → **同平台且同客户端版本**的客户端收敛成同一个
/// 伪装 device_id，符合真实用户一人多设备的模式。
/// 关：`fingerprint = client_device_id|arch|os|出站 UA` → 每个 (账号, 客户端设备) 都是独立的
/// 设备身份，客户端越多、上游看到该账号的设备数就越多，不符合正常用户的使用模式。
///
/// **出站 UA 两档都在指纹里，不受本开关影响**：一台设备只能有一个客户端版本，否则上游会看到
/// 同一个 device_id 在同一秒里自报好几个版本。见 [`crate::proxy::device_fingerprint`]。
///
/// 只在 [`SPOOF_DEVICE_ID`] 开着时有意义——那个关着时 device_id 原样透传，指纹不参与。
pub const NORMALIZE_DEVICE_FP: &str = "normalize_device_fp";

/// 缓存断点要不要写 `ttl:"1h"`（对齐官方）。缺省视为开启；关掉即沿用客户端自己传的时长。
pub const SYSTEM_CACHE_TTL: &str = "system_cache_ttl";

/// 是否给 `x-anthropic-billing-header` 补 `cch`（订阅模式独有字段）。
pub const SPOOF_BILLING_CCH: &str = "spoof_billing_cch";

/// 真实 CC 来访的 body 被改写后，是否按最终出站字节重算 billing header 的 `cch` 的
/// settings 键名。缺省视为开启，见 [`ForwardFlags::cch_real_recompute`]。
pub const CCH_REAL_RECOMPUTE: &str = "cch_real_recompute";

/// 模拟请求的 billing header `cch` 是否按出站字节算真值的 settings 键名。缺省视为开启，
/// 见 [`ForwardFlags::cch_sim_compute`]。
pub const CCH_SIM_COMPUTE: &str = "cch_sim_compute";

/// 是否替客户端补齐它没带的 `accept-encoding`/`anthropic-version`/`x-client-request-id`。
pub const FILL_CLIENT_HEADERS: &str = "fill_client_headers";

/// 是否合并/重排 `anthropic-beta` 并塞入 `oauth-2025-04-20`；关闭则原样转发客户端那串。
pub const MERGE_BETA: &str = "merge_beta";

/// 是否把 `system` 改写成官方订阅客户端的 4 块形态（拆块 + 断点全上 `ttl:1h` +
/// 基座标 `scope:"global"`）。
pub const SYSTEM_SHAPE: &str = "system_shape";

/// [`SYSTEM_SHAPE`] 的旧键名。那时它只做「给最长的 system 块标 `scope:"global"`」，
/// 现在做整套形态对齐。旧库里若把它关过，语义上就是「别动 system」，故在新键缺省时沿用它，
/// 免得升级后凭空替这些人打开一项会涨价的改写（1h 缓存写单价是 5m 的 2 倍）。
pub const CACHE_SCOPE_GLOBAL: &str = "cache_scope_global";

/// 是否按官方拼写与顺序发出头名（`wreq` 的 `OrigHeaderMap`）；关闭则退回全小写 + 队尾追加。
pub const ORIG_HEADER_CASE: &str = "orig_header_case";

/// 上游以「thinking 块签名无效」拒绝时，是否降级历史 thinking 块后重试一次的 settings 键名。
/// 缺省视为开启：它只在那一种 400 上触发，重试失败也会原样透传最初那条响应，开着不会更差。
pub const THINKING_SIGNATURE_RETRY: &str = "thinking_signature_retry";

/// 上游以「`redacted_thinking` 块的 `data` 无效」拒绝时，是否降级历史 thinking 块后重试一次的
/// settings 键名。缺省视为开启。与上面那项同一个兜底（[`crate::proxy::demote_thinking_blocks`]
/// 对 `redacted_thinking` 是整块删），只是上游点名的是那段密文。
pub const REDACTED_THINKING_RETRY: &str = "redacted_thinking_retry";

/// 非 Claude Code 客户端的请求，是否按官方抓包形态模拟成 CC 请求的 settings 键名。
/// 缺省视为开启：关掉的话这类请求会因缺 `You are Claude Code, …` 被上游拒掉，等于不可用。
pub const SIMULATE_CC: &str = "simulate_cc";

/// 模拟路径是否补齐官方 `system` 第四块的 settings 键名。缺省视为开启，
/// 见 [`ForwardFlags::simulate_full_system`]。
pub const SIMULATE_FULL_SYSTEM: &str = "simulate_full_system";

/// 模拟路径是否给不带 `tools` 的来访也补官方工具的 settings 键名。缺省视为开启，
/// 见 [`ForwardFlags::fill_absent_tools`]。
pub const FILL_ABSENT_TOOLS: &str = "fill_absent_tools";

/// 模拟路径注入的官方工具是否去掉 Artifact / ListAgents / SendFeedback 三条的 settings 键名。
/// 缺省视为启用，见 [`ForwardFlags::sim_trim_tools`]。
pub const SIM_TRIM_TOOLS: &str = "sim_trim_tools";

/// 模拟路径是否只注入 `system[0]` billing header、其余注入全部跳过的 settings 键名。
/// 缺省视为停用，见 [`ForwardFlags::sim_billing_only`]。
pub const SIM_BILLING_ONLY: &str = "sim_billing_only";

/// 模拟路径是否按官方 message threads 形态写 `thread`（首轮 `create`、接得上的续轮 `continue`
/// 只发增量）的 settings 键名。缺省视为开启，见 [`ForwardFlags::sim_message_threads`]。
pub const SIM_MESSAGE_THREADS: &str = "sim_message_threads";

/// 已是 CC 形态、但不带 `metadata.user_id` 的请求，是否补一份官方形态身份的 settings 键名。
/// 缺省视为开启：官方**每条**请求都带那个字段，缺了就是一处白给的判据。
pub const FILL_METADATA: &str = "fill_metadata";

/// 上游 429 时是否打冷却并换号重试的 settings 键名。缺省视为开启：不开的话被限流的号会
/// 一直被粘性绑定的设备撞上，而其它账号闲着。
pub const RATE_LIMIT_RETRY: &str = "rate_limit_retry";

/// 非流式 `/v1/messages` 是否改成流式发给上游、再聚合成整段 JSON 回给客户端的 settings
/// 键名。缺省视为开启：CC 从不发非流式的 `/v1/messages`，透传等于每条这类请求都留一处
/// 100% 稳定的判据。见 [`ForwardFlags::nonstream_as_sse`]。
pub const NONSTREAM_AS_SSE: &str = "nonstream_as_sse";

/// 工具声明要不要补 `eager_input_streaming: true`（按已证实的 profile）。缺省视为开启。
/// 见 [`ForwardFlags::eager_tool_streaming`]。
pub const EAGER_TOOL_STREAMING: &str = "eager_tool_streaming";

/// 是否剥掉官方从不发送的顶层字段的 settings 键名。缺省视为开启。
/// 见 [`ForwardFlags::strip_extra_fields`]。
pub const STRIP_EXTRA_FIELDS: &str = "strip_extra_fields";

/// 是否把第三方工具名混淆成假名转发的 settings 键名。缺省视为开启。
/// 见 [`ForwardFlags::tool_name_mimic`]。
pub const TOOL_NAME_MIMIC: &str = "tool_name_mimic";

/// 模拟路径下是否注入 `thinking` 的 settings 键名。缺省视为开启。
/// 见 [`ForwardFlags::inject_thinking`]。
pub const INJECT_THINKING: &str = "inject_thinking";

/// 是否本地拒绝带 OpenAI 格式转换残留的请求的 settings 键名。
/// 缺省视为开启：messages 里的 `role:"system"`、`call_` 前缀的工具调用 id、OpenAI 专属顶层
/// 字段等一律 400，不转发。关掉后原样转发，由上游返回官方的 400。
pub const REJECT_OPENAI_SHAPE: &str = "reject_openai_shape";

/// 来访的会话 id 在**头与体两处不一致**时是否本地拒绝的 settings 键名。缺省视为开启。
///
/// 官方 CC 的 `X-Claude-Code-Session-Id` 与 `metadata.user_id` 里那个 `session_id`
/// **逐字相同**；两处给出两个不同的合法 uuid，是官方从不产生的形态。开着即 400 挡在门口
/// （连带避免「按哪一个建会话链」这个没有正确答案的问题）；关掉则取头那个并打一条 warn。
/// 见 [`ForwardFlags::reject_session_conflict`]。
pub const REJECT_SESSION_CONFLICT: &str = "reject_session_conflict";

/// 是否本地拒绝**探针类**请求的 settings 键名。缺省视为开启。
///
/// 下游中转拿账号做「探活/测活」时发的请求有一组只有它们才会有的强特征（自报 CC 的 UA，
/// 却是无 tools 的单条小消息、`max_tokens` 只有个位数、或身份句在 system 里重复）。身份字段
/// 写错的不算探针、不在这里拒，由模拟路径重建身份。
/// 这些请求每一条都是上游侧「一台设备开一个一次性会话只问一句话」的记录，真实用户从不产生，
/// 是封号复盘里最显眼的判据。开着即在门口就地回一条最小的正常回复（200 + 一句「OK」，
/// 0.3.101 之前是 403）；关掉则照常转发。只管形态判据这一件事：
/// 从响应学来的两类规则（拒答提示词、零输出请求类）各有自己的开关，见 [`REJECT_REFUSALS`]
/// 与 [`REJECT_EMPTY_REPLIES`]（0.3.93 之前三者共用这一个键，关掉探针就把学到的规则一起放行了）。
/// 见 [`ForwardFlags::reject_probes`] 与 `proxy::probe_signature`。
pub const REJECT_PROBES: &str = "reject_probes";

/// 是否本地拒绝**上游分类器已经拒答过的那条提示词**（同一模型、`system` + `messages` +
/// `tools` + `tool_choice` 逐字相同的重发，不分凭证；`kind = "refusal"`）。默认开。只挡出站不带 `fallbacks`
/// 的请求：带了 fallback 的上游会换模型重跑，本地拦下反而让 fallback 永远没机会。
/// 旧库没写过这个键时沿用 [`REJECT_PROBES`] 的取值（拆分前共用）。
/// 见 [`ForwardFlags::reject_refusals`] 与 `proxy::known_refused_prompt`。
pub const REJECT_REFUSALS: &str = "reject_refusals";

/// 是否本地拒绝**上游回过 200 却零输出的请求类**（模型 + 无 tools 单条消息 + 同一个
/// `max_tokens`；`kind = "empty_reply"`，不限 UA）。默认开。旧库没写过这个键时沿用
/// [`REJECT_PROBES`] 的取值（拆分前共用）。
/// 见 [`ForwardFlags::reject_empty_replies`] 与 `proxy::known_empty_reply`。
pub const REJECT_EMPTY_REPLIES: &str = "reject_empty_replies";

/// 是否从上游 400 里学请求形态错误、并在本地拒掉同样的组合（`kind = "shape"`）。默认开；
/// 关掉即不学也不拦。见 [`ForwardFlags::reject_learned_shapes`]。
pub const REJECT_LEARNED_SHAPES: &str = "reject_learned_shapes";

/// 探针拒绝的**严格模式**：ping 不再要求无 tools，并新增「短开场」判据（无 system、无 tools、
/// 一条几个字的用户消息）。默认关——会误伤真人用裸聊天客户端发的第一句「你好」。
/// 见 [`ForwardFlags::reject_probes_strict`] 与 `proxy::probe_signature`。
pub const REJECT_PROBES_STRICT: &str = "reject_probes_strict";

/// 是否替每条转发的 `/v1/messages` 上报官方客户端形态的遥测（`tengu_api_*` 事件链、
/// Datadog 日志、OTel 指标）的 settings 键名。缺省视为开启。见 [`ForwardFlags::api_telemetry`]。
pub const API_TELEMETRY: &str = "api_telemetry";

/// 保活是否还发遥测（每 30 分钟的空闲版本检查事件 + Datadog 日志 + GrowthBook 画像）的
/// settings 键名。缺省视为开启。见 [`ForwardFlags::keepalive_telemetry`]。
pub const KEEPALIVE_TELEMETRY: &str = "keepalive_telemetry";

/// 主线程 **fable 族**补不补服务端 refusal fallback（`fallbacks: [{"model":"claude-opus-5"}]`
/// 加 `server-side-fallback` beta）的 settings 键名。缺省视为**关**：形态虽逐字取自官方
/// 2.1.260 抓包，但开着等于替用户决定「拒答就换 opus-5 作答」——作答模型、计价、约一小时的
/// 粘连都随之改变，用户还看不到拒答本身；这该由用户自己拨开。
/// 见 [`ForwardFlags::fable_refusal_fallback`]。
pub const FABLE_REFUSAL_FALLBACK: &str = "fable_refusal_fallback";

/// 主线程 **opus-5 族**补不补 luban 自定的 refusal fallback 链（4.8 → 4.6）的 settings 键名。
/// 缺省视为**关闭**：官方 opus 客户端不发这个字段，补了就是一份官方客户端从不产生的请求
/// 形态；封号复盘里查不出它导致了 `account_on_hold`，但作为风控形态风险它该是独立的实验
/// 开关而不是默认行为。见 [`ForwardFlags::opus_refusal_fallback`]。
pub const OPUS_REFUSAL_FALLBACK: &str = "opus_refusal_fallback";

/// v0.3.91 的单一开关键名（fable 与 opus 一起管）。v0.3.92 起拆成 [`FABLE_REFUSAL_FALLBACK`]
/// 与 [`OPUS_REFUSAL_FALLBACK`]；旧键只在 fable 新键缺省时沿用（旧库里关过即「fable 也别补」），
/// **不**沿用到 opus——把 opus 那条默认关掉正是拆分的目的，旧库里开着也不算数。
pub const REFUSAL_FALLBACK_LEGACY: &str = "refusal_fallback";

/// 上次从 `downloads.claude.ai/claude-code-releases/latest` 学到的官方最新 Claude Code 版本
/// （`主.次.修` 串）的 settings 键名。启动时垫进 [`crate::oauth::latest_release`] 的缓存，
/// 学到新值时写回；是来访 UA 自报版本的上限（见 `proxy::known_latest_release`）。
pub const LATEST_CC_RELEASE: &str = "latest_cc_release";

/// 官方基座那个缓存断点要不要带 `scope:"global"` 的 settings 键名。缺省视为开启：基座
/// 全网同一份，跨账号共享缓存是白捡的。
///
/// **键名不能叫 `cache_scope_global`**——那个名字被 [`CACHE_SCOPE_GLOBAL`] 占着，在旧库里
/// 是 [`SYSTEM_SHAPE`] 的曾用名，复用会让旧库里关过那个开关的人莫名其妙丢掉整套 system 对齐。
pub const SYSTEM_CACHE_SCOPE: &str = "system_cache_scope";

/// 布尔型设置的统一口径：仅 `"0"`/`"false"`（忽略大小写与首尾空白）为关，其余为开。
fn setting_is_on(value: &str) -> bool {
    !matches!(value.trim().to_ascii_lowercase().as_str(), "0" | "false")
}

/// 是否要求请求携带有效设备身份的 settings 键名；`"0"`/`"false"` 关闭（放行裸请求），
/// 缺省或其它值视为要求（无有效 `metadata.user_id` 的请求直接 403）。
pub const REQUIRE_DEVICE_ID: &str = "require_device_id";

/// 允许接入的最低 Claude Code 客户端版本的 settings 键名；空串或未设置表示不限。
///
/// 值是版本号本身（`2.1.220`、`2.1`、`2` 都收），不是布尔。判定只针对 UA 里带
/// `claude-cli/<版本>` 的请求：这道闸是给「逼旧版 CC 升级」用的，别的客户端（SDK、
/// 浏览器、自写脚本）UA 里根本没有版本可比，拿它们跟一个 CC 版本号比毫无意义，故一律放行。
pub const MIN_CLIENT_VERSION: &str = "min_client_version";

/// 登录时申请哪些 OAuth scope 的 settings 键名；未设置或空串表示用默认的
/// [`crate::config::SCOPES`]（官方 Claude Code 那一整套）。
///
/// 值是空格分隔的 scope 串本身，不是布尔。只在**新登录**时起作用：已存下来的凭证按当初授权
/// 的范围来，改这一项不会追溯——要换范围就得把号重新登一次。刷新 token 发的是固定的
/// [`crate::config::REFRESH_SCOPES`]（官方那五项），与这一项无关；也因此选了精简 scope 的号
/// 会在第一次刷新后被扩回五项，见那个常量的注释。
///
/// 想少授权的一档现成值是 [`crate::config::SCOPES_MINIMAL`]，代价见那里的注释。
pub const OAUTH_SCOPES: &str = "oauth_scopes";

/// 全局默认设备数上限的 settings 键名；`<= 0` 表示显式不限。
/// 账号自身 `device_limit == 0`（默认值）时套用它，无需逐个账号配置。
pub const DEFAULT_DEVICE_LIMIT: &str = "default_device_limit";

/// 未写入 `default_device_limit` 时的默认上限。用于防止新账号在多个设备、session
/// 并行使用时无限扩张；写入 settings 的值仍优先，账号级 `< 0` 仍可明确不限。
pub const DEFAULT_DEVICE_LIMIT_VALUE: i64 = 5;

/// 全局默认**模拟会话**数上限的 settings 键名；`<= 0` 表示显式不限。
/// 账号自身 `session_limit == 0`（默认值）时套用它。语义见 [`Select::session_key`]。
pub const DEFAULT_SESSION_LIMIT: &str = "default_session_limit";

/// 未写入 `default_session_limit` 时的默认上限。取设备默认的两倍：一台设备上同时开几个
/// 对话是常态，会话名额本就该比设备名额宽；但仍要封顶——「每条请求一个新会话」那种流量
/// （封号复盘里最显眼的形态）在这里被挡住。写入 settings 的值仍优先，账号级 `< 0` 仍可明确不限。
pub const DEFAULT_SESSION_LIMIT_VALUE: i64 = 10;

/// 全局默认账号 RPM 上限的 settings 键名；`<= 0` 表示默认不限。
/// 账号自身 `rpm_limit == 0`（默认值）时套用它，无需逐个账号配置。
pub const DEFAULT_RPM_LIMIT: &str = "default_rpm_limit";

/// 每设备 RPM 上限的 settings 键名；`<= 0` 表示不限（默认）。见
/// [`CredentialStore::take_device_rpm_slot`]。
///
/// 全局一个值，不逐台配置：设备是自动发现的，逐台配置的运维成本远高于逐账号——真要给某台
/// 设备开小灶，那更像是给它单独配一个账号的活。
pub const DEVICE_RPM_LIMIT: &str = "device_rpm_limit";

/// 每会话 RPM 上限的 settings 键名；`<= 0` 表示不限（默认）。见
/// [`CredentialStore::take_session_rpm_slot`]。
///
/// 与 [`DEVICE_RPM_LIMIT`] 是两个粒度、**要一起配**，别只留一个：
/// - 只配会话：一台机器开 N 个会话就是 N 倍额度，且客户端换个会话 id 就重置——`/clear` 一下
///   便是满血的新桶，等于没有护栏；
/// - 只配设备：同机的多个会话共用一个桶，安分的那个窗口会被刷疯的那个挤没，而这正是设备闸
///   自己想解决的问题在下一层的复现。
///
/// 推荐的配法是会话给贴合单个对话真实节奏的值、设备给它的几倍当总量兜底。别把设备闸配得比
/// 会话闸还小：那样会话这道永远轮不到判定，等于白配。
pub const SESSION_RPM_LIMIT: &str = "session_rpm_limit";

/// 每会话**并发在途**上限的 settings 键名；`<= 0`（含未设置）表示不限。
///
/// 与 [`SESSION_RPM_LIMIT`] 互补：RPM 控的是分钟窗口内的总量，并发上限控的是**瞬时同时在飞**
/// 的请求数。Claude Desktop 启动时会并行发 20+ 条 `max_tokens=1` 的 cache 预热请求，
/// 瞬间打爆上游的每组织速率限制；RPM 窗口管不住这种「一秒内全发完」的脉冲。给一个 3~5 的
/// 并发上限就能把脉冲拉平，不必等到上游 429 再补救。
pub const SESSION_CONCURRENCY_LIMIT: &str = "session_concurrency_limit";

/// 并发上限默认值：5。一个正常的 Claude Code 会话在稳态下很少超过 3~4 条并行请求
/// （主请求 + 1~2 个 subagent），5 留出余量不卡正常使用，同时把 20+ 的预热爆发削掉
/// 四分之三。设为 0 表示不限。
pub const DEFAULT_SESSION_CONCURRENCY_LIMIT: i64 = 5;

/// 单凭证裸请求速率上限的 settings 键名；`<= 0` 表示不限（默认）。见
/// [`CredentialStore::bare_rate_limit`]。
pub const BARE_RATE_LIMIT: &str = "bare_rate_limit";

/// 裸请求速率窗口（秒）的 settings 键名；`<= 0` 时退回 [`DEFAULT_BARE_RATE_WINDOW_SECS`]。
pub const BARE_RATE_WINDOW_SECS: &str = "bare_rate_window_secs";

/// 裸请求速率窗口默认值：60 秒（即上限的语义是「每分钟多少条」）。
pub const DEFAULT_BARE_RATE_WINDOW_SECS: i64 = 60;

/// 上游 429 时最多换几个号重试的 settings 键名；`0` 表示不重试。
pub const RATE_LIMIT_RETRY_MAX: &str = "rate_limit_retry_max";

/// 额度使用率到多少百分比就提前把号挪出调度池的 settings 键名；`0` 表示关闭本机制
/// （退回「收到 429 才停」的老行为）。见 [`CredentialStore::quota_pause_pct`]。
pub const QUOTA_PAUSE_PCT: &str = "quota_pause_pct";

/// 天级窗口（`7d`）提前停调度阈值的 settings 键名；`0`（默认）表示不按这个窗口停号。
///
/// **为什么和 [`QUOTA_PAUSE_PCT`] 分成两档**：同一个百分比在两个窗口上的后果差着数量级。
/// 5h 到 90% 停号，最多歇几小时就自己回来了，那是「省下一发注定失败的 429」；7d 到 90%
/// 停号，停的是**到下个 7d 重置为止**——按 [`CredentialStore::quota_pause_pct`] 原来的
/// 混用口径，一个周用量偏高的号会被整段挪出池子，哪怕它这 5 小时里一点没用、还能正常干活。
/// 而 7d 真满了本来也有兜底：那时上游自己会回 429，账号级冷却照常接手。
///
/// 所以默认只按 5h 停，天级窗口要不要提前停由使用者自己开——真要开，配个比 5h 更高的数
/// （如 95~99）更合用：既留出「快满了别再往里灌」的余量，又不至于为了几个百分点把号停上几天。
pub const QUOTA_PAUSE_PCT_7D: &str = "quota_pause_pct_7d";

/// 天级窗口提前停调度的默认阈值：`0` = 关。理由见 [`QUOTA_PAUSE_PCT_7D`]。
pub const DEFAULT_QUOTA_PAUSE_PCT_7D: i64 = 0;

/// 提前停调度的默认阈值：90%。
///
/// 不取 100：上游报的是**已用**比例，等它到 1.0 时下一条请求必然吃 429——那正是本机制要
/// 省掉的那一发。留出 10% 而不是贴着上限卡：使用率是**一条响应报一次**的，两次上报之间
/// 一轮长对话就能吃掉好几个百分点，阈值贴太近等于还没来得及停就已经撞上去了。剩下的那点
/// 额度也不算白扔——号是到窗口 reset 就回来的，而不是作废。嫌保守就往上调，见
/// [`CredentialStore::quota_pause_pct`]。
pub const DEFAULT_QUOTA_PAUSE_PCT: i64 = 90;

/// 换号重试次数默认值：2。
///
/// 取 2 而不是更大：多数情况下第一次换号就落到一个额度充足的号上，真要连撞好几个，
/// 说明整批账号都被限了，那时继续换只是把一次注定失败的请求拖长——429 早点回给客户端更好。
pub const DEFAULT_RATE_LIMIT_RETRY_MAX: i64 = 2;

/// 账号实际生效的设备数上限：返回 `0` 表示不限。
///
/// `cred_limit` 三态——`> 0` 账号独立上限（覆盖全局）；`0` 跟随全局默认 `default_limit`；
/// `< 0` 账号明确不限（即便全局有默认值也不限）。旧库启动时若未显式配置，会写入
/// [`DEFAULT_DEVICE_LIMIT_VALUE`]；显式写入的 0（不限）保持不变。
pub fn effective_device_limit(cred_limit: i64, default_limit: i64) -> i64 {
    match cred_limit {
        n if n > 0 => n,
        0 => default_limit.max(0),
        _ => 0,
    }
}

/// 账号实际生效的模拟会话数上限：返回 `0` 表示不限。三态语义与 [`effective_device_limit`]
/// 逐条对应，直接委托它（理由同 [`effective_rpm_limit`]）。
pub fn effective_session_limit(cred_limit: i64, default_limit: i64) -> i64 {
    effective_device_limit(cred_limit, default_limit)
}

/// 账号实际生效的 RPM 上限：返回 `0` 表示不限。三态语义与
/// [`effective_device_limit`] 逐条对应（账号独立 / 跟随全局 / 明确不限），故直接委托它——
/// 两处各写一份 `match`，哪天改了三态语义就只会改到其中一处。
pub fn effective_rpm_limit(cred_limit: i64, default_limit: i64) -> i64 {
    effective_device_limit(cred_limit, default_limit)
}

/// 账号实际生效的提前停调度阈值（百分比，`0` = 这一档不停）：账号自己配了
/// （[`Credential::quota_pause_pct`] / `quota_pause_pct_7d`）就用它，否则跟随全局那档。
///
/// 两档各自调用一次，别把 5h 的账号值配上 7d 的全局值——同 [`CredentialStore::quota_pause_pct`]
/// 那两档「各算各的」的口径。
pub fn effective_quota_pause_pct(cred_pct: Option<i64>, global_pct: i64) -> i64 {
    cred_pct.unwrap_or(global_pct).clamp(0, 100)
}
