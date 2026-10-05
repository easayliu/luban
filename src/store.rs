//! 多凭证的 SQLite 持久化层（参照 kiro.rs 的做法）。
//!
//! 单连接 + `parking_lot::Mutex` 串行化；WAL + `synchronous=NORMAL`；STRICT 表 +
//! `CHECK`/`UNIQUE` 约束。token 轮换走单行 `UPDATE`，不重写整库。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Row, TransactionBehavior, params};

use crate::credentials::{
    Credential, PRIORITY_DEFAULT, PRIORITY_MAX, PRIORITY_MIN, priority_tiers_by_rank,
};

/// 查询列顺序，与 [`row_to_cred`] 一一对应。
const COLS: &str = "id, label, tier, access_token, refresh_token, expires_at, priority, disabled, \
     created_at, updated_at, device_limit, ban_reason, account_uuid, resume_at, org_type, proxy, \
     rpm_limit, rate_limit_tier, org_uuid, subscription_created_at, quota_pause_pct, \
     quota_pause_pct_7d, session_limit, org_name, seat_tier, subscription_status, \
     extra_usage_enabled";

/// 凭证 SQLite 存储。
pub struct CredentialStore {
    /// 后台统计专用的**只读**连接池（同一个库文件，WAL 下读不挡写），见 [`Self::read_conn`]。
    ///
    /// 控制台的聚合查询（额度快照、趋势、流水翻页等）要扫几十万行流水，一次几十到几百毫秒。
    /// 它们以前和转发路径共用 `conn`，持锁期间所有选号、落流水都排在后面，而等锁的正是
    /// tokio 工作线程——整个运行时的 SSE 会跟着一起停。拆成两把锁后转发路径不再等它们。
    ///
    /// 不止一条：概览页同时在拉账号列表（30s）、实时指标（10s）、24h/7d 两组趋势，只有一条
    /// 只读连接时它们互相排队，账号列表要等前面那几条 7 天扫描跑完才轮到。WAL 下多条读连接
    /// 各读各的快照、真正并行，条数见 [`READER_POOL_SIZE`]。
    ///
    /// 内存库（测试）开不出第二条连接去读同一份数据，此时为空，读退回 `conn`。
    ///
    /// **必须声明在 `conn` 之前**：字段按声明顺序析构，它们得先关。主连接关闭时若只读连接还
    /// 开着，主连接就不是最后一条，SQLite 会跳过关库时的 checkpoint；只读连接自己又做不了
    /// checkpoint，于是优雅退出后 `-wal` 留在磁盘上、最近的写入没回写进 `.db`——只拷
    /// `luban.db` 做备份或迁移的人会漏掉这一段。
    readers: Vec<Mutex<Connection>>,
    /// 只读连接全忙时下一个去排队的下标，轮着排，别都挤在第一条上。
    next_reader: std::sync::atomic::AtomicUsize,
    conn: Mutex<Connection>,
    /// 每凭证一把刷新锁，串行化 token 刷新，见 [`valid_access_token_for_device`]。
    /// 上游刷新会**轮换 refresh_token**：并发刷新时后完成的那次会把已被作废的 token 写回库，
    /// 该凭证之后所有刷新都 `invalid_grant`，等于账号被自己废掉。
    refresh_locks: Mutex<HashMap<i64, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    /// 裸请求的每凭证限流窗口（进程内），见 [`RateWindow`] 与 [`CredentialStore::bare_rate_limit`]。
    bare_rate: RateWindow,
    /// 每账号 RPM 的限流窗口（进程内，窗口固定 [`RPM_WINDOW_SECS`]），
    /// 见 [`CredentialStore::default_rpm_limit`]。
    ///
    /// 与 [`Self::bare_rate`] 用同一种计数器、但**各算各的**：那个只卡没有设备身份的流量，
    /// 这个卡该账号的全部转发。两者都配了的话一条裸请求要同时过两道窗口。
    rpm_rate: RateWindow,
    /// 每**设备** RPM 的限流窗口（进程内，窗口同 [`RPM_WINDOW_SECS`]），
    /// 见 [`CredentialStore::take_device_rpm_slot`]。
    ///
    /// 键是客户端自报的 `device_id`（不是伪装后那个：要限的是发请求的那台机器）。上面两个
    /// 窗口都按账号分桶，管的是「一个号别被打爆」；这个按设备分桶，管的是「一台机器别把
    /// 同账号下其他设备的额度挤没」——账号 RPM 打满时，安分的设备和刷疯了的那台一起被拒。
    device_rate: RateWindow<String>,
    /// 每**会话** RPM 的限流窗口（进程内，窗口同 [`RPM_WINDOW_SECS`]），
    /// 见 [`CredentialStore::take_session_rpm_slot`]。
    ///
    /// 键是客户端自报的会话 id（`X-Claude-Code-Session-Id` 头，或 `metadata.user_id` 里的
    /// session 段，两处官方逐字相同）。与 [`Self::device_rate`] 是**同一件事的两个粒度**：
    /// 一台机器上开三个 CC 窗口，真实并发是三份对话的并发，按设备一刀切会让它们互相挤额度；
    /// 按会话分桶才对得上负载的来源。
    ///
    /// 但它**替代不了**设备那道闸，两道要一起配：会话 id 轮换是免费的（`/clear`、开新窗口、
    /// 重启都换一个新的，立刻是个满血的桶），而设备 id 轮换要付代价（改绑凭证、连累 thinking
    /// 签名、吃 `device_limit` 名额）。故会话闸给的是贴合真实并发的细粒度节流，设备闸兜的是
    /// 「这台机器总量别失控」——后者的阈值该给到前者的几倍，见 [`SESSION_RPM_LIMIT`]。
    session_rate: RateWindow<String>,
    /// 被上游 429 过的凭证的冷却表（进程内），见 [`RateLimitCooldown`]。
    cooldown: RateLimitCooldown,
    /// `settings` 全表的内存镜像，见 [`CredentialStore::get_setting`]。
    ///
    /// **每条转发请求要读 8 项设置**（接入 key、设备身份校验、6 个转发形态开关、重试次数、
    /// 绑定 TTL、设备上限、裸请求限流两项），逐项走 SQL 就是每请求 8 次查询，且全部串行在
    /// 上面那把全局 `conn` 锁上——转发路径的落库、后台的列表查询都得排在它们后面。设置项
    /// 极少变动，缓存住之后这些查询直接归零。
    ///
    /// 写路径只有 [`CredentialStore::set_setting`]/[`CredentialStore::delete_setting`] 两处，
    /// 都是先落库再更新缓存，故进程内不会漂移。**多进程共享同一个库时会读到陈旧值**——
    /// luban 是单进程本地代理，没有这个场景（同 [`RateWindow`] 的取舍）。
    settings: parking_lot::RwLock<HashMap<String, String>>,
}

/// 硬性设备上限触发：所有启用凭证的设备名额均已占满。
///
/// 通过 `anyhow` 向上传递，代理层 `downcast` 后映射为 HTTP 429。
#[derive(Debug)]
pub struct DeviceLimitReached;

impl std::fmt::Display for DeviceLimitReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "all credentials have reached their device limits; no slot is available")
    }
}

impl std::error::Error for DeviceLimitReached {}

/// 硬性**模拟会话**上限触发：所有启用凭证的会话名额均已占满。
///
/// 与 [`DeviceLimitReached`] 是同一件事的另一个粒度：设备上限管带设备身份的来访，这个管
/// **模拟路径上没有设备身份**的来访——它们按会话键（[`Select::session_key`]）粘住账号并占
/// 名额。同样经 `anyhow` 上传、代理层映射为 429。
#[derive(Debug)]
pub struct SessionLimitReached;

impl std::fmt::Display for SessionLimitReached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "all credentials have reached their session limits; no slot is available")
    }
}

impl std::error::Error for SessionLimitReached {}

/// 请求的模型在**所有**可调度的号上都已被上游判成「套餐不含」（见
/// [`CredentialStore::deny_model`]）：不是限流、等多久都没用，换台机器也没用。
///
/// 同 [`DeviceLimitReached`] 走 `anyhow` 上传，代理层 `downcast` 后映射为 403
/// `permission_error`——照上游拒绝一个没权限模型时的口径回，客户端能一眼看懂「换模型」。
#[derive(Debug)]
pub struct ModelUnsupported {
    pub model: String,
    /// 有几个启用中的号被判过不支持它（即被排除掉的那些）。
    pub accounts: usize,
}

impl std::fmt::Display for ModelUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "none of the {} enabled account(s) can use model {}: upstream reported that their plans do not include it (add a Max account, or clear the block from the console after enabling extra usage)",
            self.accounts, self.model
        )
    }
}

impl std::error::Error for ModelUnsupported {}

/// 一条「这个号用不了这个模型」的记录，见 [`CredentialStore::deny_model`]。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ModelDenial {
    /// 归一化后的模型键，见 [`model_denial_key`]。
    pub model: String,
    /// 上游给出的依据（限流头摘要），供控制台展示。
    pub reason: String,
    /// 学到这条记录的时刻（Unix 秒）。
    pub learned_at: i64,
    /// 到点自动失效重新试探的时刻（Unix 秒）；`None` 表示一直有效，直到被显式清掉。
    pub expires_at: Option<i64>,
}

/// 从上游响应里学到的一条规则，落库供重启后回填进程内记忆，见
/// [`CredentialStore::remember_rejections`]。三类共用一张表：
/// - `kind = "shape"`：某模型不接受某字段的某取值（`effort: 'xhigh'`），命中本地拒；
/// - `kind = "deprecated"`：某模型已废弃某字段（`temperature`），命中转发前剥掉，`value` 为空串；
/// - `kind = "empty_reply"`：某模型对「无 tools 的单条消息 + 这个 `max_tokens`」回过 200 却零
///   输出（`field = "max_tokens"`，`value` 是那个数），同类请求命中本地拒，`message` 是当时
///   截下的上游回复开头，见 `crate::proxy::known_empty_reply`；
/// - `kind = "refusal"`：上游分类器拒答过某条提示词（`field = "prompt_sha"`，`value` 是提示词
///   哈希），逐字相同的重发命中时**原样回放上游那次的响应**（[`Self::reply`]：200 + 同一段体），
///   `message` 是「[类别] stop_details=…」的判决文案，控制台与日志看，见
///   `crate::proxy::known_refused_prompt`；
/// - `kind = "app_refusal"`：上游分类器拒答过某个**识别不了会话的应用**（`field = "system_sha"`，
///   `value` 是来访 system 的哈希），同一模型 + 同一份 system 的请求命中时同样回放；拒答至少
///   3 条且占该应用请求数三成以上才学，见 `crate::proxy::record_app_request`。
#[derive(Debug, Clone, PartialEq)]
pub struct LearnedRejection {
    pub kind: String,
    pub model: String,
    pub field: String,
    pub value: String,
    /// 上游原话，日志与控制台列表用。
    pub message: String,
    /// 上游那次的**完整响应体**，命中时原样回放；只有拒答那类有，其余为 `None`。
    pub reply: Option<LearnedReply>,
}

/// 学到规则时上游那次回复的原样响应体（拒答那类专用），见 [`LearnedRejection::reply`]。
///
/// 存的是**上游发来的字节**：来访要流式时上游回的是 SSE（`sse = true`），要非流式时是整段
/// JSON；回放时按来访这次要的形态给——形态一致原样发，不一致才在两种形态间转换。状态码不存：
/// 拒答恒是 200（`stop_reason: "refusal"` 裹在正常 Message 里），非 200 的响应根本学不进来。
#[derive(Debug, Clone, PartialEq)]
pub struct LearnedReply {
    /// 体是 SSE 事件流（`text/event-stream`）还是整段 JSON。
    pub sse: bool,
    /// 响应体原文（UTF-8）。
    pub body: String,
}

/// 学到的规则落库后最多活多久（秒）：7 天。
///
/// 这是持久化这类推断的唯一安全阀。它们是从一条报错里学来的，上游哪天放开了某个取值或恢复了
/// 某个参数，本地没有任何信号能知道——不设期限就是「永久拒掉一个其实已经支持的取值」。
/// 7 天后丢掉重学，代价是每周每种组合白撞一次 400。
pub const LEARNED_REJECTION_TTL_SECS: i64 = 7 * 24 * 3600;

/// 「订阅未生效」那档暂停写进 `ban_reason` 的原因里的固定片段（上游原话是组织不允许 OAuth）。
/// 完整原因见 [`crate::proxy::park_org_oauth_disallowed`]，形如
/// `[subscription-inactive 403] <片段> (…); paused until …`。
///
/// 认这一档一律按 luban 自己的格式认：**开头**是 [`SUBSCRIPTION_PAUSE_TAG`] 前缀。不用
/// `[403] ` 这种开头——封号原因在上游错误没带类型时写的就是 `[403] <上游原话>`，上游哪天的
/// 措辞恰好以这几个词开头，一个真封号就会被当成可以自动恢复的暂停；其余 luban 写的原因
/// （`[keepalive/…]`、`[refresh …]`、`[proxy]`）也都拼不出这个开头。三处同一口径、都区分大小写：
/// [`is_subscription_pause_reason`]、[`SUBSCRIPTION_PAUSE_SQL`]（库里）、前端 `isSubscriptionPause`
/// （`credential-shared.tsx`）。改文案须三处一起改。
pub const ORG_OAUTH_SUSPEND_MARKER: &str = "organization does not allow OAuth authentication";

/// 订阅未生效暂停原因的开头标签：`[` + 它 + ` <三位状态码>] `，见 [`ORG_OAUTH_SUSPEND_MARKER`]。
pub const SUBSCRIPTION_PAUSE_TAG: &str = "subscription-inactive";

/// `ban_reason` 是不是「订阅未生效」那档暂停写的，见 [`ORG_OAUTH_SUSPEND_MARKER`]。
pub fn is_subscription_pause_reason(reason: &str) -> bool {
    let Some(rest) = reason.strip_prefix('[').and_then(|r| r.strip_prefix(SUBSCRIPTION_PAUSE_TAG))
    else {
        return false;
    };
    let b = rest.as_bytes();
    b.len() > 6
        && b[0] == b' '
        && b[1..4].iter().all(u8::is_ascii_digit)
        && &b[4..6] == b"] "
        && rest[6..].starts_with(ORG_OAUTH_SUSPEND_MARKER)
}

/// [`is_subscription_pause_reason`] 的 SQL 版（`ban_reason` 列上的 GLOB 条件，区分大小写）。
/// `[[]` 是 GLOB 里字面的 `[`；标签与片段里只有字母、连字符与空格，不含 GLOB 元字符。
const SUBSCRIPTION_PAUSE_SQL: &str = "ban_reason GLOB \
     '[[]subscription-inactive [0-9][0-9][0-9]] organization does not allow OAuth authentication*'";

/// 人工停用（单个 [`CredentialStore::set_disabled`] 与批量 [`CredentialStore::set_disabled_many`]
/// 共用）：停用、清 `resume_at`，并清掉两种暂停留下的原因——限流暂停的「几点恢复」、订阅未生效
/// 的那句——号就是普通的「手动停用」。不清的话，限流那句会在 `resume_at` 清空后被当成封号原因
/// 显示，订阅那句会让连通性测试通过时把管理员关掉的号又打开。封号原因不动。
///
/// SQLite 的 `UPDATE` 里各表达式读的都是改之前的行，`CASE` 看到的 `resume_at` 是旧值。
fn manual_disable_sql() -> String {
    format!(
        "UPDATE credentials SET disabled = 1, resume_at = NULL, \
                ban_reason = CASE WHEN resume_at IS NOT NULL OR {SUBSCRIPTION_PAUSE_SQL} \
                                  THEN NULL ELSE ban_reason END, \
                updated_at = unixepoch() \
         WHERE id = ?1"
    )
}

/// 把模型名归一成「套餐门禁」的粒度：小写、去掉 `[1m]` 上下文后缀与 `-YYYYMMDD` 日期后缀。
///
/// 上游按套餐放不放行看的是模型本身，`claude-fable-5-1[1m]` 与 `claude-fable-5-1` 不会一个
/// 放一个拒；分开记只会让每个变体各白撞一次。但**不**把 `fable-5` 与 `fable-5-1` 并成一族：
/// 两代的准入未必同步，宁可多撞一次也别猜。
pub fn model_denial_key(model: &str) -> String {
    let mut m = model.trim().to_ascii_lowercase();
    if let Some(stripped) = m.strip_suffix("[1m]") {
        m = stripped.to_string();
    }
    // 形如 `-20251114` 的日期后缀：最后一段全是数字且恰好 8 位。
    if let Some((head, tail)) = m.rsplit_once('-')
        && tail.len() == 8
        && tail.chars().all(|c| c.is_ascii_digit())
    {
        m = head.to_string();
    }
    m
}

/// 该模型是否属于「只有高档套餐才含」的那一族（fable / mythos）。
///
/// **只用于选号排序**（见 [`CredentialStore::select_for_device`] 的 `plan_rank`），不是准入
/// 判据：真正的「能不能用」由上游回答并记进 [`ModelDenial`]。这里猜错的代价仅是多一次换号。
pub fn premium_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    m.contains("fable") || m.contains("mythos")
}

/// 裸请求速率上限触发：所有启用凭证在当前窗口内都已发满。
///
/// 同 [`DeviceLimitReached`] 走 `anyhow` 上传，代理层 `downcast` 后映射为 429，
/// 并带上 `retry-after`——这里的等待时间是可算的（窗口长度），告诉客户端比让它盲目重试好。
#[derive(Debug)]
pub struct BareRateLimited {
    /// 建议的重试间隔（秒），取窗口长度。
    pub retry_after_secs: i64,
}

impl std::fmt::Display for BareRateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "all credentials have reached the bare-request rate limit; retry in {} seconds",
            self.retry_after_secs
        )
    }
}

impl std::error::Error for BareRateLimited {}

/// 账号 RPM 上限触发：本次请求可用的号在最近 60 秒里都已发满。
///
/// 同 [`BareRateLimited`] 走 `anyhow` 上传，代理层 `downcast` 后映射为 429 + `retry-after`。
/// 等待时间是**算得准**的：窗口里最早那条记录滚出 60 秒的那一刻就有名额，故直接给到秒。
#[derive(Debug)]
pub struct RpmLimited {
    /// 建议的重试间隔（秒），取最早腾出名额的那个号。
    pub retry_after_secs: i64,
    /// 是**设备绑定的那个号**打满了（`true`），还是候选池里所有号都打满（`false`）。
    ///
    /// 两者是不同的故障：前者只影响这一台设备（换台设备照样能发），后者是整个代理没名额了。
    /// 拒绝时那行 `refusing to forward` 日志是唯一能区分它们的地方。
    pub sticky: bool,
}

impl std::fmt::Display for RpmLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let who = if self.sticky {
            "the credential bound to this device has reached its RPM limit"
        } else {
            "all credentials have reached their RPM limits"
        };
        write!(f, "{}; retry in {} seconds", who, self.retry_after_secs)
    }
}

impl std::error::Error for RpmLimited {}

/// 限流冷却硬门禁触发：本次请求可选的凭证**全部**处于上游 429 冷却中。
///
/// 同 [`BareRateLimited`] 走 `anyhow` 上传，代理层 `downcast` 后映射为 429 + `retry-after`
/// （取所有候选号中最早解冻的那个的剩余秒数——早一秒都是白撞）。
///
/// 曾经这里是「全员冷却就忽略冷却照常选」的软行为，理由是上游 reset 不准时硬门禁会把整个
/// 代理锁死几小时。现在按需求改成硬的：额度真耗尽时继续发只是把 429 换个地方产生，还平白
/// 消耗上游的失败计数。翻车时的逃生口是控制台的「解除冷却」（`DELETE
/// /credentials/{id}/cooldown` → [`CredentialStore::clear_rate_limited`]），以及连通性
/// 测试成功时的自动解除。
#[derive(Debug)]
pub struct AllRateLimited {
    /// 建议的重试间隔（秒），取最早解冻的那个号的剩余冷却时间。
    pub retry_after_secs: i64,
    /// 最早回来的那个号是「token 刷新失败」暂停的（见 [`REFRESH_FAIL_PAUSE_TAG`]）：这时全池
    /// 在等的不是上游额度，提示不能说成限流；转发那边据此把流水记到这个号名下。
    pub refresh_failed: Option<RefreshFailed>,
}

impl std::fmt::Display for AllRateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.refresh_failed {
            Some(rf) => write!(
                f,
                "all credentials are paused; credential #{} is paused after a token refresh failure; retry in {} seconds",
                rf.cred_id, self.retry_after_secs
            ),
            None => write!(
                f,
                "all credentials are cooling down after upstream rate limits; retry in {} seconds",
                self.retry_after_secs
            ),
        }
    }
}

impl std::error::Error for AllRateLimited {}

/// 滑动窗口计数器（**进程内，不落库**）。按 `K` 分桶：账号维度是 `cred_id`，设备维度是
/// `device_id`。
///
/// 三处在用，各持一份、互不干扰：
/// - [`CredentialStore::bare_rate`]（键：cred_id）只数**无 `metadata.user_id` 的请求**——带
///   设备身份的那些已由设备绑定 + `device_limit` 约束着，而裸请求既不写绑定也不占名额，
///   `device_limit` 对它们完全不生效，这份补的正是那个口子；
/// - [`CredentialStore::rpm_rate`]（键：cred_id）数该账号的**全部**转发，窗口固定 60 秒，
///   即账号 RPM 上限；
/// - [`CredentialStore::device_rate`]（键：device_id）数**单台设备**的全部转发，同样 60 秒
///   窗口，即设备 RPM 上限。前两者管的是「一个号别被打爆」，这个管的是「一台机器别把同号
///   的其他设备挤没」。
///
/// **不落库是有意的**：短窗口限流本来就不该跨重启（重启后放行几条远好于把人锁在门外），
/// 而每请求一次 `usage_logs` 聚合查询的代价，比一把内存锁高一个数量级。代价是多实例部署时
/// 各限各的——luban 是单进程本地代理，没有这个场景；真有了再换成落库的实现。
///
/// 内存占用有上限：每个键最多存 `limit` 个时间戳（超限时不再追加），过期的在每次检查时
/// 顺手清掉；不限（`limit <= 0`）时一条都不记——没人会去读那个队列，记了只会无界增长。
/// 键本身的回收见 [`Self::forget`] 与 [`Self::sweep_if_crowded`]。
struct RateWindow<K = i64> {
    /// 键 → 窗口内每条请求的时刻（升序，用单调时钟，不受系统时间调整影响）。
    hits: Mutex<HashMap<K, VecDeque<Instant>>>,
}

// 手写而非 `#[derive(Default)]`：derive 会给 `K` 加上 `K: Default` 这个用不着的约束。
impl<K> Default for RateWindow<K> {
    fn default() -> Self {
        Self { hits: Mutex::new(HashMap::new()) }
    }
}

impl<K: std::hash::Hash + Eq + Clone> RateWindow<K> {
    /// 该键在窗口内是否还有名额（`limit <= 0` 即不限）。只问不记，顺手清掉过期的。
    ///
    /// 与 [`Self::take`] 拆开，是因为**一次选号要过两道窗口**（裸请求上限 + 账号 RPM）：
    /// 若边问边记，一个过了第一道却卡在第二道的号会白扣一个名额，而它压根没被用上。
    /// 拆开后「问过了但没发」的窗口并不存在——选号全程持着 `conn` 锁（见
    /// [`CredentialStore::select_for_device`]），选号彼此串行，中间插不进第二次选号。
    ///
    /// 单闸场景（设备 RPM）没有这个顾虑，用 [`Self::try_take`] 一次问完记完。
    fn has_room(&self, key: K, limit: i64, window: Duration) -> bool {
        if limit <= 0 {
            return true; // 未配置上限 = 不限
        }
        let mut hits = self.hits.lock();
        let q = hits.entry(key).or_default();
        prune(q, window);
        (q.len() as i64) < limit
    }

    /// 给该键记一条。不限时不记（理由见结构体文档）；已满时也不记（越界的那条不该进队列，
    /// 它是被拒掉的）。
    fn take(&self, key: K, limit: i64, window: Duration) {
        if limit <= 0 {
            return;
        }
        let mut hits = self.hits.lock();
        let q = hits.entry(key).or_default();
        prune(q, window);
        if (q.len() as i64) < limit {
            q.push_back(Instant::now());
        }
    }

    /// 有名额就记一条并返回 `true`，否则原样返回 `false`。**问与记在同一把锁里**，故不存在
    /// 两条请求同时看到「还剩最后一个名额」的竞态——[`Self::has_room`] + [`Self::take`]
    /// 那条路靠外层的 `conn` 锁串行化，这条路自己就够。
    fn try_take(&self, key: K, limit: i64, window: Duration) -> bool {
        if limit <= 0 {
            return true;
        }
        let mut hits = self.hits.lock();
        let q = hits.entry(key).or_default();
        prune(q, window);
        if (q.len() as i64) >= limit {
            return false;
        }
        q.push_back(Instant::now());
        true
    }

    /// 该键要等多少秒才腾出下一个名额：窗口里最早那条滚出去的那一刻。至少 1 秒——
    /// 回 0 等于让客户端立刻再撞一次。空窗口（本来就有名额）同样按 1 秒算。
    fn retry_after_secs(&self, key: &K, window: Duration) -> i64 {
        let hits = self.hits.lock();
        let left = hits
            .get(key)
            .and_then(|q| q.front().copied())
            .map(|t| window.saturating_sub(t.elapsed()))
            .unwrap_or_default();
        // 向上取整：不足 1 秒的余量截断成 0 就又成了「立刻重试」。
        (left.as_secs() as i64 + i64::from(left.subsec_nanos() > 0)).max(1)
    }

    /// 键失效后清掉它的窗口（凭证被删除/停用），免得 map 里留下永远不再访问的键。
    fn forget(&self, key: &K) {
        self.hits.lock().remove(key);
    }

    /// 键数超过 `max_keys` 时清掉所有已空的窗口。
    ///
    /// 凭证维度不需要这个（键有限且删号时会 [`Self::forget`]），设备维度需要：device_id 是
    /// 客户端自报的，一个乱编 id 的脚本能往 map 里塞进无数个键。空队列（窗口内一条都没有）
    /// 是安全的清理对象——清掉与留着的判定结果完全一样。
    fn sweep_if_crowded(&self, window: Duration, max_keys: usize) {
        let mut hits = self.hits.lock();
        if hits.len() <= max_keys {
            return;
        }
        hits.retain(|_, q| {
            prune(q, window);
            !q.is_empty()
        });
    }
}

/// 丢掉队首所有已滚出窗口的时间戳（队列按时刻升序，故遇到第一个还在窗口内的即可停）。
fn prune(q: &mut VecDeque<Instant>, window: Duration) {
    while q.front().is_some_and(|t| t.elapsed() >= window) {
        q.pop_front();
    }
}

/// 被上游 429 过的凭证的冷却表（**进程内，不落库**），按 `(账号, 模型)` 分格。
///
/// **为什么要分模型**：实测只有 fable 会在账号基础窗口（5h/7d）远未跑满时回 429——
/// 要么是模型级容量限制，要么是 fable 专用的超额池（`7d_oi`）吃满了，两种都不是账号
/// 额度耗尽：同一时刻 sonnet/opus 在这个号上照常可用。把整个账号打进冷却等于因为一个
/// 模型不可用就把这个号的其余流量一起赶走。故冷却分两档，由
/// [`crate::proxy::rate_limit_scope`] 依限流头判定：
///
/// - **账号级**（`model = None`）：**基础窗口**被拒或打满（额度确实耗尽），该号所有
///   模型一起让位；
/// - **模型级**（`model = Some(m)`）：基础窗口都有余量却被拒（容量限制或超额池满），
///   只让这个模型让位，其余照常。
///
/// **和「停用」是两回事**：停用是人工/封号那种需要介入的终态，冷却到点自动恢复，
/// 不写库、不进 `ban_reason`、控制台上也不该显示成账号出了问题。
///
/// **不落库的取舍**：账号级冷却动辄几小时到几天（5h/7d 窗口耗尽），远长于一次重启，
/// 重启后忘掉冷却会让下一条请求再撞一次 429——但它撞完就会重新打上冷却，属于自愈，
/// 代价是一次往返；换来的是不动 schema、也不必处理「库里写着冷却但上游其实早恢复了」的
/// 陈旧状态。硬门禁下这一条同时也是最后一道保险：真被一个离谱的 reset 锁住时，重启即解。
///
/// **冷却是硬门禁**：冷却中的号一律不参与调度，全部凭证都在冷却时
/// [`CredentialStore::select_for_device`] 直接返回 [`AllRateLimited`]（代理映射为 429 +
/// `retry-after`），不会「忽略冷却照常选」。额度真耗尽时继续发只是把 429 换个地方产生。
///
/// 硬门禁的代价是上游 reset 报得过长（或我们算错）时会把代理白白锁住，故留了两个逃生口：
/// 控制台的「解除冷却」（[`CredentialStore::clear_rate_limited`]），以及连通性测试成功时的
/// 自动解除（见 [`Self::clear`]）。冷却时长直接睡满上游给的 reset（5h 窗口就是 5h、7d 就是
/// 7d，见 `proxy::RateLimitInfo::cooldown`），到点自动回到调度池参与正常选号——不定时探活、
/// 也不提前放出去撞：额度没到点是不会自己长回来的，提前试探每次都要白扔一发 429。
#[derive(Default)]
struct RateLimitCooldown {
    /// `(cred_id, 模型)` → 该格的冷却（单调时钟）。模型为空串表示**整个账号**。
    until: Mutex<HashMap<(i64, String), Cooling>>,
}

/// 一格冷却的两条**独立**时间线。
///
/// - `gate`：额度类冷却（账号级基础窗口耗尽、模型级超额池满）。这类 429 是**跟着账号走**的
///   ——这个号确实没额度了，挡住它去调度是对的。
/// - `soft`：瞬时限流（容量 / 请求速率，见 `proxy::LimitScope::Transient`）。这类 429
///   **不跟着账号走**，拿它挡调度是有害的：号被挡掉之后设备会改绑到下一个号，客户端每重试
///   一次就点掉一个号，转够一圈全池的这个模型都在冷却，新请求一条都进不来。所以这一条只用于
///   展示，不参与选号。
///
/// 分成两条而不是一条加个布尔：同一格完全可能同时挂着两种（超额池满打了 40 分钟的门禁，
/// 半分钟后又撞了一发瞬时限速）。合成一条的话两者只能取其一——要么让瞬时那档把门禁提前解掉，
/// 要么让门禁把瞬时那档拖长，两种都是错的。
#[derive(Default, Clone, Copy)]
struct Cooling {
    gate: Option<Instant>,
}

impl Cooling {
    /// 此刻是否仍挡着选号。
    fn gating(&self, now: Instant) -> bool {
        self.gate.is_some_and(|t| t > now)
    }

    /// 此刻是否还有未到期的冷却；为假即可以把这一格清掉。
    fn live(&self, now: Instant) -> bool {
        self.gating(now)
    }

    fn secs_until(deadline: Option<Instant>, now: Instant) -> i64 {
        deadline.filter(|t| *t > now).map(|t| t.duration_since(now).as_secs() as i64).unwrap_or(0)
    }

    fn remaining(&self, now: Instant) -> i64 {
        Self::secs_until(self.gate, now)
    }

    fn gate_remaining(&self, now: Instant) -> i64 {
        Self::secs_until(self.gate, now)
    }
}

impl RateLimitCooldown {
    /// 打上**参与选号门禁**的冷却。`model` 为 `None` 即账号级（所有模型）。
    /// 同一条时间线重复命中时取**较晚**的那个结束时刻，不让新的短冷却缩短旧的长冷却。
    fn mark(&self, cred_id: i64, model: Option<&str>, dur: Duration) {
        let deadline = Instant::now() + dur;
        let mut until = self.until.lock();
        let slot = until.entry((cred_id, model.unwrap_or_default().to_string())).or_default();
        if slot.gate.is_none_or(|t| t < deadline) {
            slot.gate = Some(deadline);
        }
    }

    /// 该凭证此刻对该模型是否仍在冷却中：账号级冷却对所有模型生效，模型级只挡自己那一个。
    /// 顺手清掉已到期的项。
    fn is_cooling(&self, cred_id: i64, model: Option<&str>) -> bool {
        let now = Instant::now();
        let mut until = self.until.lock();
        let mut hit = false;
        for key in [String::new(), model.unwrap_or_default().to_string()] {
            match until.get(&(cred_id, key.clone())) {
                // 只认门禁那条线：还挂着 soft 的格子留着给界面看，但不挡选号。
                Some(c) if c.live(now) => hit = hit || c.gating(now),
                Some(_) => {
                    until.remove(&(cred_id, key));
                }
                None => {}
            }
        }
        hit
    }

    /// 解除冷却。`model` 指定时清账号级 + 该模型那格——用于连通性测试成功：上游此刻放行了
    /// 「这个账号 + 这个模型」，这两格的冷却都不再成立，其它模型的格子不动（sonnet 通了
    /// 证明不了 fable 通）。`None` 时清掉该凭证的**所有**格——用于手动解除：冷却只是选号
    /// 提示，解除错了最坏也只是再撞一次 429、重新打上，和「全员冷却时忽略冷却」同一条哲学。
    fn clear(&self, cred_id: i64, model: Option<&str>) {
        let mut until = self.until.lock();
        match model {
            Some(m) => {
                until.remove(&(cred_id, String::new()));
                until.remove(&(cred_id, m.to_string()));
            }
            None => until.retain(|(id, _), _| *id != cred_id),
        }
    }

    /// 该凭证对该模型还要冷却多少秒（未冷却返回 0）：账号级与模型级两格都得过期才算解冻，
    /// 故取两者的**较大**值。硬门禁下用它算 `retry-after`，见 [`AllRateLimited`]。
    fn remaining_for(&self, cred_id: i64, model: Option<&str>) -> i64 {
        let now = Instant::now();
        let until = self.until.lock();
        [String::new(), model.unwrap_or_default().to_string()]
            .into_iter()
            .filter_map(|key| until.get(&(cred_id, key)))
            .map(|c| c.gate_remaining(now))
            .max()
            .unwrap_or(0)
    }

    /// 账号级冷却的剩余秒数（未冷却返回 0），供控制台展示。
    ///
    /// 刻意只看账号级（key 为空串）：模型级冷却是「这个号的某个模型暂时不可用」，账号本身
    /// 照常在调度，把它显示成账号被限流会误导。模型级那档走 [`Self::model_remaining`]。
    ///
    /// **注意这一档在正常路径上几乎恒为 0**：账号级 429 现在走
    /// [`CredentialStore::pause_for_rate_limit`] 落库（`resume_at`），只有落库失败的兜底
    /// 分支才会退回进程内冷却。留着它正是为了让那个兜底状态在后台能看见。
    fn remaining_secs(&self, cred_id: i64) -> i64 {
        let now = Instant::now();
        self.until.lock().get(&(cred_id, String::new())).map(|c| c.gate_remaining(now)).unwrap_or(0)
    }

    /// 该凭证**模型级**冷却的明细：`(模型名, 剩余秒数)`，按剩余时间倒序。未冷却时为空。
    ///
    /// 补的是一个真实的观测盲区：模型级 429（实测里 fable 撞超额池就是这一档）只写进
    /// `(cred_id, 模型)` 那些格子，而后台读的是账号级那一格，于是选号侧明明已经跳过这个
    /// 模型、界面上却什么都看不到——「冷却中」那套筛选与徽章形同虚设。
    fn model_remaining(&self, cred_id: i64) -> Vec<(String, i64, bool)> {
        let now = Instant::now();
        let mut out: Vec<(String, i64, bool)> = self
            .until
            .lock()
            .iter()
            .filter(|((id, model), c)| *id == cred_id && !model.is_empty() && c.live(now))
            .map(|((_, model), c)| (model.clone(), c.remaining(now), c.gating(now)))
            .collect();
        // 剩得最久的排前面；同秒数按模型名，保证展示顺序稳定（HashMap 迭代序是随机的）。
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    fn forget(&self, cred_id: i64) {
        self.until.lock().retain(|(id, _), _| *id != cred_id);
    }
}

/// 迁移用的一条凭证：导出与导入**共用同一个形态**，导出的文件原样喂回来就是导入的入参。
///
/// 刻意不带的三类字段：
/// - `id` / `created_at` / `updated_at`：id 由目标库自己发（[`CredentialStore::import_credential`]
///   按账号身份匹配，不认 id），时间戳属于「这条记录在这个库里的历史」，搬过去只会造出一份
///   假的过去；
/// - 用量、绑定、账本（`usage_logs`/`device_bindings`/`credential_stats`/`device_costs`）：
///   费用与额度快照是**按 cred_id 关联**的历史，跟着账号搬过去会与目标库自己的流水混在一起，
///   而设备绑定压根是「哪台机器绑在哪个号上」的本机状态，换台机器毫无意义；
/// - 管理密码：见 [`CredentialStore::settings_snapshot`]。
///
/// 代理池中的一条记录。
#[derive(serde::Serialize, Clone)]
pub struct SavedProxy {
    pub id: i64,
    pub label: String,
    pub url: String,
    pub created_at: u64,
}

/// 迁移用的代理池条目：只保留 label 和 url，id 由目标库自己发。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PortableProxy {
    #[serde(default)]
    pub label: String,
    pub url: String,
}

impl From<&SavedProxy> for PortableProxy {
    fn from(p: &SavedProxy) -> Self {
        Self { label: p.label.clone(), url: p.url.clone() }
    }
}

/// 全字段都给了 `#[serde(default)]`：迁移文件是会被人手改的（删掉几个号、改个优先级），
/// 少一个字段就整份导入失败太脆。缺 `expires_at` 退化成 0，即「已过期」——首次使用时用
/// refresh_token 换一份新的，正是想要的行为。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PortableCredential {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub org_type: Option<String>,
    /// 额度档原值（`default_claude_max_5x`）；statsig eval 的 `rateLimitTier` 要它。
    /// 不带上的话，迁移后到下一次成功拉 profile 之前，那个字段会一直缺着。
    #[serde(default)]
    pub rate_limit_tier: Option<String>,
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub expires_at: u64,
    /// 缺省（`None`）落默认档 [`PRIORITY_DEFAULT`]，不能让 serde 填 0 变成最高档 P0。
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub device_limit: i64,
    #[serde(default)]
    pub rpm_limit: i64,
    #[serde(default)]
    pub ban_reason: Option<String>,
    #[serde(default)]
    pub account_uuid: Option<String>,
    /// 组织 UUID 与订阅创建时刻原串，同 `rate_limit_tier` 的道理：不带上的话，迁移后到
    /// 下一次成功拉 profile 之前，遥测 `auth.organization_uuid` 与 eval 的
    /// `subscriptionCreatedAt` 会一直缺着。
    #[serde(default)]
    pub org_uuid: Option<String>,
    #[serde(default)]
    pub subscription_created_at: Option<String>,
    /// 组织名称、席位档、订阅状态、超额用量开关：只给后台看，带上免得迁移后到下次刷新前空着。
    #[serde(default)]
    pub org_name: Option<String>,
    #[serde(default)]
    pub seat_tier: Option<String>,
    #[serde(default)]
    pub subscription_status: Option<String>,
    #[serde(default)]
    pub extra_usage_enabled: Option<bool>,
    #[serde(default)]
    pub resume_at: Option<u64>,
    #[serde(default)]
    pub proxy: Option<String>,
    /// 逐账号的提前停调度阈值（5h / 7d 两档）；`None` 跟随全局。见
    /// [`Credential::quota_pause_pct`]。
    #[serde(default)]
    pub quota_pause_pct: Option<i64>,
    #[serde(default)]
    pub quota_pause_pct_7d: Option<i64>,
    /// 模拟会话数上限，三态同 `device_limit`；旧导出没有这一项时按 0（跟随全局）。
    #[serde(default)]
    pub session_limit: i64,
}

impl From<&Credential> for PortableCredential {
    fn from(c: &Credential) -> Self {
        Self {
            label: c.label.clone(),
            tier: c.tier.clone(),
            org_type: c.org_type.clone(),
            rate_limit_tier: c.rate_limit_tier.clone(),
            access_token: c.access_token.clone(),
            refresh_token: c.refresh_token.clone(),
            expires_at: c.expires_at,
            priority: Some(c.priority),
            disabled: c.disabled,
            device_limit: c.device_limit,
            rpm_limit: c.rpm_limit,
            ban_reason: c.ban_reason.clone(),
            account_uuid: c.account_uuid.clone(),
            org_uuid: c.org_uuid.clone(),
            subscription_created_at: c.subscription_created_at.clone(),
            org_name: c.org_name.clone(),
            seat_tier: c.seat_tier.clone(),
            subscription_status: c.subscription_status.clone(),
            extra_usage_enabled: c.extra_usage_enabled,
            resume_at: c.resume_at,
            proxy: c.proxy.clone(),
            quota_pause_pct: c.quota_pause_pct,
            quota_pause_pct_7d: c.quota_pause_pct_7d,
            session_limit: c.session_limit,
        }
    }
}

/// 导入一条凭证的结果：目标库里原本没有这个账号（`Added`），还是已经有、被这条覆盖了
/// （`Updated`）。调用方据此报「新增 N 个、更新 M 个」——迁移最想知道的就是这两个数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportOutcome {
    Added,
    Updated,
}

impl CredentialStore {
    /// 数据库文件路径。默认 `~/.luban/luban.db`；`LUBAN_HOME` 可覆盖基目录。
    pub fn db_path() -> Result<PathBuf> {
        let base = match std::env::var_os("LUBAN_HOME") {
            Some(dir) => PathBuf::from(dir),
            None => dirs::home_dir()
                .context("could not determine the user home directory")?
                .join(".luban"),
        };
        Ok(base.join("luban.db"))
    }

    /// 在默认路径打开（或新建）凭证库并初始化 schema。
    pub fn open_default() -> Result<Self> {
        Self::open_at(&Self::db_path()?)
    }

    /// 在指定路径打开（或新建）凭证库并初始化 schema，另开一条后台统计用的只读连接。
    fn open_at(path: &std::path::Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory: {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("failed to open credential database: {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        // 有了独立的只读连接，自动 checkpoint 可能撞上它的读事务，WAL 那一刻重置不了、只能
        // 接着往后长（改动前读写同锁串行，没有这个情况）。WAL 本身不会自己缩，这里给个上限：
        // 下次重置时截回 64MB，一阵连续的慢查询过后磁盘占用能降回来。
        conn.pragma_update(None, "journal_size_limit", 64 * 1024 * 1024)?;
        init_schema(&conn)?;
        let mut store = Self::with_conn(conn);
        // schema 已由主连接建好，只读连接不做迁移。开不出来不影响服务：少几条就少几条并行，
        // 一条都没有时后台读退回主连接，只是回到拆分之前的性能。
        for _ in 0..READER_POOL_SIZE {
            match open_reader(path) {
                Ok(reader) => store.readers.push(Mutex::new(reader)),
                Err(e) => {
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        opened = store.readers.len(),
                        "failed to open a read-only database connection"
                    );
                    break;
                }
            }
        }
        if store.readers.is_empty() {
            tracing::warn!("no read-only database connection; admin queries share the main one");
        }
        Ok(store)
    }

    /// 内存库（**仅测试**）：schema 已初始化，进程退出即消失。
    ///
    /// 给 crate 内其它模块的测试用（`with_conn`/`init_schema` 都是本模块私有的）；
    /// store 自己的测试直接用 `with_conn`。
    #[cfg(test)]
    pub(crate) fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        init_schema(&conn)?;
        Ok(Self::with_conn(conn))
    }

    /// 由已初始化的连接构造（`open_default` 与测试共用）。
    fn with_conn(conn: Connection) -> Self {
        // 设置表整张读进内存，见 `settings` 字段的说明。读失败（表还不存在等）就从空表起步，
        // 所有取值退回各自的默认值——绝不能因为读设置失败而让整个服务起不来。
        let settings = load_settings(&conn).unwrap_or_default();
        Self {
            conn: Mutex::new(conn),
            readers: Vec::new(),
            next_reader: std::sync::atomic::AtomicUsize::new(0),
            refresh_locks: Mutex::new(HashMap::new()),
            bare_rate: RateWindow::default(),
            rpm_rate: RateWindow::default(),
            device_rate: RateWindow::default(),
            session_rate: RateWindow::default(),
            cooldown: RateLimitCooldown::default(),
            settings: parking_lot::RwLock::new(settings),
        }
    }

    /// 后台统计用的只读连接（没有就退回主连接）。
    ///
    /// **只给纯读、且只由管理接口调用的方法用**：连接以只读方式打开，写语句会直接报错；
    /// 转发路径也不该用它——它可能正被一条几百毫秒的聚合查询占着。调用方（web 的 handler）
    /// 要放在 `spawn_blocking` 里跑，等这把锁同样不能占 tokio 工作线程。
    ///
    /// 先挑一条空闲的；全忙才排队，排哪条轮着来。
    fn read_conn(&self) -> parking_lot::MutexGuard<'_, Connection> {
        if self.readers.is_empty() {
            return self.conn.lock();
        }
        if let Some(guard) = self.readers.iter().find_map(|r| r.try_lock()) {
            return guard;
        }
        let i = self.next_reader.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.readers[i % self.readers.len()].lock()
    }

    /// 取该凭证的刷新锁（不存在则创建）。
    pub(crate) fn refresh_lock(&self, cred_id: i64) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.refresh_locks.lock().entry(cred_id).or_default().clone()
    }

    /// 插入一条新凭证，返回带 id 的完整记录。
    // 参数多是因为一条凭证本来就有这么多字段，且调用点只有「加号」那一处；
    // 打包成结构体只会多一个只用一次的类型。
    #[allow(clippy::too_many_arguments)]
    pub fn insert(
        &self,
        label: &str,
        tier: Option<&str>,
        access_token: &str,
        refresh_token: &str,
        expires_at: u64,
        account_uuid: Option<&str>,
        org_type: Option<&str>,
    ) -> Result<Credential> {
        let conn = self.conn.lock();
        // 新凭证一律落在默认档 P2：同档内按设备数负载均衡，新账号立刻参与分摊。
        // 需要瀑布式（榨干一个再用下一个）时，手动/批量把账号调到不同优先级即可。
        // 显式写 priority：老库的列默认值还是 0，不能指望它。
        conn.execute(
            "INSERT INTO credentials
                 (label, tier, access_token, refresh_token, expires_at, account_uuid, org_type,
                  priority)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                label,
                tier,
                access_token,
                refresh_token,
                expires_at as i64,
                account_uuid,
                org_type,
                PRIORITY_DEFAULT
            ],
        )
        .context("failed to insert credential (the refresh_token may already exist)")?;
        let id = conn.last_insert_rowid();
        conn.query_row(&format!("SELECT {COLS} FROM credentials WHERE id = ?1"), [id], row_to_cred)
            .context("failed to read the newly inserted credential")
    }

    /// 列出全部凭证，按 (priority, id) 升序。
    ///
    /// 先惰性恢复到点的限流暂停号（[`Self::resume_due`]），否则后台会一直显示成「已停用」，
    /// 直到下一条转发请求碰巧来触发恢复——控制台上看到的必须是此刻真实的调度状态。
    pub fn list(&self) -> Result<Vec<Credential>> {
        let conn = self.conn.lock();
        Self::resume_due(&conn)?;
        let mut stmt =
            conn.prepare(&format!("SELECT {COLS} FROM credentials ORDER BY priority ASC, id ASC"))?;
        let rows = stmt.query_map([], row_to_cred)?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 按 id 读取单条。
    pub fn get(&self, id: i64) -> Result<Option<Credential>> {
        let conn = self.conn.lock();
        conn.query_row(&format!("SELECT {COLS} FROM credentials WHERE id = ?1"), [id], row_to_cred)
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                other => Err(other.into()),
            })
    }

    /// 删除一条，返回是否确有删除。口径同 [`Self::remove`]（仅测试用）。
    #[cfg(test)]
    pub fn delete(&self, id: i64) -> Result<bool> {
        Ok(self.remove(&[id])? > 0)
    }

    /// 清空所有凭证，返回删除条数。连带清空设备绑定与全部用量日志（口径同
    /// [`Self::delete`]：账号没了，历史用量不再保留）。
    pub fn clear(&self) -> Result<usize> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM usage_logs", [])?;
        tx.execute("DELETE FROM device_bindings", [])?;
        tx.execute("DELETE FROM session_bindings", [])?;
        tx.execute("DELETE FROM credential_stats", [])?;
        tx.execute("DELETE FROM device_costs", [])?;
        tx.execute("DELETE FROM model_denials", [])?;
        let n = tx.execute("DELETE FROM credentials", [])?;
        tx.commit()?;
        Ok(n)
    }

    /// 导出全部凭证的可迁移形态，顺序同 [`Self::list`]（priority, id）。
    ///
    /// **含明文 access/refresh token**——迁移要的就是它们，脱敏过的导出等于没导。谁能调到
    /// 这个口子就等于拿到了这些账号，故接口侧另加了一道闸（见 `crate::web` 的 `export`）。
    pub fn export_credentials(&self) -> Result<Vec<PortableCredential>> {
        Ok(self.list()?.iter().map(PortableCredential::from).collect())
    }

    /// 导出代理池的可迁移形态。
    pub fn export_proxies(&self) -> Result<Vec<PortableProxy>> {
        Ok(self.list_proxies()?.iter().map(PortableProxy::from).collect())
    }

    /// 导入一条代理：URL 已存在则更新 label，不存在则新增。返回是 Added 还是 Updated。
    pub fn import_proxy(&self, p: &PortableProxy) -> Result<ImportOutcome> {
        anyhow::ensure!(!p.url.is_empty(), "proxy URL must not be empty");
        let conn = self.conn.lock();
        let existing: Option<i64> = conn
            .query_row("SELECT id FROM proxies WHERE url = ?1", [&p.url], |r| r.get(0))
            .optional()?;
        match existing {
            Some(id) => {
                conn.execute("UPDATE proxies SET label = ?2 WHERE id = ?1", params![id, p.label])?;
                Ok(ImportOutcome::Updated)
            }
            None => {
                conn.execute(
                    "INSERT INTO proxies (label, url) VALUES (?1, ?2)",
                    params![p.label, p.url],
                )?;
                Ok(ImportOutcome::Added)
            }
        }
    }

    /// 可迁移的设置快照（`settings` 全表），**去掉管理密码**。
    ///
    /// 管理密码是「谁能进这台机器的控制台」，属于部署本身而不是被迁移的配置：把源站的口令
    /// 悄悄盖到目标站上，等于一次导入顺手改掉了目标站的登录方式，而做导入的人未必知道自己
    /// 改了这个。接入 key（[`CLIENT_API_KEY`]）反过来**要带**：它是客户端侧配好的东西，
    /// 迁移后不跟着走，所有客户端都得重配一遍。
    pub fn settings_snapshot(&self) -> HashMap<String, String> {
        let mut out = self.settings.read().clone();
        for k in CONSOLE_AUTH_KEYS {
            out.remove(*k);
        }
        out
    }

    /// 导入时按账号找目标行：同一个账号 UUID 下按组织 UUID 认。
    ///
    /// - 组织 UUID 两边都有且相等 → 就是它；
    /// - 导入的有组织 UUID、库里没有同组织的 → 退回库里这个账号下**唯一一条**组织 UUID 还空着的
    ///   行（旧号还没回填），有多条说不清是哪个就不认；
    /// - 导入的没有组织 UUID → 库里这个账号只有一行才认它，多行（个人 + 团队）说不清就不认。
    ///
    /// 不认的交给调用方按 refresh_token 兜底，再不中就新增——宁可多一行，也不要拿一个订阅的
    /// 状态覆盖掉另一个订阅。
    fn match_import_target(
        tx: &rusqlite::Transaction<'_>,
        account_uuid: &str,
        org_uuid: Option<&str>,
    ) -> Result<Option<i64>> {
        let mut stmt = tx.prepare(
            "SELECT id, NULLIF(TRIM(COALESCE(org_uuid, '')), '') FROM credentials
              WHERE account_uuid = ?1",
        )?;
        let rows: Vec<(i64, Option<String>)> = stmt
            .query_map([account_uuid], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let only = |v: Vec<i64>| if v.len() == 1 { Some(v[0]) } else { None };
        Ok(match org_uuid {
            Some(org) => {
                if let Some((id, _)) = rows.iter().find(|(_, o)| o.as_deref() == Some(org)) {
                    Some(*id)
                } else {
                    only(rows.iter().filter(|(_, o)| o.is_none()).map(|(id, _)| *id).collect())
                }
            }
            None => only(rows.iter().map(|(id, _)| *id).collect()),
        })
    }

    /// 导入一条凭证：目标库已有这个账号就整行覆盖，没有就新增。
    ///
    /// **匹配顺序是「账号 UUID + 组织 UUID」优先、`refresh_token` 兜底**，这个先后有实际后果：同一个
    /// 账号在源站重新授权过之后 refresh_token 已经是新值，只按 token 匹配会把它当成一个新
    /// 账号插进去，目标库里同一个账号出现两行（两行还会各自去刷新同一个上游账号）。反过来，
    /// 老库里可能有 `account_uuid` 还没拉到的号（profile 没取成功），故 token 这条兜底不能去。
    ///
    /// 只按账号 UUID 不够：同一个人可以既有个人订阅、又在团队里占一个席位，两次授权拿到的是
    /// **同一个** `account_uuid`、不同的 `org_uuid`，在库里是两行。只认账号的话，导入时后一行会
    /// 把前一行整行覆盖掉。见 [`Self::match_import_target`]。
    ///
    /// 命中后是**整行覆盖**而不是只更新 token：迁移文件是源站此刻的完整状态，优先级、设备
    /// 上限、代理这些都是操作者在源站上调好的。想保留目标站自己的调法，就别对已有的号做导入
    /// （或者导入后再调）——半覆盖半保留的规则说不清也记不住。
    pub fn import_credential(&self, c: &PortableCredential) -> Result<ImportOutcome> {
        if c.access_token.trim().is_empty() || c.refresh_token.trim().is_empty() {
            anyhow::bail!("credential has an empty access_token or refresh_token");
        }
        // 空串的 uuid 当没有：老库里存过空串，拿它去匹配会把所有这类号连成一个。
        let uuid = c.account_uuid.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let org = c.org_uuid.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let proxy = c.proxy.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let priority = c.priority.map_or(PRIORITY_DEFAULT, |p| p.clamp(PRIORITY_MIN, PRIORITY_MAX));
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let by_account = match uuid {
            Some(u) => Self::match_import_target(&tx, u, org)?,
            None => None,
        };
        let existing: Option<i64> = match by_account {
            Some(id) => Some(id),
            None => tx
                .query_row(
                    "SELECT id FROM credentials WHERE refresh_token = ?1",
                    [&c.refresh_token],
                    |r| r.get(0),
                )
                .optional()?,
        };
        let outcome = match existing {
            Some(id) => {
                tx.execute(
                    "UPDATE credentials SET
                         label = ?2, tier = ?3, org_type = ?4, access_token = ?5,
                         refresh_token = ?6, expires_at = ?7, priority = ?8, disabled = ?9,
                         device_limit = ?10, rpm_limit = ?11, ban_reason = ?12,
                         account_uuid = ?13, resume_at = ?14, proxy = ?15,
                         rate_limit_tier = ?16, org_uuid = ?17, subscription_created_at = ?18,
                         quota_pause_pct = ?19, quota_pause_pct_7d = ?20, session_limit = ?21,
                         org_name = ?22, seat_tier = ?23, subscription_status = ?24,
                         extra_usage_enabled = ?25, updated_at = unixepoch()
                     WHERE id = ?1",
                    params![
                        id,
                        c.label,
                        c.tier,
                        c.org_type,
                        c.access_token,
                        c.refresh_token,
                        c.expires_at as i64,
                        priority,
                        c.disabled as i64,
                        c.device_limit,
                        c.rpm_limit,
                        c.ban_reason,
                        uuid,
                        c.resume_at.map(|t| t as i64),
                        proxy,
                        c.rate_limit_tier,
                        c.org_uuid,
                        c.subscription_created_at,
                        c.quota_pause_pct,
                        c.quota_pause_pct_7d,
                        c.session_limit,
                        c.org_name,
                        c.seat_tier,
                        c.subscription_status,
                        c.extra_usage_enabled.map(i64::from),
                    ],
                )
                .context("failed to update the existing credential")?;
                ImportOutcome::Updated
            }
            None => {
                tx.execute(
                    "INSERT INTO credentials
                         (label, tier, org_type, access_token, refresh_token, expires_at,
                          priority, disabled, device_limit, rpm_limit, ban_reason,
                          account_uuid, resume_at, proxy, rate_limit_tier, org_uuid,
                          subscription_created_at, quota_pause_pct, quota_pause_pct_7d,
                          session_limit, org_name, seat_tier, subscription_status,
                          extra_usage_enabled)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                             ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24)",
                    params![
                        c.label,
                        c.tier,
                        c.org_type,
                        c.access_token,
                        c.refresh_token,
                        c.expires_at as i64,
                        priority,
                        c.disabled as i64,
                        c.device_limit,
                        c.rpm_limit,
                        c.ban_reason,
                        uuid,
                        c.resume_at.map(|t| t as i64),
                        proxy,
                        c.rate_limit_tier,
                        c.org_uuid,
                        c.subscription_created_at,
                        c.quota_pause_pct,
                        c.quota_pause_pct_7d,
                        c.session_limit,
                        c.org_name,
                        c.seat_tier,
                        c.subscription_status,
                        c.extra_usage_enabled.map(i64::from),
                    ],
                )
                .context("failed to insert the credential (its refresh_token may already exist)")?;
                ImportOutcome::Added
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// 导入设置：逐项写库并同步内存镜像，返回实际写入的项数。
    ///
    /// 管理密码一律跳过（口径同 [`Self::settings_snapshot`]，导出不带、导入也不认——万一有人
    /// 手工把它塞回文件里）。**只写文件里有的键**：目标库里多出来的设置保持原值，不做「以文件
    /// 为准清空其余」——那样一份手改过的、只留了几项的文件会把目标站其余配置全部重置成默认。
    pub fn import_settings(&self, settings: &HashMap<String, String>) -> Result<usize> {
        let mut n = 0;
        for (k, v) in settings {
            if CONSOLE_AUTH_KEYS.contains(&k.as_str()) {
                continue;
            }
            self.set_setting(k, v)?;
            n += 1;
        }
        Ok(n)
    }

    /// 设置停用状态（管理员手动开关）。
    ///
    /// 停用时立即清空其设备绑定，让已绑定设备的下一次请求马上改选其它凭证，
    /// 而不必等绑定 TTL 惰性过期；重新启用时清除 `ban_reason`（若之前是被自动停用）。
    ///
    /// **两个方向都清 `resume_at`**：手动开 = 立刻回调度池，不该再留着一个到点又要动它的
    /// 时间戳；手动关 = 管理员的意思是「关着」，绝不能被限流那套惰性恢复自己打开。
    /// 于是「限流自动停用」这一状态只可能由 [`Self::pause_for_rate_limit`] 产生，
    /// 任何一次人工干预都会把它降级成普通的手动状态。
    pub fn set_disabled(&self, id: i64, disabled: bool) -> Result<bool> {
        let conn = self.conn.lock();
        if disabled {
            conn.execute("DELETE FROM device_bindings WHERE cred_id = ?1", [id])?;
            conn.execute("DELETE FROM session_bindings WHERE cred_id = ?1", [id])?;
            // 两种暂停留下的原因也一并清掉，理由见 [`manual_disable_sql`]。
            Ok(conn.execute(&manual_disable_sql(), [id])? > 0)
        } else {
            Ok(conn.execute(
                "UPDATE credentials SET disabled = 0, ban_reason = NULL, resume_at = NULL, \
                        updated_at = unixepoch() \
                 WHERE id = ?1",
                [id],
            )? > 0)
        }
    }

    /// 上游确认限流（账号级 429）时调用：把这个号停用并记下**到点自动恢复的时刻**，
    /// 同时清空其设备绑定，让绑在它上面的设备下一条请求立刻改选别的号。
    ///
    /// 与 [`Self::record_ban`] 的唯一结构差别是多写一个 `resume_at`，而那正是
    /// 「限流暂停」与「封号/人工停用」的分界：`resume_at` 非空的号会被
    /// [`Self::resume_due`] 到点自动启用、也会被连通性测试成功时自动启用
    /// （见 [`Self::resume_if_rate_limited`]），另外两种则必须人工介入。
    ///
    /// 为什么落库而不是只记内存（原来的 [`RateLimitCooldown`] 做法）：额度耗尽动辄几小时到
    /// 几天，远长于一次进程重启；记内存则重启即忘，一重启就又拿这个号去撞一发 429。
    /// 落库之后重启也记得，代价是必须自己保证「到点恢复」不依赖进程一直活着——所以恢复做成
    /// 惰性的（选号时顺手扫一遍），而不是挂一个后台定时器。
    ///
    /// `reason` 直接写进 `ban_reason`，后台卡片原样展示，故调用方应带上人话的恢复时刻。
    ///
    /// 只动**启用中或已在限流暂停中**的号（见 [`Self::park_row`]）：先回来的那条把号封了 /
    /// 按订阅停了，后回来的 429 不能把它改写成「到点自己回来」。返回是否确有写入。
    pub fn pause_for_rate_limit(&self, id: i64, reason: &str, resume_at: u64) -> Result<bool> {
        self.park_row(id, reason, Some(resume_at as i64))
    }

    /// 两种自动暂停（[`Self::pause_for_rate_limit`]、[`Self::suspend_for_inactive_subscription`]）
    /// 共用的落库：停用、写原因与恢复时刻（`None` = 不会到点自己回来）、清绑定。
    ///
    /// 守卫只在这一处：只动**启用中或已在限流暂停中**的号，封号、人工停用、订阅未生效暂停
    /// 一概不碰——同一个号常有几条请求同时在飞，先回来的那条已经把号处置了，后回来的不能
    /// 改写它。返回是否确有写入。
    fn park_row(&self, id: i64, reason: &str, resume_at: Option<i64>) -> Result<bool> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let updated = tx.execute(
            "UPDATE credentials SET disabled = 1, ban_reason = ?2, resume_at = ?3, \
                    updated_at = unixepoch() \
             WHERE id = ?1 AND (disabled = 0 OR resume_at IS NOT NULL)",
            params![id, reason, resume_at],
        )? > 0;
        if updated {
            tx.execute("DELETE FROM device_bindings WHERE cred_id = ?1", [id])?;
            tx.execute("DELETE FROM session_bindings WHERE cred_id = ?1", [id])?;
        }
        tx.commit()?;
        Ok(updated)
    }

    /// 订阅未生效——付费档到期未续费、Free 档没订阅（见 [`crate::proxy::park_org_oauth_disallowed`]）：停调度、清绑定，
    /// **不写 `resume_at`**——不会到点自己回来，只有两条路放回池子：控制台手动启用
    /// （[`Self::set_disabled`]），或连通性测试通过（[`Self::resume_if_subscription_suspended`]）。
    ///
    /// 不写恢复时刻是有意的：`resume_at` 非空在选号那边的意思是「等一会就好」，全池都在等时
    /// 回 429 + 最早恢复时刻（见 [`Self::select_for_device`]）；这里等多久都没用，得有人去
    /// 续费或订阅，与封号同形（`disabled + ban_reason`，`resume_at` 空）。
    /// 与封号的区别只在原因文案（含 [`ORG_OAUTH_SUSPEND_MARKER`]）、不落封号事件，以及
    /// 连通性测试通过能放回来。
    ///
    /// 只动**启用中或限时暂停中**的号：人工停用、封禁、已经这样暂停的不碰——保活对人工停用的
    /// 号照发，不能让一发 403 改掉管理员的决定或覆盖封号原因。额度暂停中的号会被改成这一档：
    /// 额度回来了，没订阅照样不放行。
    ///
    /// 返回是否确有写入；`false` 即号已在池外（或不存在），调用方不必再记一遍。
    pub fn suspend_for_inactive_subscription(&self, id: i64, reason: &str) -> Result<bool> {
        self.park_row(id, reason, None)
    }

    /// 连通性测试通过时调用：若该号是 [`Self::suspend_for_inactive_subscription`] 停下的，当场恢复调度。
    /// 测试通过说明已经续费 / 订阅。认的是 luban 自己写的原因开头（[`is_subscription_pause_reason`] 同一口径），
    /// 封号、人工停用不受影响。返回是否确有恢复。
    pub fn resume_if_subscription_suspended(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute(
            &format!(
                "UPDATE credentials SET disabled = 0, ban_reason = NULL, resume_at = NULL, \
                    updated_at = unixepoch() \
             WHERE id = ?1 AND disabled = 1 AND resume_at IS NULL AND {SUBSCRIPTION_PAUSE_SQL}"
            ),
            [id],
        )? > 0)
    }

    /// 把所有「限流暂停且已到恢复时刻」的号重新启用，返回实际恢复的条数。
    ///
    /// 惰性执行（选号与列表各调一次，见 [`Self::select_for_device`]/[`Self::list`]），
    /// 和设备绑定的 TTL 过期同一套路子：不挂后台定时器，进程没在跑的时候也不需要它跑——
    /// 反正没人发请求。条件里的 `resume_at IS NOT NULL` 是关键，它保证只碰限流暂停的号，
    /// 封号与人工停用的不会被顺手打开。
    fn resume_due(conn: &Connection) -> Result<usize> {
        Ok(conn.execute(
            "UPDATE credentials SET disabled = 0, ban_reason = NULL, resume_at = NULL, \
                    updated_at = unixepoch() \
             WHERE disabled = 1 AND resume_at IS NOT NULL AND resume_at <= unixepoch()",
            [],
        )?)
    }

    /// 连通性测试通过时调用：若该号是被限流自动停用的（`resume_at` 非空），当场恢复调度。
    ///
    /// 测试成功是「上游此刻确实放这个号过」的一手证据，比我们从限流头算出来的恢复时刻更硬——
    /// 那个时刻偏保守时，好号会被白白晾着。返回是否确有恢复。
    ///
    /// 只认 `resume_at` 非空的号：人工关掉的号不该被一次连通性测试打开，那是管理员的决定。
    pub fn resume_if_rate_limited(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        Ok(conn.execute(
            "UPDATE credentials SET disabled = 0, ban_reason = NULL, resume_at = NULL, \
                    updated_at = unixepoch() \
             WHERE id = ?1 AND resume_at IS NOT NULL",
            [id],
        )? > 0)
    }

    /// 自动检测到上游账号级错误（如封号）时调用：停用凭证并记录原因，
    /// 同时清空其设备绑定，使下一次请求立即改选其它凭证。
    ///
    /// 与 [`Self::set_disabled`] 的区别在于会写入 `ban_reason`，供后台 UI 区分
    /// 「管理员手动停用」与「上游自动判定停用」。封号是需要人工介入的终态，不写
    /// `resume_at`（对比 [`Self::pause_for_rate_limit`]）。
    ///
    /// 这是 [`Self::record_ban`] 的简写：只有一句原因、没有别的上下文（事件来源记为
    /// `manual`）。**已弃用**：生产路径一律走 `record_ban`，把状态码、完整报文、请求 id
    /// 一并存进封号事件——此前保活循环拿它记 401/403，事件里只剩「upstream 401/403」，
    /// token 吊销、组织权限、区域限制与真封号分不开（见 `web::handle_keepalive_rejection`）。
    /// 保留为兼容入口并标 deprecated，新调用点编译时会被警告；测试里用它造「已封禁」状态。
    #[allow(dead_code)] // 兼容入口：本 crate 内只剩测试在用
    #[deprecated(
        since = "0.3.97",
        note = "走 record_ban 并带上 BanContext（状态码、错误正文、request id），别只留一句原因"
    )]
    pub fn mark_banned(&self, id: i64, reason: &str) -> Result<bool> {
        self.record_ban(
            id,
            &BanContext { reason: reason.to_string(), source: "manual", ..Default::default() },
        )
    }

    /// 设置优先级。范围由调用方（admin API）校验，这里不再截断。
    pub fn set_priority(&self, id: i64, priority: i64) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET priority = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, priority],
        )
    }

    /// 批量设置优先级：把 `ids` 里的账号统一改到 `priority`，返回实际更新的条数。
    /// 单事务内完成，避免中途失败留下一半新一半旧的调度档位。`ids` 为空时直接返回 0。
    pub fn set_priorities(&self, ids: &[i64], priority: i64) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE credentials SET priority = ?2, updated_at = unixepoch() WHERE id = ?1",
            )?;
            for id in ids {
                n += stmt.execute(params![id, priority])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 批量平移优先级：`ids` 里的账号各自在原值上加 `delta`（负数 = 提高），超出
    /// [`PRIORITY_MIN`]..=[`PRIORITY_MAX`] 的截到边界。选中账号之间的先后顺序不变
    /// （碰到边界的除外）。单事务，返回实际更新的条数。`ids` 里重复的只平移一次。
    pub fn shift_priorities(&self, ids: &[i64], delta: i64) -> Result<usize> {
        // 平移不是幂等的：同一个 id 出现两次就会被加两次 delta，先去重。
        let mut ids = ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE credentials SET priority = MIN(MAX(priority + ?2, ?3), ?4),
                     updated_at = unixepoch() WHERE id = ?1",
            )?;
            for id in &ids {
                n += stmt.execute(params![id, delta, PRIORITY_MIN, PRIORITY_MAX])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 批量删除：口径同 [`Self::remove`]，返回实际删除的条数（仅测试用）。
    #[cfg(test)]
    pub fn delete_many(&self, ids: &[i64]) -> Result<usize> {
        self.remove(ids)
    }

    /// 删号：账号行与挂在它上面的小表（绑定、账本、设备费用、模型拒绝）在同一个短事务里
    /// 删掉，返回实际删除的账号数。
    ///
    /// **用量流水不删**，留给 [`Self::prune_usage_logs`] 按保留期自然裁掉。此前删号时
    /// 顺手把这个号的流水也清掉，一个号保留期内的流水动辄几万行，表又宽、挂着十来条索引，
    /// 分批删也要连着几分钟和转发抢 `conn` 这把锁，删号期间整个后台和转发都跟着慢。而留着
    /// 这些行并没有害处：
    /// - 账号 id 自增不复用（见 migrates_and_stops_id_reuse），不会被记到新号头上；
    /// - 每行自带 `cred_label`，请求日志、按账号拆分照样显示得出是哪个号；
    /// - 账号列表的费用/最近使用走账本（这里一并删了），选号与 RPM 只看在册账号。
    ///
    /// 全局口径的统计（总览、请求日志、按账号拆分）因此会带上已删账号保留期内的用量——
    /// 那些请求确实发生过、钱也确实花了，算进去才对得上账。
    pub fn remove(&self, ids: &[i64]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut binds = tx.prepare("DELETE FROM device_bindings WHERE cred_id = ?1")?;
            let mut sbinds = tx.prepare("DELETE FROM session_bindings WHERE cred_id = ?1")?;
            let mut stats = tx.prepare("DELETE FROM credential_stats WHERE cred_id = ?1")?;
            let mut costs = tx.prepare("DELETE FROM device_costs WHERE cred_id = ?1")?;
            let mut denials = tx.prepare("DELETE FROM model_denials WHERE cred_id = ?1")?;
            let mut cred = tx.prepare("DELETE FROM credentials WHERE id = ?1")?;
            for id in ids {
                binds.execute([id])?;
                sbinds.execute([id])?;
                stats.execute([id])?;
                costs.execute([id])?;
                denials.execute([id])?;
                n += cred.execute([id])?;
            }
        }
        tx.commit()?;
        // 号没了，它们在内存里的限流窗口与冷却也留着没用（id 不会被复用，见
        // migrates_and_stops_id_reuse）。
        for id in ids {
            self.bare_rate.forget(id);
            self.rpm_rate.forget(id);
            self.cooldown.forget(*id);
        }
        Ok(n)
    }

    /// 批量启停：语义与 [`Self::set_disabled`] 一致（停用时清设备绑定使其立即改选其它
    /// 凭证；启用时清 `ban_reason`），返回实际更新的条数。单事务内完成。
    pub fn set_disabled_many(&self, ids: &[i64], disabled: bool) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            if disabled {
                let mut binds = tx.prepare("DELETE FROM device_bindings WHERE cred_id = ?1")?;
                let mut sbinds = tx.prepare("DELETE FROM session_bindings WHERE cred_id = ?1")?;
                // 同 `set_disabled`：人工操作两个方向都清 `resume_at`，
                // 限流那套惰性恢复不该越过管理员的决定。
                // 两种暂停留下的原因也一并清掉，理由见 [`manual_disable_sql`]。
                let mut stmt = tx.prepare(&manual_disable_sql())?;
                for id in ids {
                    binds.execute([id])?;
                    sbinds.execute([id])?;
                    n += stmt.execute([id])?;
                }
            } else {
                let mut stmt = tx.prepare(
                    "UPDATE credentials SET disabled = 0, ban_reason = NULL, resume_at = NULL, \
                     updated_at = unixepoch() WHERE id = ?1",
                )?;
                for id in ids {
                    n += stmt.execute([id])?;
                }
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 批量设置设备数上限（三态语义同 [`Self::set_device_limit`]），返回实际更新的条数。
    pub fn set_device_limits(&self, ids: &[i64], limit: i64) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE credentials SET device_limit = ?2, updated_at = unixepoch() WHERE id = ?1",
            )?;
            for id in ids {
                n += stmt.execute(params![id, limit])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 设置该账号的设备数上限。三态：`> 0` 本账号独立上限；`0` 跟随全局默认
    /// （见 [`DEFAULT_DEVICE_LIMIT`]）；`< 0` 本账号明确不限（不受全局默认约束）。
    pub fn set_device_limit(&self, id: i64, limit: i64) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET device_limit = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, limit],
        )
    }

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

    /// 批量设置模拟会话数上限（三态语义同 [`Self::set_session_limit`]），返回实际更新的条数。
    pub fn set_session_limits(&self, ids: &[i64], limit: i64) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE credentials SET session_limit = ?2, updated_at = unixepoch() WHERE id = ?1",
            )?;
            for id in ids {
                n += stmt.execute(params![id, limit])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 设置模拟会话数上限，返回是否确有更新。三态同 [`Self::set_device_limit`]：`> 0` 本账号
    /// 独立上限；`0` 跟随全局默认（[`DEFAULT_SESSION_LIMIT`]）；`< 0` 本账号明确不限。
    pub fn set_session_limit(&self, id: i64, limit: i64) -> Result<bool> {
        let n = self.conn.lock().execute(
            "UPDATE credentials SET session_limit = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, limit],
        )?;
        Ok(n > 0)
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

    /// 批量设置账号自己的提前停调度阈值（两档整份覆盖）；三态同 [`Self::set_quota_pause_pcts`]。
    pub fn set_quota_pause_pcts_many(
        &self,
        ids: &[i64],
        short_pct: Option<i64>,
        long_pct: Option<i64>,
    ) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let short_pct = short_pct.map(|p| p.clamp(0, 100));
        let long_pct = long_pct.map(|p| p.clamp(0, 100));
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE credentials SET quota_pause_pct = ?2, quota_pause_pct_7d = ?3, \
                 updated_at = unixepoch() WHERE id = ?1",
            )?;
            for id in ids {
                n += stmt.execute(params![id, short_pct, long_pct])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 批量设置账号 RPM 上限；三态同 [`Self::set_rpm_limit`]。
    pub fn set_rpm_limits(&self, ids: &[i64], limit: i64) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE credentials SET rpm_limit = ?2, updated_at = unixepoch() WHERE id = ?1",
            )?;
            for id in ids {
                n += stmt.execute(params![id, limit])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    // ---------- 代理池 ----------

    /// 列出代理池中所有记录。
    pub fn list_proxies(&self) -> Result<Vec<SavedProxy>> {
        let conn = self.conn.lock();
        let mut stmt =
            conn.prepare("SELECT id, label, url, created_at FROM proxies ORDER BY id ASC")?;
        let rows = stmt.query_map([], |row| {
            Ok(SavedProxy {
                id: row.get(0)?,
                label: row.get(1)?,
                url: row.get(2)?,
                created_at: row.get::<_, i64>(3)? as u64,
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }

    /// 读取代理池中的单条记录。
    pub fn get_proxy(&self, id: i64) -> Result<Option<SavedProxy>> {
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT id, label, url, created_at FROM proxies WHERE id = ?1",
            [id],
            |row| {
                Ok(SavedProxy {
                    id: row.get(0)?,
                    label: row.get(1)?,
                    url: row.get(2)?,
                    created_at: row.get::<_, i64>(3)? as u64,
                })
            },
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            other => Err(other.into()),
        })
    }

    /// 确保代理在池中存在：不在则自动添加（label 取 host:port），已在则忽略。
    pub fn ensure_proxy_in_pool(&self, url: &str) {
        let conn = self.conn.lock();
        let label = url_to_label(url);
        if let Err(e) = conn.execute(
            "INSERT OR IGNORE INTO proxies (label, url) VALUES (?1, ?2)",
            params![label, url],
        ) {
            tracing::debug!(error = %e, url, "ensure_proxy_in_pool: insert ignored");
        }
    }

    /// 添加一条代理到池中，返回新记录。`url` 应已经过 `crate::clients::validate_proxy` 校验。
    pub fn add_proxy(&self, label: &str, url: &str) -> Result<SavedProxy> {
        let conn = self.conn.lock();
        conn.execute("INSERT INTO proxies (label, url) VALUES (?1, ?2)", params![label, url])
            .context("failed to add proxy (the URL may already exist in the pool)")?;
        let id = conn.last_insert_rowid();
        conn.query_row(
            "SELECT id, label, url, created_at FROM proxies WHERE id = ?1",
            [id],
            |row| {
                Ok(SavedProxy {
                    id: row.get(0)?,
                    label: row.get(1)?,
                    url: row.get(2)?,
                    created_at: row.get::<_, i64>(3)? as u64,
                })
            },
        )
        .context("failed to read the newly inserted proxy")
    }

    /// 批量添加代理，单事务内完成；返回与入参一一对应的结果，地址已在池里（唯一索引撞了）的
    /// 那条是 `None`，不报错也不影响其它条。`url` 应已经过 `crate::clients::validate_proxy` 校验。
    pub fn add_proxies(&self, items: &[(String, String)]) -> Result<Vec<Option<SavedProxy>>> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut out = Vec::with_capacity(items.len());
        {
            let mut insert =
                tx.prepare("INSERT OR IGNORE INTO proxies (label, url) VALUES (?1, ?2)")?;
            let mut read =
                tx.prepare("SELECT id, label, url, created_at FROM proxies WHERE id = ?1")?;
            for (label, url) in items {
                if insert.execute(params![label, url])? == 0 {
                    out.push(None);
                    continue;
                }
                let id = tx.last_insert_rowid();
                out.push(Some(read.query_row([id], |row| {
                    Ok(SavedProxy {
                        id: row.get(0)?,
                        label: row.get(1)?,
                        url: row.get(2)?,
                        created_at: row.get::<_, i64>(3)? as u64,
                    })
                })?));
            }
        }
        tx.commit()?;
        Ok(out)
    }

    /// 更新代理池中一条记录的名称和/或地址。
    pub fn update_proxy(&self, id: i64, label: &str, url: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "UPDATE proxies SET label = ?2, url = ?3 WHERE id = ?1",
            params![id, label, url],
        )?;
        Ok(n > 0)
    }

    /// 从池中删除一条代理（不影响已配置该代理的凭证）。
    pub fn delete_proxy(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute("DELETE FROM proxies WHERE id = ?1", [id])?;
        Ok(n > 0)
    }

    /// 批量删除代理池记录，单事务内完成；返回实际删掉的条数（不存在的 id 不计）。
    /// 与 [Self::delete_proxy] 一样只动代理池，不改凭证上的代理设置。
    pub fn delete_proxies(&self, ids: &[i64]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare("DELETE FROM proxies WHERE id = ?1")?;
            for id in ids {
                n += stmt.execute([id])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 统计每个代理地址有多少凭证在使用。键是代理 URL，值是使用该 URL 的凭证数量。
    pub fn proxy_usage_counts(&self) -> Result<HashMap<String, i64>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT proxy, COUNT(*) FROM credentials \
             WHERE proxy IS NOT NULL AND proxy != '' GROUP BY proxy",
        )?;
        let rows =
            stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?;
        let mut out = HashMap::new();
        for r in rows {
            let (url, count) = r?;
            out.insert(url, count);
        }
        Ok(out)
    }

    /// 返回每个代理 URL 对应的使用者标签列表（`proxy_url → [label1, label2, ...]`）。
    pub fn proxy_usage_labels(&self) -> Result<HashMap<String, Vec<String>>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT proxy, label FROM credentials \
             WHERE proxy IS NOT NULL AND proxy != '' ORDER BY proxy, label",
        )?;
        let rows =
            stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?;
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for r in rows {
            let (url, label) = r?;
            out.entry(url).or_default().push(label);
        }
        Ok(out)
    }

    /// 批量设置出站代理：把 `ids` 里的账号统一改到 `proxy`（`None` 或空串改回直连）。
    /// 单事务内完成。
    pub fn set_proxies(&self, ids: &[i64], proxy: Option<&str>) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let proxy = proxy.map(str::trim).filter(|s| !s.is_empty());
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut stmt = tx.prepare(
                "UPDATE credentials SET proxy = ?2, updated_at = unixepoch() WHERE id = ?1",
            )?;
            for id in ids {
                n += stmt.execute(params![id, proxy])?;
            }
        }
        tx.commit()?;
        Ok(n)
    }

    /// 设置该账号每分钟最多转发多少条请求。三态同设备上限：`> 0` 本账号独立上限；
    /// `0` 跟随全局默认（见 [`DEFAULT_RPM_LIMIT`]）；`< 0` 本账号明确不限。
    ///
    /// 计数在进程内存里（见 [`RateWindow`]），改完即时生效，不影响已经记在窗口里的那些。
    pub fn set_rpm_limit(&self, id: i64, limit: i64) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET rpm_limit = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, limit],
        )
    }

    /// 设置该账号自己的「额度用到多少就提前停调度」阈值（5h / 7d 两档，百分比）。
    /// 每档 `None` = 跟随全局、`Some(0)` = 本账号这一档不停、`Some(1..=100)` = 独立阈值；
    /// 取值夹到 `0..=100`。生效值见 [`effective_quota_pause_pct`]，判定在
    /// `crate::proxy::park_if_quota_nearly_exhausted`——下一条带限流头的响应起生效。
    pub fn set_quota_pause_pcts(
        &self,
        id: i64,
        short_pct: Option<i64>,
        long_pct: Option<i64>,
    ) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET quota_pause_pct = ?2, quota_pause_pct_7d = ?3, \
             updated_at = unixepoch() WHERE id = ?1",
            params![id, short_pct.map(|p| p.clamp(0, 100)), long_pct.map(|p| p.clamp(0, 100))],
        )
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

    /// 给这台设备记一条转发；名额已满时不记，返回**建议等待的秒数**（窗口里最早那条滚出去
    /// 的时刻）。上限未配置时恒为 `None`（不限，且一条都不记）。
    ///
    /// 与账号 RPM 刻意不同的两点：
    /// - **不参与选号**。账号打满可以换个号发，设备打满换哪个号都是同一台机器在刷，故这道闸
    ///   在代理入口独立判定、直接 429，不进 [`Self::select_for_device`]（那里换号是为了绕开
    ///   一个满了的号，对设备维度没有意义，只会白白改绑设备）。
    /// - **问与记在同一把锁里**（[`RateWindow::try_take`]）。选号那两道窗口靠 `conn` 锁串行，
    ///   这里没有那把锁，同一台设备的并发请求必须自己防住「都看到最后一个名额」。
    ///
    /// 口径与账号 RPM 一致：**含失败的、含 `count_tokens`**，两个数才比得了。代价同样一致——
    /// 记在这里的是「获准转发」的条数，上游若把它拒了也照算。
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

    /// 给这个会话记一条转发；名额已满时不记，返回建议等待的秒数。口径、锁的粒度、以及
    /// 「不参与选号」这三点都与 [`Self::take_device_rpm_slot`] 完全一致——差别只在分桶的键。
    ///
    /// 键的清扫比设备维度更要紧：设备 id 一台机器一个、长期不变，会话 id 每 `/clear`、每个
    /// 新窗口都是一个新值，正常使用下就在稳定产生。故阈值单列（[`SESSION_RATE_MAX_KEYS`]）
    /// 而不是复用设备那个。
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

    /// 这台设备是否已有绑定记录（任一凭证、不论是否仍在 TTL 内）。有记录就是「见过的设备」；
    /// 绑定被保留期清掉后它会再次被当成没见过，那时它也确实很久没来了。供探针判定
    /// （`proxy::probe_signature`）区分「老设备的一条小请求」与「凭空冒出来的一次性会话」。
    pub fn device_is_known(&self, device_id: &str) -> bool {
        self.conn
            .lock()
            .query_row(
                "SELECT 1 FROM device_bindings WHERE device_id = ?1 LIMIT 1",
                [device_id],
                |_| Ok(()),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
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

    /// 给凭证打上「被上游限流」的冷却，见 [`RateLimitCooldown`]。时长与作用域都由调用方
    /// 从上游响应头算出（`crate::proxy::rate_limit_scope`）：`model` 为 `None` 即账号级
    /// （额度真耗尽），`Some(m)` 即只冷却该模型（窗口没跑满却被拒，多半是模型容量限制）。
    pub fn mark_rate_limited(&self, cred_id: i64, model: Option<&str>, dur: Duration) {
        self.cooldown.mark(cred_id, model, dur);
    }

    /// 该凭证**账号级**冷却的剩余秒数（未冷却为 0）。见 [`RateLimitCooldown::remaining_secs`]，
    /// 注意正常路径上账号级限流走的是落库的 `resume_at`，这一档只反映落库失败的兜底状态。
    pub fn rate_limited_secs(&self, cred_id: i64) -> i64 {
        self.cooldown.remaining_secs(cred_id)
    }

    /// 该凭证**模型级**冷却的明细 `(模型名, 剩余秒数, 是否挡选号)`，未冷却为空。
    ///
    /// 这一档都不影响账号整体调度：其余模型照常可用，见 [`RateLimitCooldown`]。第三项（gated）
    /// 现在总为 `true`——额度池满和瞬时限速两档都走门禁，区别在于持续时间：瞬时限速的 gate
    /// 从 2s 起步（ladder 退避），远短于额度池满那一档。
    pub fn rate_limited_models(&self, cred_id: i64) -> Vec<(String, i64, bool)> {
        self.cooldown.model_remaining(cred_id)
    }

    /// 解除该凭证的限流冷却，见 [`RateLimitCooldown::clear`]：`Some(model)` 清账号级 +
    /// 该模型格（连通性测试成功照真实判决恢复），`None` 清全部格（后台手动解除）。
    pub fn clear_rate_limited(&self, cred_id: i64, model: Option<&str>) {
        self.cooldown.clear(cred_id, model);
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

    /// 单条凭证当前**占名额**的设备数：已排除超过 TTL 未活跃的绑定（与选路时判上限的口径
    /// 一致），故后台显示会随时间自然回落，不必等下一次请求触发 sweep。
    /// TTL `<= 0`（永不过期）时按全量计。
    ///
    /// 数不到休眠中的软绑定是有意的：它们不占名额，只是还记着「这台设备上次用的是这个号」
    /// （见 [`Self::select_for_device`]），列进来会让「设备 x/y」这个名额口径失真。
    pub fn device_count(&self, cred_id: i64) -> Result<i64> {
        let ttl = self.device_binding_ttl();
        let conn = self.conn.lock();
        let n = if ttl > 0 {
            conn.query_row(
                "SELECT COUNT(*) FROM device_bindings \
                 WHERE cred_id = ?1 AND last_seen_at >= unixepoch() - ?2",
                params![cred_id, ttl],
                |r| r.get(0),
            )?
        } else {
            conn.query_row(
                "SELECT COUNT(*) FROM device_bindings WHERE cred_id = ?1",
                [cred_id],
                |r| r.get(0),
            )?
        };
        Ok(n)
    }

    /// 单条凭证当前**有效**绑定的设备明细（含费用），按最近活跃倒序。
    ///
    /// **真实绑定**那部分的过滤口径与 [`Self::device_count`] 完全一致（同一个 TTL），否则后台
    /// 会出现「设备数写着 2、展开却列出 5 条」这种自相矛盾的展示。末尾追加的模拟伪设备
    /// （`simulated` 为真）**不在这个口径内**——它们不写绑定、不占名额，故 `device_count`
    /// 数不到它们，两者本就不该相等。前端要显示「设备数」时只能数 `!simulated` 那些。
    ///
    /// 费用来自 `device_costs` 账本（写日志时同事务累加），与绑定表是两套账，刻意不合并：
    /// 绑定行会被解绑/停用/TTL 清掉并从零重新计数，账本则终身累计。所以「本账号费用」
    /// 覆盖的时间范围可能比 `request_count` 长——它统计的是这台设备历史上经本账号花掉的钱，
    /// 而不是「本次绑定期间」。同时给出跨账号合计，便于识别换号仍在持续烧钱的同一台设备。
    pub fn list_devices(&self, cred_id: i64) -> Result<Vec<DeviceBinding>> {
        let ttl = self.device_binding_ttl();
        let conn = self.read_conn();
        let ttl_clause = if ttl > 0 { "AND b.last_seen_at >= unixepoch() - ?2" } else { "" };
        let sql = format!(
            "SELECT b.device_id, b.request_count, b.created_at, b.last_seen_at, \
                    COALESCE((SELECT dc.cost_usd FROM device_costs dc \
                               WHERE dc.cred_id = b.cred_id AND dc.device_id = b.device_id), 0), \
                    COALESCE((SELECT SUM(dc.cost_usd) FROM device_costs dc \
                               WHERE dc.device_id = b.device_id), 0) \
               FROM device_bindings b \
              WHERE b.cred_id = ?1 {ttl_clause} ORDER BY b.last_seen_at DESC, b.device_id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let map_row = |r: &Row| {
            Ok(DeviceBinding {
                device_id: r.get(0)?,
                request_count: r.get(1)?,
                created_at: r.get(2)?,
                last_seen_at: r.get(3)?,
                cost_usd: r.get(4)?,
                cost_usd_all: r.get(5)?,
                simulated: false,
            })
        };
        let mut rows: Vec<DeviceBinding> = if ttl > 0 {
            stmt.query_map(params![cred_id, ttl], map_row)?.collect::<rusqlite::Result<_>>()?
        } else {
            stmt.query_map([cred_id], map_row)?.collect::<rusqlite::Result<_>>()?
        };
        drop(stmt);

        // 模拟客户端的伪设备：它们不写绑定（故上面那条 SQL 一条都查不到），但用量与费用
        // 照常落进 `device_costs`。不接 TTL——那是绑定的过期规则，这里没有绑定可过期。
        // 排在真实设备之后：真实设备是「谁在用这个号」的主线，伪设备是一条汇总。
        let mut sim = conn.prepare(
            "SELECT dc.device_id, dc.request_count, dc.cost_usd, \
                    COALESCE((SELECT SUM(d2.cost_usd) FROM device_costs d2 \
                               WHERE d2.device_id = dc.device_id), 0) \
               FROM device_costs dc \
              WHERE dc.cred_id = ?1 AND dc.device_id LIKE 'sim:%' \
              ORDER BY dc.request_count DESC, dc.device_id ASC",
        )?;
        let sim_rows = sim.query_map([cred_id], |r| {
            Ok(DeviceBinding {
                device_id: r.get(0)?,
                request_count: r.get(1)?,
                created_at: None,
                last_seen_at: None,
                cost_usd: r.get(2)?,
                cost_usd_all: r.get(3)?,
                simulated: true,
            })
        })?;
        for row in sim_rows {
            rows.push(row?);
        }
        Ok(rows)
    }

    /// 手动解除一条设备绑定，返回是否确有删除。
    ///
    /// 按 `(cred_id, device_id)` 双条件删除，而不是只按 `device_id`：后台拿到的设备列表可能
    /// 已经过期（设备刚被换到别的号上），只按 device_id 删会把它从**当前**所在账号上摘掉。
    ///
    /// 不受绑定 TTL 影响：TTL 外那些休眠的软绑定虽然不占名额，但还留着亲和性，解绑就是要把
    /// 这份记忆一并抹掉（下次来当新设备重新分号）。明细按 TTL 过滤，后台能点到的必然是活跃
    /// 绑定，休眠那些只能等保留期到点自己消失。
    pub fn unbind_device(&self, cred_id: i64, device_id: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "DELETE FROM device_bindings WHERE cred_id = ?1 AND device_id = ?2",
            params![cred_id, device_id],
        )?;
        Ok(n > 0)
    }

    /// 所有凭证当前**有效**绑定的设备数（cred_id → count）；口径同 [`Self::device_count`]，
    /// 排除超过 TTL 未活跃的绑定。TTL `<= 0` 时按全量计。
    pub fn device_counts(&self) -> Result<HashMap<i64, i64>> {
        let ttl = self.device_binding_ttl();
        let conn = self.read_conn();
        let mut stmt = conn.prepare(&active_counts_sql("device_bindings", ttl))?;
        let map_row = |r: &Row| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?));
        let rows =
            if ttl > 0 { stmt.query_map([ttl], map_row)? } else { stmt.query_map([], map_row)? };
        let mut out = HashMap::new();
        for row in rows {
            let (cid, n) = row?;
            out.insert(cid, n);
        }
        Ok(out)
    }

    /// 单条凭证当前**占名额**的模拟会话数；口径同 [`Self::device_count`]（TTL 内活跃的绑定）。
    pub fn session_count(&self, cred_id: i64) -> Result<i64> {
        let ttl = self.session_binding_ttl();
        let conn = self.conn.lock();
        let n = if ttl > 0 {
            conn.query_row(
                "SELECT COUNT(*) FROM session_bindings \
                 WHERE cred_id = ?1 AND last_seen_at >= unixepoch() - ?2",
                params![cred_id, ttl],
                |r| r.get(0),
            )?
        } else {
            conn.query_row(
                "SELECT COUNT(*) FROM session_bindings WHERE cred_id = ?1",
                [cred_id],
                |r| r.get(0),
            )?
        };
        Ok(n)
    }

    /// 所有凭证当前**有效**的模拟会话绑定数（cred_id → count）；口径同 [`Self::session_count`]。
    pub fn session_counts(&self) -> Result<HashMap<i64, i64>> {
        let ttl = self.session_binding_ttl();
        let conn = self.read_conn();
        let mut stmt = conn.prepare(&active_counts_sql("session_bindings", ttl))?;
        let map_row = |r: &Row| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?));
        let rows =
            if ttl > 0 { stmt.query_map([ttl], map_row)? } else { stmt.query_map([], map_row)? };
        let mut out = HashMap::new();
        for row in rows {
            let (cid, n) = row?;
            out.insert(cid, n);
        }
        Ok(out)
    }

    /// 单条凭证当前**有效**的模拟会话明细，按最近活跃倒序；过滤口径与 [`Self::session_count`]
    /// 完全一致，否则后台会出现「会话数写着 2、展开却列出 5 条」。
    pub fn list_sessions(&self, cred_id: i64) -> Result<Vec<SessionBinding>> {
        let ttl = self.session_binding_ttl();
        let conn = self.read_conn();
        // 会话 id 要账号 uuid 才算得出；凭证不存在时按空 uuid 算（调用方已先判过 404）。
        let account_uuid: Option<String> = conn
            .query_row("SELECT account_uuid FROM credentials WHERE id = ?1", [cred_id], |r| {
                r.get(0)
            })
            .optional()?
            .flatten();
        let ttl_clause = if ttl > 0 { "AND last_seen_at >= unixepoch() - ?2" } else { "" };
        let sql = format!(
            "SELECT session_key, slot, request_count, created_at, last_seen_at, last_model \
               FROM session_bindings \
              WHERE cred_id = ?1 {ttl_clause} ORDER BY last_seen_at DESC, session_key ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let map_row = |r: &Row| {
            let session_key: String = r.get(0)?;
            let slot: i64 = r.get(1)?;
            // 真实客户端的会话键带来访会话 id 时（`lb:v2:sid:<id>`）取最后一段按账号钉住；钉不出来
            // （号没有 account_uuid）时上游收到的就是来访原值。按前缀指纹分的（`…:pfx:…`）来访
            // 根本没带会话 id，出站也就没有，留空。
            let session_id = if slot < 0 {
                match session_key.rsplit_once(':') {
                    Some((head, sid)) if head.ends_with(":sid") => {
                        crate::credentials::pinned_session_id(account_uuid.as_deref(), sid)
                            .unwrap_or_else(|| sid.to_string())
                    }
                    _ => String::new(),
                }
            } else {
                crate::credentials::sim_slot_session_id(account_uuid.as_deref(), cred_id, slot)
            };
            Ok(SessionBinding {
                session_key,
                slot,
                session_id,
                request_count: r.get(2)?,
                created_at: r.get(3)?,
                last_seen_at: r.get(4)?,
                last_model: r.get(5)?,
            })
        };
        let rows = if ttl > 0 {
            stmt.query_map(params![cred_id, ttl], map_row)?.collect::<rusqlite::Result<_>>()?
        } else {
            stmt.query_map([cred_id], map_row)?.collect::<rusqlite::Result<_>>()?
        };
        Ok(rows)
    }

    /// 这条会话键在该凭证上占的槽位；没绑在这个号上为 `None`。转发路径不再用它——槽位随
    /// 选号结果一起返回（[`Self::select_with_slot`]），这里只给测试核对绑定行。
    #[cfg(test)]
    pub fn session_slot(&self, cred_id: i64, session_key: &str) -> Result<Option<i64>> {
        Ok(self
            .conn
            .lock()
            .query_row(
                "SELECT slot FROM session_bindings WHERE session_key = ?1 AND cred_id = ?2",
                params![session_key, cred_id],
                |r| r.get(0),
            )
            .optional()?)
    }

    /// 一键清掉该凭证的**全部**模拟会话绑定（含休眠的软绑定），返回删掉的条数。会话比设备
    /// 多得多、又是 luban 自己派生的键，逐条解绑不现实；清掉只是腾名额、抹亲和性，下一条请求
    /// 照常重新选号。
    pub fn unbind_all_sessions(&self, cred_id: i64) -> Result<usize> {
        let conn = self.conn.lock();
        Ok(conn.execute("DELETE FROM session_bindings WHERE cred_id = ?1", [cred_id])?)
    }

    /// 手动解除一条模拟会话绑定，返回是否确有删除。按 `(cred_id, session_key)` 双条件删，
    /// 理由同 [`Self::unbind_device`]。
    pub fn unbind_session(&self, cred_id: i64, session_key: &str) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(
            "DELETE FROM session_bindings WHERE cred_id = ?1 AND session_key = ?2",
            params![cred_id, session_key],
        )?;
        Ok(n > 0)
    }

    /// 更新账号等级。
    /// 写回组织类型（`claude_team` 等）。与 [`Self::set_tier`] 分开：等级会随额度档变，
    /// 组织类型只在换账号时才变，两者的来源虽同是 profile，语义不是一回事。
    pub fn set_org_type(&self, id: i64, org_type: Option<&str>) -> Result<bool> {
        Ok(self.conn.lock().execute(
            "UPDATE credentials SET org_type = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, org_type],
        )? > 0)
    }

    /// 写回额度档原值（`default_claude_max_5x` 之类）。
    /// 见 [`crate::credentials::Credential::rate_limit_tier`]。
    pub fn set_rate_limit_tier(&self, id: i64, raw: Option<&str>) -> Result<bool> {
        Ok(self.conn.lock().execute(
            "UPDATE credentials SET rate_limit_tier = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, raw],
        )? > 0)
    }

    /// 把一份刚拉到的 profile 写回凭证：等级、账号 UUID、组织类型、额度档原值、组织 UUID、
    /// 订阅创建时刻。**每一项只在 profile 给了值时才写**——profile 缺项不能把库里已有的清掉。
    /// `fallback_org_uuid` 是 profile 没给组织 id 时的兜底（交换响应里那个），官方同一次序。
    ///
    /// 三条路共用：登录、手动刷新、**自动刷新**（[`ensure_fresh_token`]）。此前只有前两条写
    /// profile 字段，自动刷新只换 token，于是「旧库刷新一次即回填」对绝大多数号——它们只会
    /// 被自动刷新——根本不成立。
    pub fn apply_profile(
        &self,
        id: i64,
        profile: &crate::oauth::Profile,
        fallback_org_uuid: Option<&str>,
    ) -> Result<()> {
        if profile.tier.is_some() {
            self.set_tier(id, profile.tier.as_deref())?;
        }
        if let Some(uuid) = profile.account_uuid.as_deref() {
            self.set_account_uuid(id, uuid)?;
        }
        if profile.org_type.is_some() {
            self.set_org_type(id, profile.org_type.as_deref())?;
        }
        if profile.rate_limit_tier.is_some() {
            self.set_rate_limit_tier(id, profile.rate_limit_tier.as_deref())?;
        }
        let org_uuid = profile.org_uuid.as_deref().or(fallback_org_uuid);
        if org_uuid.is_some() {
            self.set_org_uuid(id, org_uuid)?;
        }
        if profile.subscription_created_at.is_some() {
            self.set_subscription_created_at(id, profile.subscription_created_at.as_deref())?;
        }
        // 只给后台看的几列：拉到就整组覆盖（席位档个人号本来就没有，缺了要写回空）。
        // 至少拿到组织名称才算这一组有效，免得一份残缺的响应把已有的值清掉。
        if profile.org_name.is_some() {
            self.conn.lock().execute(
                "UPDATE credentials SET org_name = ?2, seat_tier = ?3, subscription_status = ?4,
                        extra_usage_enabled = ?5, updated_at = unixepoch()
                  WHERE id = ?1",
                params![
                    id,
                    profile.org_name,
                    profile.seat_tier,
                    profile.subscription_status,
                    profile.extra_usage_enabled.map(i64::from),
                ],
            )?;
        }
        Ok(())
    }

    /// 写回组织 UUID。见 [`crate::credentials::Credential::org_uuid`]。
    pub fn set_org_uuid(&self, id: i64, org_uuid: Option<&str>) -> Result<bool> {
        Ok(self.conn.lock().execute(
            "UPDATE credentials SET org_uuid = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, org_uuid],
        )? > 0)
    }

    /// 写回订阅创建时刻原串。见 [`crate::credentials::Credential::subscription_created_at`]。
    pub fn set_subscription_created_at(&self, id: i64, raw: Option<&str>) -> Result<bool> {
        Ok(self.conn.lock().execute(
            "UPDATE credentials SET subscription_created_at = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, raw],
        )? > 0)
    }

    /// 写回账号等级。**等级变了就把它的模型准入记录全清掉**：那些记录是在旧套餐下学到的
    /// （Pro 号不含 fable），升级到 Max 后再留着就等于把新买的额度锁在门外。
    pub fn set_tier(&self, id: i64, tier: Option<&str>) -> Result<bool> {
        let conn = self.conn.lock();
        let previous: Option<Option<String>> = conn
            .query_row("SELECT tier FROM credentials WHERE id = ?1", [id], |r| r.get(0))
            .optional()?;
        let Some(previous) = previous else { return Ok(false) };
        if previous.as_deref() != tier {
            conn.execute("DELETE FROM model_denials WHERE cred_id = ?1", [id])?;
        }
        Ok(conn.execute(
            "UPDATE credentials SET tier = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, tier],
        )? > 0)
    }

    // ---------- 模型准入（套餐不含某模型） ----------

    /// 记下「这个号用不了这个模型」。
    ///
    /// 依据是上游的 429 形态：一个额度窗口头都不带、只带 `overage-disabled-reason`——说明
    /// 这个模型根本不在该套餐的任何额度窗口里，要走按量计费的 usage credits，而组织又没开。
    /// 这与「超额池满」（`7d_oi` rejected，账号确实有 fable 额度只是用完了）是两回事，后者走
    /// 进程内冷却，见 [`RateLimitCooldown`]。
    ///
    /// `expires_at` 取上游给的 `unified-reset`（credits 的月度窗口）：到点让它自动失效、
    /// 下一条请求再去试一次——用户中途开了 extra usage 的话就此自愈，代价是每月每号白撞一发。
    /// 上游没给时间就一直有效。三条显式解除的路：该号对该模型连通性测试通过、等级刷新后变了
    /// （见 [`Self::set_tier`]）、控制台手动解除。
    pub fn deny_model(
        &self,
        cred_id: i64,
        model: &str,
        reason: &str,
        expires_at: Option<i64>,
    ) -> Result<()> {
        let key = model_denial_key(model);
        let reason: String = reason.chars().take(300).collect();
        self.conn.lock().execute(
            "INSERT INTO model_denials (cred_id, model, reason, learned_at, expires_at)              VALUES (?1, ?2, ?3, unixepoch(), ?4)              ON CONFLICT(cred_id, model) DO UPDATE SET                 reason = excluded.reason, learned_at = excluded.learned_at,                 expires_at = excluded.expires_at",
            params![cred_id, key, reason, expires_at],
        )?;
        Ok(())
    }

    /// 解除模型准入记录：`Some(model)` 只清那一个模型（连通性测试通过），`None` 清该号全部
    /// （控制台手动解除）。返回清掉的条数。
    pub fn clear_model_denials(&self, cred_id: i64, model: Option<&str>) -> Result<usize> {
        let conn = self.conn.lock();
        Ok(match model {
            Some(m) => conn.execute(
                "DELETE FROM model_denials WHERE cred_id = ?1 AND model = ?2",
                params![cred_id, model_denial_key(m)],
            )?,
            None => conn.execute("DELETE FROM model_denials WHERE cred_id = ?1", [cred_id])?,
        })
    }

    /// 该号当前仍有效的模型准入记录（到期的顺手删掉），按学到的时间倒序。
    pub fn denied_models(&self, cred_id: i64) -> Result<Vec<ModelDenial>> {
        let conn = self.conn.lock();
        Self::purge_expired_denials(&conn)?;
        let mut stmt = conn.prepare(
            "SELECT model, reason, learned_at, expires_at FROM model_denials              WHERE cred_id = ?1 ORDER BY learned_at DESC, model ASC",
        )?;
        let rows = stmt.query_map([cred_id], Self::row_to_denial)?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// 全部号的有效准入记录，按号分组（列表页一次取齐，免得逐号查）。
    pub fn all_model_denials(&self) -> Result<HashMap<i64, Vec<ModelDenial>>> {
        let conn = self.conn.lock();
        Self::purge_expired_denials(&conn)?;
        let mut stmt = conn.prepare(
            "SELECT cred_id, model, reason, learned_at, expires_at FROM model_denials              ORDER BY learned_at DESC, model ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                ModelDenial {
                    model: r.get(1)?,
                    reason: r.get(2)?,
                    learned_at: r.get(3)?,
                    expires_at: r.get(4)?,
                },
            ))
        })?;
        let mut out: HashMap<i64, Vec<ModelDenial>> = HashMap::new();
        for row in rows {
            let (cid, d) = row?;
            out.entry(cid).or_default().push(d);
        }
        Ok(out)
    }

    fn row_to_denial(r: &Row) -> rusqlite::Result<ModelDenial> {
        Ok(ModelDenial {
            model: r.get(0)?,
            reason: r.get(1)?,
            learned_at: r.get(2)?,
            expires_at: r.get(3)?,
        })
    }

    fn purge_expired_denials(conn: &Connection) -> Result<()> {
        conn.execute(
            "DELETE FROM model_denials WHERE expires_at IS NOT NULL AND expires_at <= unixepoch()",
            [],
        )?;
        Ok(())
    }

    /// 限流暂停中、且**不在** `denied` 里的号里最早的 `resume_at`；没有这样的号则 `None`。
    /// 选号时用，调用方已持锁。
    fn soonest_paused_resume(conn: &Connection, denied: &HashSet<i64>) -> Result<Option<i64>> {
        let mut stmt = conn.prepare(
            "SELECT id, resume_at FROM credentials WHERE disabled = 1 AND resume_at IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        let mut soonest: Option<i64> = None;
        for row in rows {
            let (id, at) = row?;
            if !denied.contains(&id) {
                soonest = Some(soonest.map_or(at, |s| s.min(at)));
            }
        }
        Ok(soonest)
    }

    /// 被判过不支持 `model` 的号（已到期的不算）。选号时用，调用方已持锁。
    fn denied_creds_for(conn: &Connection, model: &str) -> Result<HashSet<i64>> {
        let mut stmt = conn.prepare(
            "SELECT cred_id FROM model_denials WHERE model = ?1                AND (expires_at IS NULL OR expires_at > unixepoch())",
        )?;
        let rows = stmt.query_map([model_denial_key(model)], |r| r.get::<_, i64>(0))?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    // ---------- 从上游 400 学到的规则 ----------

    /// 落库一批刚学到的规则（已存在的组合原样保留，不刷新时间——保鲜期从第一次学到算）。
    pub fn remember_rejections(&self, rows: &[LearnedRejection]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "INSERT OR IGNORE INTO learned_rejections (kind, model, field, value, message, reply_sse, reply_body) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        )?;
        for r in rows {
            let message: String = r.message.chars().take(500).collect();
            // 回放体不截：拒答的体本来就只有几百字节到几 KB，学的那头已按上限把过大的挡掉了
            // （`proxy::UsageSniffer::refusal_reply`），截一刀等于回放一段残缺的 SSE。
            let (reply_sse, reply_body) = match &r.reply {
                Some(reply) => (reply.sse as i64, reply.body.as_str()),
                None => (0, ""),
            };
            stmt.execute(params![
                r.kind, r.model, r.field, r.value, message, reply_sse, reply_body
            ])?;
        }
        Ok(())
    }

    /// 读出仍在保鲜期内的全部规则（过期的顺手删掉），启动时回填进程内记忆用。
    pub fn learned_rejections(&self) -> Result<Vec<LearnedRejection>> {
        Ok(self.learned_rejections_with_time()?.into_iter().map(|(r, _)| r).collect())
    }

    /// 同 [`Self::learned_rejections`]，附每条学到的时刻（Unix 秒），控制台列表用。
    pub fn learned_rejections_with_time(&self) -> Result<Vec<(LearnedRejection, i64)>> {
        let conn = self.conn.lock();
        conn.execute(
            "DELETE FROM learned_rejections WHERE learned_at <= unixepoch() - ?1",
            [LEARNED_REJECTION_TTL_SECS],
        )?;
        let mut stmt = conn.prepare(
            "SELECT kind, model, field, value, message, learned_at, reply_sse, reply_body \
             FROM learned_rejections ORDER BY learned_at ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            // 空体即「没有」：这两列是 0.3.98 补的，之前学的行读出来就是默认值。
            let reply_sse: i64 = r.get(6)?;
            let reply_body: String = r.get(7)?;
            let reply = (!reply_body.is_empty())
                .then_some(LearnedReply { sse: reply_sse != 0, body: reply_body });
            Ok((
                LearnedRejection {
                    kind: r.get(0)?,
                    model: r.get(1)?,
                    field: r.get(2)?,
                    value: r.get(3)?,
                    message: r.get(4)?,
                    reply,
                },
                r.get::<_, i64>(5)?,
            ))
        })?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// 清空全部学到的规则（控制台逃生口：上游放开了某个取值、本地却还在拒）。返回删掉的条数。
    pub fn clear_learned_rejections(&self) -> Result<usize> {
        Ok(self.conn.lock().execute("DELETE FROM learned_rejections", [])?)
    }

    /// 只清某一种类（`kind` 列）的规则，返回删掉的条数。控制台「清空这一类」用：几百条拒答
    /// 提示词淹没列表时，不必连 `deprecated` 那几条有用的一起清掉。
    /// 拒答规则不设条数上限（进程内与库里都是），只靠 7 天保鲜期与这里的手动清理收口。
    pub fn clear_learned_rejections_of_kind(&self, kind: &str) -> Result<usize> {
        Ok(self.conn.lock().execute("DELETE FROM learned_rejections WHERE kind = ?1", [kind])?)
    }

    /// 删掉一条学到的规则（按主键四元组），返回是否确有其行。
    pub fn forget_learned_rejection(&self, r: &LearnedRejection) -> Result<bool> {
        Ok(self.conn.lock().execute(
            "DELETE FROM learned_rejections WHERE kind = ?1 AND model = ?2 AND field = ?3 AND value = ?4",
            params![r.kind, r.model, r.field, r.value],
        )? > 0)
    }

    /// 最近 `days` 天流水里出现过的模型（去重），附最后一次出现的时刻，按时刻倒序。
    /// 连通性测试自己打的那些（`device_id = 'probe'`）不算——它们是人挑的，不是客户端在用的。
    ///
    /// 不按 ts 扫窗口再 GROUP BY：那样要把窗口内的全部流水逐行回表读 model / device_id，线上
    /// 规模（30 天保留期时 180 万行、5GB 多）实测 12 秒，而且这里以前持的是转发主连接的锁。改成在
    /// `idx_usage_logs_model_ts` 上跳着走：先逐个取下一个不同的模型名，再对每个模型从最新一条
    /// 往回找第一条非 probe 的——模型就十来个，整条查询亚毫秒。
    pub fn recent_models(&self, days: i64) -> Result<Vec<(String, i64)>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare(
            "WITH RECURSIVE m(model) AS (
                 SELECT MIN(model) FROM usage_logs WHERE model > ''
                 UNION ALL
                 SELECT (SELECT MIN(model) FROM usage_logs WHERE model > m.model)
                   FROM m WHERE m.model IS NOT NULL
             )
             SELECT model, last_ts FROM (
                 SELECT model,
                        (SELECT MAX(ts) FROM usage_logs u
                          WHERE u.model = m.model
                            AND u.ts >= unixepoch() - ?1 * 86400
                            AND (u.device_id IS NULL OR u.device_id != 'probe')) AS last_ts
                   FROM m WHERE model IS NOT NULL
             )
             WHERE last_ts IS NOT NULL
             ORDER BY last_ts DESC",
        )?;
        let rows = stmt.query_map([days], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
        rows.collect::<rusqlite::Result<_>>().map_err(Into::into)
    }

    /// 回填账号 UUID（旧库凭证登录时未存、刷新 token 时补上）。仅在非空时覆盖。
    pub fn set_account_uuid(&self, id: i64, account_uuid: &str) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET account_uuid = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, account_uuid],
        )
    }

    /// 重命名（设置显示名）。
    /// 设置/清除该凭证的专用出站代理。`None` 或空串写成 NULL（直连）。
    ///
    /// 入参必须是 [`crate::clients::validate_proxy`] 校验过的串——这里只负责存，
    /// 校验放在入库之前那一层，免得存进去一条建不出客户端的代理，等到下次真有请求
    /// 选中这个号才炸。
    pub fn set_proxy(&self, id: i64, proxy: Option<&str>) -> Result<bool> {
        let proxy = proxy.map(str::trim).filter(|s| !s.is_empty());
        self.update_one(
            "UPDATE credentials SET proxy = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, proxy],
        )
    }

    pub fn set_label(&self, id: i64, label: &str) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET label = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, label],
        )
    }

    /// 刷新后回写新的 token 三元组（单行 UPDATE）。
    pub fn update_tokens(
        &self,
        id: i64,
        access_token: &str,
        refresh_token: &str,
        expires_at: u64,
    ) -> Result<bool> {
        self.update_one(
            "UPDATE credentials
                SET access_token = ?2, refresh_token = ?3, expires_at = ?4, updated_at = unixepoch()
              WHERE id = ?1",
            params![id, access_token, refresh_token, expires_at as i64],
        )
    }

    fn update_one(&self, sql: &str, p: impl rusqlite::Params) -> Result<bool> {
        let conn = self.conn.lock();
        let n = conn.execute(sql, p)?;
        Ok(n > 0)
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
        if let Some(v) = on(THINKING_MODIFIED_RETRY) {
            flags.thinking_modified_retry = v;
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
        if let Some(v) = on(FLATTEN_TOOL_SCHEMAS) {
            flags.flatten_tool_schemas = v;
        }
        if let Some(v) = on(STRIP_EMPTY_TEXT) {
            flags.strip_empty_text = v;
        }
        if let Some(v) = on(HOIST_SYSTEM_ROLE) {
            flags.hoist_system_role = v;
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

    /// 4.6+ 模型收到 assistant message prefill 时的处理策略。
    ///
    /// 走内存缓存，零查询。缺省（未设置）= [`PrefillPolicy::Strip`]（剥掉后转发）。
    pub fn prefill_policy(&self) -> PrefillPolicy {
        match self.settings.read().get(PREFILL_POLICY).map(|v| v.trim().to_ascii_lowercase()) {
            Some(v) if v == "reject" => PrefillPolicy::Reject,
            Some(v) if v == "off" => PrefillPolicy::Off,
            _ => PrefillPolicy::Strip,
        }
    }

    /// 4.7+ 模型收到 sampling 参数（`temperature`/`top_p`/`top_k`）时的处理策略。
    ///
    /// 走内存缓存，零查询。缺省（未设置）= [`PrefillPolicy::Strip`]（剥掉后转发）。
    /// 复用 [`PrefillPolicy`] 枚举——三档语义完全相同。
    pub fn sampling_policy(&self) -> PrefillPolicy {
        match self.settings.read().get(SAMPLING_POLICY).map(|v| v.trim().to_ascii_lowercase()) {
            Some(v) if v == "reject" => PrefillPolicy::Reject,
            Some(v) if v == "off" => PrefillPolicy::Off,
            _ => PrefillPolicy::Strip,
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
fn load_settings(conn: &Connection) -> Result<HashMap<String, String>> {
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

/// 上游以「thinking 块被修改」拒绝时，是否降级历史 thinking 块后重试一次的 settings 键名。
/// 缺省视为开启。成因通常是 JSON 序列化改变了 thinking 块的编码。
pub const THINKING_MODIFIED_RETRY: &str = "thinking_modified_retry";

/// 上游以「`redacted_thinking` 块的 `data` 无效」拒绝时，是否降级历史 thinking 块后重试一次的
/// settings 键名。缺省视为开启。与上面两项同一个兜底（[`crate::proxy::demote_thinking_blocks`]
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

/// 是否展平 tool `input_schema` 顶层的 `allOf`/`oneOf`/`anyOf` 的 settings 键名。
/// 缺省视为开启：上游不支持这些关键字，直接 400。
pub const FLATTEN_TOOL_SCHEMAS: &str = "flatten_tool_schemas";

/// 是否剥除 messages 里的空 text 内容块 `{"type":"text","text":""}` 的 settings 键名。
/// 缺省视为开启：上游要求 text 块非空，部分第三方客户端常发空块。
pub const STRIP_EMPTY_TEXT: &str = "strip_empty_text";

/// 是否将 messages 里的 `role:"system"` 消息提升到顶层 `system` 字段的 settings 键名。
/// 缺省视为开启：Anthropic API 不支持 messages 里出现 `role:"system"`（直接 400），
/// litellm 等第三方客户端常用此格式。
pub const HOIST_SYSTEM_ROLE: &str = "hoist_system_role";

/// 是否本地拒绝带 OpenAI 格式转换残留的请求的 settings 键名。
/// 缺省视为开启：messages 里的 `role:"system"`、`call_` 前缀的工具调用 id、OpenAI 专属顶层
/// 字段等一律 400，不修补不转发。关掉后退回 `hoist_system_role` 等修补路径。
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

/// 4.6+ 模型不支持 assistant message prefill 时的处理策略的 settings 键名。
///
/// 取值：`"strip"`（默认）= 主动剥掉末尾 assistant 轮后转发；`"reject"` = 本地直接
/// 400 拒绝、不转发；`"off"` = 不做任何处理，交给上游（被动重试兜底）。
pub const PREFILL_POLICY: &str = "prefill_policy";

/// 4.6+ 模型收到 assistant message prefill 时的处理策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrefillPolicy {
    /// 主动剥掉末尾 assistant 轮后转发（默认）。
    Strip,
    /// 本地直接 400 拒绝，不转发。
    Reject,
    /// 不做任何处理，让上游的 400 触发被动重试兜底。
    Off,
}

impl PrefillPolicy {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Strip => "strip",
            Self::Reject => "reject",
            Self::Off => "off",
        }
    }
}

impl std::fmt::Display for PrefillPolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 4.7+ 模型不支持 sampling 参数（`temperature`/`top_p`/`top_k`）时的处理策略的 settings 键名。
///
/// 取值与 [`PREFILL_POLICY`] 相同：`"strip"`（默认）= 主动剥掉后转发；`"reject"` = 本地
/// 直接 400 拒绝；`"off"` = 不做任何处理，交给运行时学习兜底。
pub const SAMPLING_POLICY: &str = "sampling_policy";

/// 官方基座那个缓存断点要不要带 `scope:"global"` 的 settings 键名。缺省视为开启：基座
/// 全网同一份，跨账号共享缓存是白捡的。
///
/// **键名不能叫 `cache_scope_global`**——那个名字被 [`CACHE_SCOPE_GLOBAL`] 占着，在旧库里
/// 是 [`SYSTEM_SHAPE`] 的曾用名，复用会让旧库里关过那个开关的人莫名其妙丢掉整套 system 对齐。
pub const SYSTEM_CACHE_SCOPE: &str = "system_cache_scope";

/// 转发开关的集合。**默认全开**。
///
/// 前六项是**形态对齐**：上游实测（8 发对照，见 [`crate::config::known_fingerprint_gaps`]）
/// 全关掉也照样 200，唯一被强制的是 `system` 里那句 `You are Claude Code, …`，而它由客户端
/// 自己发。所以它们都是「像不像官方客户端」而非「能不能用」，可以按需一项项关掉做排查，
/// 全开 = 加入开关机制之前的既有行为。
///
/// 最后一项 [`Self::thinking_signature_retry`] 不是形态对齐而是**错误恢复**，只在特定
/// 400 上触发，正常路径完全不经过它。放在同一个集合里纯粹是因为它同样按请求读、同样
/// 一条 SQL 读齐、同样在「转发」那个设置面板里拨。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForwardFlags {
    /// 改写 `metadata.user_id` 里的 account_uuid/device_id 为凭证自洽身份。
    pub spoof_identity: bool,
    /// 来访**自带** `device_id` 时要不要换成派生值（[`Self::spoof_identity`] 的子项，
    /// 它关着这项就无从谈起）。
    ///
    /// 单独成一项的依据来自抓包：同一台机器、同一个客户端，`cap/raw/00002`（API-key 模式
    /// 经 luban）与 `00006`（订阅模式直连）发的 **`device_id` 完全相同**，两种模式的
    /// `metadata` 里只有 `account_uuid` 不同（前者空串、后者真 uuid）。也就是说把 API-key
    /// 形态转成订阅形态**并不需要**动 `device_id`——换掉它是**反关联**策略，不是形态要求。
    ///
    /// 两边各有代价，故交给用户拨：
    /// - **开**（默认，既有行为）：`device_id = f(账号, 机器)`，多个账号落在同一台机器上会
    ///   得到各不相同的设备身份，账号之间不因共用设备 id 而被串起来。代价是真实 CC 的
    ///   `device_id` 是**机器标识、跨账号恒定**，于是经 luban 的流量里「一台机器多个账号」
    ///   这个真实用户群里很常见的模式一次都不会出现，每个 (账号,机器) 都是全新设备。
    /// - **关**：来访自带的 `device_id` 原样透传，与官方两模式逐字节一致（`account_uuid`
    ///   照样补）。代价是同一台机器用多个账号时，上游能凭这个 id 把这些账号关联起来。
    ///
    /// **只作用于来访自带身份的那条路**（[`crate::proxy::spoof_identity`]）。模拟路径与
    /// 「CC 形态但缺 `metadata.user_id`」那条路上来访压根没有 `device_id`，只能派生，
    /// 不受本开关影响——否则产出的是一份没有 `device_id` 的 `metadata`，那是官方从不发的形态。
    pub spoof_device_id: bool,
    /// 设备指纹只取平台（arch/os），不含客户端原始 `device_id`（[`Self::spoof_device_id`]
    /// 的子项，它关着时指纹不参与，本项无从谈起）。
    ///
    /// - **开**（默认）：`fingerprint = arch|os|出站 UA` → 同平台**且同客户端版本**的客户端
    ///   收敛成同一个伪装 device_id，符合真实用户一人多设备的模式。
    /// - **关**：`fingerprint = client_device_id|arch|os|出站 UA` → 每个 (账号, 客户端设备)
    ///   都是独立的设备身份，客户端越多、上游看到该账号的设备数就越多。
    ///
    /// 出站 UA 那段两档都有、不受本开关约束：一台设备只能有一个客户端版本，换版本即换设备。
    /// 理由与代价（升级会换一次 device_id）见 [`crate::proxy::device_fingerprint`]。
    pub normalize_device_fp: bool,
    /// 给 `x-anthropic-billing-header` 补 `cch`。
    pub billing_cch: bool,
    /// 真实 CC 来访的 `cch` 策略（与 [`Self::billing_cch`] 互相独立）。`cch` 是官方出口层对
    /// 最终出站 body 算的 xxHash64（见 `proxy::body::compute_cch`），luban 一改写 body，
    /// 来访自带的那个就对不上了。
    ///
    /// - **开**（默认）：body 被改写过就按最终出站字节重算（来访自带的真值与 luban 补的占位
    ///   一视同仁）；没改写的原样透传，自带值本来就对。
    /// - **关**：来访自带的 `cch` 原样保留（改写后与 body 不符）；luban 替它补的那条填随机值。
    pub cch_real_recompute: bool,
    /// 模拟请求的 `cch` 策略（[`Self::simulate_cc`] 的子项）。
    ///
    /// - **开**（默认）：按最终出站字节算真值，与官方出口层同一算法。
    /// - **关**：每请求填一个随机的 5 位小写 hex（形状对、语义不对的旧做法）。
    pub cch_sim_compute: bool,
    /// 补齐客户端未携带的 `accept-encoding`/`anthropic-version`/`x-client-request-id`。
    pub fill_client_headers: bool,
    /// 合并并按官方顺序重排 `anthropic-beta`（含塞入 oauth beta）。
    pub merge_beta: bool,
    /// 把 `system` 对齐成官方订阅客户端的 4 块形态（见 [`crate::proxy::align_system_shape`]）。
    pub system_shape: bool,
    /// 按官方拼写与顺序发出头名（见 [`crate::config::CC_HEADER_ORDER`]）。
    pub orig_header_case: bool,
    /// 上游以「thinking 块签名无效」拒绝时，把历史 thinking 降级成 text 后重试一次
    /// （见 [`crate::proxy::demote_thinking_blocks`]）。
    pub thinking_signature_retry: bool,
    /// 上游以「thinking 块被修改」拒绝时，降级历史 thinking 块后重试一次。
    /// 成因通常是 JSON 序列化改变了 thinking 块的编码。
    pub thinking_modified_retry: bool,
    /// 上游以「`redacted_thinking` 块的 `data` 无效」拒绝时，降级历史 thinking 块后重试一次。
    /// 那段密文是上游自己签发的，验不过通常是会话中途换了号、或那一轮 assistant 消息被改写过
    /// （见 [`crate::proxy::trace_thinking_block`] 那行日志怎么分辨）。
    pub redacted_thinking_retry: bool,
    /// 非 Claude Code 客户端的请求，按官方抓包形态模拟成 CC 请求（注入 system 前缀 +
    /// 整套官方头，见 [`crate::proxy::Simulation`]）。
    pub simulate_cc: bool,
    /// 模拟路径补齐官方 `system` 的**第四块**（基座之后那段一万字节上下的「其余」正文，
    /// [`crate::config::CC_SYSTEM_REST`] 模板按请求填占位）。客户端自己的 system 两种取值下都
    /// 单独占末块，**位置**不受这项影响；受影响的是它压不压得住——这一块在告诉模型它是
    /// Claude Code（[`Self::simulate_cc`] 的子项，它关着这项无从谈起）。
    ///
    /// - **开**（默认）：出站 `system` 是 `[billing, 身份, 基座, 其余]` 再跟客户端那块，末块之前
    ///   与 2.1.277 四族同形；「其余」里唯一随机器变的记忆目录优先用来访自己写的工作目录，
    ///   来访没写才按账号加设备派生一个假路径（[`crate::proxy::client_env`]）。代价是每条模拟
    ///   请求多约 2700 token 的前缀——带 `ttl:1h` 断点、同一设备同一会话内稳定，基本走缓存读价；
    ///   模型也会被这段官方提示词带得更像 Claude Code：实测客户端要求「只回某个标记」时，它会
    ///   照回那个标记，但后面还会再加一句自我介绍。
    /// - **关**：这一块不发，出站 `system` 只剩 `[billing, 身份, 基座]` 加客户端那块（haiku 实测
    ///   385 token，开着是 2978），不注入任何环境信息，客户端的 system 也不被官方提示词稀释。
    pub simulate_full_system: bool,
    /// 模拟路径上来访**一个工具都没声明**（没有 `tools` 键、`tools: null`、`tools: []`）时，也按
    /// 主线程补齐官方工具（[`Self::simulate_cc`] 的子项）；来访 `tool_choice` 是 `any` / 指定工具
    /// 时不补。
    ///
    /// - **开**（默认）：补上那 14 个官方工具。模拟路径只发主线程 profile，官方主线程一条不带
    ///   工具的样本都没有（`cap/2.1.280` 恒为 19 / 20 个），「主线程的 beta 与 system、零个工具」
    ///   是官方不产生的组合。代价两条：这类来访多半是没有工具循环的纯聊天客户端，模型调了注入
    ///   的工具时它拿到的是一个处理不了的 `tool_use`（流水 `rewrites` 列：补了的打 `tools_filled`，
    ///   真调了再打 `injected_tool_called`，两者一比是命中率）；每个新会话首轮多付约两万 token 的工具声明写入价，之后走缓存读价。
    /// - **关**：不带工具的请求一个工具都不注（空数组原样发出）。自己带了工具的两种取值下都补缺。
    ///   luban 自己的连通性探测不受这项管，恒补。
    pub fill_absent_tools: bool,
    /// 模拟路径的主线程按官方 2.1.285 的 message threads 形态写 `thread`（[`Self::simulate_cc`]
    /// 的子项，fable-5-1 除外——官方那一代不发）。
    ///
    /// - **开**（默认）：会话里一段对话的第一条写 `thread: {type: create}`；之后来访的历史若正好是
    ///   「上一轮 + 上游那条回复 + 新消息」，写 `thread: {type: continue}`、只发新增消息，接不上
    ///   （改了历史、重新生成、换了模型 / effort / system / tools、上一条失败）就再 `create`。
    ///   官方 opus / sonnet / haiku 主线程每条都带 `thread`，同一会话里除首轮与上述事件外全是
    ///   `continue`（`cap/auto-2.1.285-20260930`）。每条主线程请求（含 fable-5-1）末尾同时补上
    ///   官方的 `<total_tokens>N tokens left</total_tokens>` 提醒，N 按官方倒数算法（新输入重置为
    ///   1500 万，工具续轮减去本轮上下文的增量）。
    /// - **关**：不写 `thread`、不补提醒，每轮发完整上下文（2.1.280 非 auto 模式 opus / fable 的形态）。
    pub sim_message_threads: bool,
    /// 已是 CC 形态、但不带 `metadata.user_id` 的请求，补一份官方形态的身份
    /// （见 [`crate::proxy::bare_session_id`]）。
    pub fill_metadata: bool,
    /// 上游回 429 时给该号打冷却并换号重试（次数见
    /// [`CredentialStore::rate_limit_retry_max`]）；关掉即原样透传 429、也不打冷却。
    pub rate_limit_retry: bool,
    /// 官方基座那块的缓存断点带不带 `scope:"global"`（跨账号共享同一份基座缓存）。
    ///
    /// 单独成一项而不是并进 [`Self::system_shape`]：它要上游的 `prompt-caching-scope` beta
    /// 认（故还要 [`Self::merge_beta`] 开着），而且**官方从不单独发 `scope`**——官方那份总是
    /// `{type, ttl:1h, scope}`，luban 不再替客户端写 `ttl`（那是客户端掏钱买的时长），
    /// 于是发出去的是 `{type, scope}`。收益（跨账号复用基座）与这处形态偏差谁更重要，
    /// 交给用户自己拨。
    pub cache_scope_global: bool,
    /// 缓存断点写不写 `ttl:"1h"`。
    ///
    /// **默认开（对齐官方）**：四份订阅直连抓包的三个断点 3/3 全是 `ttl:"1h"`，而 API-key
    /// 模式那四份是裸的 `{"type":"ephemeral"}`——也就是说这个字段正是两种模式之间真实存在的
    /// 差别之一，不写就等于每条请求都留一处稳定差异。
    ///
    /// **代价要知情**：1h 的缓存**写入**单价是默认 5m 的 2 倍。命中与否取决于使用节奏——
    /// 长会话里 1h 往往反而更省（5m 内没接上话，下一轮就得按写入价重写整个前缀），
    /// 短促的一次性请求则是纯多付。所以给了开关：关掉即沿用客户端自己传的时长，
    /// luban 一个字节都不改。客户端自己写了 `ttl` 的任何情况下都照发，不被覆盖。
    ///
    /// 与 [`Self::cache_scope_global`] 一样要 beta 认（`extended-cache-ttl-2025-04-11`，
    /// 由 `merge_beta` 补），故还连着那个开关，见 [`crate::proxy::rewrite_body`]。
    pub cache_ttl_1h: bool,
    /// 工具声明补 `eager_input_streaming: true`，只补**抓包证实**该 profile 全带的那些组合
    /// （[`crate::config::CcEagerTools`]）：真 CC 按来访版本 × 模型 × 用途查表，模拟路径按出站
    /// profile；客户端显式写了的值不覆盖，`mcp__*` / 延迟占位 / 服务端工具不动。
    ///
    /// 订阅端主线程的每个内建工具都带这个字段、API-key 端一个不带，是两种模式间一处逐工具
    /// 重复的固定差异。它与出站头上的 `advanced-tool-use` beta 同现，故还连着 `merge_beta`
    /// （耦合点在 [`crate::proxy::rewrite_body`]）。收益是缩小声明差异，对封号率的影响幅度
    /// 没有量过，所以单独给开关。
    pub eager_tool_streaming: bool,
    /// 非流式 `/v1/messages` 改成流式发给上游，再把 SSE 聚合回整段 JSON 给客户端。
    ///
    /// **这是形态对齐里最硬的一项**：官方 CC 的 `/v1/messages` **恒为 `stream:true`**，
    /// 一条 `stream:false` 转发出去就是 100% 的判据，比 UA、比头序都硬（那些至少还有
    /// 第三方客户端会撞对）。而流/非流在**头上完全同形**——官方即便流式也发
    /// `accept: application/json`（见 `simulated_headers_replace_client_headers`），
    /// 差别只在 body 那一个字段，所以改起来只动一个 bool、不碰任何头。
    ///
    /// 只作用于计费路径（[`crate::proxy::is_billable_messages`]）：`count_tokens`
    /// 官方本来就是非流 JSON，动它反而制造偏差。
    ///
    /// 客户端侧完全无感：回给它的仍是 `content-type: application/json` + 整段 Message，
    /// 由 [`crate::proxy::aggregate_sse`] 按官方那套事件语义攒出来。上游中途出错时那条
    /// `event: error` 虽然裹在 200 里，也会按 `error.type` 翻译成非流式那边该有的状态码
    /// （见 [`crate::proxy::error_status`]），故客户端的错误分支照旧能走。
    ///
    /// 代价是整段响应要在内存里攒齐才发出（上限即 `max_tokens`，长文本级别，不是流量级别），
    /// 以及 `ttft_ms` 记的是上游首字节、与客户端的感知对不上——后者由 `usage_logs` 的
    /// `sse_aggregated` 列标出来。
    pub nonstream_as_sse: bool,
    /// 剥掉官方客户端**从不发送**的顶层字段（见 [`crate::proxy::strip_extra_fields`]）。
    ///
    /// 依据是两份直连抓包（`cap/raw/00006` opus-5、`00009` sonnet-5）的顶层键完全一致：
    /// `model, messages, system, tools, metadata, max_tokens, thinking, context_management,
    /// output_config, stream`。多出来的键都是官方不产生的形态。目前剥两样：
    /// 等价于缺省的 `tool_choice:{"type":"auto"}`，以及 `thinking.display`。
    ///
    /// **`thinking.display` 那项有代价**：剥掉后回程的 `thinking` 块文本为空，客户端看不到
    /// 思考摘要（功能不坏，只是没内容）。默认仍开——被判成第三方应用是**整条请求打不通**，
    /// 拿摘要换连通性划算；不接受这个代价就关掉本项。
    ///
    /// 对真实 CC 是空操作：它本来就不发这两样。
    pub strip_extra_fields: bool,
    /// 把上游会判成第三方应用的工具名换成假名转发，回程再还原（见
    /// [`crate::proxy::ToolNameMap`]）。
    ///
    /// **实测**：`tools[*].name` 是上游判定第三方的一个判据——不在官方 CC 工具名集合内的
    /// custom tool 名会让整条请求回 400（`Third-party apps now draw from your extra usage…`，
    /// 额度改扣超额池）。映射到已验证豁免的 `mcp__luban__*` 命名空间后，同一条请求回 200。
    ///
    /// **白名单策略**：三类保留原名——server tool、`mcp__` 前缀（实测豁免）、
    /// [`crate::config::CC_TOOL_NAMES`] 里的官方 CC 工具名。其余 custom tool 一律混淆。
    /// 故对真实 CC 是空操作，不必再叠客户端判定。
    ///
    /// 代价：回程每个 chunk 要做 N 次字节替换（N = 被混淆的工具数），且客户端增删工具会让
    /// 整套假名重算、上游 prompt cache 失效一次。关掉即完全退回原样转发。
    pub tool_name_mimic: bool,
    /// 模拟路径下是否注入 `thinking`（及配套的 `context_management`）。
    ///
    /// 官方 CC 恒带 `thinking`，缺了可能被判第三方。但注入 thinking 会改变模型行为
    /// （输出更长、token 消耗更多），且会强制 `temperature=1`（`strip_extra_fields` 自动剥）。
    /// 不想要这些副作用就关掉——代价是模拟形态少一个正面信号。
    pub inject_thinking: bool,
    /// 展平 tool `input_schema` 顶层的 `allOf`/`oneOf`/`anyOf`。
    /// 上游不支持这些关键字，直接 400。
    pub flatten_tool_schemas: bool,
    /// 剥除 messages 里的空 text 内容块 `{"type":"text","text":""}`。
    /// 上游要求 text 块非空，部分第三方客户端常发空块。
    pub strip_empty_text: bool,
    /// 将 messages 里的 `role:"system"` 消息提升到顶层 `system` 字段。
    ///
    /// 上游对首条 user/assistant 之前的 `role:"system"` 直接 400，老模型对对话中途的也 400；
    /// litellm 等第三方客户端采用 OpenAI 格式，会把 system 内容放在 messages 里。开启后自动把
    /// 这些消息（不论位置）的 content 提升到顶层 `system`（已有则追加），再从 messages 里移除。
    ///
    /// **CC 形态的请求整个跳过**：官方自己在 messages 里合法使用 `role:"system"`
    /// （deferred tools），硬提升会破坏形态。唯一的例外是**空壳**（content 为空数组 / 空串 /
    /// `null` / 缺失 / 整条只有空 text 块）：不论这个开关与来访形态，一律在出站前丢掉，见
    /// `proxy::drop_empty_system_messages`——上游对它恒回 400，而它一个内容块都没有。
    pub hoist_system_role: bool,
    /// 本地拒绝带 OpenAI 格式转换残留的请求（messages 开头的 `role:"system"`、`call_` 前缀的
    /// 工具调用 id、OpenAI 方言的 `tool_choice` / `tools`、`n` / `stop` / `user` 等 OpenAI 专属
    /// 顶层字段），不修补、不转发，见 `proxy::find_openai_marker`。
    ///
    /// 开着时 `hoist_system_role` 整个不跑：开头的 system 入口就拒了，能放行的只剩对话中途的
    /// 原生 system 消息，原样出站（见 `proxy::hoists_system_role`）；关掉才退回修补。
    /// 模拟路径不受影响：它只接管本来就是 Anthropic 形态的非 CC 请求。
    pub reject_openai_shape: bool,
    /// 来访的会话 id 在**头与体两处不一致**时本地拒绝（400），不替它选一个。
    ///
    /// 官方两处逐字相同，不同值说明来访自己就不自洽——而 luban 拿会话 id 当会话链
    /// （`cc_prompt_id` / `cc_prev_req` / `diagnostics`）的键，选错一个就是把两条链
    /// 接到了一起。默认开：宁可让客户端修好自己的形态，也不猜。
    ///
    /// 关掉后退回「取头那个 + 打一条 warn」。见 [`crate::proxy::session_id_conflict`]。
    pub reject_session_conflict: bool,
    /// 本地就地回答**探针类**请求（200 + 一条最小的正常回复，见 `proxy::probe_reply`），
    /// 不转发。判据是一组只有探活脚本才会有的强特征，任一命中即算，不限 UA，
    /// 见 [`crate::proxy::probe_signature`]：
    /// - 带 system、没有 tools、只有一条消息、`max_tokens` 在 2..=16；
    /// - 带 system、没有 tools、只有一条消息、不是官方那两种无 tools 形态，且来自一台从没
    ///   见过的设备；
    /// - system 里 CC 身份句出现在不止一块里。
    ///
    /// 身份字段写错的（device 不是 64 位 hex、session 不是 uuid）**不在这里拒**：那是抄错了，
    /// 不是探针，由模拟路径重建身份（`proxy::cc_identity_well_formed`）。「没有 tools」按值算：
    /// 缺失、`null`、`[]` 都是没有，加空字段绕不过。只看形态、一条就判，不做计数。官方 CC 的
    /// 四种无 tools 请求（cache 预热、Helper 子代理、标题生成、安全分类）按 system 结构 + beta
    /// 头 + body 取值逐项对、都在判据之外，见 `cap/` 抓包与 `proxy::probe_signature`。
    ///
    /// 命中回的是 200 而不是 403（0.3.101 起）：探活正是下游中转判断「这个号还能不能用」的
    /// 那条请求，luban 的 403 在它那侧与「号被封了」长得一样，整个 key 会被摘下去、真流量跟着
    /// 停——而这条请求根本没到上游、账号一点事没有。本地作答的这条标得出来：响应头
    /// `x-luban-local: probe_reply` 与 `x-luban-probe-kind: <判据>`、Message id 以 `msg_luban`
    /// 开头、流水里标 `probe_reply` 且花费记 0。
    ///
    /// 只管这三条形态判据。从响应学来的两类规则各有自己的开关（[`Self::reject_refusals`]、
    /// [`Self::reject_empty_replies`]）：三件事的依据、误伤面、该不该开都不一样，共用一个键
    /// 就没法单独关一件。默认开。
    pub reject_probes: bool,
    /// 探针拒绝的**严格模式**（随 [`Self::reject_probes`] 一起才生效）：ping 不再要求无 tools
    /// （`max_tokens` 不超过 16 装不下一次 tool_use，带工具只给 16 个 token 只能是测活）；新增
    /// 「短开场」——无 system、无 tools、恰好一条不超过 32 字节（中文约十个字）的用户消息、`max_tokens != 1`
    /// （测活脚本的「hi」「ping」「test」）。覆盖封号复盘里默认判据放过去的那几批 Go-http-client
    /// 探活（4 个 tools + max_tokens 16；max_tokens 50 / 1024 / 32000 只问一句）。代价是真人用
    /// 裸聊天客户端经中转站发的第一句「你好」也会被拦下、收到探活那条一样的「OK」，
    /// 故**默认关**。
    pub reject_probes_strict: bool,
    /// 是否本地拦下 **上游分类器已经拒答过的那条提示词**的逐字重发（`kind = "refusal"`，
    /// 见 `proxy::known_refused_prompt`）——拦下时**原样回放上游那次的响应**（200 + 同一段
    /// `stop_reason: "refusal"` 的体，见 [`LearnedReply`]），不是 luban 自己造一条 403：客户端
    /// 看到的与上游亲自拒一次完全一样。拒答是内容分类器给那一条提示词的确定性判决
    /// （`stop_details.category` 非空），换个号、换个形态重发结果一样，本地拦下省一次白跑。
    /// **只挡出站不带 `fallbacks` 的请求**：客户端自带、或 luban 按族开关补上 fallback 的请求，
    /// 上游会换模型重跑，那正是拒答该走的路，本地拦下反而让它永远走不到。规则随其他学到的
    /// 规则落库、7 天到期、控制台可删。默认开。
    pub reject_refusals: bool,
    /// 是否本地 403 **上游回过 200 却零输出的请求类**（模型 + 无 tools 单条消息 + 同一个
    /// `max_tokens`，不限 UA；`kind = "empty_reply"`，见 `proxy::known_empty_reply`）——那是
    /// 上游收了输入的钱、一个字没回，每条都是一次白白留下的「一台设备只问一句话」记录。
    /// 规则随其他学到的规则落库、7 天到期、控制台可删。默认开。
    pub reject_empty_replies: bool,
    /// 替每条转发成功的 `/v1/messages` 上报官方客户端会发的那串遥测：一方事件
    /// （`tengu_api_query` → `tengu_api_success` → `tengu_turn_end`，带上游 `request-id`、
    /// 逐项 token 与花费）、Datadog 日志、OTel 指标，身份取实际发往上游的那份，节奏照
    /// 抓包（30s / 10s / 5min 攒批）。见 [`crate::telemetry`]。
    ///
    /// 失败的请求走另一条收尾（`tengu_api_error` + `tengu_feature_bad{api_request}` +
    /// `terminal_reason: "api_error"` 的 `tengu_turn_end`），与官方一致。
    ///
    /// 关掉即只剩 [`crate::oauth`] 的保活遥测——上游那边这个账号就成了「有大量 API 用量、
    /// 遥测里却一条 API 调用都没有」的形态。
    pub api_telemetry: bool,
    /// 保活循环里的遥测那一半：每 30 分钟一组空闲版本检查事件（event_logging + Datadog）与
    /// 每 6 小时一次 GrowthBook 画像。有近期真实会话的凭证，事件挂到那个会话的身份上
    /// （同一 session_id / device_id / 版本），没有的才用按账号派生的空闲身份。
    ///
    /// 关掉只停这一半：token 刷新、bootstrap / policy_limits / settings 握手与 401/403
    /// 探测照常。不影响 [`Self::api_telemetry`]。
    pub keepalive_telemetry: bool,
    /// **fable 族**主线程请求补服务端 refusal fallback：安全分类器拒答（`stop_reason:
    /// "refusal"`，如 cyber 类）时由上游在同一次调用里换模型重跑，客户端拿到的是回答而不是
    /// 拒答。补的是官方 2.1.260 那份 `[{"model":"claude-opus-5"}]`（形态逐字同官方），出站头
    /// 一并带 `server-side-fallback-2026-06-01`。客户端自己带了数组形态的不动；只对主线程
    /// profile 补，辅助请求（helper / 标题 / 分类 / 额度探测）官方都不发。上游以 400 拒掉某个
    /// fallback 目标时，剥掉重发一次并记进「从上游学到的规则」，之后该模型不再补。落到
    /// fallback 的回复按实际服务的模型计价。**默认关**：形态有官方 2.1.260 抓包依据，但它替
    /// 用户决定了拒答后由 opus-5 作答、按 opus 计价、同一对话约一小时粘在 opus 上，且用户看不到
    /// 拒答本身——这些改变该由用户自己拨开；关着时客户端自带的 `fallbacks` 照样保留、只归一
    /// 形态（`"default"` → 数组），头上的 beta 由 [`Self::merge_beta`] 按版本补，「有 beta 没
    /// 字段」正是 2.1.260 之前的官方形态。见 `crate::proxy::refusal_fallbacks_for`。
    pub fable_refusal_fallback: bool,
    /// **opus-5 族**主线程请求补 luban 自定的 refusal fallback 链
    /// `[{"model":"claude-opus-4-8"},{"model":"claude-opus-4-6"}]`（`config::OPUS_REFUSAL_FALLBACKS`）。
    /// 官方 2.1.260 的 opus 客户端**不发**这个字段：补了就是一份官方从不产生的请求形态，
    /// 是风控层面的自证风险，故**默认关**、作为独立实验开关保留；关着时 opus-5 的请求形态与
    /// 官方一致（有 beta 没字段）。其余行为（只补主线程、客户端自带的不动、400 学习后不再补、
    /// 按实际作答模型计价）同 [`Self::fable_refusal_fallback`]。
    pub opus_refusal_fallback: bool,
}

impl ForwardFlags {
    /// 真实客户端（不走模拟）带设备身份时**按会话**占名额、设备上限不生效：设备指纹归一化开着、
    /// 身份伪装连同 device 一起换，出站 device_id 只剩「账号 + 平台 + 客户端版本」那几种，绑了
    /// 几台真实机器上游看不见。见 [`Select::per_session`] 与 `crate::proxy::session_plan`。
    pub fn devices_by_session(self) -> bool {
        self.normalize_device_fp && self.spoof_identity && self.spoof_device_id
    }
}

impl Default for ForwardFlags {
    fn default() -> Self {
        Self {
            spoof_identity: true,
            spoof_device_id: true,
            normalize_device_fp: true,
            billing_cch: true,
            cch_real_recompute: true,
            cch_sim_compute: true,
            fill_client_headers: true,
            merge_beta: true,
            system_shape: true,
            orig_header_case: true,
            thinking_signature_retry: true,
            thinking_modified_retry: true,
            redacted_thinking_retry: true,
            simulate_cc: true,
            simulate_full_system: true,
            fill_absent_tools: true,
            sim_message_threads: true,
            fill_metadata: true,
            rate_limit_retry: true,
            cache_scope_global: true,
            cache_ttl_1h: true,
            eager_tool_streaming: true,
            nonstream_as_sse: true,
            strip_extra_fields: true,
            tool_name_mimic: true,
            inject_thinking: true,
            flatten_tool_schemas: true,
            strip_empty_text: true,
            hoist_system_role: true,
            reject_openai_shape: true,
            reject_session_conflict: true,
            reject_probes: true,
            reject_probes_strict: false,
            reject_refusals: true,
            reject_empty_replies: true,
            api_telemetry: true,
            keepalive_telemetry: true,
            fable_refusal_fallback: false,
            opus_refusal_fallback: false,
        }
    }
}

/// 布尔型设置的统一口径：仅 `"0"`/`"false"`（忽略大小写与首尾空白）为关，其余为开。
/// 从代理 URL 中提取 `host:port` 作为人可读的标签。
///
/// 先去掉 `scheme://`，再去掉 `user:pass@`，保留剩余部分（`host:port`）。
/// 解析失败时回退到完整 URL。
fn url_to_label(raw: &str) -> String {
    let after_scheme = raw.find("://").map(|i| &raw[i + 3..]).unwrap_or(raw);
    let after_auth =
        after_scheme.rfind('@').map(|i| &after_scheme[i + 1..]).unwrap_or(after_scheme);
    if after_auth.is_empty() { raw.to_string() } else { after_auth.to_string() }
}

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

/// 设备限流窗口表里最多留多少个键，超过就清掉空窗口，见 [`RateWindow::sweep_if_crowded`]。
/// 取 4096：比任何真实部署的设备数高一两个数量级，正常规模下这条清扫永远不会触发。
const DEVICE_RATE_MAX_KEYS: usize = 4096;

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

/// 会话限流窗口表里最多留多少个键，见 [`CredentialStore::take_session_rpm_slot`]。
/// 比设备那个高一档（16384）：会话 id 正常使用下就在不断产生新值，撞上清扫的机会本就更大，
/// 而清扫要遍历全表，不该在还装得下的时候触发。
const SESSION_RATE_MAX_KEYS: usize = 16384;

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

/// 待写入的一条用量日志（代理层组装后交给 [`CredentialStore::insert_usage_log`]）。
#[derive(Debug, Default)]
pub struct UsageRecord {
    pub cred_id: Option<i64>,
    pub cred_label: String,
    /// 完整 device_id（供本地分析；对外展示可自行截断）。
    pub device_id: Option<String>,
    pub model: Option<String>,
    pub path: String,
    /// **来访**客户端的 `User-Agent`（已截断，见 [`crate::proxy::ua_of`]）；没带头时为 `None`。
    pub ua: Option<String>,
    /// **实际发给上游**的那份 `User-Agent`；模拟路径恒为官方那串，非模拟路径同 `ua`。
    /// 连通性测试只有这一份（没有来访客户端）。
    pub ua_out: Option<String>,
    pub status: u16,
    /// 是否从响应中解析到用量。
    pub has_usage: bool,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    /// 缓存写细分：5 分钟 / 1 小时档。
    pub cache_5m_tokens: Option<i64>,
    pub cache_1h_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub total_ms: Option<i64>,
    pub unified_status: Option<String>,
    pub rl_5h_status: Option<String>,
    pub rl_5h_reset: Option<i64>,
    pub rl_5h_utilization: Option<f64>,
    pub rl_7d_status: Option<String>,
    pub rl_7d_reset: Option<i64>,
    pub rl_7d_utilization: Option<f64>,
    pub rl_representative: Option<String>,
    /// 本次请求是否动用了 usage credits（`…-overage-in-use`）：套餐额度满了但照样 200，花的是钱。
    pub rl_overage_in_use: Option<bool>,
    /// 上游本次报告的全部额度窗口，见 [`QuotaWindow`]。只写进账本快照，不进流水——
    /// 流水那边已有 `ratelimit_raw` 保着原始头，再存一份结构化的纯属重复。
    pub windows: Vec<QuotaWindow>,
    pub ratelimit_raw: Option<String>,
    /// 等价 API 费用（USD）。
    pub cost_usd: Option<f64>,
    /// 这条来访本来是非流式、被改写成流式发给上游再聚合回整段 JSON（见
    /// [`ForwardFlags::nonstream_as_sse`]）。
    pub sse_aggregated: bool,
    /// 这条入站请求的 id：来访带了合法 `X-Request-Id` 就是它，否则是 luban 生成的
    /// `req_` + 16 位 base62（形态照 Stripe / Anthropic）；
    /// 同时回在响应头 `X-Request-Id` / `X-Oneapi-Request-Id` 上。New API 会把上游响应头里的
    /// `X-Oneapi-Request-Id` 存成它日志的
    /// `upstream_request_id`，于是拿它那边的一条日志就能在这里精确找到对应的流水。
    /// 一条入站请求可能对应多次上游请求（429 换号、prefill 重试），故主键是 luban 自己的 id。
    pub request_id: Option<String>,
    /// 上游**最后一次**响应头里的 `request-id`，对 Anthropic 工单用。
    pub upstream_request_id: Option<String>,
    /// 取证字段，见 [`Forensics`]。
    pub forensics: Forensics,
}

/// 一条流水的**取证**字段：封号事后回溯用，选号、限流、计费一概不读。
///
/// 上游封号只给一句所有人都一样的文案，「为什么」只能从封前流量的形态反推：走的哪个出口、
/// 请求体长什么样、被上游判成第三方没有、经历了哪些改写。这些此前只进 tracing 日志，日志一
/// 滚就没了，而排查恰恰要按号聚合、拿被封的号和活着的号对照。故逐条落库。
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct Forensics {
    /// 这一发实际走的出站代理（密码已打码，见 [`redact_proxy`]）；`None` 为直连。
    pub proxy: Option<String>,
    /// 走了模拟路径（非 CC 客户端被改写成 CC 形态发出）。
    pub simulated: bool,
    /// 走模拟的原因标签（`not_cc_client` / `identity_malformed` / `not_cc_shaped` /
    /// `no_base_prompt` / `tools_not_cc` / `probe`），见 `crate::proxy::SimulationReason`；
    /// 没走模拟为 `None`。0.3.99 之前的旧记录也是 `None`。
    pub sim_reason: Option<String>,
    /// 出站请求体的结构摘要（JSON 文本，不含用户正文），见 `crate::proxy::shape_summary`。
    pub shape: Option<String>,
    /// **实际发给上游**的 session_id（出站体 `metadata.user_id` 末段 / 出站头
    /// `X-Claude-Code-Session-Id`）。
    pub session_id: Option<String>,
    /// **来访客户端自报**的 session_id（来访头或来访体里的那个，见
    /// `crate::proxy::incoming_session_id`）；没带或形态不合法为 `None`。
    ///
    /// 与上面那个分开记，理由和 `device_id` / [`Self::device_id_out`] 那一对完全一样：走模拟
    /// 路径时来访那个会被换成派生值（按槽位派生，或按账号钉住），**上游看到的与客户端自己
    /// 知道的不是同一个 uuid**。只存出站那个的话，下游拿着自己的会话 id 来查这条请求，在
    /// 请求查询里一条都查不到——而那恰恰是他手里唯一有的线索。反过来拿上游侧的 id 回查是
    /// 哪条来访会话，也只有这一对对得上。
    ///
    /// 本地拒绝的行只有这一个（没到上游，没有出站值）。0.3.139 之前的旧记录为 `None`。
    pub session_id_in: Option<String>,
    /// 这条请求落在哪个**模拟会话绑定**上（`session_bindings.session_key`，形如
    /// `lb:v2:sid:<uuid>` / `lb:v2:pfx:<hex>`，见 `crate::proxy::session_binding_key`）；
    /// 带设备身份的来访与非模拟路径为 `None`。
    ///
    /// 与上面的 `session_id` 分开记，两者不是一回事：`session_id` 是**上游看到的**那个 uuid，
    /// 按「账号 + 槽位」派生、槽位释放后被下一个对话复用——按它筛会把先后占过同一个槽位的
    /// 几个对话混成一条。`session_key` 才是这条对话自己的身份，后台「活跃模拟会话」里那一行
    /// 点「看请求」筛的就是它。
    pub session_key: Option<String>,
    /// **实际发给上游**的 device_id（出站体 `metadata.user_id` 里的 device 段）。
    ///
    /// 与流水的 `device_id` 列分开记：那一列是**来访**客户端的原始 id（设备绑定、设备上限都按
    /// 它算），而上游看到的是按「账号 + 平台指纹」派生出来的另一个值（见
    /// `Credential::spoof_device_id`）。上游侧拿到一个 device_id 要回查是哪台机器、哪个号，
    /// 只有这一列对得上；反过来看一个号在上游眼里有几台设备，也只能数这一列。
    pub device_id_out: Option<String>,
    /// 非 2xx 时上游 `error.type` / `error.message`（message 截断到 [`ERROR_MESSAGE_MAX`]）。
    pub error_type: Option<String>,
    pub error_message: Option<String>,
    /// 上游把这条请求判成了第三方应用（`Third-party apps now draw from your extra usage…`）。
    pub third_party: bool,
    /// 这条请求在 luban 里经历的改写/重试标签（逗号分隔），如 `demoted_thinking`、`no_prefill`。
    pub rewrites: Option<String>,
    /// 上游回了 **200 却零输出**（有 `usage`、`output_tokens = 0`）时截取的响应体开头
    /// （字符数上限 [`RESPONSE_EXCERPT_MAX`]），见 `crate::proxy::UsageSniffer::excerpt`。
    ///
    /// 正常回复不存：一条流水带一份正文，表会成倍膨胀，而排查只需要异常那几条。零输出是
    /// 「上游收了输入的钱、一个字没回」——状态码、用量、错误列三处都看不出它回了什么，这一列
    /// 是唯一能看到上游原话的地方（`stop_reason`、有没有 `content`、是不是 `refusal`）。
    pub response_excerpt: Option<String>,
}

/// `Forensics::response_excerpt` 的落库上限（字符）。零输出的回复本身就很短（一段没有
/// `content` 的 Message JSON 几百字节），4000 足以整份留下；超出的多半是别的东西，截掉。
pub const RESPONSE_EXCERPT_MAX: usize = 4000;

/// `Forensics::error_message` 的落库上限（字符）。上游错误文案通常几百字，个别会把整段
/// 请求体回显进来，那种整段存一遍是浪费。
pub const ERROR_MESSAGE_MAX: usize = 2000;

/// 把代理 URL 里的密码打码：`socks5h://user:secret@host:1080` → `socks5h://user:***@host:1080`。
/// 出口是谁（host）是取证要看的，密码不是；流水表会被导出、被贴到别处，不能带明文密码。
pub fn redact_proxy(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else { return url.to_string() };
    let Some((userinfo, host)) = rest.rsplit_once('@') else { return url.to_string() };
    match userinfo.split_once(':') {
        Some((user, _)) => format!("{scheme}://{user}:***@{host}"),
        None => url.to_string(),
    }
}

/// 一条落库后的用量日志（读取用）。
#[derive(Debug, serde::Serialize)]
pub struct UsageLog {
    pub id: i64,
    pub ts: i64,
    pub cred_id: Option<i64>,
    pub cred_label: String,
    pub device_id: Option<String>,
    pub model: Option<String>,
    pub path: String,
    /// **来访**客户端的 `User-Agent`（已截断）；旧记录与没带该头的请求为 `None`。
    pub ua: Option<String>,
    /// **实际发给上游**的那份 `User-Agent`；旧记录为 `None`。
    pub ua_out: Option<String>,
    pub status: u16,
    /// 这条是非流转流聚合回来的（见 [`ForwardFlags::nonstream_as_sse`]）。
    /// 该列是 0.2.63 加的，旧记录一律为 `false`。
    pub sse_aggregated: bool,
    pub has_usage: bool,
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_creation_tokens: Option<i64>,
    pub cache_5m_tokens: Option<i64>,
    pub cache_1h_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub ttft_ms: Option<i64>,
    pub total_ms: Option<i64>,
    pub unified_status: Option<String>,
    pub rl_5h_status: Option<String>,
    pub rl_5h_reset: Option<i64>,
    pub rl_5h_utilization: Option<f64>,
    pub rl_7d_status: Option<String>,
    pub rl_7d_reset: Option<i64>,
    pub rl_7d_utilization: Option<f64>,
    pub rl_representative: Option<String>,
    pub rl_overage_in_use: Option<bool>,
    pub ratelimit_raw: Option<String>,
    pub cost_usd: Option<f64>,
    /// 见 [`UsageRecord::request_id`]；0.3.70 之前的旧记录为 `None`。
    pub request_id: Option<String>,
    /// 见 [`UsageRecord::upstream_request_id`]。
    pub upstream_request_id: Option<String>,
    /// 取证字段，见 [`Forensics`]。0.3.76 之前的旧记录全部为空/false。
    #[serde(flatten)]
    pub forensics: Forensics,
}

/// [`CredentialStore::query_usage_logs`] 的入参。
///
/// **页码翻页要靠 `until_id` 钉住范围，光有 OFFSET 不够**：流水是只增的，翻页期间新请求会
/// 不断插到最前面，纯 `LIMIT/OFFSET` 会把第二页整体往回错、重复吐出第一页尾部的记录。
/// 调用方先取一次 `max(id)` 当锚点（[`UsageLogStats::max_id`]），之后每页都带着它，
/// 于是整轮翻页看到的是同一个快照，页码、总条数、总花费三者始终自洽。锚点钉在 id 上而不是
/// `ts` 上：它同时是排序键（自增，同秒内仍严格有序，不会像按 `ts` 分界那样漏记录）。
#[derive(Debug, Clone, Default)]
pub struct UsageLogQuery {
    /// 只看这个凭证的流水；`None` 为全部。
    ///
    /// 已删账号的流水保留原 `cred_id` 直到保留期满（见 [`CredentialStore::remove`]），按号筛
    /// 照样筛得出；后台的账号下拉只列在册账号，所以平时筛不到它们。
    pub cred_id: Option<i64>,
    /// 翻页锚点：只取 id **小于等于**它的记录；`None` 为不设上界（即含最新写入的那些）。
    pub until_id: Option<i64>,
    /// 跳过前多少条（页码 × 每页条数）。
    pub offset: i64,
    /// 最多返回条数。调用方负责收敛，这里不设默认上限。
    pub limit: i64,
    /// 只看这一个请求 id（精确匹配，见 [`UsageRecord::request_id`]）；空白视同 `None`。
    pub request_id: Option<String>,
    /// 只看这个模型（精确匹配 `model` 列）；空白视同 `None`。趋势对话框的拆分表点进来用。
    pub model: Option<String>,
    /// 只看这个时刻（Unix 秒）之后的；`None` 为不限。
    pub since: Option<i64>,
    /// 只看这条**模拟会话**的请求（精确匹配 `session_key` 列，见 `Forensics::session_key`）；
    /// 空白视同 `None`。后台「活跃模拟会话」里那一行点「看请求」用的就是它。
    pub session_key: Option<String>,
    /// 只看这个**会话 id** 的请求：`session_id`（出站）与 `session_id_in`（来访）**任一命中**
    /// 即算；空白视同 `None`。
    ///
    /// 两侧一起匹配是这条筛选的全部意义：走模拟路径时两者是两个不同的 uuid（见
    /// `Forensics::session_id_in`），而来查的人手里只会有其中一个——下游用户知道的是自己
    /// 那个，从上游侧回查的人拿到的是出站那个，两边都得能查到同一批请求。
    pub session_id: Option<String>,
}

impl UsageLogQuery {
    /// 把筛选条件拼成 `WHERE …`（可能为空串）与对应的绑定参数。
    ///
    /// **按条件动态拼而不是写 `(?1 IS NULL OR col = ?1)`**：那种写法 SQLite 用不上索引，每翻
    /// 一页都是整表扫；流水保留期内动辄几十万行，按号翻页与按请求 id 查都得走索引才像样
    /// （`idx_usage_logs_cred_id` / `idx_usage_logs_request_id`）。统计与取页共用这一份，
    /// 「共 N 条」与翻得到的记录永远是同一个集合。
    fn where_clause(&self) -> (String, Vec<rusqlite::types::Value>) {
        use rusqlite::types::Value;
        let mut clauses: Vec<String> = Vec::new();
        let mut params: Vec<Value> = Vec::new();
        if let Some(c) = self.cred_id {
            params.push(Value::Integer(c));
            clauses.push(format!("cred_id = ?{}", params.len()));
        }
        if let Some(u) = self.until_id {
            params.push(Value::Integer(u));
            clauses.push(format!("id <= ?{}", params.len()));
        }
        if let Some(r) = self.request_id.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
            params.push(Value::Text(r.to_string()));
            clauses.push(format!("request_id = ?{}", params.len()));
        }
        if let Some(m) = self.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
            params.push(Value::Text(m.to_string()));
            clauses.push(format!("model = ?{}", params.len()));
        }
        if let Some(s) = self.since {
            params.push(Value::Integer(s));
            clauses.push(format!("ts >= ?{}", params.len()));
        }
        if let Some(k) = self.session_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
            params.push(Value::Text(k.to_string()));
            clauses.push(format!("session_key = ?{}", params.len()));
        }
        // 出站与来访任一命中。两个 OR 分支各有自己的部分索引，SQLite 会走 MULTI-INDEX OR
        // （测试里用 EXPLAIN QUERY PLAN 钉住），不会退成整表扫。
        if let Some(sid) = self.session_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
            params.push(Value::Text(sid.to_string()));
            let n = params.len();
            clauses.push(format!("(session_id = ?{n} OR session_id_in = ?{n})"));
        }
        let sql = if clauses.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", clauses.join(" AND "))
        };
        (sql, params)
    }
}

/// 一批流水的整体口径：条数、花费合计、以及可作翻页锚点的最大 id。
///
/// 缓存命中率趋势里的一个**小时桶**：`ts` 是这一小时的起点（Unix 秒）。
///
/// 回两个原始数，比率由界面算：一个 300 token 的小时里的「命中 0%」与 17K 前缀那种
/// 小时里的「命中 94%」是两件事，光看比率判断不了。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CacheBucket {
    pub ts: i64,
    /// 全部输入 token（含缓存命中与缓存写入）。
    pub input_tokens: i64,
    /// 其中来自缓存的部分（`cache_read_tokens`）。
    pub cached_tokens: i64,
    /// 其中写进缓存的部分（`cache_creation_tokens`，旧行按 5m + 1h 两段之和）。命中率一个数
    /// 分不出「没命中」和「没写入」，拆开才知道该查什么：写入多命中少是前缀每轮在变，
    /// 写入命中都少是客户端根本没标断点。
    pub written_tokens: i64,
}

impl CacheBucket {
    fn empty(ts: i64) -> Self {
        Self { ts, input_tokens: 0, cached_tokens: 0, written_tokens: 0 }
    }
}

/// TTFT（首字时延）趋势里的一个**小时桶**。
#[derive(Debug, Clone, serde::Serialize)]
pub struct TtftBucket {
    pub ts: i64,
    /// 算术平均，留给老口径对照；偶发的几十秒超时会把它拉高，看 p50 / p95。
    pub avg_ms: i64,
    /// 中位数（nearest-rank）。
    pub p50_ms: i64,
    /// 95 分位（nearest-rank）。
    pub p95_ms: i64,
    /// 参与统计的成功请求数（status = 200 且记了 TTFT）。
    pub count: i64,
    /// 输出吞吐（token / 秒）：`Σ output_tokens / Σ (total_ms − ttft_ms)`，只算两者都有且
    /// 生成阶段时长为正的请求；没有这样的请求为 `None`。
    pub tokens_per_sec: Option<f64>,
}

/// 趋势接口的一次返回：各桶、整窗口合计、近 60 分钟合计。
#[derive(Debug, Clone, serde::Serialize)]
pub struct CacheReport {
    pub points: Vec<CacheBucket>,
    pub summary: CacheBucket,
    pub recent: CacheBucket,
}

/// 同上，延迟那份。
#[derive(Debug, Clone, serde::Serialize)]
pub struct TtftReport {
    pub points: Vec<TtftBucket>,
    pub summary: TtftBucket,
    pub recent: TtftBucket,
}

/// 延迟原始行的查询：WHERE 与 `idx_usage_logs_latency` 的部分谓词逐字相同，规划器才会
/// 选它；测试里用 EXPLAIN QUERY PLAN 盯着这一点。
const LATENCY_ROWS_SQL: &str = "SELECT ts, ttft_ms, total_ms, output_tokens
       FROM usage_logs
      WHERE ts >= ?1 AND ttft_ms IS NOT NULL AND status = 200";

/// 一条成功请求在延迟统计里要用的几个数，见 [`CredentialStore::ttft_series`]。
#[derive(Debug, Clone, Copy)]
struct LatencyRow {
    ts: i64,
    ttft_ms: i64,
    total_ms: Option<i64>,
    output_tokens: Option<i64>,
}

/// 一条请求里缓存省下的钱：命中与写入都按原价算一遍减去实际计价。命中省 0.9 倍输入价，
/// 5 分钟档写入多付 0.25 倍、1 小时档多付 1 倍；模型认不出价目时记 0。
fn cache_saved_usd(
    model: Option<&str>,
    plain: i64,
    cached: i64,
    creation: Option<i64>,
    c5: Option<i64>,
    c1: Option<i64>,
) -> f64 {
    use crate::pricing::{Usage, estimate_usd};
    let written = creation.unwrap_or(c5.unwrap_or(0) + c1.unwrap_or(0));
    let actual = estimate_usd(Usage {
        model,
        input_tokens: Some(plain),
        cache_read_tokens: Some(cached),
        cache_creation_total: creation,
        cache_5m_tokens: c5,
        cache_1h_tokens: c1,
        ..Default::default()
    });
    let baseline = estimate_usd(Usage {
        model,
        input_tokens: Some(plain + cached + written),
        ..Default::default()
    });
    match (actual, baseline) {
        (Some(a), Some(b)) => b - a,
        _ => 0.0,
    }
}

/// nearest-rank 分位：`sorted` 已升序、非空，`p` 在 (0, 1]。
fn percentile(sorted: &[i64], p: f64) -> i64 {
    let rank = ((sorted.len() as f64) * p).ceil() as usize;
    sorted[rank.clamp(1, sorted.len()) - 1]
}

/// 把一组成功请求汇总成一个 [`TtftBucket`]；空集返回全零、吞吐 `None`。
fn summarize_latency(ts: i64, rows: &[LatencyRow]) -> TtftBucket {
    if rows.is_empty() {
        return TtftBucket { ts, avg_ms: 0, p50_ms: 0, p95_ms: 0, count: 0, tokens_per_sec: None };
    }
    let mut sorted: Vec<i64> = rows.iter().map(|r| r.ttft_ms).collect();
    sorted.sort_unstable();
    let sum: i64 = sorted.iter().sum();
    let (mut out_tokens, mut gen_ms) = (0i64, 0i64);
    for r in rows {
        if let (Some(total), Some(out)) = (r.total_ms, r.output_tokens)
            && total > r.ttft_ms
            && out > 0
        {
            out_tokens += out;
            gen_ms += total - r.ttft_ms;
        }
    }
    TtftBucket {
        ts,
        avg_ms: sum / sorted.len() as i64,
        p50_ms: percentile(&sorted, 0.5),
        p95_ms: percentile(&sorted, 0.95),
        count: sorted.len() as i64,
        tokens_per_sec: (gen_ms > 0).then(|| out_tokens as f64 * 1000.0 / gen_ms as f64),
    }
}

/// 拆分维度，见 [`CredentialStore::usage_breakdown`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BreakdownBy {
    Model,
    Account,
}

/// 按模型或按账号拆开的一行：这段时间里它的请求数、延迟分位、吞吐与缓存三段 token。
/// 池子平均看不出「谁在拖后腿」，这张表就是给这个问题的。
#[derive(Debug, Clone, serde::Serialize)]
pub struct BreakdownRow {
    /// 模型名，或凭证 id 的十进制串。
    pub key: String,
    /// 展示名：模型名本身，或凭证的 label（已删的号是 `#<id>`）。
    pub label: String,
    /// 按账号拆时该号的套餐（`Max 5x` / `Pro` …）；按模型拆或号已删为 `None`。Max 号和 Pro 号
    /// 上游的排队本来就不同，混在一起比延迟没有意义。
    pub tier: Option<String>,
    /// 这段时间里的全部请求数（含失败的）。
    pub requests: i64,
    /// 缓存给这一组省下的钱（USD）：把命中与写入都按原价算一遍再减去实际——命中按十分之一
    /// 计价省下来的，减掉写入按 1.25 倍（1 小时档 2 倍）多付的。可能为负：只写不命中就是亏。
    /// 模型认不出价目的行不计。
    pub cache_saved_usd: f64,
    /// 延迟统计（只算成功且记了 TTFT 的请求），`ts` 一律是窗口起点。
    pub latency: TtftBucket,
    /// 缓存三段 token（所有请求）。
    pub cache: CacheBucket,
}

/// 单账号用量统计的一格：一个时间桶，或整个窗口的合计（`ts` 是桶起点 / 窗口起点）。
///
/// token 四项与官方 `usage` 同口径、互不重叠，**不加权**；费用是写流水时按价目表估的等价 API
/// 费用，模型认不出价目的记录按 0 计。
#[derive(Debug, Clone, Copy, Default, PartialEq, serde::Serialize)]
pub struct CredentialStatsBucket {
    pub ts: i64,
    /// 全部请求数（含失败与本地拒绝）。
    pub requests: i64,
    /// 其中非 2xx 的条数。
    pub errors: i64,
    /// 其中 luban 本地拒掉、没发到上游的条数（`rewrites` 以 `rejected_locally` 开头）。
    pub rejected: i64,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_write_tokens: i64,
    pub cache_read_tokens: i64,
    pub cost_usd: f64,
}

impl CredentialStatsBucket {
    fn add(&mut self, row: &CredentialStatsBucket) {
        self.requests += row.requests;
        self.errors += row.errors;
        self.rejected += row.rejected;
        self.input_tokens += row.input_tokens;
        self.output_tokens += row.output_tokens;
        self.cache_write_tokens += row.cache_write_tokens;
        self.cache_read_tokens += row.cache_read_tokens;
        self.cost_usd += row.cost_usd;
    }
}

/// 单账号按某个维度拆开的一组。`key` 是模型名 / device_id / 来访 UA / 状态码的原值，
/// 缺失时为空串（没带设备身份的裸请求、没带 UA 的来访）。
#[derive(Debug, Clone, Default, PartialEq, serde::Serialize)]
pub struct CredentialStatsGroup {
    pub key: String,
    pub requests: i64,
    pub errors: i64,
    /// 四项 token 之和（同 [`CredentialStatsBucket`] 的口径）。
    pub tokens: i64,
    pub cost_usd: f64,
    /// 这一组最近一条请求的时刻（Unix 秒）。
    pub last_ts: i64,
}

impl CredentialStatsGroup {
    fn add(&mut self, row: &CredentialStatsBucket) {
        self.requests += row.requests;
        self.errors += row.errors;
        self.tokens +=
            row.input_tokens + row.output_tokens + row.cache_write_tokens + row.cache_read_tokens;
        self.cost_usd += row.cost_usd;
        self.last_ts = self.last_ts.max(row.ts);
    }
}

/// [`CredentialStore::credential_stats`] 的结果。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct CredentialStats {
    /// 有请求的桶（空桶不返回，前端自己补齐）。
    pub points: Vec<CredentialStatsBucket>,
    /// 整个窗口的合计。
    pub summary: CredentialStatsBucket,
    pub by_model: Vec<CredentialStatsGroup>,
    pub by_device: Vec<CredentialStatsGroup>,
    pub by_client: Vec<CredentialStatsGroup>,
    pub by_status: Vec<CredentialStatsGroup>,
}

/// 与 [`CredentialStore::query_usage_logs`] 走同一套筛选条件，好让「共 N 条」「合计 $X」
/// 与实际翻得到的记录是同一个集合——分两处各写一份 WHERE 迟早会漂开。
#[derive(Debug, Clone, Copy, Default, serde::Serialize)]
pub struct UsageLogStats {
    /// 命中条数。
    pub total: i64,
    /// 花费合计（USD）。`cost_usd` 为 NULL 的记录（模型不在价目表里）按 0 计。
    pub cost_usd: f64,
    /// 命中记录里最大的 id；空集为 `None`。首次查询拿它当锚点，后续每页原样带回。
    pub max_id: Option<i64>,
}

/// 上游报告的**一个**额度窗口。窗口名原样保留（`5h`/`7d`/`7d_oi`/`overage` …）。
///
/// 存在的理由：快照原先只有 5h/7d 两组写死的列，而上游的窗口种类是它说了算的——实测里
/// 真正被拒的常常是超额池 `7d_oi`（见 `crate::proxy::rate_limit_scope` 记录的那次 fable-5
/// 429）。它不落库，后台就只能看到「5h/7d 都没满」，却解释不了这个号为什么在烧钱或被拒，
/// 前端只能把状态挂成一个永远摘不掉的「超额待确认」。
///
/// 以 JSON 数组整体存进 `credential_stats.windows`，而不是拆成一张表：快照永远是「最新一份、
/// 整体覆盖」，没有按窗口查询或聚合的需求，一张表换来的只是删号时多四处级联清理。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QuotaWindow {
    /// 窗口名，取自 `anthropic-ratelimit-unified-<窗口>-*` 的中段。
    pub name: String,
    /// `…-status`（`allowed`/`allowed_warning`/`rejected`/`rate_limited`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// `…-utilization`，0~1（超额池可能 > 1）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub utilization: Option<f64>,
    /// `…-reset`，Unix 秒。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset: Option<i64>,
}

/// 单个凭证最新一次的额度快照（用于凭证卡片展示）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct QuotaSnapshot {
    /// 该快照对应的请求时间（Unix 秒）。
    pub ts: i64,
    pub unified_status: Option<String>,
    pub rl_5h_utilization: Option<f64>,
    pub rl_5h_reset: Option<i64>,
    pub rl_7d_utilization: Option<f64>,
    pub rl_7d_reset: Option<i64>,
    pub rl_representative: Option<String>,
    /// 最近一次带限流头的响应是否动用了 **usage credits**：套餐额度满了但上游照样 200，
    /// 烧的是按量计费的钱。卡片靠它把「满了在烧钱的号」和健康号区分开。
    pub overage_in_use: Option<bool>,
    /// 当前 5h / 7d 窗口内该凭证已用的等价费用（USD）。窗口起点由对应 reset 反推。
    pub cost_5h: Option<f64>,
    pub cost_7d: Option<f64>,
    /// 当前 5h / 7d 窗口内经该凭证转发的请求数。口径与窗口费用完全一致。
    pub requests_5h: Option<i64>,
    pub requests_7d: Option<i64>,
    /// 当前 5h / 7d 窗口内该凭证用掉的**总 token**。窗口与上面两项完全一致，只是换了个量纲。
    ///
    /// 口径按官方 `usage` 对象的四项相加：`input_tokens` + `output_tokens` +
    /// `cache_creation_input_tokens` + `cache_read_input_tokens`。官方这四项互不重叠——缓存命中
    /// 的那部分**不**再计进 `input_tokens`——所以直接相加就是这个窗口真实吞掉的 token 量。
    ///
    /// **不加权**：计价那边给缓存写 ×1.25、缓存读 ×0.1（见 [`crate::pricing`]），但那是**钱**的
    /// 口径；token 数一旦跟着加权，就和上游用量页上的数字对不上了。于是「token 很多、花费很少」
    /// 是常态（缓存读通常占大头），两个数放在一起看才有意义。
    pub tokens_5h: Option<i64>,
    pub tokens_7d: Option<i64>,
    /// 上游本次报告的**全部**窗口（含上面那两个，也含 `7d_oi` 这类没有专用列的）。
    ///
    /// 5h/7d 的专用列没有被它取代，两者并存是有意的：只有这两个窗口有配套的窗口内费用与
    /// 请求数（要靠 `reset` 反推窗口起点去聚合流水），而这里的窗口只有上游给的三个字段。
    /// 前端拿它补齐「专用列覆盖不到的那些窗口」，见 admin-ui 的 quotaRiskMeta。
    #[serde(default)]
    pub windows: Vec<QuotaWindow>,
}

/// 一条设备绑定明细（凭证卡片展开「已绑定设备」时展示）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DeviceBinding {
    /// 客户端 `metadata.user_id` 里的原始 device_id（非伪装后的那个）。
    pub device_id: String,
    /// 该设备经此凭证转发过的累计请求数（终身，来自 `device_costs` 账本）。
    pub request_count: i64,
    /// 首次绑定到该凭证的时间（Unix 秒）。模拟客户端没有绑定行，故为 `None`。
    pub created_at: Option<i64>,
    /// 最近一次活跃时间（Unix 秒）；TTL 就是按它算的。模拟客户端不参与 TTL，故为 `None`。
    pub last_seen_at: Option<i64>,
    /// 是否是**模拟客户端**的伪设备（`sim:` 前缀，见 [`crate::proxy::sim_device_id`]）：
    /// 不写绑定、不占 [`Self::device_count`] 名额、不能解绑，只有用量与费用是真的。
    pub simulated: bool,
    /// 该设备经**本凭证**花掉的等价 API 费用（USD 合计，来自 `usage_logs`）。
    ///
    /// 与 `request_count` 不同源：绑定行会被解绑/停用清掉并从零重数，用量日志不会，
    /// 所以这个数覆盖的时间范围可能比 `request_count` 更长。
    pub cost_usd: f64,
    /// 该设备在**所有凭证**上的累计费用（USD）；用来看清换号后仍在烧钱的同一台设备。
    pub cost_usd_all: f64,
}

/// 一条**模拟会话**绑定（`session_bindings` 的一行），供后台列表用。口径与
/// [`CredentialStore::session_count`] 一致：只含 TTL 内仍活跃的。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SessionBinding {
    /// 会话键：`lb:v2:sid:<来访自带的会话 id>` 或 `lb:v2:pfx:<缓存前缀加首条用户消息的指纹>`，
    /// 见 `crate::proxy::session_binding_key` 与 `Select::session_key`。
    pub session_key: String,
    /// 在该凭证上占的槽位（0 起）；会话 id 由它派生，释放后被下一个对话复用。
    /// [`PASSTHROUGH_SLOT`]（`-1`）表示真实客户端的会话：沿用来访自己的会话 id，不占槽位。
    pub slot: i64,
    /// 上游看到的会话 id（`X-Claude-Code-Session-Id`）。模拟会话按「账号 + 槽位」派生
    /// （`crate::credentials::sim_slot_session_id`），真实客户端的会话是来访 id 按账号钉住后的值
    /// （`crate::credentials::pinned_session_id`），都与转发路径同一个函数。真实客户端没带会话 id
    /// （按前缀指纹分会话）时为空串：出站也没有。
    pub session_id: String,
    /// 绑定之后再命中的请求数（建行那一轮不计，口径同 `device_bindings.request_count`；
    /// 随绑定行走，解绑即归零）。
    pub request_count: i64,
    /// 首次绑定到该凭证的时间（Unix 秒）。
    pub created_at: i64,
    /// 最近一次活跃时间（Unix 秒）；TTL 按它算。
    pub last_seen_at: i64,
    /// 最近一轮请求的模型；**不参与键**（理由见 `crate::proxy::session_binding_key`），只用来
    /// 在后台一眼看出这条会话在跑什么。旧库补列出来是 `None`，下次命中即回填。
    pub last_model: Option<String>,
}

/// 5 小时窗口秒数。
const WINDOW_5H_SECS: i64 = 5 * 3600;
/// 7 天窗口秒数。
const WINDOW_7D_SECS: i64 = 7 * 24 * 3600;
/// 用量日志流水的保留时长：8 天。必须大于最长的统计窗口（7 天）：7d 窗口起点是 reset 往前推
/// 7 天，封号取证要回看 7 天加 10 分钟（[`FREEZE_WINDOW_SECS`]），正好 7 天会在边界上少算，
/// cost_7d 平白变小。再往前的流水没人看——终身口径都在账本里——留着只是让表、索引和每条
/// 按时间扫的查询跟着变大（线上 30 天时 180 万行、5GB 多）。
pub const USAGE_LOG_RETENTION_SECS: i64 = 8 * 24 * 3600;
/// RPM（每分钟请求数）的统计窗口：最近 60 秒。
pub const RPM_WINDOW_SECS: i64 = 60;

/// 流水统计（条数、花费合计、最大 id）的 SQL，见 [`CredentialStore::usage_log_stats`]。
/// 拎成函数是为了让测试对**实际执行的这条**跑 EXPLAIN QUERY PLAN。
fn usage_log_stats_sql(where_sql: &str) -> String {
    format!("SELECT COUNT(*), COALESCE(SUM(cost_usd), 0), MAX(id) FROM usage_logs{where_sql}")
}

/// 流水取页的 SQL，见 [`CredentialStore::query_usage_logs`]。`n` 是参数个数，最后两个是
/// LIMIT / OFFSET。
///
/// **分两步**：子查询只按筛选取出这一页的 id，外层再按 id 读整行。子查询只碰 id，各条筛选
/// 索引都能覆盖它，不回表；一步到位的写法里，规划器若按 `(model, ts)` 圈窗口，就得先把窗口
/// 内每一行整行读出来排序再丢掉，一页 50 条要几百毫秒。OFFSET 跳过的那些行同理，只在
/// 索引里跳。外层的 `id IN (…)` 按主键逐条取，IN 列表本身有序，不再排序。
fn usage_log_page_sql(where_sql: &str, n: usize) -> String {
    format!(
        "SELECT id, {USAGE_LOG_COLS}
           FROM usage_logs
          WHERE id IN (SELECT id FROM usage_logs{where_sql}
                        ORDER BY id DESC LIMIT ?{} OFFSET ?{})
          ORDER BY id DESC",
        n - 1,
        n
    )
}

/// 额度快照 + 窗口统计的那条 SQL，见 [`CredentialStore::quota_snapshots`]。拎成常量是为了让
/// 测试能对它跑 EXPLAIN QUERY PLAN，钉住「流水那一侧只走覆盖索引、不回表」。
const QUOTA_SNAPSHOTS_SQL: &str = "SELECT s.cred_id, s.snapshot_ts, s.unified_status,
            s.rl_5h_utilization, s.rl_5h_reset,
            s.rl_7d_utilization, s.rl_7d_reset, s.rl_representative, s.overage_in_use,
            s.windows,
            CASE WHEN s.rl_5h_reset IS NULL THEN NULL ELSE
                COALESCE(SUM(CASE WHEN u.ts >= s.rl_5h_reset - ?1 THEN u.cost_usd END), 0)
            END,
            CASE WHEN s.rl_7d_reset IS NULL THEN NULL ELSE
                COALESCE(SUM(CASE WHEN u.ts >= s.rl_7d_reset - ?2 THEN u.cost_usd END), 0)
            END,
            CASE WHEN s.rl_5h_reset IS NULL THEN NULL ELSE
                SUM(CASE WHEN u.ts >= s.rl_5h_reset - ?1 THEN 1 ELSE 0 END)
            END,
            CASE WHEN s.rl_7d_reset IS NULL THEN NULL ELSE
                SUM(CASE WHEN u.ts >= s.rl_7d_reset - ?2 THEN 1 ELSE 0 END)
            END,
            -- 窗口内的总 token（口径见 QuotaSnapshot::tokens_5h）。四项逐个 COALESCE 成 0
            -- 再相加：没嗅探到 usage 的那些行（4xx/429）各列都是 NULL，而 NULL + x 在
            -- SQLite 里是 NULL，会把整条流水的 token 抹掉。
            -- 缓存写取合计列，它为空时退回 5m/1h 两档之和——同 crate::pricing 的兜底。
            CASE WHEN s.rl_5h_reset IS NULL THEN NULL ELSE
                COALESCE(SUM(CASE WHEN u.ts >= s.rl_5h_reset - ?1
                    THEN COALESCE(u.input_tokens, 0) + COALESCE(u.output_tokens, 0)
                       + COALESCE(u.cache_creation_tokens,
                                  COALESCE(u.cache_5m_tokens, 0)
                                + COALESCE(u.cache_1h_tokens, 0))
                       + COALESCE(u.cache_read_tokens, 0)
                END), 0)
            END,
            CASE WHEN s.rl_7d_reset IS NULL THEN NULL ELSE
                COALESCE(SUM(CASE WHEN u.ts >= s.rl_7d_reset - ?2
                    THEN COALESCE(u.input_tokens, 0) + COALESCE(u.output_tokens, 0)
                       + COALESCE(u.cache_creation_tokens,
                                  COALESCE(u.cache_5m_tokens, 0)
                                + COALESCE(u.cache_1h_tokens, 0))
                       + COALESCE(u.cache_read_tokens, 0)
                END), 0)
            END
       FROM credential_stats s
       LEFT JOIN usage_logs u
              ON u.cred_id = s.cred_id
             -- 只连**可能落进某个窗口**的流水。没有这个下界，索引
             -- idx_usage_logs_cred_usage 只能按 cred_id 定位，然后把该账号保留期内
             -- （保留期）的全部流水逐行走一遍、靠上面的 CASE 过滤——而窗口最长才 7 天。
             -- 账号列表每次刷新都要跑一遍这条 SQL。
             -- 下界引用外层的 s，故 SQLite 能把它压成 (cred_id=? AND ts>=?) 的范围扫描；
             -- 上面读的 u.* 列都在那条索引里，整段扫描不回表（见 init_schema 的注）。
             --
             -- 取两个窗口起点里更早的那个。COALESCE 的第二个参数是给「只有一个窗口
             -- 有 reset」准备的：min(NULL, x) 在 SQLite 里是 NULL，会把条件变成假、
             -- 一行都连不上，那就把窗口费用算成 0 了。两个都没有时退化为 0（无下界），
             -- 此时两个 CASE 本来就恒为 NULL，多连的行不影响结果。
             AND u.ts >= MIN(
                   COALESCE(s.rl_5h_reset - ?1, s.rl_7d_reset - ?2, 0),
                   COALESCE(s.rl_7d_reset - ?2, s.rl_5h_reset - ?1, 0))
      WHERE s.snapshot_ts IS NOT NULL
        AND (?3 IS NULL OR s.cred_id = ?3)
      GROUP BY s.cred_id";

impl CredentialStore {
    /// 每个凭证「最新一条带限流信息」的额度快照（cred_id → 快照），
    /// 并附带当前 5h / 7d 窗口内的累计费用与请求数。
    pub fn latest_quotas(&self) -> Result<HashMap<i64, QuotaSnapshot>> {
        self.quota_snapshots(None)
    }

    /// 单个凭证的额度快照；口径与 [`Self::latest_quotas`] 完全一致（同一条 SQL）。
    pub fn latest_quota(&self, cred_id: i64) -> Result<Option<QuotaSnapshot>> {
        Ok(self.quota_snapshots(Some(cred_id))?.remove(&cred_id))
    }

    /// 额度快照 + 窗口费用/请求数，一条 SQL 出全部结果。`only` 为 `Some(id)` 时只算该凭证。
    ///
    /// 快照直接读账本（credential_stats，写日志时同事务落好），不再从 usage_logs 里
    /// 扫「最新一条带限流信息的行」——那条 CTE 的过滤列不在索引里，表越大回表越多。
    /// 窗口统计（起点 = 快照的 reset 反推一个窗口时长）仍从流水条件聚合：窗口最长 7 天
    /// 多一点，流水的保留期（见 [`Self::prune_usage_logs`]）覆盖它绰绰有余。
    fn quota_snapshots(&self, only: Option<i64>) -> Result<HashMap<i64, QuotaSnapshot>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare(QUOTA_SNAPSHOTS_SQL)?;
        // LEFT JOIN：快照在账本里长存，而窗口内的流水可能已被裁剪清空（此时窗口统计为 0，
        // 语义正确——窗口比保留期短，裁掉的必然是窗口外的行；真正空窗口就该是 0）。
        let rows = stmt.query_map(params![WINDOW_5H_SECS, WINDOW_7D_SECS, only], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                QuotaSnapshot {
                    ts: r.get(1)?,
                    unified_status: r.get(2)?,
                    rl_5h_utilization: r.get(3)?,
                    rl_5h_reset: r.get(4)?,
                    rl_7d_utilization: r.get(5)?,
                    rl_7d_reset: r.get(6)?,
                    rl_representative: r.get(7)?,
                    overage_in_use: r.get(8)?,
                    // 老库补出来的列是 NULL；真存坏了也只当没有窗口，不让一条脏 JSON
                    // 把整张账号列表打成 500。
                    windows: r
                        .get::<_, Option<String>>(9)?
                        .and_then(|s| serde_json::from_str(&s).ok())
                        .unwrap_or_default(),
                    cost_5h: r.get(10)?,
                    cost_7d: r.get(11)?,
                    requests_5h: r.get(12)?,
                    requests_7d: r.get(13)?,
                    tokens_5h: r.get(14)?,
                    tokens_7d: r.get(15)?,
                },
            ))
        })?;
        let mut out = HashMap::new();
        for row in rows {
            let (cid, q) = row?;
            out.insert(cid, q);
        }
        Ok(out)
    }

    /// 每个凭证最近 60 秒的请求数，即当前 RPM（cred_id → 条数）。窗口内没有请求的凭证
    /// **不出现**在结果里，调用方按 0 处理。
    ///
    /// 口径与 `requests_5h`/`requests_7d` 完全一致——数的是 `usage_logs` 的流水条数，
    /// 也就是真正发给上游的请求，失败的（4xx/5xx）同样计入，只是窗口固定为 60 秒。
    ///
    /// **刻意不复用 [`BareRateWindow`] 那个内存计数器**：它只数无 `metadata.user_id` 的
    /// 裸请求（带设备身份的一条都不进），且重启即清零，拿来当 RPM 会系统性地偏小。
    /// 而 60 秒的流水靠 `idx_usage_logs_ts` 只扫一小段范围，比那把锁贵不了多少。
    pub fn recent_rpm(&self) -> Result<HashMap<i64, i64>> {
        let conn = self.read_conn();
        // 时间下界用 SQLite 的时钟，与写入侧（insert_usage_log_at）同源：两边若各取各的
        // 时钟，机器时间稍有偏差就会把刚写进去的那几条数丢或多数。
        let mut stmt = conn.prepare(
            "SELECT cred_id, COUNT(*) FROM usage_logs
              WHERE ts >= unixepoch() - ?1 AND cred_id IS NOT NULL
              GROUP BY cred_id",
        )?;
        let rows =
            stmt.query_map([RPM_WINDOW_SECS], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (cid, n) = row?;
            out.insert(cid, n);
        }
        Ok(out)
    }

    /// **全局 RPM**：最近 60 秒经 luban 转发的请求总数。
    ///
    /// 口径与 [`Self::recent_rpm`] 逐条对齐（同一张表、同一个窗口、同样只数落到某个账号头上
    /// 的那些），所以它恒等于各账号 RPM 之和——两个数摆在同一屏上，对不上会比看不到更让人
    /// 犯疑。代价是没选到号就失败的请求（全员限流、无可用凭证）不计入：它们压根没发出去。
    pub fn total_rpm(&self) -> Result<i64> {
        let conn = self.read_conn();
        let n = conn.query_row(
            "SELECT COUNT(*) FROM usage_logs WHERE ts >= unixepoch() - ?1 AND cred_id IS NOT NULL",
            [RPM_WINDOW_SECS],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// 单个凭证当前的 RPM；口径同 [`Self::recent_rpm`]，无请求时为 0。
    pub fn recent_rpm_of(&self, cred_id: i64) -> Result<i64> {
        let conn = self.read_conn();
        // 这条走 idx_usage_logs_cred_usage 的 (cred_id, ts) 前缀，直接定位到该号最近 60 秒那一小段。
        let n = conn.query_row(
            "SELECT COUNT(*) FROM usage_logs WHERE cred_id = ?1 AND ts >= unixepoch() - ?2",
            params![cred_id, RPM_WINDOW_SECS],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// 每个凭证最近一次被使用（有转发记录）的时间（cred_id → Unix 秒）。读账本，
    /// 不扫流水——流水会被裁剪，账本才是终身口径（下同，cost_by_cred / cost_of 亦然）。
    pub fn last_used(&self) -> Result<HashMap<i64, i64>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare(
            "SELECT cred_id, last_used_at FROM credential_stats WHERE last_used_at IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (cid, ts) = row?;
            out.insert(cid, ts);
        }
        Ok(out)
    }

    /// 单个凭证最近一次被使用的时间；无记录时为 `None`。口径同 [`Self::last_used`]。
    pub fn last_used_at(&self, cred_id: i64) -> Result<Option<i64>> {
        let conn = self.conn.lock();
        // 账本行可能还不存在（该凭证从未有过流水），optional 后拍平。
        let ts = conn
            .query_row(
                "SELECT last_used_at FROM credential_stats WHERE cred_id = ?1",
                [cred_id],
                |r| r.get::<_, Option<i64>>(0),
            )
            .optional()?;
        Ok(ts.flatten())
    }

    /// 每个凭证累计的等价 API 费用（cred_id → USD 合计）。
    pub fn cost_by_cred(&self) -> Result<HashMap<i64, f64>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare("SELECT cred_id, cost_total_usd FROM credential_stats")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, f64>(1)?)))?;
        let mut out = HashMap::new();
        for row in rows {
            let (cid, sum) = row?;
            out.insert(cid, sum);
        }
        Ok(out)
    }

    /// 单个凭证累计的等价 API 费用（USD）；无记录时为 0。口径同 [`Self::cost_by_cred`]。
    pub fn cost_of(&self, cred_id: i64) -> Result<f64> {
        let conn = self.conn.lock();
        let sum = conn.query_row(
            "SELECT COALESCE((SELECT cost_total_usd FROM credential_stats WHERE cred_id = ?1), 0)",
            [cred_id],
            |r| r.get(0),
        )?;
        Ok(sum)
    }

    /// 写入一条用量日志。
    pub fn insert_usage_log(&self, rec: &UsageRecord) -> Result<()> {
        self.insert_usage_log_at(rec, None)
    }

    /// 写入一条用量日志，并在**同一事务**里把账本（credential_stats / device_costs）记上。
    ///
    /// 账本承接三个终身口径：最近使用、累计费用、最新额度快照。流水（usage_logs）只保留
    /// 近期（见 [`Self::prune_usage_logs`]），这些口径若继续从流水聚合，裁剪一跑数字就会
    /// 跟着变小；写时落账之后，读路径不再依赖流水的历史深度。同一事务保证两边不漂移。
    ///
    /// `ts` 为 `None` 时取当前时间；拆出这个参数是给测试用的——窗口/裁剪相关的用例
    /// 需要指定「这条流水发生在何时」。
    fn insert_usage_log_at(&self, rec: &UsageRecord, ts: Option<i64>) -> Result<()> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let ts = match ts {
            Some(t) => t,
            // 用 SQLite 的时钟，与建表 DEFAULT unixepoch() 同源。
            None => tx.query_row("SELECT unixepoch()", [], |r| r.get(0))?,
        };
        tx.execute(
            "INSERT INTO usage_logs
                (ts, cred_id, cred_label, device_id, model, path, status, has_usage,
                 input_tokens, output_tokens, cache_creation_tokens, cache_5m_tokens,
                 cache_1h_tokens, cache_read_tokens, ttft_ms, total_ms,
                 unified_status, rl_5h_status, rl_5h_reset, rl_5h_utilization,
                 rl_7d_status, rl_7d_reset, rl_7d_utilization, rl_representative,
                 rl_overage_in_use, ratelimit_raw, cost_usd, ua, ua_out, sse_aggregated,
                 request_id, upstream_request_id,
                 proxy, simulated, shape, session_id, error_type, error_message, third_party,
                 rewrites, device_id_out, response_excerpt, sim_reason, session_key,
                 session_id_in)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29,
                     ?30, ?31, ?32, ?33, ?34, ?35, ?36, ?37, ?38, ?39, ?40, ?41, ?42, ?43, ?44,
                     ?45)",
            params![
                ts,
                rec.cred_id,
                rec.cred_label,
                rec.device_id,
                rec.model,
                rec.path,
                rec.status as i64,
                rec.has_usage as i64,
                rec.input_tokens,
                rec.output_tokens,
                rec.cache_creation_tokens,
                rec.cache_5m_tokens,
                rec.cache_1h_tokens,
                rec.cache_read_tokens,
                rec.ttft_ms,
                rec.total_ms,
                rec.unified_status,
                rec.rl_5h_status,
                rec.rl_5h_reset,
                rec.rl_5h_utilization,
                rec.rl_7d_status,
                rec.rl_7d_reset,
                rec.rl_7d_utilization,
                rec.rl_representative,
                rec.rl_overage_in_use,
                rec.ratelimit_raw,
                rec.cost_usd,
                rec.ua,
                rec.ua_out,
                rec.sse_aggregated as i64,
                rec.request_id,
                rec.upstream_request_id,
                rec.forensics.proxy,
                rec.forensics.simulated as i64,
                rec.forensics.shape,
                rec.forensics.session_id,
                rec.forensics.error_type,
                rec.forensics.error_message.as_deref().map(|m| head_chars(m, ERROR_MESSAGE_MAX)),
                rec.forensics.third_party as i64,
                rec.forensics.rewrites,
                rec.forensics.device_id_out,
                rec.forensics
                    .response_excerpt
                    .as_deref()
                    .map(|m| head_chars(m, RESPONSE_EXCERPT_MAX)),
                rec.forensics.sim_reason,
                rec.forensics.session_key,
                rec.forensics.session_id_in,
            ],
        )?;
        // 刚封的号：封号事件落地时冻结的是**当时已有**的流水，而触发封号的那一发（以及同时
        // 在途的几发）要等响应流结束才落库，冻结时还不存在。故封后 FREEZE_TAIL_SECS 内到达
        // 的这个号的流水，写入时顺手补进冻结表——不然最要紧的那一条恰好缺席。
        if let Some(cid) = rec.cred_id {
            let recent_ban: Option<i64> = tx
                .query_row(
                    "SELECT id FROM ban_events WHERE cred_id = ?1 AND ts >= ?2 - ?3
                      ORDER BY id DESC LIMIT 1",
                    params![cid, ts, FREEZE_TAIL_SECS],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(ban_id) = recent_ban {
                let row_id = tx.last_insert_rowid();
                tx.execute(
                    &format!(
                        "INSERT INTO usage_logs_frozen (ban_event_id, src_id, {USAGE_LOG_COLS})
                         SELECT ?1, id, {USAGE_LOG_COLS} FROM usage_logs WHERE id = ?2"
                    ),
                    params![ban_id, row_id],
                )?;
            }
        }
        // 落账。cred_id 为空的流水（还没选到凭证就失败的请求）无处归属，只记日志不记账。
        if let Some(cid) = rec.cred_id {
            tx.execute(
                "INSERT INTO credential_stats (cred_id, last_used_at, cost_total_usd)
                 VALUES (?1, ?2, COALESCE(?3, 0))
                 ON CONFLICT(cred_id) DO UPDATE SET
                     last_used_at   = excluded.last_used_at,
                     cost_total_usd = cost_total_usd + COALESCE(?3, 0)",
                params![cid, ts, rec.cost_usd],
            )?;
            // 快照只在响应带**窗口级**限流信息时覆盖，口径同旧版「最新一条带限流信息的行」
            // ——更晚的普通响应不能把快照抹掉。
            //
            // 判据里的 `!rec.windows.is_empty()` 不是冗余：旧口径只认 5h/7d 两个专用字段，
            // 于是一个只上报 `7d_oi` 之类窗口的账号**永远写不进快照**，卡片恒为「暂无数据」，
            // 哪怕它此刻正靠 usage credits 放行。窗口种类是上游说了算的，判据不能写死窗口名。
            //
            // 仍然不认「只有 unified_status / overage_in_use、一个窗口都没有」的响应：
            // 那种覆盖会把已有的窗口列一并抹成空，拿一条信息更少的快照换掉信息更多的。
            if rec.rl_5h_utilization.is_some()
                || rec.rl_7d_utilization.is_some()
                || !rec.windows.is_empty()
            {
                // 序列化失败在这里不可达（三个 Option + String 的定长结构），真失败也只是
                // 少存这一列，不该把整条用量日志连坐掉。
                let windows = serde_json::to_string(&rec.windows).ok();
                tx.execute(
                    "UPDATE credential_stats SET
                         snapshot_ts = ?2, unified_status = ?3,
                         rl_5h_utilization = ?4, rl_5h_reset = ?5,
                         rl_7d_utilization = ?6, rl_7d_reset = ?7, rl_representative = ?8,
                         overage_in_use = ?9, windows = ?10
                      WHERE cred_id = ?1",
                    params![
                        cid,
                        ts,
                        rec.unified_status,
                        rec.rl_5h_utilization,
                        rec.rl_5h_reset,
                        rec.rl_7d_utilization,
                        rec.rl_7d_reset,
                        rec.rl_representative,
                        rec.rl_overage_in_use,
                        windows,
                    ],
                )?;
            }
            // 只要认得出设备就记一笔：**请求数无条件 +1**，费用取不到（模型未知）时按 0 计。
            // 不能像费用那样连请求数一起跳过——4xx/429 这些没有 usage 的请求同样是这台设备
            // 打出去的，漏掉它们会让「请求数」少一大截，而排查限流恰恰要看这些。
            if let Some(dev) = &rec.device_id {
                let cost = rec.cost_usd.unwrap_or(0.0);
                tx.execute(
                    "INSERT INTO device_costs (device_id, cred_id, cost_usd, request_count)
                          VALUES (?1, ?2, ?3, 1)
                     ON CONFLICT(device_id, cred_id) DO UPDATE
                            SET cost_usd = cost_usd + ?3, request_count = request_count + 1",
                    params![dev, cid, cost],
                )?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// 缓存命中率趋势的桶。回三个原始数（输入、命中、写入），比率由前端算——一个 300 token
    /// 的小时里的「命中 0%」与 17K 前缀那种小时里的「命中 94%」是两件事，光看比率判断不了。
    ///
    /// `bucket_secs` 是桶宽；`tz_offset_secs` 是本地时区相对 UTC 的偏移，按天分桶时桶边界
    /// 落在**本地**零点上——前端按本地日期铺格子，后端不按同一套边界切，日桶就会跨两天。
    /// `input_tokens` 是全部输入 token（含缓存命中与缓存写入）。
    pub fn cache_series(
        &self,
        since: i64,
        bucket_secs: i64,
        tz_offset_secs: i64,
    ) -> Result<Vec<CacheBucket>> {
        let bucket_secs = bucket_secs.max(1);
        let conn = self.read_conn();
        let mut stmt = conn.prepare(
            "SELECT ((ts + ?3) / ?2) * ?2 - ?3 AS bucket,
                    SUM(
                        COALESCE(input_tokens, 0)
                        + COALESCE(cache_creation_tokens,
                                   COALESCE(cache_5m_tokens, 0) + COALESCE(cache_1h_tokens, 0))
                        + COALESCE(cache_read_tokens, 0)
                    ),
                    COALESCE(SUM(cache_read_tokens), 0),
                    COALESCE(SUM(COALESCE(cache_creation_tokens,
                                          COALESCE(cache_5m_tokens, 0) + COALESCE(cache_1h_tokens, 0))), 0)
               FROM usage_logs
              WHERE ts >= ?1
              GROUP BY bucket
              HAVING SUM(
                        COALESCE(input_tokens, 0)
                        + COALESCE(cache_creation_tokens,
                                   COALESCE(cache_5m_tokens, 0) + COALESCE(cache_1h_tokens, 0))
                        + COALESCE(cache_read_tokens, 0)
                     ) > 0
              ORDER BY bucket",
        )?;
        let rows = stmt.query_map(params![since, bucket_secs, tz_offset_secs], |r| {
            Ok(CacheBucket {
                ts: r.get(0)?,
                input_tokens: r.get(1)?,
                cached_tokens: r.get(2)?,
                written_tokens: r.get(3)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 趋势接口一次要的三样：各桶、整窗口合计、近 60 分钟合计。整窗口合计直接把各桶加起来
    /// （桶是窗口的划分，不必再扫一遍），近 1 小时另走一次 1 小时的小范围扫描。
    pub fn cache_report(
        &self,
        since: i64,
        bucket_secs: i64,
        tz_offset_secs: i64,
    ) -> Result<CacheReport> {
        let points = self.cache_series(since, bucket_secs, tz_offset_secs)?;
        let summary = points.iter().fold(CacheBucket::empty(since), |mut acc, b| {
            acc.input_tokens += b.input_tokens;
            acc.cached_tokens += b.cached_tokens;
            acc.written_tokens += b.written_tokens;
            acc
        });
        let now: i64 = self.read_conn().query_row("SELECT unixepoch()", [], |r| r.get(0))?;
        let recent = self.cache_summary(now - 3600)?;
        Ok(CacheReport { points, summary, recent })
    }

    /// `since` 起到现在的缓存三段 token 合计（一个桶），`ts` 是 `since`。给「近 1 小时」与
    /// 整个窗口的汇总用。
    pub fn cache_summary(&self, since: i64) -> Result<CacheBucket> {
        // 桶宽取一个远大于窗口的数，所有行落进同一个桶；偏移取 since 让桶起点等于 since。
        let mut rows = self.cache_series(since, 1 << 40, -since)?;
        Ok(rows
            .pop()
            .map(|b| CacheBucket { ts: since, ..b })
            .unwrap_or_else(|| CacheBucket::empty(since)))
    }

    /// `since` 起的成功请求（status = 200 且记了 TTFT）的延迟原始行，连同 SQLite 此刻的时钟
    /// （近 1 小时的窗口按它算，与写入侧同源）。WHERE 与 `idx_usage_logs_latency` 的部分谓词
    /// 逐字对应，整条查询在索引里走完、不回表；不排序——分桶用 BTreeMap，分位数各桶自己排。
    fn latency_rows(&self, since: i64) -> Result<(i64, Vec<LatencyRow>)> {
        let conn = self.read_conn();
        let now: i64 = conn.query_row("SELECT unixepoch()", [], |r| r.get(0))?;
        let mut stmt = conn.prepare(LATENCY_ROWS_SQL)?;
        let rows = stmt.query_map([since], |r| {
            Ok(LatencyRow {
                ts: r.get(0)?,
                ttft_ms: r.get(1)?,
                total_ms: r.get(2)?,
                output_tokens: r.get(3)?,
            })
        })?;
        Ok((now, rows.collect::<rusqlite::Result<Vec<_>>>()?))
    }

    /// 趋势接口一次要的三样：各桶、整窗口、近 60 分钟——同一次扫描分三路汇总，不扫三遍。
    pub fn ttft_report(
        &self,
        since: i64,
        bucket_secs: i64,
        tz_offset_secs: i64,
    ) -> Result<TtftReport> {
        let bucket_secs = bucket_secs.max(1);
        let (now, rows) = self.latency_rows(since)?;
        let mut buckets: std::collections::BTreeMap<i64, Vec<LatencyRow>> = Default::default();
        let mut recent_rows = Vec::new();
        for r in &rows {
            let bucket =
                ((r.ts + tz_offset_secs).div_euclid(bucket_secs)) * bucket_secs - tz_offset_secs;
            buckets.entry(bucket).or_default().push(*r);
            if r.ts >= now - 3600 {
                recent_rows.push(*r);
            }
        }
        Ok(TtftReport {
            points: buckets.into_iter().map(|(ts, rows)| summarize_latency(ts, &rows)).collect(),
            summary: summarize_latency(since, &rows),
            recent: summarize_latency(now - 3600, &recent_rows),
        })
    }

    /// TTFT（首字时延）趋势的桶：每桶平均、p50、p95、请求数与输出吞吐。分位数没法从更细
    /// 的桶合并出来，所以桶宽与时区偏移由调用方按前端要画的格子给（同 [`Self::cache_series`]），
    /// 在 Rust 里对每桶的原始值排序取分位——保留期量级也就几十万个整数，没有压力。
    #[cfg(test)]
    pub fn ttft_series(
        &self,
        since: i64,
        bucket_secs: i64,
        tz_offset_secs: i64,
    ) -> Result<Vec<TtftBucket>> {
        Ok(self.ttft_report(since, bucket_secs, tz_offset_secs)?.points)
    }

    /// `since` 起到现在的延迟汇总（一个桶），`ts` 是 `since`。线上走 [`Self::ttft_report`]
    /// 一次拿齐，这个只给测试核对。
    #[cfg(test)]
    pub fn ttft_summary(&self, since: i64) -> Result<TtftBucket> {
        let (_, rows) = self.latency_rows(since)?;
        Ok(summarize_latency(since, &rows))
    }

    /// `since` 起按模型或按账号拆开的用量：每组的请求数、延迟分位与吞吐、缓存三段 token，
    /// 按请求数降序，最多 `limit` 行。一次把窗口内的原始行拉回来在 Rust 里聚合——分位数在
    /// SQL 里算不了，而这张表只在打开趋势对话框时查一次。
    pub fn usage_breakdown(
        &self,
        since: i64,
        by: BreakdownBy,
        limit: usize,
    ) -> Result<Vec<BreakdownRow>> {
        struct Group {
            label: String,
            tier: Option<String>,
            requests: i64,
            latency: Vec<LatencyRow>,
            cache: CacheBucket,
            saved_usd: f64,
        }
        let conn = self.read_conn();
        let key_expr = match by {
            BreakdownBy::Model => "COALESCE(u.model, '')",
            BreakdownBy::Account => "COALESCE(CAST(u.cred_id AS TEXT), '')",
        };
        let label_expr = match by {
            BreakdownBy::Model => "COALESCE(u.model, '')",
            BreakdownBy::Account => {
                // 已删账号的流水留到保留期满（见 remove），账号表里没它了就退回流水自带的名字。
                "COALESCE(c.label, NULLIF(u.cred_label, ''), '#' || COALESCE(CAST(u.cred_id AS TEXT), '?'))"
            }
        };
        let mut stmt = conn.prepare(&format!(
            "SELECT {key_expr}, {label_expr}, u.ts, u.status, u.ttft_ms, u.total_ms, u.output_tokens,
                    COALESCE(u.input_tokens, 0),
                    COALESCE(u.cache_read_tokens, 0),
                    u.cache_creation_tokens, u.cache_5m_tokens, u.cache_1h_tokens,
                    u.model, c.tier
               FROM usage_logs u
               LEFT JOIN credentials c ON c.id = u.cred_id
              WHERE u.ts >= ?1"
        ))?;
        let mut groups: std::collections::HashMap<String, Group> = Default::default();
        let mut rows = stmt.query([since])?;
        while let Some(r) = rows.next()? {
            let key: String = r.get(0)?;
            let label: String = r.get(1)?;
            let ts: i64 = r.get(2)?;
            let status: i64 = r.get(3)?;
            let ttft_ms: Option<i64> = r.get(4)?;
            let total_ms: Option<i64> = r.get(5)?;
            let output_tokens: Option<i64> = r.get(6)?;
            let plain: i64 = r.get(7)?;
            let cached: i64 = r.get(8)?;
            let creation: Option<i64> = r.get(9)?;
            let c5: Option<i64> = r.get(10)?;
            let c1: Option<i64> = r.get(11)?;
            let model: Option<String> = r.get(12)?;
            let tier: Option<String> = r.get(13)?;
            let written = creation.unwrap_or(c5.unwrap_or(0) + c1.unwrap_or(0));
            let g = groups.entry(key).or_insert_with(|| Group {
                label,
                tier: if by == BreakdownBy::Account { tier } else { None },
                requests: 0,
                latency: Vec::new(),
                cache: CacheBucket::empty(since),
                saved_usd: 0.0,
            });
            g.requests += 1;
            g.cache.input_tokens += plain + written + cached;
            g.cache.cached_tokens += cached;
            g.cache.written_tokens += written;
            g.saved_usd += cache_saved_usd(model.as_deref(), plain, cached, creation, c5, c1);
            if status == 200
                && let Some(ttft_ms) = ttft_ms
            {
                g.latency.push(LatencyRow { ts, ttft_ms, total_ms, output_tokens });
            }
        }
        let mut out: Vec<BreakdownRow> = groups
            .into_iter()
            .map(|(key, g)| BreakdownRow {
                key,
                label: g.label,
                tier: g.tier,
                requests: g.requests,
                cache_saved_usd: g.saved_usd,
                latency: summarize_latency(since, &g.latency),
                cache: g.cache,
            })
            .collect();
        out.sort_by(|a, b| b.requests.cmp(&a.requests).then_with(|| a.key.cmp(&b.key)));
        out.truncate(limit);
        Ok(out)
    }

    /// 单个账号 `since` 起的用量统计：按时间分桶（桶宽与时区偏移同 [`Self::ttft_report`]）、
    /// 整个窗口的合计，以及按模型 / 设备 / 来访客户端 / 状态码四个维度拆开的分组（各自按请求数
    /// 降序、最多 `group_limit` 组）。一次扫描把这几路都汇总出来——走 `idx_usage_logs_cred_usage` 的前缀
    /// 卡住账号与起点，量级是单号保留期内的流水，在 Rust 里聚合比拼五条 GROUP BY 省一半扫描。
    pub fn credential_stats(
        &self,
        cred_id: i64,
        since: i64,
        bucket_secs: i64,
        tz_offset_secs: i64,
        group_limit: usize,
    ) -> Result<CredentialStats> {
        let bucket_secs = bucket_secs.max(1);
        let conn = self.read_conn();
        let mut stmt = conn.prepare(
            "SELECT ts, status, COALESCE(model, ''), COALESCE(device_id, ''), COALESCE(ua, ''),
                    COALESCE(input_tokens, 0), COALESCE(output_tokens, 0),
                    cache_creation_tokens, cache_5m_tokens, cache_1h_tokens,
                    COALESCE(cache_read_tokens, 0), COALESCE(cost_usd, 0),
                    COALESCE(rewrites LIKE 'rejected_locally%', 0)
               FROM usage_logs
              WHERE cred_id = ?1 AND ts >= ?2",
        )?;
        let mut buckets: std::collections::BTreeMap<i64, CredentialStatsBucket> =
            Default::default();
        let mut summary = CredentialStatsBucket { ts: since, ..Default::default() };
        let mut by_model: std::collections::HashMap<String, CredentialStatsGroup> =
            Default::default();
        let mut by_device: std::collections::HashMap<String, CredentialStatsGroup> =
            Default::default();
        let mut by_client: std::collections::HashMap<String, CredentialStatsGroup> =
            Default::default();
        let mut by_status: std::collections::HashMap<String, CredentialStatsGroup> =
            Default::default();
        let mut rows = stmt.query(params![cred_id, since])?;
        while let Some(r) = rows.next()? {
            let ts: i64 = r.get(0)?;
            let status: i64 = r.get(1)?;
            let model: String = r.get(2)?;
            let device: String = r.get(3)?;
            let ua: String = r.get(4)?;
            let creation: Option<i64> = r.get(7)?;
            let c5: Option<i64> = r.get(8)?;
            let c1: Option<i64> = r.get(9)?;
            let row = CredentialStatsBucket {
                ts,
                requests: 1,
                errors: i64::from(!(200..300).contains(&status)),
                rejected: r.get(12)?,
                input_tokens: r.get(5)?,
                output_tokens: r.get(6)?,
                // 同 usage_breakdown：老记录只有细分档没有合计列时，拿两档相加兜底。
                cache_write_tokens: creation.unwrap_or(c5.unwrap_or(0) + c1.unwrap_or(0)),
                cache_read_tokens: r.get(10)?,
                cost_usd: r.get(11)?,
            };
            let bucket =
                ((ts + tz_offset_secs).div_euclid(bucket_secs)) * bucket_secs - tz_offset_secs;
            buckets
                .entry(bucket)
                .or_insert(CredentialStatsBucket { ts: bucket, ..Default::default() })
                .add(&row);
            summary.add(&row);
            for (groups, key) in [
                (&mut by_model, model),
                (&mut by_device, device),
                (&mut by_client, ua),
                (&mut by_status, status.to_string()),
            ] {
                groups
                    .entry(key.clone())
                    .or_insert_with(|| CredentialStatsGroup { key, ..Default::default() })
                    .add(&row);
            }
        }
        let ranked = |groups: std::collections::HashMap<String, CredentialStatsGroup>| {
            let mut out: Vec<_> = groups.into_values().collect();
            out.sort_by(|a, b| b.requests.cmp(&a.requests).then_with(|| a.key.cmp(&b.key)));
            out.truncate(group_limit);
            out
        };
        Ok(CredentialStats {
            points: buckets.into_values().collect(),
            summary,
            by_model: ranked(by_model),
            by_device: ranked(by_device),
            by_client: ranked(by_client),
            by_status: ranked(by_status),
        })
    }

    /// `since` 起本地拒绝的条数，按原因分类（`rewrites` 里 `rejected_locally:<kind>` 的 kind；
    /// 没分类的算 `other`），按条数降序。给概览「近 1 小时被拒了多少、为什么」用。
    pub fn local_rejections(&self, since: i64) -> Result<Vec<(String, i64)>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare(
            "SELECT rewrites, COUNT(*) FROM usage_logs
              WHERE ts >= ?1 AND rewrites LIKE 'rejected_locally%'
              GROUP BY rewrites",
        )?;
        let mut counts: std::collections::HashMap<String, i64> = Default::default();
        for row in stmt.query_map([since], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? {
            let (tag, n) = row?;
            let kind =
                tag.split_once(':').map(|(_, k)| k.to_string()).unwrap_or_else(|| "other".into());
            *counts.entry(kind).or_default() += n;
        }
        let mut out: Vec<(String, i64)> = counts.into_iter().collect();
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Ok(out)
    }

    /// 删掉「连保留期都过了」的设备绑定与会话绑定，返回两张表各删了多少行。
    ///
    /// 这件事此前挂在 [`Self::select_with_slot`] 里、每条转发请求跑一遍，而两条 DELETE 都按
    /// `last_seen_at` 划线——那一列当时没有索引，于是每请求两次全表扫加两次写事务，全程还
    /// 压着那把全局 `conn` 锁。现在索引补上了（`idx_*_bindings_seen`），清理本身也挪到了
    /// 后台定时（见 `web::run`）。
    ///
    /// **清理时机不影响选号**：过了保留期、还没被删掉的行在
    /// [`Self::select_with_slot`] 那侧已被显式滤掉（`bound` 查询上那道保留期条件），
    /// 故「还在表里」与「已经删了」对选号是同一个结果，这里跑得早跑得晚都一样。
    ///
    /// TTL 与保留期都取当前配置：两项都可以在后台改，改小之后下一次清理就按新值收。
    /// 任一项配成 `<= 0`（永不过期／永久保留）时那张表整张不动，见 [`effective_retention`]。
    pub fn prune_expired_bindings(&self) -> Result<(usize, usize)> {
        // 两个 getter 内部各自取锁，parking_lot 不可重入，故都在拿 `conn` 之前读完。
        let device =
            effective_retention(self.device_binding_ttl(), self.device_binding_retention());
        let session =
            effective_retention(self.session_binding_ttl(), self.session_binding_retention());
        let conn = self.conn.lock();
        let devices = match device {
            Some(secs) => conn.execute(
                "DELETE FROM device_bindings WHERE last_seen_at < unixepoch() - ?1",
                [secs],
            )?,
            None => 0,
        };
        let sessions = match session {
            Some(secs) => conn.execute(
                "DELETE FROM session_bindings WHERE last_seen_at < unixepoch() - ?1",
                [secs],
            )?,
            None => 0,
        };
        Ok((devices, sessions))
    }

    /// 裁掉超过保留期（[`USAGE_LOG_RETENTION_SECS`]）的用量日志流水，返回删除条数。
    ///
    /// 流水裁剪不影响任何终身口径——最近使用/累计费用/最新快照都在账本里
    /// （credential_stats / device_costs，写时落账）；还要读流水的只剩两处：
    /// 5h/7d 窗口统计（最多回看 7 天多）和请求日志页（只翻近期），8 天都覆盖得住。
    ///
    /// 分批删：日志表可能积了几百万行，一条大 DELETE 会把写锁按住很久，转发路径的
    /// 落库全得排队。批间放锁、歇一下，让在线写入插队。
    ///
    /// 一批 500 行：每行 3KB 上下、挂着十来条索引，删一行要动的页不少。线上规模实测一批
    /// 5000 行持锁近 2 秒（每天一次、连着十几批），500 行约 0.1 秒。
    ///
    /// 会睡眠，只能在阻塞线程里调（见 `web::run` 的 `spawn_blocking`）。
    pub fn prune_usage_logs(&self) -> Result<usize> {
        const BATCH: usize = 500;
        const PAUSE: Duration = Duration::from_millis(50);
        let mut total = 0;
        loop {
            let n = self.conn.lock().execute(
                "DELETE FROM usage_logs WHERE id IN (
                     SELECT id FROM usage_logs WHERE ts < unixepoch() - ?1 LIMIT ?2)",
                params![USAGE_LOG_RETENTION_SECS, BATCH as i64],
            )?;
            total += n;
            if n < BATCH {
                break;
            }
            std::thread::sleep(PAUSE);
        }
        Ok(total)
    }

    /// 最近的用量日志，按时间倒序，最多 `limit` 条。测试用；线上那两条路径都带筛选，
    /// 直接走 [`Self::query_usage_logs`]。
    #[cfg(test)]
    pub fn list_usage_logs(&self, limit: i64) -> Result<Vec<UsageLog>> {
        self.query_usage_logs(UsageLogQuery { limit, ..Default::default() })
    }

    /// 同一批筛选条件下的条数、花费合计与最大 id。见 [`UsageLogStats`]。
    ///
    /// `q` 里的 `limit`/`offset` **不参与**——统计的是整个集合，不是当前这一页。
    pub fn usage_log_stats(&self, q: UsageLogQuery) -> Result<UsageLogStats> {
        let (where_sql, params) = q.where_clause();
        let conn = self.read_conn();
        conn.query_row(&usage_log_stats_sql(&where_sql), rusqlite::params_from_iter(params), |r| {
            Ok(UsageLogStats { total: r.get(0)?, cost_usd: r.get(1)?, max_id: r.get(2)? })
        })
        .map_err(Into::into)
    }

    /// 按条件查用量流水，恒按 `id` 倒序。见 [`UsageLogQuery`] 与 [`usage_log_page_sql`]。
    pub fn query_usage_logs(&self, q: UsageLogQuery) -> Result<Vec<UsageLog>> {
        let (where_sql, mut params) = q.where_clause();
        params.push(rusqlite::types::Value::Integer(q.limit));
        params.push(rusqlite::types::Value::Integer(q.offset));
        let conn = self.read_conn();
        let mut stmt = conn.prepare(&usage_log_page_sql(&where_sql, params.len()))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params), usage_log_from_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    // ---------- 封号事件 ----------

    /// 自动停用并**落一条封号事件**（[`BanContext`]），同时把该号最近
    /// [`FREEZE_WINDOW_SECS`] 的流水冻结进 `usage_logs_frozen`。
    ///
    /// 事件与冻结流水都是取证材料：解封不清、删号不删、裁剪不碰（对比 `credentials.ban_reason`
    /// 会在重新启用时被清空、`usage_logs` 会随删号级联删除并只留保留期内的）。
    ///
    /// 事件里除了上游给的那几句，还**当场快照**一组账号侧读数（等级、组织类型、代理、账龄、
    /// 终身请求数与费用、封前 7 天的请求数/设备数/模型/客户端）：这些在事后从别处凑不齐——
    /// 账号可能已被删、流水可能已被裁，而它们正是拿被封的号和活着的号对照时最先要看的列。
    ///
    /// 凭证不存在时返回 `false`（此时也不落事件——没有主体）。
    pub fn record_ban(&self, id: i64, ctx: &BanContext) -> Result<bool> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let ts: i64 = tx.query_row("SELECT unixepoch()", [], |r| r.get(0))?;
        tx.execute("DELETE FROM device_bindings WHERE cred_id = ?1", [id])?;
        tx.execute("DELETE FROM session_bindings WHERE cred_id = ?1", [id])?;
        let updated = tx.execute(
            "UPDATE credentials SET disabled = 1, ban_reason = ?2, resume_at = NULL, \
                    updated_at = unixepoch() \
             WHERE id = ?1",
            params![id, ctx.reason],
        )? > 0;
        if !updated {
            tx.commit()?;
            return Ok(false);
        }
        // 账号侧快照。
        let (label, tier, org_type, proxy, created_at): (
            String,
            Option<String>,
            Option<String>,
            Option<String>,
            i64,
        ) = tx.query_row(
            "SELECT label, tier, org_type, proxy, created_at FROM credentials WHERE id = ?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
        )?;
        let (lifetime_cost, last_used_at): (f64, Option<i64>) = tx
            .query_row(
                "SELECT cost_total_usd, last_used_at FROM credential_stats WHERE cred_id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((0.0, None));
        let lifetime_requests: i64 = tx.query_row(
            "SELECT COALESCE(SUM(request_count), 0) FROM device_costs WHERE cred_id = ?1",
            [id],
            |r| r.get(0),
        )?;
        let since = ts - FREEZE_WINDOW_SECS;
        // 设备数分两侧：device_id 是来访客户端自报的，device_id_out 是实际发给 Anthropic 的
        // （伪装开着时是派生值）。上游看到的是后者——「一个号在上游眼里有几台设备」看它。
        let (requests_7d, devices_7d, devices_out_7d): (i64, i64, i64) = tx.query_row(
            "SELECT COUNT(*), COUNT(DISTINCT device_id), COUNT(DISTINCT device_id_out)
               FROM usage_logs WHERE cred_id = ?1 AND ts >= ?2",
            params![id, since],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let distinct = |col: &str| -> Result<String> {
            let mut st = tx.prepare(&format!(
                "SELECT {col}, COUNT(*) AS n FROM usage_logs
                  WHERE cred_id = ?1 AND ts >= ?2 AND {col} IS NOT NULL
                  GROUP BY {col} ORDER BY n DESC LIMIT 50"
            ))?;
            let rows = st.query_map(params![id, since], |r| {
                Ok(serde_json::json!({ "value": r.get::<_, String>(0)?, "count": r.get::<_, i64>(1)? }))
            })?;
            Ok(serde_json::Value::Array(rows.collect::<rusqlite::Result<Vec<_>>>()?).to_string())
        };
        let models_7d = distinct("model")?;
        let uas_7d = distinct("ua")?;
        let proxies_7d = distinct("proxy")?;
        let device_ids_out_7d = distinct("device_id_out")?;
        // 封前最后一条流水的限流快照：额度是不是早就满了、在不在烧 credits。
        let (last_unified, last_overage): (Option<String>, Option<i64>) = tx
            .query_row(
                "SELECT unified_status, overage_in_use FROM credential_stats WHERE cred_id = ?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?
            .unwrap_or((None, None));
        tx.execute(
            "INSERT INTO ban_events
                (ts, cred_id, cred_label, source, reason, status, error_type, error_message,
                 request_id, upstream_request_id, tier, org_type, proxy, account_created_at,
                 lifetime_requests, lifetime_cost_usd, last_used_at, requests_7d, devices_7d,
                 models_7d, uas_7d, proxies_7d, last_unified_status, last_overage_in_use,
                 devices_out_7d, device_ids_out_7d)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                     ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)",
            params![
                ts,
                id,
                label,
                ctx.source,
                ctx.reason,
                ctx.status.map(i64::from),
                ctx.error_type,
                ctx.error_message.as_deref().map(|m| head_chars(m, ERROR_MESSAGE_MAX * 4)),
                ctx.request_id,
                ctx.upstream_request_id,
                tier,
                org_type,
                proxy.as_deref().map(redact_proxy),
                created_at,
                lifetime_requests,
                lifetime_cost,
                last_used_at,
                requests_7d,
                devices_7d,
                models_7d,
                uas_7d,
                proxies_7d,
                last_unified,
                last_overage,
                devices_out_7d,
                device_ids_out_7d,
            ],
        )?;
        let ban_id = tx.last_insert_rowid();
        let frozen = tx.execute(
            &format!(
                "INSERT INTO usage_logs_frozen (ban_event_id, src_id, {USAGE_LOG_COLS})
                 SELECT ?1, id, {USAGE_LOG_COLS} FROM usage_logs
                  WHERE cred_id = ?2 AND ts >= ?3 ORDER BY id"
            ),
            params![ban_id, id, since],
        )?;
        tx.execute(
            "UPDATE ban_events SET frozen_rows = ?2 WHERE id = ?1",
            params![ban_id, frozen as i64],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// 封号事件列表（新的在前）。`cred_id` 为 `Some` 时只看那个号（含已删的号：事件按 id 存，
    /// 不随删号消失）。
    pub fn list_ban_events(&self, cred_id: Option<i64>, limit: i64) -> Result<Vec<BanEvent>> {
        let conn = self.read_conn();
        let (where_sql, params): (&str, Vec<rusqlite::types::Value>) = match cred_id {
            Some(c) => (" WHERE cred_id = ?1", vec![c.into(), limit.into()]),
            None => ("", vec![limit.into()]),
        };
        let n = params.len();
        let mut stmt = conn.prepare(&format!(
            "SELECT id, ts, cred_id, cred_label, source, reason, status, error_type, error_message,
                    request_id, upstream_request_id, tier, org_type, proxy, account_created_at,
                    lifetime_requests, lifetime_cost_usd, last_used_at, requests_7d, devices_7d,
                    models_7d, uas_7d, proxies_7d, last_unified_status, last_overage_in_use,
                    frozen_rows, devices_out_7d, device_ids_out_7d
               FROM ban_events{where_sql} ORDER BY id DESC LIMIT ?{n}"
        ))?;
        let rows = stmt.query_map(rusqlite::params_from_iter(params), |r| {
            let json_list = |i: usize| -> rusqlite::Result<Vec<serde_json::Value>> {
                let raw: Option<String> = r.get(i)?;
                Ok(raw
                    .and_then(|t| serde_json::from_str::<Vec<serde_json::Value>>(&t).ok())
                    .unwrap_or_default())
            };
            Ok(BanEvent {
                id: r.get(0)?,
                ts: r.get(1)?,
                cred_id: r.get(2)?,
                cred_label: r.get(3)?,
                source: r.get(4)?,
                reason: r.get(5)?,
                status: r.get::<_, Option<i64>>(6)?.map(|v| v as u16),
                error_type: r.get(7)?,
                error_message: r.get(8)?,
                request_id: r.get(9)?,
                upstream_request_id: r.get(10)?,
                tier: r.get(11)?,
                org_type: r.get(12)?,
                proxy: r.get(13)?,
                account_created_at: r.get(14)?,
                lifetime_requests: r.get(15)?,
                lifetime_cost_usd: r.get(16)?,
                last_used_at: r.get(17)?,
                requests_7d: r.get(18)?,
                devices_7d: r.get(19)?,
                models_7d: json_list(20)?,
                uas_7d: json_list(21)?,
                proxies_7d: json_list(22)?,
                last_unified_status: r.get(23)?,
                last_overage_in_use: r.get::<_, Option<i64>>(24)?.map(|v| v != 0),
                frozen_rows: r.get(25)?,
                devices_out_7d: r.get::<_, Option<i64>>(26)?.unwrap_or(0),
                device_ids_out_7d: json_list(27)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// 每个凭证被自动封停过几次（cred_id → 次数）；没封过的号不出现。
    pub fn ban_counts(&self) -> Result<HashMap<i64, i64>> {
        let conn = self.read_conn();
        let mut stmt = conn.prepare("SELECT cred_id, COUNT(*) FROM ban_events GROUP BY cred_id")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<HashMap<_, _>>>()?)
    }

    /// 某封号事件冻结下来的**一页**流水（按时间正序，最早的在前，读起来是一条时间线），
    /// 连同该事件冻结的总条数一起给出。
    ///
    /// 一次封号常冻下上千行、几十 MB（每行还带形态摘要 JSON），整份吐给页面既慢又白读，
    /// 所以这里只给一页。总条数与当页在同一个读事务里取：封号后的补冻结（见
    /// `insert_usage_log_at`）还会往冻结表追加行，两条语句若各看各的快照，总数与当页会差一条。
    /// `id` 是冻结表自己的主键；原流水的 id 不返回——它在原表里可能早已被裁掉。
    pub fn frozen_usage_logs(
        &self,
        ban_event_id: i64,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<UsageLog>)> {
        let conn = self.read_conn();
        let tx = conn.unchecked_transaction()?;
        let total: i64 = tx.query_row(
            "SELECT COUNT(*) FROM usage_logs_frozen WHERE ban_event_id = ?1",
            [ban_event_id],
            |r| r.get(0),
        )?;
        let mut stmt = tx.prepare(&format!(
            "SELECT id, {USAGE_LOG_COLS} FROM usage_logs_frozen
              WHERE ban_event_id = ?1 ORDER BY ts, id LIMIT ?2 OFFSET ?3"
        ))?;
        let logs = stmt
            .query_map([ban_event_id, limit, offset], usage_log_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        // 只读事务，提交与回滚等价；显式结束，别等 drop 时静默回滚吞掉错误。
        tx.commit()?;
        Ok((total, logs))
    }
}

/// `usage_logs` 与 `usage_logs_frozen` 共用的列清单（不含各自的主键）。**读与写都用它**：
/// 冻结是 `INSERT … SELECT` 逐列照搬，两张表的列必须一一对齐，清单只此一份才不会漂。
/// 顺序即 [`usage_log_from_row`] 的下标顺序（从 1 起，0 号是主键）。
const USAGE_LOG_COLS: &str = "ts, cred_id, cred_label, device_id, model, path, status, has_usage,
        input_tokens, output_tokens, cache_creation_tokens, cache_5m_tokens,
        cache_1h_tokens, cache_read_tokens, ttft_ms, total_ms,
        unified_status, rl_5h_status, rl_5h_reset, rl_5h_utilization,
        rl_7d_status, rl_7d_reset, rl_7d_utilization, rl_representative, ratelimit_raw,
        cost_usd, rl_overage_in_use, ua, ua_out, sse_aggregated,
        request_id, upstream_request_id,
        proxy, simulated, shape, session_id, error_type, error_message, third_party, rewrites,
        device_id_out, response_excerpt, sim_reason, session_key, session_id_in";

/// 按 [`USAGE_LOG_COLS`] 的顺序把一行读成 [`UsageLog`]（0 号列是主键）。
fn usage_log_from_row(r: &Row<'_>) -> rusqlite::Result<UsageLog> {
    Ok(UsageLog {
        id: r.get(0)?,
        ts: r.get(1)?,
        cred_id: r.get(2)?,
        cred_label: r.get(3)?,
        device_id: r.get(4)?,
        model: r.get(5)?,
        path: r.get(6)?,
        status: r.get::<_, i64>(7)? as u16,
        has_usage: r.get::<_, i64>(8)? != 0,
        input_tokens: r.get(9)?,
        output_tokens: r.get(10)?,
        cache_creation_tokens: r.get(11)?,
        cache_5m_tokens: r.get(12)?,
        cache_1h_tokens: r.get(13)?,
        cache_read_tokens: r.get(14)?,
        ttft_ms: r.get(15)?,
        total_ms: r.get(16)?,
        unified_status: r.get(17)?,
        rl_5h_status: r.get(18)?,
        rl_5h_reset: r.get(19)?,
        rl_5h_utilization: r.get(20)?,
        rl_7d_status: r.get(21)?,
        rl_7d_reset: r.get(22)?,
        rl_7d_utilization: r.get(23)?,
        rl_representative: r.get(24)?,
        ratelimit_raw: r.get(25)?,
        cost_usd: r.get(26)?,
        rl_overage_in_use: r.get(27)?,
        ua: r.get(28)?,
        ua_out: r.get(29)?,
        sse_aggregated: r.get::<_, i64>(30)? != 0,
        request_id: r.get(31)?,
        upstream_request_id: r.get(32)?,
        forensics: Forensics {
            proxy: r.get(33)?,
            simulated: r.get::<_, Option<i64>>(34)?.unwrap_or(0) != 0,
            shape: r.get(35)?,
            session_id: r.get(36)?,
            error_type: r.get(37)?,
            error_message: r.get(38)?,
            third_party: r.get::<_, Option<i64>>(39)?.unwrap_or(0) != 0,
            rewrites: r.get(40)?,
            device_id_out: r.get(41)?,
            response_excerpt: r.get(42)?,
            sim_reason: r.get(43)?,
            session_key: r.get(44)?,
            session_id_in: r.get(45)?,
        },
    })
}

/// 按字符截断（不是按字节：文案里有中文，按字节切会切在多字节中间）。
fn head_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n).collect() }
}

/// 封号时冻结的流水回看窗口：7 天。上游的额度窗口最长 7 天，判封的依据不太可能更久远；
/// 再长冻结表就会比流水表还大。
pub const FREEZE_WINDOW_SECS: i64 = 7 * 24 * 3600;

/// 封号事件之后多长时间内到达的流水也补进冻结表，见 `insert_usage_log_at`。触发那一发的
/// 响应流通常几十秒内结束，10 分钟够覆盖并发在途的所有请求。
pub const FREEZE_TAIL_SECS: i64 = 600;

/// 一次自动停用的上下文，见 [`CredentialStore::record_ban`]。
#[derive(Debug, Default, Clone)]
pub struct BanContext {
    /// 写进 `credentials.ban_reason` 的一句话（200 字符内）。
    pub reason: String,
    /// 触发来源：`forward`（转发 4xx）、`forward_401`（转发 401 换号）、`probe`（连通性
    /// 测试）、`keepalive`（保活端点 401/403）、`refresh`（刷新 token 被作废）、`proxy`
    /// （代理建不出来）、`manual`（其它，目前只有测试用）。前端
    /// `ban-events-dialog.tsx` 的 `sourceLabel` 与这份表一一对应。
    pub source: &'static str,
    /// 上游 HTTP 状态码（有的话）。
    pub status: Option<u16>,
    /// 上游 `error.type` 与**完整** `error.message`（reason 是截断过的）。
    pub error_type: Option<String>,
    pub error_message: Option<String>,
    /// 触发那条请求的 luban 请求 id 与上游 `request-id`——拿它能在冻结流水里精确找到那一发。
    pub request_id: Option<String>,
    pub upstream_request_id: Option<String>,
}

/// 一条封号事件（读取用），见 [`CredentialStore::record_ban`]。
#[derive(Debug, Clone, serde::Serialize)]
pub struct BanEvent {
    pub id: i64,
    pub ts: i64,
    pub cred_id: i64,
    pub cred_label: String,
    pub source: String,
    pub reason: String,
    pub status: Option<u16>,
    pub error_type: Option<String>,
    pub error_message: Option<String>,
    pub request_id: Option<String>,
    pub upstream_request_id: Option<String>,
    pub tier: Option<String>,
    pub org_type: Option<String>,
    /// 封号当时该号配的代理（密码已打码）。
    pub proxy: Option<String>,
    pub account_created_at: i64,
    pub lifetime_requests: i64,
    pub lifetime_cost_usd: f64,
    pub last_used_at: Option<i64>,
    /// 封前 7 天的请求数、去重设备数，以及模型 / 来访 UA / 代理的分布（`{value, count}`，按次数降序）。
    pub requests_7d: i64,
    pub devices_7d: i64,
    pub models_7d: Vec<serde_json::Value>,
    pub uas_7d: Vec<serde_json::Value>,
    pub proxies_7d: Vec<serde_json::Value>,
    /// 封前账本里最后一次限流快照的 unified_status / overage_in_use。
    pub last_unified_status: Option<String>,
    pub last_overage_in_use: Option<bool>,
    /// 冻结进 `usage_logs_frozen` 的流水条数（含封后补进去的）。
    pub frozen_rows: i64,
    /// 封前 7 天**发给 Anthropic** 的去重设备数与分布（`device_id_out`，伪装开着时是派生值）。
    /// 与 `devices_7d`（来访自报）分开：上游眼里这个号有几台设备，看的是这一对。
    pub devices_out_7d: i64,
    pub device_ids_out_7d: Vec<serde_json::Value>,
}

/// 按凭证数「TTL 内活跃」绑定的 SQL（`table` 是 `device_bindings` 或 `session_bindings`，
/// 只由代码里的常量传入）。TTL `<= 0` 时不过滤、按全量计。选号与后台列表共用，口径才一致。
///
/// **`GROUP BY +cred_id` 的一元加号不能删**：它让 SQLite 不再拿 `(cred_id)` 索引来分组。
/// 不加时，没跑过 ANALYZE 的库（luban 从不跑）会选择「按 cred 索引走全表、逐行回表判
/// last_seen_at」，代价随保留期内的**总行数**线性涨——会话表攒到 3 万行时单次 5–12ms，
/// 而且是在选号那把全局锁里、每条转发请求一次。加号之后改走 `last_seen_at` 索引，只扫
/// TTL 内的那一小段（同样 3 万行约 80µs）。不过滤时没有范围可走，保留原样让它扫索引。
fn active_counts_sql(table: &str, ttl_secs: i64) -> String {
    if ttl_secs > 0 {
        format!(
            "SELECT cred_id, COUNT(*) FROM {table} \
             WHERE last_seen_at >= unixepoch() - ?1 GROUP BY +cred_id"
        )
    } else {
        format!("SELECT cred_id, COUNT(*) FROM {table} GROUP BY cred_id")
    }
}

/// 后台统计只读连接的条数，见 [`CredentialStore::readers`]。控制台同一时刻在跑的重查询也就
/// 三四条（账号列表、实时指标、两组趋势）；再多只是多占几份页缓存和文件句柄。
const READER_POOL_SIZE: usize = 3;

/// 打开后台统计用的只读连接，见 [`CredentialStore::readers`]。
///
/// `SQLITE_OPEN_READ_ONLY`：误把写语句挂到这条连接上会当场报错，而不是悄悄绕开主连接的
/// 串行化。WAL 模式由主连接设在库文件上，这里不用（也不能）再设；busy_timeout 与主连接
/// 一致，兜住 checkpoint 等极短的排他窗口。
fn open_reader(path: &std::path::Path) -> Result<Connection> {
    use rusqlite::OpenFlags;
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .with_context(|| format!("failed to open read-only connection: {}", path.display()))?;
    conn.busy_timeout(Duration::from_secs(5))?;
    Ok(conn)
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS credentials (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            label         TEXT    NOT NULL DEFAULT '',
            tier          TEXT,
            org_type      TEXT,
            access_token  TEXT    NOT NULL,
            refresh_token TEXT    NOT NULL,
            expires_at    INTEGER NOT NULL,
            priority      INTEGER NOT NULL DEFAULT 2,
            disabled      INTEGER NOT NULL DEFAULT 0 CHECK (disabled IN (0,1)),
            created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
            updated_at    INTEGER NOT NULL DEFAULT (unixepoch())
        ) STRICT;

        CREATE UNIQUE INDEX IF NOT EXISTS uq_credentials_refresh_token
            ON credentials(refresh_token);
        CREATE INDEX IF NOT EXISTS idx_credentials_priority
            ON credentials(priority, id);

        -- 键值设置表（如接入用的 client api key）。
        CREATE TABLE IF NOT EXISTS settings (
            key   TEXT PRIMARY KEY,
            value TEXT NOT NULL
        ) STRICT;

        -- 设备→凭证的粘性绑定：同一 device_id 始终命中同一凭证。
        CREATE TABLE IF NOT EXISTS device_bindings (
            device_id     TEXT    PRIMARY KEY,
            cred_id       INTEGER NOT NULL,
            request_count INTEGER NOT NULL DEFAULT 0,
            created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
            last_seen_at  INTEGER NOT NULL DEFAULT (unixepoch())
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_device_bindings_cred
            ON device_bindings(cred_id);
        -- 保留期清理（`prune_expired_bindings`）按 last_seen_at 划线删行。没有这个索引就是
        -- 整表扫，而那条 DELETE 此前挂在选号路径上、每条转发请求跑一次。
        CREATE INDEX IF NOT EXISTS idx_device_bindings_seen
            ON device_bindings(last_seen_at);

        -- 会话→凭证的粘性绑定：同一会话键始终命中同一凭证，并占该凭证的**会话名额**。
        -- 走模拟路径且没有设备身份的请求写它；设备身份在上游已收敛时（`Select::per_session`）
        -- 带设备身份的来访也写它（键的取法见 proxy 的 `session_binding_key`：
        -- `lb:v2:sid:<来访会话 id>` 或 `lb:v2:pfx:<缓存前缀指纹>`）。一条请求只占一份名额。
        -- 真实客户端的会话沿用来访 id 出站，slot 记 -1（PASSTHROUGH_SLOT）。
        -- slot：这条会话在该凭证上占的槽位（0 起，取活跃绑定里最小的空位），出站会话 id 由
        -- 「账号 + 槽位」派生——槽位释放后被下一个对话复用，上游看到的会话 id 数有界。
        -- last_model：最近一轮请求的模型，**只记不参与键**（键里带模型会把同一条对话换模型
        -- 的那一轮劈成两条会话、占两份名额，见 `session_binding_key` 的记述）；给后台列。
        CREATE TABLE IF NOT EXISTS session_bindings (
            session_key   TEXT    PRIMARY KEY,
            cred_id       INTEGER NOT NULL,
            slot          INTEGER NOT NULL DEFAULT 0,
            request_count INTEGER NOT NULL DEFAULT 0,
            last_model    TEXT,
            created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
            last_seen_at  INTEGER NOT NULL DEFAULT (unixepoch())
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_session_bindings_cred
            ON session_bindings(cred_id);
        -- 同 device_bindings：给保留期清理用。这张表还更容易长——键是每条对话一个，
        -- 默认保留 24 小时，多客户端时攒到几万行不稀奇。
        CREATE INDEX IF NOT EXISTS idx_session_bindings_seen
            ON session_bindings(last_seen_at);

        -- 每次转发的用量日志：从上游响应里嗅探到的 token 用量（若响应带了 usage）。
        CREATE TABLE IF NOT EXISTS usage_logs (
            id             INTEGER PRIMARY KEY,
            ts             INTEGER NOT NULL DEFAULT (unixepoch()),
            cred_id        INTEGER,
            cred_label     TEXT    NOT NULL DEFAULT '',
            device_id      TEXT,
            model          TEXT,
            path           TEXT    NOT NULL DEFAULT '',
            -- 两份 User-Agent（都已截断，见 crate::proxy::ua_of）：
            --   ua     = 来访客户端自报的那份，认「谁在发」用它；
            --   ua_out = 实际发给上游的那份，模拟路径恒为官方那串，非模拟路径同 ua。
            -- 分两列而不是一列：只留来访那份看不到上游收到什么，只留出站那份认不出真实客户端。
            -- 连通性测试是 luban 自己发的，没有来访客户端，故 ua 为空、ua_out 照实记。
            ua             TEXT,
            ua_out         TEXT,
            status         INTEGER NOT NULL DEFAULT 0,
            -- 是否从响应中解析到用量（1/0）；未解析到时下面各 token 列为空。
            has_usage      INTEGER NOT NULL DEFAULT 0 CHECK (has_usage IN (0,1)),
            input_tokens          INTEGER,
            output_tokens         INTEGER,
            cache_creation_tokens INTEGER,
            cache_5m_tokens       INTEGER,
            cache_1h_tokens       INTEGER,
            cache_read_tokens     INTEGER,
            ttft_ms        INTEGER,
            total_ms       INTEGER,
            -- 订阅账号限流（anthropic-ratelimit-unified-*）：状态/额度重置时刻/使用率。
            unified_status     TEXT,
            rl_5h_status       TEXT,
            rl_5h_reset        INTEGER,
            rl_5h_utilization  REAL,
            rl_7d_status       TEXT,
            rl_7d_reset        INTEGER,
            rl_7d_utilization  REAL,
            rl_representative  TEXT,
            -- 本次请求是否动用了 usage credits（overage-in-use；1/0，头缺失时为空）。
            rl_overage_in_use  INTEGER,
            -- 原始限流头（兜底：字段变化时仍可回看）。
            ratelimit_raw      TEXT,
            -- 按官方定价估算的等价 API 费用（USD）；模型未知时为空。
            cost_usd           REAL,
            -- 这条来访本来是非流式、被改写成流式发给上游再聚合回整段 JSON（1/0）。
            -- 它解释了同一条记录里 ttft_ms 与 total_ms 为什么会差很多：TTFT 记的是上游
            -- 首字节，而客户端是在末尾一次性收到整段的。见 ForwardFlags::nonstream_as_sse。
            sse_aggregated     INTEGER NOT NULL DEFAULT 0 CHECK (sse_aggregated IN (0,1))
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_usage_logs_ts   ON usage_logs(ts);
        -- 按 cred_id 分组、按 ts 卡窗口的那条索引（idx_usage_logs_cred_usage）引用的费用 /
        -- token 列在老库上是后补的，建在补列之后，见下面的迁移。旧的单列 idx_usage_logs_cred
        -- 是它的前缀，留着只是白占写入开销，随迁移删掉。
        DROP INDEX IF EXISTS idx_usage_logs_cred;
        -- 设备明细要按 device_id 汇总费用（含跨账号合计）；日志表只会越攒越多，
        -- 没这条索引时展开一次卡片就是一次全表扫描。
        CREATE INDEX IF NOT EXISTS idx_usage_logs_device ON usage_logs(device_id, cred_id);

        -- 封号事件：每次自动停用落一条，只追加。解封不清、删号不删、不裁剪——这是取证材料，
        -- 而 credentials.ban_reason 会在重新启用时清空、usage_logs 会随删号级联删除。
        -- 列含义见 BanEvent。列表类字段（models_7d 等）存 JSON 文本。
        CREATE TABLE IF NOT EXISTS ban_events (
            id                  INTEGER PRIMARY KEY AUTOINCREMENT,
            ts                  INTEGER NOT NULL DEFAULT (unixepoch()),
            cred_id             INTEGER NOT NULL,
            cred_label          TEXT    NOT NULL DEFAULT '',
            source              TEXT    NOT NULL DEFAULT '',
            reason              TEXT    NOT NULL DEFAULT '',
            status              INTEGER,
            error_type          TEXT,
            error_message       TEXT,
            request_id          TEXT,
            upstream_request_id TEXT,
            tier                TEXT,
            org_type            TEXT,
            proxy               TEXT,
            account_created_at  INTEGER NOT NULL DEFAULT 0,
            lifetime_requests   INTEGER NOT NULL DEFAULT 0,
            lifetime_cost_usd   REAL    NOT NULL DEFAULT 0,
            last_used_at        INTEGER,
            requests_7d         INTEGER NOT NULL DEFAULT 0,
            devices_7d          INTEGER NOT NULL DEFAULT 0,
            models_7d           TEXT,
            uas_7d              TEXT,
            proxies_7d          TEXT,
            last_unified_status TEXT,
            last_overage_in_use INTEGER,
            frozen_rows         INTEGER NOT NULL DEFAULT 0,
            devices_out_7d      INTEGER NOT NULL DEFAULT 0,
            device_ids_out_7d   TEXT
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_ban_events_cred_ts ON ban_events(cred_id, ts);

        -- 封号时冻结的流水：与 usage_logs 同列（见 USAGE_LOG_COLS），多出 ban_event_id 与原行
        -- id（src_id）。不裁剪、不随删号删除。读法见 frozen_usage_logs。
        CREATE TABLE IF NOT EXISTS usage_logs_frozen (
            id             INTEGER PRIMARY KEY,
            ban_event_id   INTEGER NOT NULL,
            src_id         INTEGER,
            ts             INTEGER NOT NULL DEFAULT (unixepoch()),
            cred_id        INTEGER,
            cred_label     TEXT    NOT NULL DEFAULT '',
            device_id      TEXT,
            model          TEXT,
            path           TEXT    NOT NULL DEFAULT '',
            ua             TEXT,
            ua_out         TEXT,
            status         INTEGER NOT NULL DEFAULT 0,
            has_usage      INTEGER NOT NULL DEFAULT 0,
            input_tokens          INTEGER,
            output_tokens         INTEGER,
            cache_creation_tokens INTEGER,
            cache_5m_tokens       INTEGER,
            cache_1h_tokens       INTEGER,
            cache_read_tokens     INTEGER,
            ttft_ms        INTEGER,
            total_ms       INTEGER,
            unified_status     TEXT,
            rl_5h_status       TEXT,
            rl_5h_reset        INTEGER,
            rl_5h_utilization  REAL,
            rl_7d_status       TEXT,
            rl_7d_reset        INTEGER,
            rl_7d_utilization  REAL,
            rl_representative  TEXT,
            rl_overage_in_use  INTEGER,
            ratelimit_raw      TEXT,
            cost_usd           REAL,
            sse_aggregated     INTEGER NOT NULL DEFAULT 0,
            request_id         TEXT,
            upstream_request_id TEXT
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_usage_logs_frozen_ban ON usage_logs_frozen(ban_event_id, ts);

        -- 账本：每凭证的终身累计统计与最新额度快照，与 usage_logs 的插入在同一事务内更新
        -- （见 insert_usage_log_at）。分工：usage_logs 是流水，只保留近期（prune_usage_logs），
        -- 「最近使用 / 累计费用 / 最新快照」这些终身口径落在这里，才不随流水裁剪一起变小。
        -- 老库升级时由 backfill_ledger 从既有流水一次性回填。
        CREATE TABLE IF NOT EXISTS credential_stats (
            cred_id        INTEGER PRIMARY KEY,
            last_used_at   INTEGER,
            cost_total_usd REAL NOT NULL DEFAULT 0,
            -- 最新一次带限流头响应的快照（列含义同 usage_logs 的 rl_* 列）。
            snapshot_ts        INTEGER,
            unified_status     TEXT,
            rl_5h_utilization  REAL,
            rl_5h_reset        INTEGER,
            rl_7d_utilization  REAL,
            rl_7d_reset        INTEGER,
            rl_representative  TEXT,
            overage_in_use     INTEGER,
            -- 上游本次报告的全部窗口（JSON 数组，见 QuotaWindow）。5h/7d 的专用列保留：
            -- 只有它们有配套的窗口内费用/请求数聚合，这一列补的是 7d_oi 那类没有专用列的窗口。
            windows            TEXT
        ) STRICT;
        -- 设备费用账本：终身累计。不记在 device_bindings 上——绑定行会被解绑/TTL 清掉重建，
        -- 而费用语义要求比绑定活得久（见 list_devices 的注）。
        CREATE TABLE IF NOT EXISTS device_costs (
            device_id TEXT    NOT NULL,
            cred_id   INTEGER NOT NULL,
            cost_usd  REAL    NOT NULL DEFAULT 0,
            -- 终身请求数。与 device_bindings.request_count 不同源：那个随绑定行走，
            -- 解绑/停用/TTL 清掉后从零重数；这个和费用一样终身累计，且**模拟客户端也记**
            -- （它们不写绑定，见 crate::proxy::sim_device_id）。
            request_count INTEGER NOT NULL DEFAULT 0,
            PRIMARY KEY (device_id, cred_id)
        ) STRICT, WITHOUT ROWID;

        -- 代理池：可复用的出站代理地址，供逐账号代理从中选取。
        CREATE TABLE IF NOT EXISTS proxies (
            id         INTEGER PRIMARY KEY AUTOINCREMENT,
            label      TEXT    NOT NULL DEFAULT '',
            url        TEXT    NOT NULL,
            created_at INTEGER NOT NULL DEFAULT (unixepoch())
        ) STRICT;
        CREATE UNIQUE INDEX IF NOT EXISTS uq_proxies_url ON proxies(url);

        -- 上游判成「这个号的套餐不含这个模型」的记录（Pro 号打 fable 那类 429），见
        -- CredentialStore::deny_model。落库而不放进程内冷却：这不是几十秒的事，重启也不该忘。
        CREATE TABLE IF NOT EXISTS model_denials (
            cred_id    INTEGER NOT NULL,
            model      TEXT    NOT NULL,
            reason     TEXT    NOT NULL DEFAULT '',
            learned_at INTEGER NOT NULL DEFAULT (unixepoch()),
            expires_at INTEGER,
            PRIMARY KEY (cred_id, model)
        ) STRICT;

        -- 从上游响应学到的规则（形态拒绝 / 已废弃字段 / 零输出请求类 / 拒答过的提示词），见
        -- CredentialStore::remember_rejections。只为重启回填；进程内仍以 HashMap 为准。
        -- learned_at 用来做 7 天保鲜。reply_sse / reply_body 是拒答那类要原样回放的上游响应体
        -- （0.3.98 补列，见 LearnedReply）。
        CREATE TABLE IF NOT EXISTS learned_rejections (
            kind       TEXT    NOT NULL,
            model      TEXT    NOT NULL,
            field      TEXT    NOT NULL,
            value      TEXT    NOT NULL DEFAULT '',
            message    TEXT    NOT NULL DEFAULT '',
            learned_at INTEGER NOT NULL DEFAULT (unixepoch()),
            reply_sse  INTEGER NOT NULL DEFAULT 0,
            reply_body TEXT    NOT NULL DEFAULT '',
            PRIMARY KEY (kind, model, field, value)
        ) STRICT;",
    )
    .context("failed to initialize credential database schema")?;

    // 兼容 0.3.98 之前建的 learned_rejections：补拒答回放体两列（已存在则忽略 duplicate column）。
    for col in ["reply_sse INTEGER NOT NULL DEFAULT 0", "reply_body TEXT NOT NULL DEFAULT ''"] {
        let _ = conn.execute(&format!("ALTER TABLE learned_rejections ADD COLUMN {col}"), []);
    }

    // 准入记录只该挂在高档套餐专属的模型上（见 `crate::proxy::LimitScope::Unsupported`）。
    // 0.3.65 曾把 sonnet-4-6 这种基础模型也记进去（同形态的 429 现在只做短冷却 + 换号，见
    // `LimitScope::OverageDisabled`），这里把那批记录清掉；之后的写入方不会再产生这类行，
    // 此语句只是幂等的兜底。
    conn.execute(
        "DELETE FROM model_denials WHERE model NOT LIKE '%fable%' AND model NOT LIKE '%mythos%'",
        [],
    )?;

    // 兼容旧 usage_logs：逐列幂等新增（已存在则忽略 duplicate column）。
    for col in [
        "unified_status TEXT",
        "rl_5h_status TEXT",
        "rl_5h_reset INTEGER",
        "rl_5h_utilization REAL",
        "rl_7d_status TEXT",
        "rl_7d_reset INTEGER",
        "rl_7d_utilization REAL",
        "rl_representative TEXT",
        "ratelimit_raw TEXT",
        "cost_usd REAL",
        "cache_5m_tokens INTEGER",
        "cache_1h_tokens INTEGER",
        "rl_overage_in_use INTEGER",
        "ua TEXT",
        "ua_out TEXT",
        // CHECK 只写在建表里：ADD COLUMN 带 CHECK 各版本行为不一，而这一列的写入方只有
        // insert_usage_log 一处，值恒为 0/1。
        "sse_aggregated INTEGER NOT NULL DEFAULT 0",
        // 0.3.70：luban 自己的请求 id 与上游最后一次的 request-id，见 UsageRecord::request_id。
        "request_id TEXT",
        "upstream_request_id TEXT",
        // 0.3.76：取证列，见 Forensics。
        "proxy TEXT",
        "simulated INTEGER NOT NULL DEFAULT 0",
        "shape TEXT",
        "session_id TEXT",
        "error_type TEXT",
        "error_message TEXT",
        "third_party INTEGER NOT NULL DEFAULT 0",
        "rewrites TEXT",
        // 0.3.76：出站 device_id，见 Forensics::device_id_out。
        "device_id_out TEXT",
        // 0.3.89：上游 200 却零输出 / 拒答时截取的响应体，见 Forensics::response_excerpt。
        "response_excerpt TEXT",
        // 0.3.99：走模拟路径的原因标签，见 Forensics::sim_reason。
        "sim_reason TEXT",
        // 0.3.139：这条请求落在哪个模拟会话绑定上，见 Forensics::session_key。
        "session_key TEXT",
        // 0.3.139：来访自报的 session_id，见 Forensics::session_id_in。
        "session_id_in TEXT",
    ] {
        // 冻结表与流水表同列（USAGE_LOG_COLS 逐列照搬），补列必须两张一起补。
        let _ = conn.execute(&format!("ALTER TABLE usage_logs ADD COLUMN {col}"), []);
        let _ = conn.execute(&format!("ALTER TABLE usage_logs_frozen ADD COLUMN {col}"), []);
    }
    // 这两个索引依赖上面补出来的列 / 服务翻页的排序键，**必须在补列之后建**：
    // - (cred_id, id)：按号翻页是 `cred_id = ? AND id <= ? ORDER BY id DESC`，与索引序完全
    //   一致，取页不必再排序；已有的 (cred_id, ts) 是给按时间聚合用的，排序键不同。
    // - (request_id)：按请求 id 精确查——排查时贴一个 id 进来，不能整表扫。
    // - (session_id, id) / (session_id_in, id)：按会话 id 查要同时匹配出站与来访两侧
    //   （走模拟时两者不是同一个 uuid），两条部分索引让那个 OR 走 MULTI-INDEX OR。
    // - (session_key, id)：会话行点「看请求」是 `session_key = ? AND id <= ? ORDER BY id DESC`，
    //   与 (cred_id, id) 同一个形状；**部分索引**（只收非空的），带设备身份与非模拟路径的
    //   请求这一列恒为空、占了绝大多数行，不进索引就不付这份写入与体积。
    // - 延迟趋势只看成功且记了 TTFT 的行、只读三列：**部分覆盖索引**，整段扫描全在索引里
    //   走、不回表（日志行很宽，回表才是大头）；失败行与没记 TTFT 的行不进索引，写入开销只
    //   落在成功请求上。
    // - 缓存趋势按 ts 扫全部行、只读五个 token 列，同样做成覆盖索引。
    // - 账号列表每次刷新都要按号聚合额度窗口（最长 7 天多）内的费用、条数与 token，见
    //   QUOTA_SNAPSHOTS_SQL：(cred_id, ts) 后面带上它读的那几列，整段范围扫描不回表。40 个号、
    //   7 天 40 万条流水的规模下，回表那版要读 GB 级的宽行，覆盖之后约快 10 倍。它的
    //   (cred_id, ts) 前缀照样服务按号按时间卡窗口的其它查询，原来那条 idx_usage_logs_cred_ts
    //   就是多余的写入开销，随之删掉。
    // - (model, ts)：按模型下钻流水（`model = ? AND ts >= ?`）直接圈出窗口。统计（条数、费用）
    //   只扫窗口内的行；取页那条分两步、子查询只取 id，在这条索引里就能拿齐，不回表（见
    //   usage_log_page_sql）。「近 7 天出现过的模型」也靠它跳着取不同的模型名、各取最近一条
    //   （见 recent_models）。
    //   **不用 (model, id)**：取页是快了（倒着走够 n 条就停），但统计只能按模型把整个保留期
    //   逐行回表再按时间过滤，24 小时窗口的统计慢好几倍，而统计每次翻页都要跑。预发版建过它
    //   的库在这里删掉。
    // 这几条引用的列（ttft_ms / total_ms / output_tokens / cache_*）在老库上是上面补出来的，
    // 放进建表那批会在老库上报「no such column」。
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_usage_logs_cred_id ON usage_logs(cred_id, id);
         CREATE INDEX IF NOT EXISTS idx_usage_logs_request_id ON usage_logs(request_id);
         CREATE INDEX IF NOT EXISTS idx_usage_logs_session_key
             ON usage_logs(session_key, id) WHERE session_key IS NOT NULL;
         CREATE INDEX IF NOT EXISTS idx_usage_logs_session_id
             ON usage_logs(session_id, id) WHERE session_id IS NOT NULL;
         CREATE INDEX IF NOT EXISTS idx_usage_logs_session_id_in
             ON usage_logs(session_id_in, id) WHERE session_id_in IS NOT NULL;
         CREATE INDEX IF NOT EXISTS idx_usage_logs_latency
             ON usage_logs(ts, ttft_ms, total_ms, output_tokens)
             WHERE status = 200 AND ttft_ms IS NOT NULL;
         CREATE INDEX IF NOT EXISTS idx_usage_logs_cache
             ON usage_logs(ts, input_tokens, cache_creation_tokens, cache_5m_tokens,
                           cache_1h_tokens, cache_read_tokens);
         CREATE INDEX IF NOT EXISTS idx_usage_logs_cred_usage
             ON usage_logs(cred_id, ts, cost_usd, input_tokens, output_tokens,
                           cache_creation_tokens, cache_5m_tokens, cache_1h_tokens,
                           cache_read_tokens);
         DROP INDEX IF EXISTS idx_usage_logs_cred_ts;
         CREATE INDEX IF NOT EXISTS idx_usage_logs_model_ts ON usage_logs(model, ts);
         DROP INDEX IF EXISTS idx_usage_logs_model_id;",
    )?;
    // ban_events 的出站设备两列是随 device_id_out 一起加的：先建过表的库幂等补上。
    let _ = conn
        .execute("ALTER TABLE ban_events ADD COLUMN devices_out_7d INTEGER NOT NULL DEFAULT 0", []);
    let _ = conn.execute("ALTER TABLE ban_events ADD COLUMN device_ids_out_7d TEXT", []);
    // credential_stats 是 0.2.37 加的表，这两列都在其后才有：同样幂等补列。
    let _ = conn.execute("ALTER TABLE credential_stats ADD COLUMN overage_in_use INTEGER", []);
    // device_costs 的终身请求数是后加的：老库补出来是 0，之后的请求照常累加。
    // 不回填——`usage_logs` 只留保留期内的，拿它回填会得到一个「看着像终身、其实只有几天」的数，
    // 比从 0 开始更误导。
    let _ = conn.execute(
        "ALTER TABLE device_costs ADD COLUMN request_count INTEGER NOT NULL DEFAULT 0",
        [],
    );
    // 全窗口快照。老库补出来是 NULL，前端按「只有 5h/7d」渲染（与升级前一模一样），
    // 下一条带限流头的响应就会把它填上——不必也不值得从 ratelimit_raw 回溯解析。
    let _ = conn.execute("ALTER TABLE credential_stats ADD COLUMN windows TEXT", []);

    // 兼容旧库：新增列时若已存在会报 duplicate column，忽略即可（幂等）。
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN tier TEXT", []);
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN org_type TEXT", []);
    let _ = conn
        .execute("ALTER TABLE credentials ADD COLUMN device_limit INTEGER NOT NULL DEFAULT 0", []);
    // 自动检测到的上游账号级错误原因（如封号）；NULL 表示未被自动停用，
    // 与管理员手动停用（disabled=1 且本字段为空）区分开。见 `mark_banned`。
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN ban_reason TEXT", []);
    // 账号 UUID（profile.account.uuid）；转发身份伪装用。旧库为空，刷新 token 时回填。
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN account_uuid TEXT", []);

    // 迁移：credentials.id 改为 AUTOINCREMENT。旧表（无 AUTOINCREMENT）删掉最大 id 的行后
    // 会回收复用该 id，令新账号错误继承被删账号的历史用量（usage_logs 按 cred_id 关联、
    // 删号时不清理）。此处须在上面那几条 ADD COLUMN 之后执行，确保重建时那些列已齐全；
    // **重建之后加的列一律排在它后面**（重建按写死的列清单复制，清单外的列会被整列丢掉）。
    migrate_credentials_autoincrement(conn)?;

    // 被上游限流自动停用后、到点自动重新启用的时刻（unix 秒）；NULL = 不自动恢复。
    // **必须补在重建之后**：上面那次重建按写死的列清单复制，加在它之前会被整列丢掉。
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN resume_at INTEGER", []);

    // 该账号专用的出站代理；NULL = 直连。**同样必须补在重建之后**，理由见上一条。
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN proxy TEXT", []);

    // 该账号每分钟最多转发多少条请求（三态同 device_limit：>0 独立 / 0 跟随全局 / <0 不限）。
    // 旧库补出来是 0 = 跟随全局默认，而全局默认也是 0（不限），故存量账号行为不变。
    // **同样必须补在重建之后**，理由见上面 resume_at 那条。
    let _ =
        conn.execute("ALTER TABLE credentials ADD COLUMN rpm_limit INTEGER NOT NULL DEFAULT 0", []);

    // 额度档原值（profile.organization.rate_limit_tier）；statsig eval 的 `rateLimitTier`
    // 要发它，界面上那个 `Max 5x` 是它的展示形态、顶替不了。旧库为空，下次刷新（自动或手动）回填。
    // **必须补在重建之后**：这一行原先排在重建之前，而重建按写死的列清单复制，没有它——
    // 一张旧的非 AUTOINCREMENT 库升上来，这一列就被整列丢掉，之后 `SELECT {COLS}` 直接
    // `no such column: rate_limit_tier`，list/get 全挂。见 `migrates_and_stops_id_reuse`。
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN rate_limit_tier TEXT", []);

    // 组织 UUID 与订阅创建时刻（profile 的 `organization.uuid` / `organization.subscription_created_at`
    // 原串）；遥测身份与 eval 属性用。旧库为空，下次刷新（自动或手动）回填。
    // **同样必须补在重建之后**，理由见上面 resume_at 那条。
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN org_uuid TEXT", []);
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN subscription_created_at TEXT", []);

    // 逐账号的「额度用到多少就提前停调度」阈值（5h / 7d 两档，百分比）。NULL = 跟随全局
    // [`QUOTA_PAUSE_PCT`] / [`QUOTA_PAUSE_PCT_7D`]，0 = 本账号这一档不停，1..=100 = 独立阈值。
    // 旧库补出来全是 NULL，即全部跟随全局，存量行为不变。**同样必须补在重建之后**。
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN quota_pause_pct INTEGER", []);
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN quota_pause_pct_7d INTEGER", []);

    // 该账号最多同时活跃多少条**模拟会话**（三态同 device_limit：>0 独立 / 0 跟随全局 / <0 不限）。
    // 旧库补出来是 0 = 跟随全局默认 [`DEFAULT_SESSION_LIMIT`]。**同样必须补在重建之后**。
    let _ = conn
        .execute("ALTER TABLE credentials ADD COLUMN session_limit INTEGER NOT NULL DEFAULT 0", []);
    // profile 里只给后台看的几列：组织名称、席位档、订阅状态、超额用量开关（0/1）。旧库为空，
    // 下次刷新回填（组织名称计入 `profile_incomplete`）。**同样必须补在重建之后**。
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN org_name TEXT", []);
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN seat_tier TEXT", []);
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN subscription_status TEXT", []);
    let _ = conn.execute("ALTER TABLE credentials ADD COLUMN extra_usage_enabled INTEGER", []);
    // v0.3.126 / 127 建的 session_bindings 没有 slot 列。补列时把存量行清掉：它们全落在槽位 0
    // 上，留着会让好几条活跃会话共用一个会话 id，直到各自 TTL 到期；这张表本来就只记最近一小时
    // 的亲和性，清掉的代价只是那几条对话下一轮重新选号。补列失败（列已在）什么都不动。
    if conn
        .execute("ALTER TABLE session_bindings ADD COLUMN slot INTEGER NOT NULL DEFAULT 0", [])
        .is_ok()
    {
        conn.execute("DELETE FROM session_bindings", [])?;
    }
    // 最近一轮的模型；旧库补出来是 NULL，下次命中该绑定时回填。只给后台列看，不参与选号。
    let _ = conn.execute("ALTER TABLE session_bindings ADD COLUMN last_model TEXT", []);
    // 会话键自 v0.3.137 起带命名空间与口径版本（`lb:v2:…`，见 `crate::proxy::SESSION_KEY_VERSION`）。
    // 没有这个前缀的行是旧口径算出来的——v0.3.126 那版按「账号 + 设备指纹」，与现在的「来访
    // 会话 id / 缓存前缀 + 对话起点」根本不是一回事，却同样是 32 个 hex，留着只会让新旧两种
    // 语义混在一张表里，把会话数算错、把不相干的对话粘在同一个槽位上。这张表本来就只记最近
    // 一小时的亲和性（TTL），清掉的代价是那几条对话下一轮重新选号、换一个上游会话 id。
    // 每次启动都跑：改到 v3 时同一条语句自动把 v2 的行清掉，稳定后条件不命中、代价可忽略。
    conn.execute(
        "DELETE FROM session_bindings WHERE session_key NOT LIKE ?1 || '%'",
        [crate::proxy::SESSION_KEY_VERSION],
    )
    .context("failed to drop session bindings from an older key scheme")?;

    // 0.2.81 起，socks5 在入库那一刻就归一化成 socks5h（把 DNS 交给代理端解析，理由见
    // [`crate::clients::PROXY_SCHEME_UPGRADES`]）。存量行必须一起改写，否则之前配好的号会一直
    // 本机解析 DNS——正是那个改动要治的故障（住宅代理只回一个 `unexpected EOF`），而网页上没有
    // 自助修复的路：打开代理框，里面的值与库里一致 → 不算改动 → 保存按钮是灰的。
    // 前缀 `socks5://` 是 9 个字符，故 substr 从第 10 个字符起原样接上。
    // 每次启动都跑一遍：条件严格、表也小，幂等且代价可忽略。
    conn.execute(
        "UPDATE credentials SET proxy = 'socks5h://' || substr(proxy, 10) \
         WHERE proxy LIKE 'socks5://%'",
        [],
    )
    .context("failed to normalize stored socks5 proxy schemes")?;

    // 0.2.82 起不再收 socks4/socks4a（理由见 [`crate::clients::PROXY_SCHEMES`]）。存量行
    // **既不改写也不清空**：清成直连就是拿真实 IP 去打上游，恰恰是配代理要避免的事。留着的话
    // 运行时那道校验会拒掉它，这个号整体不可用、错误也看得见，但光从「转发失败」那条日志推不
    // 回协议这一层，所以启动时先把这些号点出来。
    let socks4: Vec<String> = conn
        .prepare("SELECT label FROM credentials WHERE proxy LIKE 'socks4%'")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    if !socks4.is_empty() {
        tracing::warn!(
            credentials = ?socks4,
            "socks4/socks4a proxies are no longer supported (they cannot carry authentication); \
             these credentials will fail until their proxy is changed to socks5h://"
        );
    }

    // 未配置全局设备上限的旧库升级到受控默认值；显式写入的 0（不限）保留不动。
    //
    // 这一行**不改变判定**：`CredentialStore::default_device_limit` 在设置缺失时本来就回落到
    // 同一个 [`DEFAULT_DEVICE_LIMIT_VALUE`]，写进表只是让它在控制台里看得见、改得动。真正
    // 「多出一道上限」的是 v0.2.8 引入这个全局默认那一次，从更老的库升上来的部署会在这里第一次
    // 落这一行——所以只在**确实插入了**（`INSERT OR IGNORE` 影响行数为 1）时说一声：迁移可以
    // 安静，但改变了一台部署的可用面的那一次不该安静。已经有这一行的（绝大多数）一个字不打。
    let device_limit_seeded = conn.execute(
        "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
        params![DEFAULT_DEVICE_LIMIT, DEFAULT_DEVICE_LIMIT_VALUE.to_string()],
    )?;
    if device_limit_seeded > 0 {
        tracing::warn!(
            key = DEFAULT_DEVICE_LIMIT,
            value = DEFAULT_DEVICE_LIMIT_VALUE,
            "this database had no global default device limit; wrote {DEFAULT_DEVICE_LIMIT_VALUE}. \
             Accounts whose own device_limit is 0 now allow at most {DEFAULT_DEVICE_LIMIT_VALUE} \
             bound devices each; change it under Settings (0 = unlimited), or set a per-account \
             limit (negative = that account is explicitly unlimited)"
        );
    }

    // 全局默认模拟会话上限同样落一行，让它在控制台里看得见、改得动；理由与口径同上一段。
    // 判定不变：`default_session_limit` 在设置缺失时本来就回落到 [`DEFAULT_SESSION_LIMIT_VALUE`]。
    let session_limit_seeded = conn.execute(
        "INSERT OR IGNORE INTO settings (key, value) VALUES (?1, ?2)",
        params![DEFAULT_SESSION_LIMIT, DEFAULT_SESSION_LIMIT_VALUE.to_string()],
    )?;
    if session_limit_seeded > 0 {
        tracing::warn!(
            key = DEFAULT_SESSION_LIMIT,
            value = DEFAULT_SESSION_LIMIT_VALUE,
            "this database had no global default session limit; wrote {DEFAULT_SESSION_LIMIT_VALUE}. \
             Accounts whose own session_limit is 0 now allow at most {DEFAULT_SESSION_LIMIT_VALUE} \
             active simulated sessions each (bare requests on the simulation path, keyed by their \
             session id, else cache prefix + first user message); change it under Settings \
             (0 = unlimited), or set a \
             per-account limit (negative = that account is explicitly unlimited)"
        );
    }

    // 清理旧库遗留的无主历史数据（此前删号只清 device_bindings，用量日志留了下来）。
    // 必须在回填账本之前跑：先扫掉无主日志，回填才不会给已删账号立账。
    purge_orphan_rows(conn)?;
    backfill_ledger(conn)?;
    migrate_priority_tiers(conn)?;
    Ok(())
}

/// v0.3.196 起的标记：优先级已从最早的口径（默认 P0、可为负数）换到 P1..=P100、默认 P50。
/// 现在只用来判断待迁移的数据是哪种旧口径。
const PRIORITY_SCALE_MIGRATED: &str = "priority_scale_p50";
/// 标记优先级已换到 P0..=P4、默认 P2 的档位口径；有这一行就不再迁移。
const PRIORITY_TIERS_MIGRATED: &str = "priority_tiers_p2";

/// 优先级换成 5 档：按名次压档（见 [`priority_tiers_by_rank`]），旧默认档落 P2，
/// 两侧最靠近的各占 P1/P3，再往外的并进 P0/P4，先后顺序不变。旧口径有两种：带
/// [`PRIORITY_SCALE_MIGRATED`] 标记的是 P1..=P100（默认 50），没有的是最早的口径（默认 0）。
/// **只跑一次**：压档不是幂等的，靠 settings 里的标记挡住重复执行；新库空表上跑一遍只是落下标记。
///
/// 查标记、读旧值、写新档必须在同一个 `BEGIN IMMEDIATE` 事务里：服务和 `luban status`
/// 可能同时打开同一个库，两边若都在事务外读到「未迁移」，后提交的那个会拿已压过档的数据再压
/// 一遍（P2 → P1）。IMMEDIATE 一开头就占写锁，另一边在 busy_timeout 内排队，轮到它时已能看到标记。
fn migrate_priority_tiers(conn: &Connection) -> Result<()> {
    let tx = rusqlite::Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    let marked = |key: &str| -> Result<bool> {
        Ok(tx
            .query_row("SELECT 1 FROM settings WHERE key = ?1", [key], |_| Ok(()))
            .optional()?
            .is_some())
    };
    if marked(PRIORITY_TIERS_MIGRATED)? {
        return Ok(());
    }
    let mid = if marked(PRIORITY_SCALE_MIGRATED)? { 50 } else { 0 };
    let rows: Vec<(i64, i64)> = tx
        .prepare("SELECT id, priority FROM credentials")?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<rusqlite::Result<_>>()?;
    let tiers = priority_tiers_by_rank(rows.iter().map(|&(_, p)| p), mid);
    let mut n = 0;
    {
        // 按 id 逐行写：按旧值批量 UPDATE 会把刚写成新档的行再改一遍（新旧取值有重叠）。
        let mut stmt = tx.prepare("UPDATE credentials SET priority = ?2 WHERE id = ?1")?;
        for (id, p) in &rows {
            if tiers[p] != *p {
                n += stmt.execute(params![id, tiers[p]])?;
            }
        }
    }
    for key in [PRIORITY_SCALE_MIGRATED, PRIORITY_TIERS_MIGRATED] {
        tx.execute("INSERT OR IGNORE INTO settings (key, value) VALUES (?1, '1')", [key])?;
    }
    tx.commit()?;
    if n > 0 {
        tracing::warn!(
            count = n,
            old_default = mid,
            "moved account priorities to tiers P{PRIORITY_MIN}..P{PRIORITY_MAX} \
             (old default is now P{PRIORITY_DEFAULT}); relative order kept"
        );
    }
    Ok(())
}

/// 初次启动（账本还是空表）时，把既有 usage_logs 流水一次性回填进账本。
///
/// 账本（credential_stats / device_costs）是随「写时落账」改造新加的：老库升级上来时
/// 流水里攒着几个月的历史，账本却是空的——不回填的话，卡片上的累计费用/最近使用/额度
/// 快照全部清零重来。三条聚合各扫一遍流水即可，百万行也只是秒级，且只在账本为空的
/// 那一次启动跑；此后写时落账接管，账本非空，这里直接短路。
fn backfill_ledger(conn: &Connection) -> Result<()> {
    let stats: i64 = conn.query_row("SELECT COUNT(*) FROM credential_stats", [], |r| r.get(0))?;
    if stats > 0 {
        return Ok(());
    }
    let tx = conn.unchecked_transaction()?;
    // 终身口径：最近使用 + 累计费用。
    let n = tx.execute(
        "INSERT INTO credential_stats (cred_id, last_used_at, cost_total_usd)
         SELECT cred_id, MAX(ts), COALESCE(SUM(cost_usd), 0)
           FROM usage_logs WHERE cred_id IS NOT NULL GROUP BY cred_id",
        [],
    )?;
    // 额度快照：每凭证最新一条带限流头的行（MAX + 裸列取自该最大行，SQLite 特性）。
    // 没有这种行的凭证子查询给出全 NULL 行，快照列保持空，口径与写时落账一致。
    tx.execute(
        "UPDATE credential_stats SET
             (snapshot_ts, unified_status, rl_5h_utilization, rl_5h_reset,
              rl_7d_utilization, rl_7d_reset, rl_representative, overage_in_use) =
             (SELECT MAX(u.ts), u.unified_status, u.rl_5h_utilization, u.rl_5h_reset,
                     u.rl_7d_utilization, u.rl_7d_reset, u.rl_representative, u.rl_overage_in_use
                FROM usage_logs u
               WHERE u.cred_id = credential_stats.cred_id
                 AND (u.rl_5h_utilization IS NOT NULL OR u.rl_7d_utilization IS NOT NULL))",
        [],
    )?;
    // 设备费用账本。
    tx.execute(
        "INSERT INTO device_costs (device_id, cred_id, cost_usd)
         SELECT device_id, cred_id, SUM(cost_usd) FROM usage_logs
          WHERE cred_id IS NOT NULL AND device_id IS NOT NULL AND cost_usd IS NOT NULL
          GROUP BY device_id, cred_id",
        [],
    )?;
    tx.commit()?;
    if n > 0 {
        tracing::info!(credentials = n, "ledger backfilled from existing usage logs");
    }
    Ok(())
}

/// 清扫 cred_id 已指向不存在账号的小表行（绑定、账本、设备费用）。删号本来就在同一个
/// 事务里清了它们（[`CredentialStore::remove`]），这里兜的是更早版本留下的欠账。
///
/// **用量流水不在这里清**：已删账号的流水按设计留着，随保留期自然裁掉，理由见
/// [`CredentialStore::remove`]。这里若一条语句删掉所有无主流水，删过几个大号的库每次开机
/// 都要先删上几十万行、全程占着写锁。
fn purge_orphan_rows(conn: &Connection) -> Result<()> {
    let binds = conn
        .execute(
            "DELETE FROM device_bindings WHERE cred_id NOT IN (SELECT id FROM credentials)",
            [],
        )
        .context("failed to purge orphaned device bindings")?;
    conn.execute(
        "DELETE FROM session_bindings WHERE cred_id NOT IN (SELECT id FROM credentials)",
        [],
    )
    .context("failed to purge orphaned session bindings")?;
    // 账本同口径清扫（新表初次上线时是 no-op）。
    conn.execute(
        "DELETE FROM credential_stats WHERE cred_id NOT IN (SELECT id FROM credentials)",
        [],
    )
    .context("failed to purge orphaned credential ledger entries")?;
    conn.execute("DELETE FROM device_costs WHERE cred_id NOT IN (SELECT id FROM credentials)", [])
        .context("failed to purge orphaned device cost entries")?;
    if binds > 0 {
        tracing::info!(
            device_bindings = binds,
            "cleaned up bindings left behind by deleted credentials"
        );
    }
    Ok(())
}

/// 若 `credentials` 仍是非 AUTOINCREMENT 的旧表，则原地重建为 AUTOINCREMENT 主键。
/// 幂等：DDL 已含 AUTOINCREMENT 时直接返回。保留所有既有行与其 id——AUTOINCREMENT 会据
/// 当前 `MAX(id)` 播种 `sqlite_sequence`，此后新 id 严格递增、永不回收，杜绝历史错配。
fn migrate_credentials_autoincrement(conn: &Connection) -> Result<()> {
    let ddl: String = conn.query_row(
        "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'credentials'",
        [],
        |r| r.get(0),
    )?;
    if ddl.contains("AUTOINCREMENT") {
        return Ok(());
    }

    // 新表带**当前全部**列，复制的是「新表有、旧表也有」的那些。此前 DDL 与复制清单都是写死的
    // 一份早期列表，凡是排在重建之前才 ADD 的列都会被整列丢掉（`rate_limit_tier` 就这样丢过，
    // 见 `migrates_and_stops_id_reuse`）；改成按旧表实际有什么来复制，以后再加列也不会
    // 掉进这个坑——只要把它写进 [`CREDENTIALS_FULL_DDL`]。
    let mut stmt = conn.prepare("SELECT name FROM pragma_table_info('credentials')")?;
    let old_cols: Vec<String> =
        stmt.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
    drop(stmt);
    let copy: Vec<&str> = CREDENTIALS_FULL_DDL
        .iter()
        .map(|(name, _)| *name)
        .filter(|name| old_cols.iter().any(|c| c == name))
        .collect();
    let cols = copy.join(", ");
    let defs: Vec<String> = CREDENTIALS_FULL_DDL.iter().map(|(n, d)| format!("{n} {d}")).collect();

    conn.execute_batch(&format!(
        "BEGIN;
         CREATE TABLE credentials_new ({}) STRICT;
         INSERT INTO credentials_new ({cols}) SELECT {cols} FROM credentials;
         DROP TABLE credentials;
         ALTER TABLE credentials_new RENAME TO credentials;
         CREATE UNIQUE INDEX IF NOT EXISTS uq_credentials_refresh_token
             ON credentials(refresh_token);
         CREATE INDEX IF NOT EXISTS idx_credentials_priority
             ON credentials(priority, id);
         COMMIT;",
        defs.join(",\n            ")
    ))
    .context("failed to migrate credentials to AUTOINCREMENT")?;
    Ok(())
}

/// `credentials` 表**当前全部**列的定义，只给 [`migrate_credentials_autoincrement`] 重建用。
///
/// **加列要同步三处**：`init_schema` 里那条 ADD COLUMN、[`COLS`] / [`row_to_cred`]、以及这里。
/// 漏了这里，一张旧的非 AUTOINCREMENT 库升级时那一列就会被整列丢掉；
/// `migrates_and_stops_id_reuse` 用 [`COLS`] 逐列核对，漏了会红。
const CREDENTIALS_FULL_DDL: &[(&str, &str)] = &[
    ("id", "INTEGER PRIMARY KEY AUTOINCREMENT"),
    ("label", "TEXT NOT NULL DEFAULT ''"),
    ("tier", "TEXT"),
    ("org_type", "TEXT"),
    ("access_token", "TEXT NOT NULL"),
    ("refresh_token", "TEXT NOT NULL"),
    ("expires_at", "INTEGER NOT NULL"),
    ("priority", "INTEGER NOT NULL DEFAULT 2"),
    ("disabled", "INTEGER NOT NULL DEFAULT 0 CHECK (disabled IN (0,1))"),
    ("created_at", "INTEGER NOT NULL DEFAULT (unixepoch())"),
    ("updated_at", "INTEGER NOT NULL DEFAULT (unixepoch())"),
    ("device_limit", "INTEGER NOT NULL DEFAULT 0"),
    ("ban_reason", "TEXT"),
    ("account_uuid", "TEXT"),
    ("resume_at", "INTEGER"),
    ("proxy", "TEXT"),
    ("rpm_limit", "INTEGER NOT NULL DEFAULT 0"),
    ("rate_limit_tier", "TEXT"),
    ("org_uuid", "TEXT"),
    ("subscription_created_at", "TEXT"),
    ("quota_pause_pct", "INTEGER"),
    ("quota_pause_pct_7d", "INTEGER"),
    ("session_limit", "INTEGER NOT NULL DEFAULT 0"),
    ("org_name", "TEXT"),
    ("seat_tier", "TEXT"),
    ("subscription_status", "TEXT"),
    ("extra_usage_enabled", "INTEGER"),
];

fn row_to_cred(row: &Row) -> rusqlite::Result<Credential> {
    Ok(Credential {
        id: row.get(0)?,
        label: row.get(1)?,
        tier: row.get(2)?,
        access_token: row.get(3)?,
        refresh_token: row.get(4)?,
        expires_at: row.get::<_, i64>(5)? as u64,
        priority: row.get(6)?,
        disabled: row.get::<_, i64>(7)? != 0,
        created_at: row.get::<_, i64>(8)? as u64,
        updated_at: row.get::<_, i64>(9)? as u64,
        device_limit: row.get(10)?,
        ban_reason: row.get(11)?,
        account_uuid: row.get(12)?,
        resume_at: row.get::<_, Option<i64>>(13)?.map(|t| t as u64),
        org_type: row.get(14)?,
        proxy: row.get(15)?,
        rpm_limit: row.get(16)?,
        rate_limit_tier: row.get(17)?,
        org_uuid: row.get(18)?,
        subscription_created_at: row.get(19)?,
        quota_pause_pct: row.get(20)?,
        quota_pause_pct_7d: row.get(21)?,
        session_limit: row.get(22)?,
        org_name: row.get(23)?,
        seat_tier: row.get(24)?,
        subscription_status: row.get(25)?,
        extra_usage_enabled: row.get::<_, Option<i64>>(26)?.map(|v| v != 0),
    })
}

/// 写一条设备绑定（新建或改绑到 `cred_id`）。按设备占名额的选号与按会话占名额时的设备亲和
/// 记录（[`Select::per_session`]）共用这一份。
///
/// 过了保留期、后台还没来得及删的那一行会在这里被撞上（此前它已被删掉，走的是纯 INSERT）。
/// 那是一条**新**绑定，故 created_at 与 request_count 归零重来——否则设备明细里会显示一个
/// 几天前建立、请求数接着往上加的绑定，而那台设备其实刚被重新调度过。`SET` 右侧读的都是
/// 冲突前那一行的值（SQLite 语义），故 CASE 里的 last_seen_at 是旧值，与放在哪一行无关。
fn upsert_device_binding(
    conn: &Connection,
    device_id: &str,
    cred_id: i64,
    retention_secs: i64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO device_bindings (device_id, cred_id) VALUES (?1, ?2)
         ON CONFLICT(device_id) DO UPDATE
            SET cred_id = ?2, last_seen_at = unixepoch(), \
                created_at = CASE WHEN ?3 > 0 \
                                    AND last_seen_at < unixepoch() - ?3 \
                                  THEN unixepoch() ELSE created_at END, \
                request_count = CASE WHEN ?3 > 0 \
                                       AND last_seen_at < unixepoch() - ?3 \
                                     THEN 1 ELSE request_count + 1 END",
        params![device_id, cred_id, retention_secs],
    )?;
    Ok(())
}

/// 沿用来访会话 id、不派生的会话绑定（[`Select::passthrough_session`]）在 `slot` 列记的值。
/// 不占槽位：[`free_session_slot`] 只从 0 起找空位，负数永远不与之相撞。
pub const PASSTHROUGH_SLOT: i64 = -1;

/// 该凭证当前**空着**的最小会话槽位：活跃（TTL 内）绑定占着的槽位之外，从 0 起最小的那个；
/// `prefer` 给出的槽位空着就直接用它（休眠的软绑定回来优先回原槽位，会话 id 才不换）。
/// 休眠绑定占过的槽位算空——它们不占名额，槽位（也就是会话 id）让给活跃的对话复用。
fn free_session_slot(
    conn: &Connection,
    cred_id: i64,
    ttl_secs: i64,
    prefer: Option<i64>,
) -> Result<i64> {
    let active = if ttl_secs > 0 { "AND last_seen_at >= unixepoch() - ?2" } else { "" };
    let mut stmt = conn.prepare(&format!(
        "SELECT slot FROM session_bindings WHERE cred_id = ?1 {active} ORDER BY slot ASC"
    ))?;
    let taken: Vec<i64> = if ttl_secs > 0 {
        stmt.query_map(params![cred_id, ttl_secs], |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?
    } else {
        stmt.query_map([cred_id], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
    };
    if let Some(p) = prefer
        && p >= 0
        && !taken.contains(&p)
    {
        return Ok(p);
    }
    let mut slot = 0i64;
    for t in taken {
        if t > slot {
            break;
        }
        if t == slot {
            slot += 1;
        }
    }
    Ok(slot)
}

/// [`CredentialStore::select_for_device`] 里这条请求按哪张表粘住账号、占哪种名额。
/// 两张表列名不同、上限字段不同、拒绝的错误类型不同，其余规则逐条相同。
#[derive(Clone, Copy)]
enum Binding<'a> {
    /// 客户端自带的设备身份，`device_bindings`。
    Device(&'a str),
    /// 模拟路径上没有设备身份的来访，按会话键，`session_bindings`。见 [`Select::session_key`]。
    Session(&'a str),
}

impl<'a> Binding<'a> {
    fn table(self) -> &'static str {
        match self {
            Self::Device(_) => "device_bindings",
            Self::Session(_) => "session_bindings",
        }
    }

    fn column(self) -> &'static str {
        match self {
            Self::Device(_) => "device_id",
            Self::Session(_) => "session_key",
        }
    }

    fn key(self) -> &'a str {
        match self {
            Self::Device(k) | Self::Session(k) => k,
        }
    }

    /// 所有可调度的号名额都满了时的拒绝理由。
    fn limit_error(self) -> anyhow::Error {
        match self {
            Self::Device(_) => DeviceLimitReached.into(),
            Self::Session(_) => SessionLimitReached.into(),
        }
    }
}

/// [`CredentialStore::select_for_device`] 的入参。
///
/// 做成结构体而不是一串位置参数：两个 `Option<&str>`（`device_id` 与 `model`）挨在一起，
/// 位置传参写反了照样编译得过，而那是一个「设备粘性按模型名走」的静默错误。
#[derive(Default, Clone, Copy)]
pub struct Select<'a> {
    /// 客户端设备标识；`None` 即裸请求（不绑定、不占设备名额）。
    pub device_id: Option<&'a str>,
    /// **模拟会话键**：来访走模拟路径且没有设备身份时，代理算出来的这条会话的键，形如
    /// `lb:v2:sid:<来访自带的会话 id>` 或 `lb:v2:pfx:<缓存前缀加首条用户消息的指纹>`——命名空间
    /// 加口径版本加来源段，取法见 `crate::proxy::session_binding_key`。
    /// `Some` 且 `device_id` 为 `None` 时按它粘住账号并占该账号
    /// 的**会话名额**（`session_bindings`，上限 `session_limit` / [`DEFAULT_SESSION_LIMIT`]），
    /// 规则与设备绑定逐条相同（TTL、软绑定、改绑、全满时拒——[`SessionLimitReached`]）。
    /// `device_id` 有值时只在 [`Self::per_session`] 下才用它，否则按设备绑定，一条请求不占两份
    /// 名额。非模拟路径只有 `per_session` 时才有值（真实客户端自带的会话 id，没带时按前缀指纹）。
    ///
    /// 绑定行还记着这条会话在该号上占的**槽位**（[`free_session_slot`]）：出站会话 id 由
    /// 「账号 + 槽位」派生，槽位释放后下一个对话复用同一个 id，见 [`CredentialStore::session_slot`]。
    pub session_key: Option<&'a str>,
    /// 带设备身份的来访也**按会话**占名额（[`Self::session_key`] 那张表与 `session_limit`），
    /// 设备上限不再生效。代理在「上游看到的设备身份已经收敛」时置真：设备指纹归一化开着、
    /// 出站 device_id 由「账号 + 平台 + 客户端版本」派生，同一个号在上游只呈现寥寥几台设备，
    /// 绑了几台真实机器上游根本看不见，看得见的是每台设备下同时活跃几条会话。
    ///
    /// 设备绑定行照写，但只当**亲和记录**用：同一台设备新开的会话优先落在它上次那个号上，
    /// 一个人的活不会被均衡到一圈号上去。置真而 `session_key` 为 `None`（额度探测那类不该占
    /// 名额的请求）时只按亲和选号，不写会话绑定、不受任何名额约束。
    pub per_session: bool,
    /// 这条会话出站沿用来访自己的会话 id（真实客户端，不走模拟），不分配派生用的槽位：
    /// 绑定行 `slot` 记 `-1`，后台列会话时据此按来访 id 算上游看到的那个。
    pub passthrough_session: bool,
    /// 设备绑定**占名额**的有效期（秒）；`<= 0` 表示永不过期。
    pub ttl_secs: i64,
    /// 软绑定保留期（秒）：绑定行超过 [`Self::ttl_secs`] 后不再占名额，但在这个时长内仍然
    /// 留着，设备回来时优先回原号。`<= 0` 表示永久保留（只要不被解绑/停号就一直在）。
    pub retention_secs: i64,
    /// 模拟会话绑定的有效期与保留期，语义同上面两项，只是作用在 `session_bindings` 上、
    /// 单独配置（[`SESSION_BINDING_TTL`] / [`SESSION_BINDING_RETENTION`]）。
    pub session_ttl_secs: i64,
    pub session_retention_secs: i64,
    /// 本次请求是否计入裸请求速率上限（只有真正消耗额度的路径才该计，见
    /// `crate::proxy::is_billable_messages`）。
    pub rate_limited: bool,
    /// 本次请求已经试过的凭证（上游 429 换号重试时传入），一律出局。
    pub exclude: &'a [i64],
    /// 请求的模型名，用于按模型判定冷却（fable 那类模型级 429 不该拖累整个账号）。
    pub model: Option<&'a str>,
}

impl CredentialStore {
    /// 按 device_id 做粘性选择，返回选中的凭证（刷新在锁外由调用方处理）。
    ///
    /// 规则：
    /// 1. TTL 内的绑定（**活跃**）且该凭证仍启用 → 复用（更新 last_seen / request_count），
    ///    已占名额的设备不再受上限约束。
    /// 2. TTL 外但仍在保留期内的绑定（**软绑定**）→ 仍优先回原号，但要重新占名额：
    ///    原号必须仍启用、不在冷却、未被本轮排除，且还有空位；不满足就当新设备重选并**改绑**。
    /// 3. 绑定的凭证已停用或删除 → 作为新设备重新选择（选中谁就改绑到谁）。
    /// 4. 新设备 → 在仍有名额的启用凭证中做负载均衡：选“当前设备数最少”者并绑定；
    ///    同数时按 (priority, id) 决定，保持确定性。
    /// 5. 所有启用凭证均达设备上限 → 硬性拒绝，返回 [`DeviceLimitReached`]（代理映射为 429）。
    ///
    /// 被上游 429 打过冷却的号（见 [`RateLimitCooldown`]）在**任何**分支之前就被剔出候选，
    /// 包括已有绑定命中那一支——绑定的号在冷却中会被解绑并改选到别的号上。冷却是硬门禁：
    /// 候选被冷却清空时返回 [`AllRateLimited`]（代理映射为 429 + `retry-after`），
    /// 不再退回「忽略冷却照常选」。
    ///
    /// `device_id` 为 `None`（请求未带 metadata）时无从绑定/计数：退化为负载均衡挑选，
    /// 不写绑定、也不受**设备**上限约束——但在 `rate_limited` 为真时受**裸请求速率上限**
    /// 约束（见 [`Self::bare_rate_limit`]）：已发满的凭证在本轮被跳过，自然分流到其它号；
    /// 所有号都满才返回 [`BareRateLimited`]（代理映射为 429 + `retry-after`）。
    ///
    /// **例外是带 `session_key` 的**（模拟路径、没有设备身份，见 [`Select::session_key`]）：
    /// 它们按会话键走与设备绑定**逐条相同**的规则——`session_bindings` 表、`session_limit` /
    /// [`DEFAULT_SESSION_LIMIT`] 上限、同一套 TTL 与保留期、同样的软绑定与改绑，全满时返回
    /// [`SessionLimitReached`]。与设备绑定只差一处：它们仍是裸请求，裸请求速率上限照旧管着
    /// （命中的原号裸窗口打满时当作没位置、往下改选）。
    ///
    /// **账号 RPM 上限**（见 [`Self::default_rpm_limit`]）是所有分支共同的最后一道门，
    /// 且两个分支的行为**故意不同**：
    ///
    /// - 还没定下号的（新设备、裸请求、原号不可用要改选）→ 打满的号在本轮被跳过，
    ///   自然分流到别的号，全部打满才返回 [`RpmLimited`]；
    /// - **已经粘在某个号上的**（命中既有绑定）→ 该号打满就**直接拒**，不改选别的号。
    ///   换号意味着把设备改绑过去，而 thinking 块的签名是跟着账号走的，这条会话之后每一轮
    ///   都要先撞一次 400 再降级重发（见 `crate::proxy::retry_demoted_thinking`）；
    ///   让客户端照 `retry-after` 退避几秒，等这个号的窗口滚出名额，会话就还在原来的号上。
    ///
    /// 与裸请求上限不同，RPM **不看 `rate_limited`，每一次选号都计**：口径要和账号列表里
    /// 那个「当前 RPM」对得上（那是 `usage_logs` 最近 60 秒的条数，`count_tokens` 一样在内），
    /// 否则会出现「上限 30、显示 45」这种解释不清的画面。
    ///
    /// `rate_limited` 由调用方判定——代理只对**真正消耗额度的**路径置真
    /// （`/v1/messages`，见 `crate::proxy::is_billable_messages`）。`count_tokens` 这类
    /// 既不产生 usage、也不消耗额度的路径不计：拿它占名额只会把真正的请求挤掉，
    /// 而客户端的 `/context` 显示与压缩前预估全靠它。
    ///
    /// `ttl_secs > 0` 时超时未活跃的绑定**不再占名额**（惰性过期），但绑定行本身留到保留期
    /// （`retention_secs`）满才删——这就是「软绑定」：设备隔了几小时再来，只要原号还有空位就
    /// 回原号。thinking 块的签名是跟着账号走的，中途换号会让这条会话之后每一轮都先撞一次 400
    /// 再降级重发（见 `crate::proxy::retry_demoted_thinking`），软绑定就是为了少踩这个。
    /// `ttl_secs <= 0` 表示绑定永不过期，此时保留期无从谈起（不删任何行）。
    /// 全部操作在单次持锁内完成，避免与其它写入竞态。
    ///
    /// **限流按「选一次号」计，不是按「客户端请求」计**：刷新失败换号那条路
    /// （[`select_with_refresh_failover`]）每轮都会重选，故一次客户端请求最多可能扣掉几个
    /// 名额。那条路只在凭证被上游作废时才走（罕见），宁可多扣也好过给它开一个绕过限流的口子。
    ///
    /// 反过来，**不经选号的那些请求一条都不计**：连通性测试指定打哪个号（不走这里），却照样
    /// 写 `usage_logs`。所以列表里的 RPM 可能比限流器数到的略高一点点——探活是人手点出来的，
    /// 量级上不构成干扰，但对不上时要知道差在哪。
    #[cfg(test)]
    pub fn select_for_device(&self, sel: Select<'_>) -> Result<Credential> {
        self.select_with_slot(sel).map(|(cred, _)| cred)
    }

    /// [`Self::select_for_device`] 的完整版：连同这条请求在选中的号上占的**会话槽位**一起返回
    /// （按会话键绑定时为 `Some`，其余 `None`）。转发路径要用槽位派生会话 id，选号时刚写过
    /// 绑定行、值就在手上，不必再按键查一遍。
    pub fn select_with_slot(&self, sel: Select<'_>) -> Result<(Credential, Option<i64>)> {
        let Select {
            device_id,
            session_key,
            per_session,
            passthrough_session,
            ttl_secs,
            retention_secs,
            session_ttl_secs,
            session_retention_secs,
            rate_limited,
            exclude,
            model,
        } = sel;
        // 这条请求按什么粘住账号、占哪种名额：有设备身份按设备（`per_session` 时改按会话）；
        // 没有设备身份但带会话键（模拟路径）按会话；都没有就是裸请求。一条请求只占一份名额。
        // `per_session` 而没有会话键的（额度探测）不绑定，只按下面的设备亲和选号。
        let binding = match (device_id, session_key) {
            (Some(d), _) if !per_session => Some(Binding::Device(d)),
            (_, Some(k)) => Some(Binding::Session(k)),
            (_, None) => None,
        };
        // 按会话占名额的带设备来访，设备绑定行只当亲和记录：选号时优先它指的那个号，选完改写
        // 成这次落的号（见 [`Select::per_session`]）。
        let affinity_device = device_id.filter(|_| per_session);
        // 这几项须在取锁前读（内部自己会取锁，parking_lot 不可重入）。
        let default_limit = self.default_device_limit();
        let default_session_limit = self.default_session_limit();
        let (rate_limit, rate_window) = (self.bare_rate_limit(), self.bare_rate_window_secs());
        let default_rpm = self.default_rpm_limit();
        let conn = self.conn.lock();

        // RPM 的窗口就是账号列表那一列的窗口（60 秒），两处共用同一个常量：限的和看到的
        // 必须是同一个口径，否则「上限 30」和列表里的「RPM 45」谁也解释不了谁。
        let rpm_window = Duration::from_secs(RPM_WINDOW_SECS as u64);
        let rpm_limit_of = |c: &Credential| effective_rpm_limit(c.rpm_limit, default_rpm);
        let rpm_room = |c: &Credential| self.rpm_rate.has_room(c.id, rpm_limit_of(c), rpm_window);
        // 全员打满时的 `retry-after`：取最早腾出名额的那个号——早一秒重试都是白撞。
        let rpm_full = |cands: &[&Credential]| -> anyhow::Error {
            let retry_after_secs = cands
                .iter()
                .map(|c| self.rpm_rate.retry_after_secs(&c.id, rpm_window))
                .min()
                .unwrap_or(1);
            RpmLimited { retry_after_secs, sticky: false }.into()
        };

        // 「连保留期都过了」的绑定行由后台定时清（[`Self::prune_expired_bindings`]，
        // 挂在 `web::run` 里），**不在这条路上删**：两条 DELETE 都按 last_seen_at 划线，
        // 而这里是每条转发请求都要走一遍的选号路径，等于每请求两次写事务。
        //
        // 清理时机与判定因此解耦：下面命中既有绑定那一步自己按保留期过滤（见 `bound` 的
        // 查询），所以「行还在但已过保留期」与「行已被删掉」对选号是同一个结果——后台
        // 什么时候跑都不影响这里选出谁。TTL 到点的那些照旧不删：它们从那一刻起就不占名额
        //（下面的 counts 按 TTL 过滤），但行还在，设备回来时还能循着它回原号。
        let device_retention = effective_retention(ttl_secs, retention_secs).unwrap_or(0);
        let session_retention =
            effective_retention(session_ttl_secs, session_retention_secs).unwrap_or(0);

        // 限流暂停到点的号先放回来，再挑——否则它们要等到有人打开控制台列表才回得了池子。
        Self::resume_due(&conn)?;

        // 启用凭证，按 (priority, id) 升序。
        let mut stmt = conn.prepare(&format!(
            "SELECT {COLS} FROM credentials WHERE disabled = 0 ORDER BY priority ASC, id ASC"
        ))?;
        let all: Vec<Credential> =
            stmt.query_map([], row_to_cred)?.collect::<rusqlite::Result<_>>()?;
        drop(stmt);
        if all.is_empty() {
            // 一个能用的都没有。若其中有「限流暂停、还没到点」的，这不是配置问题而是限流：
            // 回 429 + 最早那个的恢复时刻，比一句「没有可用凭证，请先登录」诚实得多
            // （后者会把运维引去查登录，而实际上号都在、只是在等额度回血）。
            let soonest: Option<(i64, String, Option<String>, i64)> = conn
                .query_row(
                    "SELECT id, label, ban_reason, resume_at FROM credentials \
                      WHERE disabled = 1 AND resume_at IS NOT NULL \
                      ORDER BY resume_at ASC, id ASC LIMIT 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
                .optional()?;
            if let Some((id, label, reason, at)) = soonest {
                let retry_after_secs = (at - crate::credentials::now_secs() as i64).max(1);
                let refresh_failed = reason
                    .as_deref()
                    .and_then(refresh_pause_detail)
                    .map(|detail| RefreshFailed::paused(id, label, detail));
                return Err(AllRateLimited { retry_after_secs, refresh_failed }.into());
            }
            anyhow::bail!("no available credentials; add an account first");
        }

        // 上游判过「套餐不含这个模型」的号先出局（见 [`Self::deny_model`]）：这不是等一会就好
        // 的事，也不该拿去撞。**所有启用号**都被判过才是「换模型」——这一判必须排在下面「排除
        // 已试过的号」之前：换号重试时刚被判的那个号就在 `exclude` 里，先排除再看会把它漏数，
        // 单个 Pro 号的池子永远报不出这条、只会把上游 429 原样透传。
        //
        // 但先看有没有**只是被限流暂停**（`disabled = 1` 且 `resume_at` 非空）且没被判过的号：
        // 那是「等一会」不是「换模型」——两小时后回来的 Max 号被说成不存在、客户端拿着 403 去
        // 换模型，比一发带恢复时刻的 429 糟得多。
        let denied: HashSet<i64> = match model {
            Some(m) => Self::denied_creds_for(&conn, m)?,
            None => HashSet::new(),
        };
        if let Some(m) = model
            && all.iter().all(|c| denied.contains(&c.id))
        {
            if let Some(at) = Self::soonest_paused_resume(&conn, &denied)? {
                let retry_after_secs = (at - crate::credentials::now_secs() as i64).max(1);
                return Err(AllRateLimited { retry_after_secs, refresh_failed: None }.into());
            }
            return Err(ModelUnsupported { model: m.to_string(), accounts: all.len() }.into());
        }
        // 本次请求已经试过的号（上游 429 换号重试时传进来）直接出局——重试再撞同一个号毫无意义。
        let mut pool = all;
        pool.retain(|c| !exclude.contains(&c.id) && !denied.contains(&c.id));
        if pool.is_empty() {
            anyhow::bail!(
                "no other available credentials remain after excluding those already tried"
            );
        }
        // 冷却中的号让位给还能用的；**全部都在冷却就直接拒**——冷却是硬门禁，被上游 429 过的
        // 号在解冻前一律不调度。等待时间取最早解冻的那个，客户端照它重试即可。
        let creds: Vec<Credential> =
            pool.iter().filter(|c| !self.cooldown.is_cooling(c.id, model)).cloned().collect();
        if creds.is_empty() {
            let retry_after_secs = pool
                .iter()
                .map(|c| self.cooldown.remaining_for(c.id, model))
                .min()
                .unwrap_or(0)
                .max(1);
            return Err(AllRateLimited { retry_after_secs, refresh_failed: None }.into());
        }

        // 各凭证当前**占名额**的设备数或模拟会话数：只数 TTL 内活跃的绑定，休眠的软绑定不占位
        // （口径与 [`Self::device_counts`] / [`Self::session_counts`] 一致，后台看到的数就是这里
        // 用来判上限的数）。只数本次绑定的那一种：名额与负载均衡都只看它，另一张表数了也不用。
        let active_counts = |table: &str, ttl_secs: i64| -> Result<HashMap<i64, i64>> {
            let mut cstmt = conn.prepare(&active_counts_sql(table, ttl_secs))?;
            let map_row = |r: &Row| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?));
            let rows = if ttl_secs > 0 {
                cstmt.query_map([ttl_secs], map_row)?
            } else {
                cstmt.query_map([], map_row)?
            };
            let mut counts = HashMap::new();
            for row in rows {
                let (cid, n) = row?;
                counts.insert(cid, n);
            }
            Ok(counts)
        };
        let counts = match binding {
            Some(Binding::Session(_)) => active_counts("session_bindings", session_ttl_secs)?,
            _ => active_counts("device_bindings", ttl_secs)?,
        };
        // 这条请求走哪张表，TTL 与保留期就都用哪张表的：下面判「绑定还在有效期内吗」与分槽位
        // 按 TTL，判「这条绑定还算不算数」按保留期。
        let (binding_ttl, binding_retention) = match binding {
            Some(Binding::Session(_)) => (session_ttl_secs, session_retention),
            _ => (ttl_secs, device_retention),
        };

        // 当前占名额的数（已排除 TTL 外的休眠绑定）：按会话绑定时数会话，其余数设备——裸请求
        // 不占名额，但负载均衡仍按设备数排，与原来一样。
        let used = |c: &Credential| counts.get(&c.id).copied().unwrap_or(0);
        // 生效上限：账号未单独配置（== 0）时套用对应的全局默认。
        let limit_of = |c: &Credential| match binding {
            Some(Binding::Session(_)) => {
                effective_session_limit(c.session_limit, default_session_limit)
            }
            _ => effective_device_limit(c.device_limit, default_limit),
        };
        // 还塞得下一台设备 / 一条会话吗（上限 <= 0 即不限）。
        let has_room = |c: &Credential| limit_of(c) <= 0 || used(c) < limit_of(c);
        // 裸请求速率上限：没有设备身份的都算裸请求，按会话键绑定的也是——那道上限限的是「没有
        // 设备身份可依据」的流量，会话键是 luban 自己从体里算的，不是客户端的身份。
        let bare_window = Duration::from_secs(rate_window.max(1) as u64);
        let bare_ok = |c: &Credential| {
            device_id.is_some()
                || !rate_limited
                || self.bare_rate.has_room(c.id, rate_limit, bare_window)
        };

        // 1/2/3) 命中既有绑定。
        if let Some(b) = binding {
            // 第二列是「这条绑定还在 TTL 内吗」，交给 SQLite 与清理/计数用同一个 unixepoch()
            // 时钟判定，免得和进程时钟差出一个边界。
            //
            // `WHERE` 上那道保留期过滤是**清理挪去后台之后**补的：过了保留期的行在被后台删掉
            // 之前还留在表里，不滤掉的话它会被当成休眠软绑定续上，设备就回到了一个本该已经
            // 忘掉的号。滤掉之后，「行还在但过期了」与「行已删」对这里是同一个结果——后台多久
            // 跑一次都不改变选号结果。走的是主键点查，多一个条件不增加代价。
            let bound: Option<(i64, bool)> = conn
                .query_row(
                    &format!(
                        "SELECT cred_id, (?2 <= 0 OR last_seen_at >= unixepoch() - ?2) \
                           FROM {} WHERE {} = ?1 \
                            AND (?3 <= 0 OR last_seen_at >= unixepoch() - ?3)",
                        b.table(),
                        b.column()
                    ),
                    params![b.key(), binding_ttl, binding_retention],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((cid, active)) = bound {
                // 原号仍可调度（启用、不在冷却、本轮没试过）时才谈复用。
                if let Some(c) = creds.iter().find(|c| c.id == cid) {
                    // 活跃绑定本来就占着名额，直接续；休眠的软绑定要重新占一个位置，
                    // 原号满了就只能改选——否则设备上限形同虚设。按会话绑定的还要过裸请求
                    // 速率上限：原号的裸窗口打满了就当没位置，往下改选（设备绑定不受这道管）。
                    if (active || has_room(c)) && bare_ok(c) {
                        // RPM 打满 → **就地拒**，不往下走改选那条路（理由见本函数文档：
                        // 改选会改绑，而改绑会让这条会话每一轮先撞一次 thinking 签名 400）。
                        if !rpm_room(c) {
                            return Err(RpmLimited {
                                retry_after_secs: self.rpm_rate.retry_after_secs(&c.id, rpm_window),
                                sticky: true,
                            }
                            .into());
                        }
                        let slot = match b {
                            Binding::Session(key) => {
                                let old: i64 = conn.query_row(
                                    "SELECT slot FROM session_bindings WHERE session_key = ?1",
                                    [key],
                                    |r| r.get(0),
                                )?;
                                // 活跃绑定续用原槽位；休眠的会话软绑定回来要重新占槽位：原槽位
                                // 空着就还用它，被别的对话拿走了就取最小的空位——会话 id 随槽位变，
                                // 这条对话在上游成了另一条会话。沿用来访会话 id 的不占槽位（-1）；
                                // 原来记的是 -1 而这次要派生的，也得新取一个。
                                let slot = if passthrough_session {
                                    PASSTHROUGH_SLOT
                                } else if active && old >= 0 {
                                    old
                                } else {
                                    free_session_slot(&conn, c.id, session_ttl_secs, Some(old))?
                                };
                                conn.execute(
                                    "UPDATE session_bindings \
                                        SET slot = ?2, last_seen_at = unixepoch(), \
                                            request_count = request_count + 1, \
                                            last_model = COALESCE(?3, last_model) \
                                      WHERE session_key = ?1",
                                    params![key, slot, model],
                                )?;
                                (slot >= 0).then_some(slot)
                            }
                            Binding::Device(did) => {
                                conn.execute(
                                    "UPDATE device_bindings \
                                        SET last_seen_at = unixepoch(), \
                                            request_count = request_count + 1 \
                                      WHERE device_id = ?1",
                                    [did],
                                )?;
                                None
                            }
                        };
                        if let Some(did) = affinity_device {
                            upsert_device_binding(&conn, did, c.id, device_retention)?;
                        }
                        self.rpm_rate.take(c.id, rpm_limit_of(c), rpm_window);
                        if device_id.is_none() && rate_limited {
                            self.bare_rate.take(c.id, rate_limit, bare_window);
                        }
                        return Ok((c.clone(), slot));
                    }
                }
                // 回不去原号（停用/删除/冷却中/本轮已试过/名额已满）：往下重新选择，
                // 选中谁就**改绑**到谁（`INSERT … ON CONFLICT DO UPDATE cred_id`）。
                // 冷却结束后这台设备不会自己回到原号——粘性以最后一次选择为准，
                // 这正是「429 换号重试要改绑」想要的语义。
            }
        }

        // 4/5) 优先级分档调度：优先级为主键（数值小者优先），同一档内再按设备数
        //      负载均衡，最后 id 兜底。低优先级档仅在高优先级档全部占满/不可用后才触及。
        // (priority, 设备数, id) 是唯一的排序口径；两个分支都从这一份有序表里挑，
        // 逐道门过滤，第一个全过的即中。
        // fable/mythos 这类只有高档套餐才含的模型，同一优先级档内 Max 号排前面，等级未知的
        // 与团队/企业席位（`Team Standard` 之类，含不含这些模型说不准）其次，Pro/Free 垫底。**只排序不剔除**：准入以上游的判决为准（见上面的 denied），
        // 这里猜错也只是多换一次号；而排在前面能让绝大多数请求第一发就落在能用的号上。
        let premium = model.is_some_and(premium_model);
        let plan_rank = |c: &Credential| -> u8 {
            if !premium {
                return 0;
            }
            match c.tier.as_deref() {
                Some(t) if t.starts_with("Max") => 0,
                None => 1,
                Some(t) if t.starts_with("Team") || t.starts_with("Enterprise") => 1,
                Some(_) => 2,
            }
        };
        let mut ordered: Vec<&Credential> = creds.iter().collect();
        ordered.sort_by_key(|c| (c.priority, plan_rank(c), used(c), c.id));
        // 设备亲和（按会话占名额的带设备来访，见 [`Select::per_session`]）：这台设备上次落的号
        // 提到最前，新会话跟着它走——一台机器的活集中在一个号上，才像一个真实用户。它照样要过
        // 下面的名额与 RPM 两道门，过不去就按原顺序溢出。premium 模型下不越过档次更高的号：
        // 亲和是软偏好，不该把一条 fable 请求从 Max 号拉到 Pro 号上去撞。
        if let Some(did) = affinity_device
            && let Some(home) = conn
                .query_row(
                    "SELECT cred_id FROM device_bindings WHERE device_id = ?1 \
                        AND (?2 <= 0 OR last_seen_at >= unixepoch() - ?2)",
                    params![did, device_retention],
                    |r| r.get::<_, i64>(0),
                )
                .optional()?
            && let Some(pos) = ordered.iter().position(|c| c.id == home)
            && ordered.first().is_some_and(|f| plan_rank(ordered[pos]) <= plan_rank(f))
        {
            let c = ordered.remove(pos);
            ordered.insert(0, c);
        }
        let chosen = match binding {
            Some(b) => {
                // 硬限制：仅在仍有名额者（生效上限 <=0 不限，或 used<上限）中选；
                // 当前优先级档全满时其成员被过滤掉，自然溢出到下一档；全部满则拒绝。
                let with_room: Vec<&Credential> =
                    ordered.iter().copied().filter(|c| has_room(c)).collect();
                if with_room.is_empty() {
                    return Err(b.limit_error());
                }
                // 按会话绑定的还是裸请求，要过裸请求速率上限（设备绑定的 `bare_ok` 恒真）。
                let with_bare: Vec<&Credential> =
                    with_room.iter().copied().filter(|c| bare_ok(c)).collect();
                if with_bare.is_empty() {
                    return Err(BareRateLimited { retry_after_secs: rate_window }.into());
                }
                // 名额与 RPM 是两回事，故两道门分开判：都过不去时要能说清是哪一道拦的
                // ——名额满是「换台机器也没用」，RPM 满是「等几秒就好」。
                match with_bare.iter().copied().find(|c| rpm_room(c)) {
                    Some(c) => c,
                    None => return Err(rpm_full(&with_bare)),
                }
            }
            None => {
                // 无 device_id 也无会话键：不占名额，但要过裸请求速率上限。
                let with_bare: Vec<&Credential> =
                    ordered.iter().copied().filter(|c| bare_ok(c)).collect();
                if with_bare.is_empty() {
                    return Err(BareRateLimited { retry_after_secs: rate_window }.into());
                }
                match with_bare.iter().copied().find(|c| rpm_room(c)) {
                    Some(c) => c,
                    None => return Err(rpm_full(&with_bare)),
                }
            }
        };

        let slot = match binding {
            Some(Binding::Device(did)) => {
                upsert_device_binding(&conn, did, chosen.id, device_retention)?;
                None
            }
            Some(Binding::Session(key)) => {
                // 新对话（或改绑到别的号的对话）在选中的号上取最小的空槽位。上面刚判过
                // has_room，所以上限内必有空位；不限时槽位按需增长、释放后复用。沿用来访
                // 会话 id 的不派生、不占槽位。
                let slot = if passthrough_session {
                    PASSTHROUGH_SLOT
                } else {
                    free_session_slot(&conn, chosen.id, session_ttl_secs, None)?
                };
                conn.execute(
                    "INSERT INTO session_bindings (session_key, cred_id, slot, last_model) \
                     VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(session_key) DO UPDATE
                        SET cred_id = ?2, slot = ?3, last_seen_at = unixepoch(), \
                            created_at = CASE WHEN ?5 > 0 \
                                                AND last_seen_at < unixepoch() - ?5 \
                                              THEN unixepoch() ELSE created_at END, \
                            request_count = CASE WHEN ?5 > 0 \
                                                   AND last_seen_at < unixepoch() - ?5 \
                                                 THEN 1 ELSE request_count + 1 END, \
                            last_model = COALESCE(?4, last_model)",
                    params![key, chosen.id, slot, model, session_retention],
                )?;
                (slot >= 0).then_some(slot)
            }
            None => None,
        };
        if let Some(did) = affinity_device {
            upsert_device_binding(&conn, did, chosen.id, device_retention)?;
        }
        // 两个窗口都在**选定之后**才记账（而不是边问边记）：一次选号要连过两道窗口，
        // 边问边记的话，过了第一道却卡在第二道的那个号会白扣一个名额。理由详见
        // [`RateWindow::has_room`]——选号全程持着 `conn` 锁，中间插不进第二次选号。
        self.rpm_rate.take(chosen.id, rpm_limit_of(chosen), rpm_window);
        if device_id.is_none() && rate_limited {
            self.bare_rate.take(chosen.id, rate_limit, bare_window);
        }
        Ok((chosen.clone(), slot))
    }
}

/// 刷新失败后最多改选几个凭证。
///
/// 每失败一轮就停用一个凭证（可用池严格变小），循环必然收敛；这个上限只是防御性兜底，
/// 免得停用没生效时打成死循环。也顺带给单次请求的耗时封了顶——每一轮都是一次上游往返。
const MAX_REFRESH_FAILOVER: usize = 5;

/// 刷新没拿到结果（网络 / 代理 / 超时 / 5xx / 非作废的 4xx）后，这个号暂停调度多久。
///
/// 这类失败多半是这个号的出口出了问题（代理挂了、过期了），不停的话绑在它上面的设备每条
/// 请求都要白等一次刷新超时再吃 503。写的是 `resume_at`，与限流暂停同一套恢复：到点
/// 惰性放回、连通性测试通过当场放回。取得短，网络抖一下的号很快就回来了。
const REFRESH_FAIL_PAUSE_SECS: u64 = 120;

/// 刷新失败暂停写进 `ban_reason` 的开头标签，前端 `isRefreshFailurePause` 按它认。
pub const REFRESH_FAIL_PAUSE_TAG: &str = "[refresh-failed]";

/// [`ensure_fresh_token`] 刷新这一步失败、且不是 refresh_token 被作废（那条走
/// [`TokenAttempt::Revoked`]）。带上是哪个号，转发那边据此把流水记到这个号名下；
/// `detail` 是完整错误链（`{:#}`），只进日志、流水与后台，不回给客户端——代理报错里
/// 可能带着代理地址。
#[derive(Debug, Clone)]
pub struct RefreshFailed {
    pub cred_id: i64,
    pub cred_label: String,
    pub detail: String,
    /// 号已经因为刷新失败暂停着（别的请求刚停的），这次没有真去刷：换号循环不再写一遍暂停，
    /// 免得把恢复时刻一次次往后推。
    pub already_paused: bool,
}

impl RefreshFailed {
    /// 库里已有的刷新失败暂停，`detail` 取暂停原因（去掉标签）。
    fn paused(cred_id: i64, cred_label: String, detail: &str) -> Self {
        Self { cred_id, cred_label, detail: detail.to_string(), already_paused: true }
    }
}

/// `ban_reason` 是刷新失败暂停写的，就返回标签后面的原因原文。
fn refresh_pause_detail(reason: &str) -> Option<&str> {
    reason.strip_prefix(REFRESH_FAIL_PAUSE_TAG).map(str::trim_start)
}

/// 一次请求里因刷新失败最多换几次号。刷新一次最多等 [`crate::oauth`] 的 15 秒超时，
/// 本机网络或共用代理挂了的时候每个号都会失败——不封顶的话一条请求要白等
/// [`MAX_REFRESH_FAILOVER`] 轮、再连带停掉那么多个号。作废（`Revoked`）换号不算在内。
const MAX_REFRESH_FAIL_SWAPS: usize = 2;

impl std::fmt::Display for RefreshFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "credential #{} token refresh failed: {}", self.cred_id, self.detail)
    }
}

impl std::error::Error for RefreshFailed {}

/// 一次「拿到该凭证可用 access_token」的尝试结果。可重试的错误（网络抖动、5xx、限流）
/// 走 `Err` 直接冒泡，不在这里表达。
pub enum TokenAttempt {
    /// 拿到可用 access_token。
    Ready(String),
    /// 该凭证的 refresh_token 已被上游永久作废，重试没有意义——外层会停用它并改选其它号。
    /// 携带写入 `ban_reason` 的原因。
    Revoked(String),
}

/// 代理转发使用：按 device_id 粘性选出凭证并返回 (access_token, 该凭证)（必要时刷新）。
///
/// 选择见 [`CredentialStore::select_for_device`]。若命中的凭证进入刷新窗口，
/// 则调用 OAuth 刷新并回写。注意刷新是异步 IO，不持有 DB 锁。
///
/// **刷新失败要自动换号**：`select_for_device` 在返回前就写好了设备绑定，之后才轮到刷新。
/// 若刷新失败直接把错误抛出去，这个设备就被钉死在坏号上——绑定还在，下一次请求照样选中它，
/// 永远 503 直到人工介入。故这里在「refresh_token 已被作废」时停用该凭证
/// （[`CredentialStore::record_ban`] 会连带清掉它的设备绑定），再重选一个号继续。
/// 网络抖动/5xx 这类可重试错误**不**停用，原样抛出，让客户端重试时还落回同一个号。
pub async fn valid_access_token_for_device(
    store: &CredentialStore,
    clients: &crate::clients::ClientPool,
    sel: Select<'_>,
) -> Result<(String, Credential, Option<i64>)> {
    select_with_refresh_failover(store, sel, |cred| {
        Box::pin(async move { fresh_token(store, clients, &cred, true).await })
    })
    .await
}

/// 取**指定**凭证的可用 access_token（必要时刷新），不选号、不写设备绑定。
///
/// 连通性测试用（见 [`crate::proxy::probe`]）。转发那条路走
/// [`valid_access_token_for_device`]：它会按负载均衡挑号，而测试是指名道姓要测这一个，
/// 挑到别的号上去测出来的结论就不是这个号的。
///
/// **失败停用的口径与转发一致**：这里发生的刷新是一次真实的上游往返，`refresh_token`
/// 已被作废这个结论不因「是测试触发的」就打折扣——不停用的话，卡片上一切如常，
/// 只有点过测试的人知道这个号其实已经死了。区别只在**不换号**：测试指名要测这一个，
/// 停用之后如实把原因抛出去即可。网络抖动/5xx 这类可重试错误照旧不停用。
pub async fn access_token_of(
    store: &CredentialStore,
    clients: &crate::clients::ClientPool,
    cred: &Credential,
) -> Result<String> {
    match ensure_fresh_token(store, clients, cred).await? {
        TokenAttempt::Ready(token) => Ok(token),
        TokenAttempt::Revoked(reason) => {
            tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                reason = %reason,
                "refresh_token revoked upstream, disabling the credential"
            );
            if let Err(e) = store.record_ban(cred.id, &refresh_ban(&reason)) {
                tracing::warn!(error = %e, "failed to auto-disable the credential");
            }
            anyhow::bail!("{reason}")
        }
    }
}

/// 刷新 token 被上游作废时的封号上下文：没有 HTTP 往返可记，来源标成 `refresh`。
pub fn refresh_ban(reason: &str) -> BanContext {
    BanContext {
        reason: reason.to_string(),
        source: "refresh",
        error_message: Some(reason.to_string()),
        ..Default::default()
    }
}

/// [`select_with_refresh_failover`] 注入的「取一次 token」返回的 future。
///
/// 写成显式 boxed future 而不是 `impl AsyncFn`：后者的 `CallRefFuture` 带高阶生命周期，
/// 会让捕获了 `&CredentialStore`/`&wreq::Client` 的闭包推不出 `Send`
/// （报 `implementation of Send is not general enough`），而这条链最终要塞进 axum handler。
/// 固定成单个 `'a` 就没有这个问题；代价是每轮一次 Box 分配，紧挨着一次上游往返，可忽略。
type AttemptFut<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<TokenAttempt>> + Send + 'a>>;

/// [`valid_access_token_for_device`] 的重选循环本体。把「取 token」这一步抽成参数注入，
/// 是为了让换号逻辑本身能脱离网络被测到——这段逻辑此前不存在（刷新失败直接抛错），
/// 设备会被钉死在坏号上，属于只在生产才暴露的那类 bug，必须有回归测试盯着。
///
/// `attempt` 收 `Credential` 而非 `&Credential`：按值传就不会让返回的 future 借用参数，
/// `AttemptFut<'a>` 里那个 `'a` 才能是固定的。
async fn select_with_refresh_failover<'a>(
    store: &CredentialStore,
    sel: Select<'_>,
    attempt: impl Fn(Credential) -> AttemptFut<'a>,
) -> Result<(String, Credential, Option<i64>)> {
    let sel = Select {
        ttl_secs: store.device_binding_ttl(),
        retention_secs: store.device_binding_retention(),
        session_ttl_secs: store.session_binding_ttl(),
        session_retention_secs: store.session_binding_retention(),
        ..sel
    };

    // 本次请求里最近一次「刷新没拿到结果」的错误：后面换不到号时报它——它才是这条请求
    // 失败的原因，也带着是哪个号（转发那边据此把流水记到这个号名下）。
    let mut refresh_failure: Option<anyhow::Error> = None;
    let mut refresh_fails = 0;
    for round in 0..MAX_REFRESH_FAILOVER {
        // 每轮都重新选：上一轮停用的那个已被排除，且它的设备绑定已清，这里才会换到新号。
        let (cred, slot) = match store.select_with_slot(sel) {
            Ok(v) => v,
            Err(e) => {
                let Some(rf) = refresh_failure else { return Err(e) };
                // 选号那句（刚暂停的号在池外，多半是「全员冷却」）只进日志。
                tracing::warn!(error = %e, "no other credential to switch to after a token refresh failure");
                return Err(rf);
            }
        };
        match attempt(cred.clone()).await {
            Ok(TokenAttempt::Ready(token)) => return Ok((token, cred, slot)),
            Ok(TokenAttempt::Revoked(reason)) => {
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    round,
                    reason = %reason,
                    "refresh_token revoked upstream, disabling the credential and selecting another"
                );
                // 停用没生效就必须中止：否则下一轮还会选中同一个号，白转满 MAX_REFRESH_FAILOVER 圈。
                if !store.record_ban(cred.id, &refresh_ban(&reason))? {
                    anyhow::bail!(
                        "credential #{} refresh failed and could not be disabled: {reason}",
                        cred.id
                    );
                }
            }
            Err(e) => {
                let Some(rf) = e.downcast_ref::<RefreshFailed>() else { return Err(e) };
                // 刷新没拿到结果：暂停一小会、换号。号本身可能是好的，所以走限时暂停而不是封号。
                // 已经暂停着的（别的请求刚停的）不再写，恢复时刻不往后推。
                if !rf.already_paused {
                    let reason = format!(
                        "{REFRESH_FAIL_PAUSE_TAG} {}; scheduling resumes automatically in about {} minutes",
                        rf.detail,
                        REFRESH_FAIL_PAUSE_SECS / 60
                    );
                    let resume_at = crate::credentials::now_secs() + REFRESH_FAIL_PAUSE_SECS;
                    // 没写进去（号刚被人工停用 / 封掉）就不再换号，照实报这次失败。
                    if !store.pause_for_rate_limit(cred.id, &reason, resume_at)? {
                        return Err(e);
                    }
                }
                refresh_fails += 1;
                if refresh_fails >= MAX_REFRESH_FAIL_SWAPS {
                    tracing::warn!(
                        cred_id = cred.id, cred = %cred.label,
                        round,
                        "token refresh failed again, giving up instead of trying more credentials"
                    );
                    return Err(e);
                }
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    round,
                    "token refresh failed, pausing the credential and selecting another"
                );
                refresh_failure = Some(e);
            }
        }
    }

    if let Some(rf) = refresh_failure {
        return Err(rf);
    }
    anyhow::bail!(
        "all {MAX_REFRESH_FAILOVER} credential refresh attempts failed; no credentials are available"
    )
}

/// 取该凭证的可用 access_token，未进入刷新窗口就直接复用，否则刷新并回写。
///
/// 刷新走该凭证的专属锁 + 双重检查：上游刷新会轮换 refresh_token，并发刷新中后完成的那次
/// 会把已作废的 token 写回库，导致该凭证之后所有刷新都 `invalid_grant`（账号被自己废掉）。
/// 拿到锁后重新读库，若他人已刷好则直接复用，不再多打一次刷新。
pub async fn ensure_fresh_token(
    store: &CredentialStore,
    clients: &crate::clients::ClientPool,
    cred: &Credential,
) -> Result<TokenAttempt> {
    fresh_token(store, clients, cred, false).await
}

/// [`ensure_fresh_token`] 的实现。`skip_refresh_paused` 只有转发选号那条传 `true`：等锁期间
/// 别的请求刚刷失败、把号停了，就直接报那次失败，不再白等一次超时。连通性测试与保活传
/// `false`——测试正是要真刷一次看号回没回来。
async fn fresh_token(
    store: &CredentialStore,
    clients: &crate::clients::ClientPool,
    cred: &Credential,
    skip_refresh_paused: bool,
) -> Result<TokenAttempt> {
    if !cred.needs_refresh() {
        return Ok(TokenAttempt::Ready(cred.access_token.clone()));
    }

    let lock = store.refresh_lock(cred.id);
    let _guard = lock.lock().await;
    // 双重检查：等锁期间可能已被其它请求刷新过。
    let cred = store.get(cred.id)?.unwrap_or_else(|| cred.clone());
    if !cred.needs_refresh() {
        tracing::debug!(
            cred_id = cred.id,
            "credential was refreshed while waiting for the lock, reusing the new token"
        );
        return Ok(TokenAttempt::Ready(cred.access_token));
    }
    if skip_refresh_paused
        && cred.disabled
        && cred.resume_at.is_some_and(|at| at > crate::credentials::now_secs())
        && let Some(detail) = cred.ban_reason.as_deref().and_then(refresh_pause_detail)
    {
        tracing::debug!(
            cred_id = cred.id,
            "credential was paused after a refresh failure while waiting for the lock"
        );
        return Err(RefreshFailed::paused(cred.id, cred.label.clone(), detail).into());
    }

    tracing::info!(cred_id = cred.id, cred = %cred.label, "credential entered the refresh window, refreshing token");
    // 刷新也必须走这个号自己的代理：只把转发挂上代理、刷新走直连的话，每次 token 过期
    // 都会有一次带真实 IP 的请求打到上游，而且那条路径的失败最不容易被注意到。
    // 取的是双重检查之后那份 `cred`——等锁期间代理可能刚被改过。
    // 代理建不出来是永久配置错误，走 Revoked 让上层 mark_banned 踢出调度池。
    let http = match clients.for_credential(&cred) {
        Ok(c) => c,
        Err(e) => return Ok(TokenAttempt::Revoked(format!("[proxy] {e:#}"))),
    };
    let err = match crate::oauth::refresh(&http, &cred.refresh_token).await {
        Ok(tokens) => {
            store.update_tokens(
                cred.id,
                &tokens.access_token,
                &tokens.refresh_token,
                tokens.expires_at,
            )?;
            // profile 字段还缺着的号（旧库、或登录时 profile 没拉到）顺手补一次。官方
            // `refreshOAuthToken` 也是这样：手里已有完整资料就跳过，否则刷新后紧接着拉
            // profile。只在缺项时拉，故每个号至多多一次往返；失败只记日志，不影响刷新结果。
            if cred.profile_incomplete() {
                match crate::oauth::fetch_profile(&http, &tokens.access_token).await {
                    Ok(profile) => {
                        if let Err(e) = store.apply_profile(
                            cred.id,
                            &profile,
                            tokens.organization_uuid.as_deref(),
                        ) {
                            tracing::warn!(cred_id = cred.id, error = %e, "failed to backfill profile fields after refresh");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(cred_id = cred.id, error = %e, "fetching the profile after refresh failed, profile fields left as they were");
                    }
                }
            }
            return Ok(TokenAttempt::Ready(tokens.access_token));
        }
        Err(e) => e,
    };

    // 无论是否判定为永久失效，都把失败原文打出来：这个端点的失败响应形态我们没有实测样本，
    // 线上真出现一次就能据此收紧 `is_grant_revoked`。
    tracing::warn!(cred_id = cred.id, cred = %cred.label, error = %format!("{err:#}"), "token refresh failed");
    match err.downcast_ref::<crate::oauth::TokenEndpointError>() {
        Some(te) if te.is_grant_revoked() => Ok(TokenAttempt::Revoked(te.ban_reason())),
        // 网络抖动 / 5xx / 限流 / 非 invalid_grant 的 4xx：凭证本身可能是好的，不停用。
        // 带上完整错误链：最外层只有一句「request to the token endpoint failed」，
        // 连不上、超时还是代理拒绝全在里层。
        _ => Err(RefreshFailed {
            cred_id: cred.id,
            cred_label: cred.label.clone(),
            detail: format!("{err:#}"),
            already_paused: false,
        }
        .into()),
    }
}

#[cfg(test)]
mod tests;
