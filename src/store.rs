//! 多凭证的 SQLite 持久化层（参照 kiro.rs 的做法）。
//!
//! 单连接 + `parking_lot::Mutex` 串行化；WAL + `synchronous=NORMAL`；STRICT 表 +
//! `CHECK`/`UNIQUE` 约束。token 轮换走单行 `UPDATE`，不重写整库。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::credentials::Credential;

/// 查询列顺序，与 [`row_to_cred`] 一一对应。
const COLS: &str = "id, label, tier, access_token, refresh_token, expires_at, priority, disabled, \
     created_at, updated_at, device_limit, ban_reason, account_uuid, resume_at, org_type, proxy, \
     rpm_limit";

/// 凭证 SQLite 存储。
pub struct CredentialStore {
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
}

impl std::fmt::Display for AllRateLimited {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "all credentials are cooling down after upstream rate limits; retry in {} seconds",
            self.retry_after_secs
        )
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
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub expires_at: u64,
    #[serde(default)]
    pub priority: i64,
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
    #[serde(default)]
    pub resume_at: Option<u64>,
    #[serde(default)]
    pub proxy: Option<String>,
}

impl From<&Credential> for PortableCredential {
    fn from(c: &Credential) -> Self {
        Self {
            label: c.label.clone(),
            tier: c.tier.clone(),
            org_type: c.org_type.clone(),
            access_token: c.access_token.clone(),
            refresh_token: c.refresh_token.clone(),
            expires_at: c.expires_at,
            priority: c.priority,
            disabled: c.disabled,
            device_limit: c.device_limit,
            rpm_limit: c.rpm_limit,
            ban_reason: c.ban_reason.clone(),
            account_uuid: c.account_uuid.clone(),
            resume_at: c.resume_at,
            proxy: c.proxy.clone(),
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
        let path = Self::db_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create directory: {}", parent.display()))?;
        }
        let conn = Connection::open(&path)
            .with_context(|| format!("failed to open credential database: {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(5))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        init_schema(&conn)?;
        Ok(Self::with_conn(conn))
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
            refresh_locks: Mutex::new(HashMap::new()),
            bare_rate: RateWindow::default(),
            rpm_rate: RateWindow::default(),
            device_rate: RateWindow::default(),
            session_rate: RateWindow::default(),
            cooldown: RateLimitCooldown::default(),
            settings: parking_lot::RwLock::new(settings),
        }
    }

    /// 取该凭证的刷新锁（不存在则创建）。
    fn refresh_lock(&self, cred_id: i64) -> std::sync::Arc<tokio::sync::Mutex<()>> {
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
        // 新凭证一律落在默认档 P0：同档内按设备数负载均衡，新账号立刻参与分摊。
        // 需要瀑布式（榨干一个再用下一个）时，手动/批量把账号调到不同优先级即可。
        conn.execute(
            "INSERT INTO credentials
                 (label, tier, access_token, refresh_token, expires_at, account_uuid, org_type)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                label,
                tier,
                access_token,
                refresh_token,
                expires_at as i64,
                account_uuid,
                org_type
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

    /// 删除一条，返回是否确有删除。连带清除其设备绑定与历史用量日志。
    ///
    /// 用量日志一并清掉：账号已不存在，其历史记录既无处归属（后台按 cred_id 关联展示
    /// 费用/额度/最近使用），留着只会在请求日志里堆积无主行、并让费用统计包含已删账号。
    /// 三张表在同一事务内删除，避免中途失败留下半清理状态。
    pub fn delete(&self, id: i64) -> Result<bool> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM usage_logs WHERE cred_id = ?1", [id])?;
        tx.execute("DELETE FROM device_bindings WHERE cred_id = ?1", [id])?;
        tx.execute("DELETE FROM credential_stats WHERE cred_id = ?1", [id])?;
        tx.execute("DELETE FROM device_costs WHERE cred_id = ?1", [id])?;
        let n = tx.execute("DELETE FROM credentials WHERE id = ?1", [id])?;
        tx.commit()?;
        // 号没了，它的限流窗口与冷却也留着没用（id 不会被复用，见 migrates_and_stops_id_reuse）。
        self.bare_rate.forget(&id);
        self.rpm_rate.forget(&id);
        self.cooldown.forget(id);
        Ok(n > 0)
    }

    /// 清空所有凭证，返回删除条数。连带清空设备绑定与全部用量日志（口径同
    /// [`Self::delete`]：账号没了，历史用量不再保留）。
    pub fn clear(&self) -> Result<usize> {
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        tx.execute("DELETE FROM usage_logs", [])?;
        tx.execute("DELETE FROM device_bindings", [])?;
        tx.execute("DELETE FROM credential_stats", [])?;
        tx.execute("DELETE FROM device_costs", [])?;
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
        out.remove(ADMIN_PASSWORD);
        out
    }

    /// 导入一条凭证：目标库已有这个账号就整行覆盖，没有就新增。
    ///
    /// **匹配顺序是 `account_uuid` 优先、`refresh_token` 兜底**，这个先后有实际后果：同一个
    /// 账号在源站重新授权过之后 refresh_token 已经是新值，只按 token 匹配会把它当成一个新
    /// 账号插进去，目标库里同一个账号出现两行（两行还会各自去刷新同一个上游账号）。反过来，
    /// 老库里可能有 `account_uuid` 还没拉到的号（profile 没取成功），故 token 这条兜底不能去。
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
        let proxy = c.proxy.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let existing: Option<i64> = uuid
            .and_then(|u| {
                tx.query_row("SELECT id FROM credentials WHERE account_uuid = ?1", [u], |r| {
                    r.get(0)
                })
                .optional()
                .transpose()
            })
            .or_else(|| {
                tx.query_row(
                    "SELECT id FROM credentials WHERE refresh_token = ?1",
                    [&c.refresh_token],
                    |r| r.get(0),
                )
                .optional()
                .transpose()
            })
            .transpose()?;
        let outcome = match existing {
            Some(id) => {
                tx.execute(
                    "UPDATE credentials SET
                         label = ?2, tier = ?3, org_type = ?4, access_token = ?5,
                         refresh_token = ?6, expires_at = ?7, priority = ?8, disabled = ?9,
                         device_limit = ?10, rpm_limit = ?11, ban_reason = ?12,
                         account_uuid = ?13, resume_at = ?14, proxy = ?15,
                         updated_at = unixepoch()
                     WHERE id = ?1",
                    params![
                        id,
                        c.label,
                        c.tier,
                        c.org_type,
                        c.access_token,
                        c.refresh_token,
                        c.expires_at as i64,
                        c.priority,
                        c.disabled as i64,
                        c.device_limit,
                        c.rpm_limit,
                        c.ban_reason,
                        uuid,
                        c.resume_at.map(|t| t as i64),
                        proxy,
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
                          account_uuid, resume_at, proxy)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                    params![
                        c.label,
                        c.tier,
                        c.org_type,
                        c.access_token,
                        c.refresh_token,
                        c.expires_at as i64,
                        c.priority,
                        c.disabled as i64,
                        c.device_limit,
                        c.rpm_limit,
                        c.ban_reason,
                        uuid,
                        c.resume_at.map(|t| t as i64),
                        proxy,
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
            if k == ADMIN_PASSWORD {
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
            Ok(conn.execute(
                "UPDATE credentials SET disabled = 1, resume_at = NULL, updated_at = unixepoch() \
                 WHERE id = ?1",
                [id],
            )? > 0)
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
    /// 与 [`Self::mark_banned`] 的唯一结构差别是多写一个 `resume_at`，而那正是
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
    pub fn pause_for_rate_limit(&self, id: i64, reason: &str, resume_at: u64) -> Result<bool> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM device_bindings WHERE cred_id = ?1", [id])?;
        Ok(conn.execute(
            "UPDATE credentials SET disabled = 1, ban_reason = ?2, resume_at = ?3, \
                    updated_at = unixepoch() \
             WHERE id = ?1",
            params![id, reason, resume_at as i64],
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
    pub fn mark_banned(&self, id: i64, reason: &str) -> Result<bool> {
        let conn = self.conn.lock();
        conn.execute("DELETE FROM device_bindings WHERE cred_id = ?1", [id])?;
        Ok(conn.execute(
            "UPDATE credentials SET disabled = 1, ban_reason = ?2, resume_at = NULL, \
                    updated_at = unixepoch() \
             WHERE id = ?1",
            params![id, reason],
        )? > 0)
    }

    /// 设置优先级。
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

    /// 批量删除：连带清掉这些账号的用量日志与设备绑定（口径同 [`Self::delete`]），
    /// 返回实际删除的条数。单事务内完成，避免删到一半留下无主的日志/绑定。
    pub fn delete_many(&self, ids: &[i64]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let mut n = 0;
        {
            let mut logs = tx.prepare("DELETE FROM usage_logs WHERE cred_id = ?1")?;
            let mut binds = tx.prepare("DELETE FROM device_bindings WHERE cred_id = ?1")?;
            let mut stats = tx.prepare("DELETE FROM credential_stats WHERE cred_id = ?1")?;
            let mut costs = tx.prepare("DELETE FROM device_costs WHERE cred_id = ?1")?;
            let mut cred = tx.prepare("DELETE FROM credentials WHERE id = ?1")?;
            for id in ids {
                logs.execute([id])?;
                binds.execute([id])?;
                stats.execute([id])?;
                costs.execute([id])?;
                n += cred.execute([id])?;
            }
        }
        tx.commit()?;
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
                // 同 `set_disabled`：人工操作两个方向都清 `resume_at`，
                // 限流那套惰性恢复不该越过管理员的决定。
                let mut stmt = tx.prepare(
                    "UPDATE credentials SET disabled = 1, resume_at = NULL, \
                     updated_at = unixepoch() WHERE id = ?1",
                )?;
                for id in ids {
                    binds.execute([id])?;
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

    /// 全局默认设备数上限：`<= 0` 表示默认不限。未设置或解析失败时按 0（不限）。
    pub fn default_device_limit(&self) -> i64 {
        self.get_setting(DEFAULT_DEVICE_LIMIT)
            .ok()
            .flatten()
            .and_then(|s| s.trim().parse::<i64>().ok())
            .unwrap_or(0)
            .max(0)
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
        let conn = self.conn.lock();
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
        let conn = self.conn.lock();
        let where_clause = if ttl > 0 { "WHERE last_seen_at >= unixepoch() - ?1" } else { "" };
        let sql = format!(
            "SELECT cred_id, COUNT(*) FROM device_bindings {where_clause} GROUP BY cred_id"
        );
        let mut stmt = conn.prepare(&sql)?;
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

    /// 更新账号等级。
    /// 写回组织类型（`claude_team` 等）。与 [`Self::set_tier`] 分开：等级会随额度档变，
    /// 组织类型只在换账号时才变，两者的来源虽同是 profile，语义不是一回事。
    pub fn set_org_type(&self, id: i64, org_type: Option<&str>) -> Result<bool> {
        Ok(self.conn.lock().execute(
            "UPDATE credentials SET org_type = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, org_type],
        )? > 0)
    }

    pub fn set_tier(&self, id: i64, tier: Option<&str>) -> Result<bool> {
        self.update_one(
            "UPDATE credentials SET tier = ?2, updated_at = unixepoch() WHERE id = ?1",
            params![id, tier],
        )
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
        if let Some(v) = on(SIMULATE_CC) {
            flags.simulate_cc = v;
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
        if let Some(v) = on(STRIP_EXTRA_FIELDS) {
            flags.strip_extra_fields = v;
        }
        if let Some(v) = on(TOOL_NAME_MIMIC) {
            flags.tool_name_mimic = v;
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
/// 开（默认）：`fingerprint = arch|os` → 同平台的所有客户端收敛成同一个伪装 device_id，
/// 每个账号最多 2–3 个设备身份（macOS/arm64、Linux/x86_64…），符合真实用户一人多设备的模式。
/// 关：`fingerprint = client_device_id|arch|os` → 每个 (账号, 客户端设备) 都是独立的设备身份，
/// 客户端越多、上游看到该账号的设备数就越多，不符合正常用户的使用模式。
///
/// 只在 [`SPOOF_DEVICE_ID`] 开着时有意义——那个关着时 device_id 原样透传，指纹不参与。
pub const NORMALIZE_DEVICE_FP: &str = "normalize_device_fp";

/// 缓存断点要不要写 `ttl:"1h"`（对齐官方）。缺省视为开启；关掉即沿用客户端自己传的时长。
pub const SYSTEM_CACHE_TTL: &str = "system_cache_ttl";

/// 是否给 `x-anthropic-billing-header` 补 `cch`（订阅模式独有字段）。
pub const SPOOF_BILLING_CCH: &str = "spoof_billing_cch";

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

/// 非 Claude Code 客户端的请求，是否按官方抓包形态模拟成 CC 请求的 settings 键名。
/// 缺省视为开启：关掉的话这类请求会因缺 `You are Claude Code, …` 被上游拒掉，等于不可用。
pub const SIMULATE_CC: &str = "simulate_cc";

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

/// 是否剥掉官方从不发送的顶层字段的 settings 键名。缺省视为开启。
/// 见 [`ForwardFlags::strip_extra_fields`]。
pub const STRIP_EXTRA_FIELDS: &str = "strip_extra_fields";

/// 是否把第三方工具名混淆成假名转发的 settings 键名。缺省视为开启。
/// 见 [`ForwardFlags::tool_name_mimic`]。
pub const TOOL_NAME_MIMIC: &str = "tool_name_mimic";

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
    /// - **开**（默认）：`fingerprint = arch|os` → 同平台的所有客户端收敛成同一个伪装
    ///   device_id，每个账号最多 2–3 个设备身份（macOS/arm64、Linux/x86_64…），符合真实
    ///   用户一人多设备的模式。
    /// - **关**：`fingerprint = client_device_id|arch|os` → 每个 (账号, 客户端设备) 都是
    ///   独立的设备身份，客户端越多、上游看到该账号的设备数就越多。
    pub normalize_device_fp: bool,
    /// 给 `x-anthropic-billing-header` 补 `cch`。
    pub billing_cch: bool,
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
    /// 非 Claude Code 客户端的请求，按官方抓包形态模拟成 CC 请求（注入 system 前缀 +
    /// 整套官方头，见 [`crate::proxy::Simulation`]）。
    pub simulate_cc: bool,
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
}

impl Default for ForwardFlags {
    fn default() -> Self {
        Self {
            spoof_identity: true,
            spoof_device_id: true,
            normalize_device_fp: true,
            billing_cch: true,
            fill_client_headers: true,
            merge_beta: true,
            system_shape: true,
            orig_header_case: true,
            thinking_signature_retry: true,
            simulate_cc: true,
            fill_metadata: true,
            rate_limit_retry: true,
            cache_scope_global: true,
            cache_ttl_1h: true,
            nonstream_as_sse: true,
            strip_extra_fields: true,
            tool_name_mimic: true,
        }
    }
}

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
/// 的范围来，改这一项不会追溯——要换范围就得把号重新登一次。刷新 token 不带 scope，故也不受影响。
///
/// 想少授权的一档现成值是 [`crate::config::SCOPES_MINIMAL`]，代价见那里的注释。
pub const OAUTH_SCOPES: &str = "oauth_scopes";

/// 全局默认设备数上限的 settings 键名；`<= 0` 表示默认不限。
/// 账号自身 `device_limit == 0`（默认值）时套用它，无需逐个账号配置。
pub const DEFAULT_DEVICE_LIMIT: &str = "default_device_limit";

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
/// `< 0` 账号明确不限（即便全局有默认值也不限）。旧库所有账号都是 0，全局默认亦为 0
/// （不限），故行为与加入本机制前一致。
pub fn effective_device_limit(cred_limit: i64, default_limit: i64) -> i64 {
    match cred_limit {
        n if n > 0 => n,
        0 => default_limit.max(0),
        _ => 0,
    }
}

/// 账号实际生效的 RPM 上限：返回 `0` 表示不限。三态语义与
/// [`effective_device_limit`] 逐条对应（账号独立 / 跟随全局 / 明确不限），故直接委托它——
/// 两处各写一份 `match`，哪天改了三态语义就只会改到其中一处。
pub fn effective_rpm_limit(cred_limit: i64, default_limit: i64) -> i64 {
    effective_device_limit(cred_limit, default_limit)
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
}

/// [`CredentialStore::query_usage_logs`] 的入参。
///
/// **页码翻页要靠 `until_id` 钉住范围，光有 OFFSET 不够**：流水是只增的，翻页期间新请求会
/// 不断插到最前面，纯 `LIMIT/OFFSET` 会把第二页整体往回错、重复吐出第一页尾部的记录。
/// 调用方先取一次 `max(id)` 当锚点（[`UsageLogStats::max_id`]），之后每页都带着它，
/// 于是整轮翻页看到的是同一个快照，页码、总条数、总花费三者始终自洽。锚点钉在 id 上而不是
/// `ts` 上：它同时是排序键（自增，同秒内仍严格有序，不会像按 `ts` 分界那样漏记录）。
#[derive(Debug, Clone, Copy, Default)]
pub struct UsageLogQuery {
    /// 只看这个凭证的流水；`None` 为全部。
    ///
    /// 已删账号的记录 `cred_id` 为 NULL（见 `prune_orphan_usage_logs`），按号筛时自然落选。
    pub cred_id: Option<i64>,
    /// 翻页锚点：只取 id **小于等于**它的记录；`None` 为不设上界（即含最新写入的那些）。
    pub until_id: Option<i64>,
    /// 跳过前多少条（页码 × 每页条数）。
    pub offset: i64,
    /// 最多返回条数。调用方负责收敛，这里不设默认上限。
    pub limit: i64,
}

/// 一批流水的整体口径：条数、花费合计、以及可作翻页锚点的最大 id。
///
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

/// 5 小时窗口秒数。
const WINDOW_5H_SECS: i64 = 5 * 3600;
/// 7 天窗口秒数。
const WINDOW_7D_SECS: i64 = 7 * 24 * 3600;
/// 用量日志流水的保留时长：30 天。必须显著大于最长的统计窗口（7 天），
/// 否则窗口内的流水会被裁掉、cost_7d 平白变小；30 天同时给请求日志页留够翻看余量。
const USAGE_LOG_RETENTION_SECS: i64 = 30 * 24 * 3600;
/// RPM（每分钟请求数）的统计窗口：最近 60 秒。
pub const RPM_WINDOW_SECS: i64 = 60;

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
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT s.cred_id, s.snapshot_ts, s.unified_status,
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
                     -- idx_usage_logs_cred_ts 只能按 cred_id 定位，然后把该账号 30 天
                     -- （保留期）的全部流水逐行走一遍、靠上面的 CASE 过滤——而窗口最长才 7 天。
                     -- 账号列表每次刷新都要跑一遍这条 SQL，且全程持着那把全局 conn 锁。
                     -- 下界引用外层的 s，故 SQLite 能把它压成 (cred_id=? AND ts>=?) 的范围扫描。
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
              GROUP BY s.cred_id",
        )?;
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
        let conn = self.conn.lock();
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
        let conn = self.conn.lock();
        let n = conn.query_row(
            "SELECT COUNT(*) FROM usage_logs WHERE ts >= unixepoch() - ?1 AND cred_id IS NOT NULL",
            [RPM_WINDOW_SECS],
            |r| r.get(0),
        )?;
        Ok(n)
    }

    /// 单个凭证当前的 RPM；口径同 [`Self::recent_rpm`]，无请求时为 0。
    pub fn recent_rpm_of(&self, cred_id: i64) -> Result<i64> {
        let conn = self.conn.lock();
        // 这条走 idx_usage_logs_cred_ts，直接定位到 (cred_id, 最近 60 秒) 那一小段。
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
        let conn = self.conn.lock();
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
        let conn = self.conn.lock();
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
                 rl_overage_in_use, ratelimit_raw, cost_usd, ua, ua_out, sse_aggregated)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29,
                     ?30)",
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
            ],
        )?;
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

    /// 裁掉超过保留期（[`USAGE_LOG_RETENTION_SECS`]）的用量日志流水，返回删除条数。
    ///
    /// 流水裁剪不影响任何终身口径——最近使用/累计费用/最新快照都在账本里
    /// （credential_stats / device_costs，写时落账）；还要读流水的只剩两处：
    /// 5h/7d 窗口统计（最多回看 7 天多）和请求日志页（只翻近期），30 天都覆盖得住。
    ///
    /// 分批删：日志表可能积了几百万行，一条大 DELETE 会把写锁按住很久，转发路径的
    /// 落库全得排队。批间放锁，让在线写入插队。
    pub fn prune_usage_logs(&self) -> Result<usize> {
        const BATCH: usize = 5_000;
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
        let conn = self.conn.lock();
        conn.query_row(
            "SELECT COUNT(*), COALESCE(SUM(cost_usd), 0), MAX(id)
               FROM usage_logs
              WHERE (?1 IS NULL OR cred_id = ?1)
                AND (?2 IS NULL OR id <= ?2)",
            params![q.cred_id, q.until_id],
            |r| Ok(UsageLogStats { total: r.get(0)?, cost_usd: r.get(1)?, max_id: r.get(2)? }),
        )
        .map_err(Into::into)
    }

    /// 按条件查用量流水，恒按 `id` 倒序。见 [`UsageLogQuery`]。
    pub fn query_usage_logs(&self, q: UsageLogQuery) -> Result<Vec<UsageLog>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, ts, cred_id, cred_label, device_id, model, path, status, has_usage,
                    input_tokens, output_tokens, cache_creation_tokens, cache_5m_tokens,
                    cache_1h_tokens, cache_read_tokens, ttft_ms, total_ms,
                    unified_status, rl_5h_status, rl_5h_reset, rl_5h_utilization,
                    rl_7d_status, rl_7d_reset, rl_7d_utilization, rl_representative, ratelimit_raw,
                    cost_usd, rl_overage_in_use, ua, ua_out, sse_aggregated
               FROM usage_logs
              WHERE (?1 IS NULL OR cred_id = ?1)
                AND (?2 IS NULL OR id <= ?2)
              ORDER BY id DESC LIMIT ?3 OFFSET ?4",
        )?;
        let rows = stmt.query_map(params![q.cred_id, q.until_id, q.limit, q.offset], |r| {
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
            })
        })?;
        let mut out = Vec::new();
        for r in rows {
            out.push(r?);
        }
        Ok(out)
    }
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
            priority      INTEGER NOT NULL DEFAULT 0,
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
        -- 账号列表的每一项统计（最近使用 MAX(ts)、累计费用、额度窗口内费用）都是
        -- 「按 cred_id 分组、按 ts 卡窗口」。带上 ts 后这些聚合只扫索引，不必回表逐行看时间；
        -- 旧的单列 idx_usage_logs_cred 是它的前缀，留着只是白占写入开销，随迁移删掉。
        CREATE INDEX IF NOT EXISTS idx_usage_logs_cred_ts ON usage_logs(cred_id, ts);
        DROP INDEX IF EXISTS idx_usage_logs_cred;
        -- 设备明细要按 device_id 汇总费用（含跨账号合计）；日志表只会越攒越多，
        -- 没这条索引时展开一次卡片就是一次全表扫描。
        CREATE INDEX IF NOT EXISTS idx_usage_logs_device ON usage_logs(device_id, cred_id);

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
        CREATE UNIQUE INDEX IF NOT EXISTS uq_proxies_url ON proxies(url);",
    )
    .context("failed to initialize credential database schema")?;

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
    ] {
        let _ = conn.execute(&format!("ALTER TABLE usage_logs ADD COLUMN {col}"), []);
    }
    // credential_stats 是 0.2.37 加的表，这两列都在其后才有：同样幂等补列。
    let _ = conn.execute("ALTER TABLE credential_stats ADD COLUMN overage_in_use INTEGER", []);
    // device_costs 的终身请求数是后加的：老库补出来是 0，之后的请求照常累加。
    // 不回填——`usage_logs` 只留 30 天，拿它回填会得到一个「看着像终身、其实只有 30 天」的数，
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
    // 删号时不清理）。此处须在上面所有 ADD COLUMN 之后执行，确保重建时列已齐全。
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

    // 清理旧库遗留的无主历史数据（此前删号只清 device_bindings，用量日志留了下来）。
    // 必须在回填账本之前跑：先扫掉无主日志，回填才不会给已删账号立账。
    purge_orphan_rows(conn)?;
    backfill_ledger(conn)?;
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

/// 清扫 cred_id 已指向不存在账号的行（用量日志 + 设备绑定）。
///
/// 旧版删号不清 `usage_logs`，被删账号的历史记录会一直留在库里：后台请求日志里显示为
/// 无主行、费用/额度统计也仍会按 cred_id 聚合到它们。开机时做一次清扫补上这段历史欠账；
/// 删号路径（[`CredentialStore::delete`]）已同步清理，故对新库是 no-op。
///
/// `cred_id IS NULL` 的日志（尚未选到凭证就失败的请求）不属于任何账号，保留。
fn purge_orphan_rows(conn: &Connection) -> Result<()> {
    let logs = conn
        .execute(
            "DELETE FROM usage_logs
              WHERE cred_id IS NOT NULL AND cred_id NOT IN (SELECT id FROM credentials)",
            [],
        )
        .context("failed to purge orphaned usage logs")?;
    let binds = conn
        .execute(
            "DELETE FROM device_bindings WHERE cred_id NOT IN (SELECT id FROM credentials)",
            [],
        )
        .context("failed to purge orphaned device bindings")?;
    // 账本同口径清扫（新表初次上线时是 no-op）。
    conn.execute(
        "DELETE FROM credential_stats WHERE cred_id NOT IN (SELECT id FROM credentials)",
        [],
    )
    .context("failed to purge orphaned credential ledger entries")?;
    conn.execute("DELETE FROM device_costs WHERE cred_id NOT IN (SELECT id FROM credentials)", [])
        .context("failed to purge orphaned device cost entries")?;
    if logs > 0 || binds > 0 {
        tracing::info!(
            usage_logs = logs,
            device_bindings = binds,
            "cleaned up history left behind by deleted credentials"
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
    conn.execute_batch(
        "BEGIN;
         CREATE TABLE credentials_new (
            id            INTEGER PRIMARY KEY AUTOINCREMENT,
            label         TEXT    NOT NULL DEFAULT '',
            tier          TEXT,
            org_type      TEXT,
            access_token  TEXT    NOT NULL,
            refresh_token TEXT    NOT NULL,
            expires_at    INTEGER NOT NULL,
            priority      INTEGER NOT NULL DEFAULT 0,
            disabled      INTEGER NOT NULL DEFAULT 0 CHECK (disabled IN (0,1)),
            created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
            updated_at    INTEGER NOT NULL DEFAULT (unixepoch()),
            device_limit  INTEGER NOT NULL DEFAULT 0,
            ban_reason    TEXT,
            account_uuid  TEXT
         ) STRICT;
         INSERT INTO credentials_new
             (id, label, tier, access_token, refresh_token, expires_at, priority,
              disabled, created_at, updated_at, device_limit, ban_reason, account_uuid)
         SELECT id, label, tier, access_token, refresh_token, expires_at, priority,
              disabled, created_at, updated_at, device_limit, ban_reason, account_uuid
         FROM credentials;
         DROP TABLE credentials;
         ALTER TABLE credentials_new RENAME TO credentials;
         CREATE UNIQUE INDEX IF NOT EXISTS uq_credentials_refresh_token
             ON credentials(refresh_token);
         CREATE INDEX IF NOT EXISTS idx_credentials_priority
             ON credentials(priority, id);
         COMMIT;",
    )
    .context("failed to migrate credentials to AUTOINCREMENT")?;
    Ok(())
}

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
    })
}

/// [`CredentialStore::select_for_device`] 的入参。
///
/// 做成结构体而不是一串位置参数：两个 `Option<&str>`（`device_id` 与 `model`）挨在一起，
/// 位置传参写反了照样编译得过，而那是一个「设备粘性按模型名走」的静默错误。
#[derive(Default, Clone, Copy)]
pub struct Select<'a> {
    /// 客户端设备标识；`None` 即裸请求（不绑定、不占名额）。
    pub device_id: Option<&'a str>,
    /// 设备绑定**占名额**的有效期（秒）；`<= 0` 表示永不过期。
    pub ttl_secs: i64,
    /// 软绑定保留期（秒）：绑定行超过 [`Self::ttl_secs`] 后不再占名额，但在这个时长内仍然
    /// 留着，设备回来时优先回原号。`<= 0` 表示永久保留（只要不被解绑/停号就一直在）。
    pub retention_secs: i64,
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
    pub fn select_for_device(&self, sel: Select<'_>) -> Result<Credential> {
        let Select { device_id, ttl_secs, retention_secs, rate_limited, exclude, model } = sel;
        // 这几项须在取锁前读（内部自己会取锁，parking_lot 不可重入）。
        let default_limit = self.default_device_limit();
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

        // 惰性清理：只删「连保留期都过了」的绑定。TTL 到点的那些不删——它们从这一刻起就不占
        // 名额了（下面的 counts 按 TTL 过滤），但行还在，设备回来时还能循着它回原号。
        if let Some(retention) = effective_retention(ttl_secs, retention_secs) {
            conn.execute(
                "DELETE FROM device_bindings WHERE last_seen_at < unixepoch() - ?1",
                [retention],
            )?;
        }

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
            let soonest: Option<i64> = conn
                .query_row(
                    "SELECT MIN(resume_at) FROM credentials WHERE disabled = 1 \
                       AND resume_at IS NOT NULL",
                    [],
                    |r| r.get(0),
                )
                .optional()?
                .flatten();
            if let Some(at) = soonest {
                let retry_after_secs = (at - crate::credentials::now_secs() as i64).max(1);
                return Err(AllRateLimited { retry_after_secs }.into());
            }
            anyhow::bail!("no available credentials; add an account first");
        }

        // 本次请求已经试过的号（上游 429 换号重试时传进来）直接出局——重试再撞同一个号毫无意义。
        let pool: Vec<Credential> = all.into_iter().filter(|c| !exclude.contains(&c.id)).collect();
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
            return Err(AllRateLimited { retry_after_secs }.into());
        }

        // 各凭证当前**占名额**的设备数：只数 TTL 内活跃的绑定，休眠的软绑定不占位
        // （口径与 [`Self::device_counts`] 一致，后台看到的数就是这里用来判上限的数）。
        let mut counts: HashMap<i64, i64> = HashMap::new();
        {
            let active = if ttl_secs > 0 { "WHERE last_seen_at >= unixepoch() - ?1" } else { "" };
            let mut cstmt = conn.prepare(&format!(
                "SELECT cred_id, COUNT(*) FROM device_bindings {active} GROUP BY cred_id"
            ))?;
            let map_row = |r: &Row| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?));
            let rows = if ttl_secs > 0 {
                cstmt.query_map([ttl_secs], map_row)?
            } else {
                cstmt.query_map([], map_row)?
            };
            for row in rows {
                let (cid, n) = row?;
                counts.insert(cid, n);
            }
        }

        // 当前占名额的设备数（已排除 TTL 外的休眠绑定）。
        let used = |c: &Credential| counts.get(&c.id).copied().unwrap_or(0);
        // 生效上限：账号未单独配置（device_limit == 0）时套用全局默认。
        let limit_of = |c: &Credential| effective_device_limit(c.device_limit, default_limit);
        // 还塞得下一台设备吗（上限 <= 0 即不限）。
        let has_room = |c: &Credential| limit_of(c) <= 0 || used(c) < limit_of(c);

        // 1/2/3) 命中既有绑定。
        if let Some(did) = device_id {
            // 第二列是「这条绑定还在 TTL 内吗」，交给 SQLite 与清理/计数用同一个 unixepoch()
            // 时钟判定，免得和进程时钟差出一个边界。
            let bound: Option<(i64, bool)> = conn
                .query_row(
                    "SELECT cred_id, (?2 <= 0 OR last_seen_at >= unixepoch() - ?2) \
                       FROM device_bindings WHERE device_id = ?1",
                    params![did, ttl_secs],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()?;
            if let Some((cid, active)) = bound {
                // 原号仍可调度（启用、不在冷却、本轮没试过）时才谈复用。
                if let Some(c) = creds.iter().find(|c| c.id == cid) {
                    // 活跃绑定本来就占着名额，直接续；休眠的软绑定要重新占一个位置，
                    // 原号满了就只能改选——否则设备上限形同虚设。
                    if active || has_room(c) {
                        // RPM 打满 → **就地拒**，不往下走改选那条路（理由见本函数文档：
                        // 改选会改绑，而改绑会让这条会话每一轮先撞一次 thinking 签名 400）。
                        if !rpm_room(c) {
                            return Err(RpmLimited {
                                retry_after_secs: self.rpm_rate.retry_after_secs(&c.id, rpm_window),
                                sticky: true,
                            }
                            .into());
                        }
                        conn.execute(
                            "UPDATE device_bindings
                                SET last_seen_at = unixepoch(), request_count = request_count + 1
                              WHERE device_id = ?1",
                            [did],
                        )?;
                        self.rpm_rate.take(c.id, rpm_limit_of(c), rpm_window);
                        return Ok(c.clone());
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
        let mut ordered: Vec<&Credential> = creds.iter().collect();
        ordered.sort_by_key(|c| (c.priority, used(c), c.id));
        let bare_window = Duration::from_secs(rate_window.max(1) as u64);
        let chosen = if device_id.is_some() {
            // 硬限制：仅在仍有名额者（生效上限 <=0 不限，或 used<上限）中选；
            // 当前优先级档全满时其成员被过滤掉，自然溢出到下一档；全部满则拒绝。
            let with_room: Vec<&Credential> =
                ordered.iter().copied().filter(|c| has_room(c)).collect();
            if with_room.is_empty() {
                return Err(DeviceLimitReached.into());
            }
            // 设备名额与 RPM 是两回事，故两道门分开判：都过不去时要能说清是哪一道拦的
            // ——设备满是「换台机器也没用」，RPM 满是「等几秒就好」。
            match with_room.iter().copied().find(|c| rpm_room(c)) {
                Some(c) => c,
                None => return Err(rpm_full(&with_room)),
            }
        } else {
            // 无 device_id：不占设备名额，但要过裸请求速率上限。
            let bare_ok: Vec<&Credential> = ordered
                .iter()
                .copied()
                .filter(|c| !rate_limited || self.bare_rate.has_room(c.id, rate_limit, bare_window))
                .collect();
            if bare_ok.is_empty() {
                return Err(BareRateLimited { retry_after_secs: rate_window }.into());
            }
            match bare_ok.iter().copied().find(|c| rpm_room(c)) {
                Some(c) => c,
                None => return Err(rpm_full(&bare_ok)),
            }
        };

        if let Some(did) = device_id {
            conn.execute(
                "INSERT INTO device_bindings (device_id, cred_id) VALUES (?1, ?2)
                 ON CONFLICT(device_id) DO UPDATE
                    SET cred_id = ?2, last_seen_at = unixepoch(), request_count = request_count + 1",
                params![did, chosen.id],
            )?;
        }
        // 两个窗口都在**选定之后**才记账（而不是边问边记）：一次选号要连过两道窗口，
        // 边问边记的话，过了第一道却卡在第二道的那个号会白扣一个名额。理由详见
        // [`RateWindow::has_room`]——选号全程持着 `conn` 锁，中间插不进第二次选号。
        self.rpm_rate.take(chosen.id, rpm_limit_of(chosen), rpm_window);
        if device_id.is_none() && rate_limited {
            self.bare_rate.take(chosen.id, rate_limit, bare_window);
        }
        Ok(chosen.clone())
    }
}

/// 刷新失败后最多改选几个凭证。
///
/// 每失败一轮就停用一个凭证（可用池严格变小），循环必然收敛；这个上限只是防御性兜底，
/// 免得停用没生效时打成死循环。也顺带给单次请求的耗时封了顶——每一轮都是一次上游往返。
const MAX_REFRESH_FAILOVER: usize = 5;

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
/// （[`CredentialStore::mark_banned`] 会连带清掉它的设备绑定），再重选一个号继续。
/// 网络抖动/5xx 这类可重试错误**不**停用，原样抛出，让客户端重试时还落回同一个号。
pub async fn valid_access_token_for_device(
    store: &CredentialStore,
    clients: &crate::clients::ClientPool,
    sel: Select<'_>,
) -> Result<(String, Credential)> {
    select_with_refresh_failover(store, sel, |cred| {
        Box::pin(async move { ensure_fresh_token(store, clients, &cred).await })
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
            if let Err(e) = store.mark_banned(cred.id, &reason) {
                tracing::warn!(error = %e, "failed to auto-disable the credential");
            }
            anyhow::bail!("{reason}")
        }
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
) -> Result<(String, Credential)> {
    let sel = Select {
        ttl_secs: store.device_binding_ttl(),
        retention_secs: store.device_binding_retention(),
        ..sel
    };

    for round in 0..MAX_REFRESH_FAILOVER {
        // 每轮都重新选：上一轮停用的那个已被排除，且它的设备绑定已清，这里才会换到新号。
        let cred = store.select_for_device(sel)?;
        match attempt(cred.clone()).await? {
            TokenAttempt::Ready(token) => return Ok((token, cred)),
            TokenAttempt::Revoked(reason) => {
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    round,
                    reason = %reason,
                    "refresh_token revoked upstream, disabling the credential and selecting another"
                );
                // 停用没生效就必须中止：否则下一轮还会选中同一个号，白转满 MAX_REFRESH_FAILOVER 圈。
                if !store.mark_banned(cred.id, &reason)? {
                    anyhow::bail!(
                        "credential #{} refresh failed and could not be disabled: {reason}",
                        cred.id
                    );
                }
            }
        }
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
            return Ok(TokenAttempt::Ready(tokens.access_token));
        }
        Err(e) => e,
    };

    // 无论是否判定为永久失效，都把失败原文打出来：这个端点的失败响应形态我们没有实测样本，
    // 线上真出现一次就能据此收紧 `is_grant_revoked`。
    tracing::warn!(cred_id = cred.id, cred = %cred.label, error = %err, "token refresh failed");
    match err.downcast_ref::<crate::oauth::TokenEndpointError>() {
        Some(te) if te.is_grant_revoked() => Ok(TokenAttempt::Revoked(te.ban_reason())),
        // 网络抖动 / 5xx / 限流 / 非 invalid_grant 的 4xx：凭证本身可能是好的，不停用。
        _ => Err(err),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 旧库（无 AUTOINCREMENT）经 init_schema 迁移后，删号腾出的 id 不再被复用。
    #[test]
    fn migrates_and_stops_id_reuse() {
        let conn = Connection::open_in_memory().unwrap();
        // 造一张旧表：非 AUTOINCREMENT，且只含早期列（模拟老库，后续列靠 ALTER 补）。
        conn.execute_batch(
            "CREATE TABLE credentials (
                id INTEGER PRIMARY KEY,
                label TEXT NOT NULL DEFAULT '',
                tier TEXT,
                access_token TEXT NOT NULL,
                refresh_token TEXT NOT NULL,
                expires_at INTEGER NOT NULL,
                priority INTEGER NOT NULL DEFAULT 0,
                disabled INTEGER NOT NULL DEFAULT 0,
                created_at INTEGER NOT NULL DEFAULT (unixepoch()),
                updated_at INTEGER NOT NULL DEFAULT (unixepoch())
            );",
        )
        .unwrap();
        for (id, tok) in [(1, "a"), (2, "b"), (3, "c")] {
            conn.execute(
                "INSERT INTO credentials (id, access_token, refresh_token, expires_at) \
                 VALUES (?1, ?2, ?3, 0)",
                params![id, tok, format!("r{tok}")],
            )
            .unwrap();
        }

        init_schema(&conn).unwrap();

        let ddl: String = conn
            .query_row("SELECT sql FROM sqlite_master WHERE name = 'credentials'", [], |r| r.get(0))
            .unwrap();
        assert!(ddl.contains("AUTOINCREMENT"), "迁移后应为 AUTOINCREMENT");

        // 既有行与其 id 全部保留（迁移把 sqlite_sequence 播种为 MAX(id)=3）。
        let cnt: i64 =
            conn.query_row("SELECT COUNT(*) FROM credentials", [], |r| r.get(0)).unwrap();
        assert_eq!(cnt, 3);

        // 迁移后删掉最大 id，新插入应得 4，而非复用被删的 3。
        conn.execute("DELETE FROM credentials WHERE id = 3", []).unwrap();
        conn.execute(
            "INSERT INTO credentials (access_token, refresh_token, expires_at) VALUES ('d','rd',0)",
            [],
        )
        .unwrap();
        assert_eq!(conn.last_insert_rowid(), 4, "AUTOINCREMENT 不应复用被删的 id=3");

        // 迁移后再次 init_schema 必须是无副作用的 no-op（RENAME 后 DDL 仍含 AUTOINCREMENT，
        // 不应二次重建而丢数据）。
        init_schema(&conn).unwrap();
        let after: i64 =
            conn.query_row("SELECT COUNT(*) FROM credentials", [], |r| r.get(0)).unwrap();
        assert_eq!(after, 3, "二次 init_schema 不应改动数据");
    }

    /// 开机清扫：被删账号遗留的用量日志/设备绑定被清掉，在册账号与无主(NULL)日志保留。
    #[test]
    fn purges_history_of_deleted_credentials() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO credentials (id, access_token, refresh_token, expires_at) \
             VALUES (1, 'a', 'ra', 0)",
            [],
        )
        .unwrap();
        // 账号 2 已被（旧版逻辑）删掉，但历史数据还在。
        for cid in ["1", "2", "NULL"] {
            conn.execute(&format!("INSERT INTO usage_logs (cred_id) VALUES ({cid})"), []).unwrap();
        }
        for (did, cid) in [("d1", 1), ("d2", 2)] {
            conn.execute(
                "INSERT INTO device_bindings (device_id, cred_id) VALUES (?1, ?2)",
                params![did, cid],
            )
            .unwrap();
        }

        purge_orphan_rows(&conn).unwrap();

        let logs: Vec<Option<i64>> = conn
            .prepare("SELECT cred_id FROM usage_logs ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(logs, vec![Some(1), None], "只应清掉已删账号(2)的日志");
        let binds: Vec<i64> = conn
            .prepare("SELECT cred_id FROM device_bindings")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(binds, vec![1]);
    }

    /// 删号连带清掉该账号的用量日志与设备绑定，其它账号的历史不受影响。
    #[test]
    fn delete_cascades_usage_logs() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap();
        let b = store.insert("b", None, "tb", "rb", 0, None, None).unwrap();
        {
            let conn = store.conn.lock();
            for cid in [a.id, b.id] {
                conn.execute("INSERT INTO usage_logs (cred_id) VALUES (?1)", [cid]).unwrap();
            }
            conn.execute(
                "INSERT INTO device_bindings (device_id, cred_id) VALUES ('d1', ?1)",
                [a.id],
            )
            .unwrap();
        }

        assert!(store.delete(a.id).unwrap());

        let conn = store.conn.lock();
        let logs: Vec<i64> = conn
            .prepare("SELECT cred_id FROM usage_logs")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(logs, vec![b.id], "被删账号的用量日志应一并清掉");
        let binds: i64 =
            conn.query_row("SELECT COUNT(*) FROM device_bindings", [], |r| r.get(0)).unwrap();
        assert_eq!(binds, 0);
    }

    /// 设备上限三态：账号独立值覆盖全局，0 跟随全局，负值明确不限。
    #[test]
    fn effective_device_limit_tri_state() {
        assert_eq!(effective_device_limit(3, 5), 3, "账号独立上限覆盖全局");
        assert_eq!(effective_device_limit(0, 5), 5, "未配置则跟随全局默认");
        assert_eq!(effective_device_limit(0, 0), 0, "全局也不限时不限");
        assert_eq!(effective_device_limit(-1, 5), 0, "账号明确不限，忽略全局默认");
    }

    /// 新增账号一律落在 P0；批量改优先级把选中的账号统一调档、其余不动。
    #[test]
    fn insert_defaults_to_p0_and_batch_priority() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap();
        let b = store.insert("b", None, "tb", "rb", 0, None, None).unwrap();
        let c = store.insert("c", None, "tc", "rc", 0, None, None).unwrap();
        assert_eq!((a.priority, b.priority, c.priority), (0, 0, 0), "新账号都应是 P0");

        assert_eq!(store.set_priorities(&[a.id, c.id], 2).unwrap(), 2);
        let by_id: HashMap<i64, i64> =
            store.list().unwrap().into_iter().map(|x| (x.id, x.priority)).collect();
        assert_eq!(by_id[&a.id], 2);
        assert_eq!(by_id[&c.id], 2);
        assert_eq!(by_id[&b.id], 0, "未选中的账号不应被改动");
        assert_eq!(store.set_priorities(&[], 9).unwrap(), 0, "空列表为 no-op");
    }

    /// 批量启停 / 设备上限 / 删除：只作用于选中的 id，且各自保持单账号接口的语义。
    #[test]
    fn batch_ops_only_touch_selected() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap();
        let b = store.insert("b", None, "tb", "rb", 0, None, None).unwrap();
        let c = store.insert("c", None, "tc", "rc", 0, None, None).unwrap();
        // 给 a、b 各造一条设备绑定与用量日志，验证连带清理。
        {
            let conn = store.conn.lock();
            for (did, cid) in [("d1", a.id), ("d2", b.id)] {
                conn.execute(
                    "INSERT INTO device_bindings (device_id, cred_id) VALUES (?1, ?2)",
                    params![did, cid],
                )
                .unwrap();
                conn.execute("INSERT INTO usage_logs (cred_id) VALUES (?1)", [cid]).unwrap();
            }
        }

        // 批量停用 a、b：c 不受影响；停用会清掉被选中账号的设备绑定。
        assert_eq!(store.set_disabled_many(&[a.id, b.id], true).unwrap(), 2);
        let by_id = |s: &CredentialStore| -> HashMap<i64, Credential> {
            s.list().unwrap().into_iter().map(|x| (x.id, x)).collect()
        };
        let m = by_id(&store);
        assert!(m[&a.id].disabled && m[&b.id].disabled);
        assert!(!m[&c.id].disabled, "未选中的账号不应被停用");
        {
            let conn = store.conn.lock();
            let n: i64 =
                conn.query_row("SELECT COUNT(*) FROM device_bindings", [], |r| r.get(0)).unwrap();
            assert_eq!(n, 0, "停用应清掉这两个账号的设备绑定");
        }

        // 批量启用要清 ban_reason（模拟先被自动封禁）。
        store.mark_banned(a.id, "banned").unwrap();
        assert!(by_id(&store)[&a.id].ban_reason.is_some());
        assert_eq!(store.set_disabled_many(&[a.id], false).unwrap(), 1);
        let m = by_id(&store);
        assert!(!m[&a.id].disabled && m[&a.id].ban_reason.is_none(), "启用应清除封禁原因");

        // 批量设备上限：负值由 web 层收敛，这里验证按传入值原样落库。
        assert_eq!(store.set_device_limits(&[a.id, c.id], 5).unwrap(), 2);
        let m = by_id(&store);
        assert_eq!((m[&a.id].device_limit, m[&c.id].device_limit), (5, 5));
        assert_eq!(m[&b.id].device_limit, 0, "未选中的账号不应被改动");

        // 批量删除：连带清用量日志，未选中的账号及其日志保留。
        assert_eq!(store.delete_many(&[a.id]).unwrap(), 1);
        let m = by_id(&store);
        assert!(!m.contains_key(&a.id) && m.contains_key(&b.id) && m.contains_key(&c.id));
        {
            let conn = store.conn.lock();
            let logs: Vec<i64> = conn
                .prepare("SELECT cred_id FROM usage_logs")
                .unwrap()
                .query_map([], |r| r.get(0))
                .unwrap()
                .collect::<rusqlite::Result<_>>()
                .unwrap();
            assert_eq!(logs, vec![b.id], "只应清掉被删账号的用量日志");
        }

        // 空列表一律 no-op，不误伤全表。
        assert_eq!(store.delete_many(&[]).unwrap(), 0);
        assert_eq!(store.set_disabled_many(&[], true).unwrap(), 0);
        assert_eq!(store.set_device_limits(&[], 9).unwrap(), 0);
        assert_eq!(store.list().unwrap().len(), 2, "空列表操作不应改动任何账号");
    }

    /// 刷新失败自动换号所依赖的那一步：停用坏号后，原本绑在它上面的设备必须能改选到别的号。
    ///
    /// `select_for_device` 会优先命中既有绑定，所以只是「不再选中被停用的号」还不够——
    /// `mark_banned` 必须把它的 device_bindings 一并清掉，否则设备被钉死在坏号上，
    /// [`valid_access_token_for_device`] 的重选循环会一直选回同一个，白转满上限。
    #[test]
    fn banned_credential_releases_its_devices() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap();
        let b = store.insert("b", None, "tb", "rb", 0, None, None).unwrap();

        // 先把设备粘到 a 上（a 是 id 更小的那个，同优先级下会被先选中）。
        let first = store
            .select_for_device(Select {
                device_id: Some("dev-1"),
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(first.id, a.id);
        // 再选一次仍命中既有绑定，确认粘性生效——这正是坏号会把设备钉死的原因。
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .unwrap()
                .id,
            a.id
        );

        // 模拟「a 的 refresh_token 被作废」后的停用。
        assert!(store.mark_banned(a.id, "[refresh 400] invalid_grant").unwrap());

        // 重选必须换到 b，而不是继续返回 a 或直接报错。
        let after = store
            .select_for_device(Select {
                device_id: Some("dev-1"),
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .unwrap();
        assert_eq!(after.id, b.id, "停用坏号后设备应改选到其它账号");

        // a 确实被停用并记了原因。
        let a2 = store.get(a.id).unwrap().unwrap();
        assert!(a2.disabled);
        assert_eq!(a2.ban_reason.as_deref(), Some("[refresh 400] invalid_grant"));

        // 池子空了要报错，而不是把停用的号又选回来。
        assert!(store.mark_banned(b.id, "[refresh 400] invalid_grant").unwrap());
        assert!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .is_err(),
            "无可用凭证时应报错"
        );
    }

    /// 模拟客户端（`sim:` 前缀）不写绑定，故此前在设备列表里完全看不到——用量与费用都在
    /// `device_costs` 里，只是没人读。现在把它们作为伪设备追加在真实设备之后。
    ///
    /// 同时钉住三件事：请求数**无条件**计（含没有 usage 的 4xx，否则限流排查时数字对不上）、
    /// 不占 `device_count` 名额、跨账号合计仍然正确。
    #[test]
    fn lists_simulated_devices_with_request_counts() {
        let (store, ids) = store_with(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        let log = |cred: i64, dev: &str, cost: Option<f64>| {
            store
                .insert_usage_log(&UsageRecord {
                    cred_id: Some(cred),
                    cred_label: "x".into(),
                    device_id: Some(dev.into()),
                    cost_usd: cost,
                    ..Default::default()
                })
                .unwrap();
        };
        let sim = "sim:ff813c9166f0d2f3";
        log(a, sim, Some(0.01));
        log(a, sim, Some(0.02));
        // 模型认不出 → 无费用可计，但请求确实发生过，请求数照记。
        log(a, sim, None);
        // 同一个伪设备也可能落到别的账号上（换号重试／负载均衡）。
        log(b, sim, Some(0.05));

        let devs = store.list_devices(a).unwrap();
        assert_eq!(devs.len(), 1, "伪设备该出现在列表里: {devs:?}");
        let d = &devs[0];
        assert!(d.simulated, "该标记成模拟客户端");
        assert_eq!(d.device_id, sim);
        assert_eq!(d.request_count, 3, "没有 usage 的那条也要计数");
        assert!((d.cost_usd - 0.03).abs() < 1e-9, "本账号费用: {}", d.cost_usd);
        assert!((d.cost_usd_all - 0.08).abs() < 1e-9, "跨账号合计: {}", d.cost_usd_all);
        assert_eq!(d.created_at, None, "没有绑定就没有绑定时刻");
        assert_eq!(d.last_seen_at, None);

        // 不占设备名额——那是 device_bindings 的口径，伪设备一行都不写。
        assert_eq!(store.device_count(a).unwrap(), 0, "伪设备不该计入设备数");

        // 真实设备与伪设备并存时，真实的排在前面且不被标记。
        store
            .select_for_device(Select { device_id: Some("real-1"), ..Default::default() })
            .unwrap();
        log(a, "real-1", Some(1.0));
        let devs = store.list_devices(a).unwrap();
        assert_eq!(devs.len(), 2, "{devs:?}");
        assert!(!devs[0].simulated && devs[0].device_id == "real-1", "真实设备排前面: {devs:?}");
        assert!(devs[1].simulated, "伪设备排后面: {devs:?}");
        assert_eq!(store.device_count(a).unwrap(), 1, "只有真实设备占名额");
    }

    /// 设备明细必须与设备数同口径：条数等于 `device_count`、只含本凭证的绑定、
    /// 超过 TTL 未活跃的不出现。否则后台会显示「设备 1/3，展开却列出 2 台」。
    #[test]
    fn list_devices_matches_device_count() {
        let (store, ids) = store_with(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);

        // dev-1 粘到 a（同优先级下 id 小者先中），再来一次命中既有绑定、请求数 +1。
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .unwrap()
                .id,
            a
        );
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .unwrap()
                .id,
            a
        );
        // dev-2 是新设备：a 已有 1 台、b 还是 0 台，负载均衡会把它分给 b。
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-2"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .unwrap()
                .id,
            b
        );

        let a_devs = store.list_devices(a).unwrap();
        assert_eq!(a_devs.len() as i64, store.device_count(a).unwrap(), "条数应等于设备数");
        assert_eq!(a_devs.len(), 1, "只应列出绑到 a 的设备");
        assert_eq!(a_devs[0].device_id, "dev-1");
        assert_eq!(a_devs[0].request_count, 1, "第二次命中既有绑定应计数");
        assert_eq!(store.list_devices(b).unwrap()[0].device_id, "dev-2");

        // 把 dev-1 的活跃时间推到 TTL 之外：明细与计数应同步把它排除。
        store.set_setting(DEVICE_BINDING_TTL, "60").unwrap();
        store
            .conn
            .lock()
            .execute(
                "UPDATE device_bindings SET last_seen_at = unixepoch() - 600 WHERE device_id = ?1",
                ["dev-1"],
            )
            .unwrap();
        assert_eq!(store.device_count(a).unwrap(), 0);
        assert!(store.list_devices(a).unwrap().is_empty(), "超时绑定不应出现在明细里");
    }

    /// 把一条绑定的最后活跃时间往前推 `secs` 秒，模拟设备闲置。
    fn age_binding(store: &CredentialStore, device_id: &str, secs: i64) {
        let n = store
            .conn
            .lock()
            .execute(
                "UPDATE device_bindings SET last_seen_at = unixepoch() - ?2 WHERE device_id = ?1",
                params![device_id, secs],
            )
            .unwrap();
        assert_eq!(n, 1, "要推的绑定得先存在");
    }

    /// 选号入参：TTL 一分钟、保留期一小时，即「名额一分钟就还、亲和性留一小时」。
    fn soft(device_id: &str) -> Select<'_> {
        Select {
            device_id: Some(device_id),
            ttl_secs: 60,
            retention_secs: 3600,
            rate_limited: true,
            ..Default::default()
        }
    }

    /// 建库并把 TTL/保留期设成与 [`soft`] 一致——`device_count`/`list_devices` 读的是设置项，
    /// 不跟着 `Select` 走，两边不一致的话断言的就不是同一套口径了。
    fn soft_store(labels: &[&str]) -> (CredentialStore, Vec<i64>) {
        let (store, ids) = store_with(labels);
        store.set_setting(DEVICE_BINDING_TTL, "60").unwrap();
        store.set_setting(DEVICE_BINDING_RETENTION, "3600").unwrap();
        (store, ids)
    }

    /// 软绑定：TTL 过了名额就还回去，但设备再来时仍优先回原号——哪怕负载均衡指向别处。
    ///
    /// 这是 thinking 签名能续上的前提：签名跟着账号走，会话隔一小时再续跑要是换了号，
    /// 之后每一轮都要先撞一次 400 再降级重发（见 `crate::proxy::retry_demoted_thinking`）。
    #[test]
    fn dormant_binding_still_steers_the_device_back_to_its_credential() {
        let (store, ids) = soft_store(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);

        assert_eq!(store.select_for_device(soft("dev-1")).unwrap().id, a);
        age_binding(&store, "dev-1", 600);
        assert_eq!(store.device_count(a).unwrap(), 0, "休眠绑定不占名额");

        // 休眠期间来了台新设备：名额是空的，照样分给 a（同优先级取 id 小者）。
        assert_eq!(store.select_for_device(soft("dev-2")).unwrap().id, a);
        // 此刻 a 有 1 台活跃设备、b 一台都没有，纯负载均衡会把 dev-1 判给 b。
        assert_eq!(store.device_counts().unwrap().get(&b).copied().unwrap_or(0), 0);
        assert_eq!(store.select_for_device(soft("dev-1")).unwrap().id, a, "软绑定应把它带回 a");
        assert_eq!(store.device_count(a).unwrap(), 2, "回来就重新占名额");
    }

    /// 软绑定是「优先」不是「特权」：原号名额已满时照常改选并改绑，否则设备上限就被绕过了。
    #[test]
    fn dormant_binding_gives_way_when_its_credential_is_full() {
        let (store, ids) = soft_store(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        assert!(store.set_device_limit(a, 1).unwrap());

        assert_eq!(store.select_for_device(soft("dev-1")).unwrap().id, a);
        age_binding(&store, "dev-1", 600);
        // 休眠腾出的那个名额被 dev-2 占走。
        assert_eq!(store.select_for_device(soft("dev-2")).unwrap().id, a);

        // dev-1 回来时 a 已满：改选到 b，并且绑定要真的改过去（而不是留在 a 上）。
        assert_eq!(store.select_for_device(soft("dev-1")).unwrap().id, b);
        assert_eq!(store.device_count(a).unwrap(), 1);
        let a_devs: Vec<String> =
            store.list_devices(a).unwrap().into_iter().map(|d| d.device_id).collect();
        assert_eq!(a_devs, vec!["dev-2".to_string()]);
        assert_eq!(store.select_for_device(soft("dev-1")).unwrap().id, b, "改绑后应稳定在 b");
    }

    /// 保留期到点才真删行；删掉之后设备就是台新设备，回不去原号。
    #[test]
    fn binding_rows_are_dropped_once_the_retention_window_passes() {
        let (store, ids) = soft_store(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);

        assert_eq!(store.select_for_device(soft("dev-1")).unwrap().id, a);
        age_binding(&store, "dev-1", 7200);
        // 任一次选号都会顺手清一遍；这次同时让 a 上多一台活跃设备。
        assert_eq!(store.select_for_device(soft("dev-2")).unwrap().id, a);
        let rows: i64 = store
            .conn
            .lock()
            .query_row("SELECT COUNT(*) FROM device_bindings WHERE device_id = 'dev-1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0, "超过保留期的绑定行应被删除");

        assert_eq!(store.select_for_device(soft("dev-1")).unwrap().id, b, "已被遗忘，按负载均衡走");
    }

    #[test]
    fn effective_retention_tri_state() {
        assert_eq!(effective_retention(60, 3600), Some(3600), "正常配置按保留期删");
        assert_eq!(effective_retention(60, 30), Some(60), "保留期短于 TTL 时按 TTL 兜底");
        assert_eq!(effective_retention(60, 0), None, "保留期为 0 = 永久保留");
        assert_eq!(effective_retention(0, 3600), None, "绑定永不过期时不删任何行");
    }

    /// 设备明细里的费用：本账号一列只算本账号花的，跨账号合计要把换号前的也算进去，
    /// 且不因解绑/重绑而归零（用量日志与绑定行是两套账）。
    #[test]
    fn list_devices_sums_cost_per_device() {
        let (store, ids) = store_with(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .unwrap()
                .id,
            a
        );
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-2"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .unwrap()
                .id,
            b
        );

        // dev-1 在 a 上花了 0.5+0.25，换号后在 b 上又花了 1.0；dev-2 只在 b 上花了 0.125。
        log_cost(&store, a, "dev-1", Some(0.5));
        log_cost(&store, a, "dev-1", Some(0.25));
        log_cost(&store, b, "dev-1", Some(1.0));
        log_cost(&store, b, "dev-2", Some(0.125));
        // 模型未知的请求 cost_usd 为空，SUM 要能跳过而不是把整行算成 NULL。
        log_cost(&store, a, "dev-1", None);

        let d = &store.list_devices(a).unwrap()[0];
        assert_eq!(d.device_id, "dev-1");
        assert!((d.cost_usd - 0.75).abs() < 1e-9, "本账号只算 a 上的花费：{}", d.cost_usd);
        assert!((d.cost_usd_all - 1.75).abs() < 1e-9, "合计要含 b 上的：{}", d.cost_usd_all);

        // 没有任何用量日志的设备给 0，而不是 NULL 取值失败。
        assert_eq!(
            store
                .list_devices(b)
                .unwrap()
                .iter()
                .find(|x| x.device_id == "dev-2")
                .unwrap()
                .cost_usd,
            0.125
        );

        // 解绑再重绑：请求数从零重数，费用是历史累计，不受影响。
        assert!(store.unbind_device(a, "dev-1").unwrap());
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .unwrap()
                .id,
            a
        );
        let d = &store.list_devices(a).unwrap()[0];
        assert_eq!(d.request_count, 0, "重绑后是新的一条绑定");
        assert!((d.cost_usd - 0.75).abs() < 1e-9, "费用不该被解绑清掉");
    }

    /// 走真实写入口落一条用量日志（只填与费用统计相关的字段）。
    fn log_cost(store: &CredentialStore, cred_id: i64, device_id: &str, cost: Option<f64>) {
        store
            .insert_usage_log(&UsageRecord {
                cred_id: Some(cred_id),
                device_id: Some(device_id.to_string()),
                path: "/v1/messages".to_string(),
                status: 200,
                has_usage: true,
                cost_usd: cost,
                ..Default::default()
            })
            .unwrap();
    }

    /// 手动解绑：立刻腾出名额（计数与明细同步减一）、只动本凭证名下的那条绑定、
    /// 重复解绑返回 false（后台据此给 404，而不是静默成功）。
    #[test]
    fn unbind_device_frees_slot_and_is_scoped_to_credential() {
        let (store, ids) = store_with(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);

        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .unwrap()
                .id,
            a
        );
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-2"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .unwrap()
                .id,
            b
        );

        // 拿 b 的 id 去解 dev-1（模拟后台列表已过期、设备其实绑在 a 上）：不能误伤 a 的绑定。
        assert!(!store.unbind_device(b, "dev-1").unwrap(), "跨凭证解绑应无效");
        assert_eq!(store.device_count(a).unwrap(), 1, "误删他号绑定会让名额凭空消失");

        assert!(store.unbind_device(a, "dev-1").unwrap());
        assert_eq!(store.device_count(a).unwrap(), 0, "解绑后名额应立刻释放");
        assert!(store.list_devices(a).unwrap().is_empty());
        assert_eq!(store.device_count(b).unwrap(), 1, "不应波及其它账号");

        // 已经没有这条绑定了：再解一次要报「没删到」。
        assert!(!store.unbind_device(a, "dev-1").unwrap());

        // 解绑不是拉黑：设备下次请求重新走选号，仍可能落回同一个账号。
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .unwrap()
                .id,
            a
        );
        assert_eq!(store.device_count(a).unwrap(), 1);
    }

    fn store_with(labels: &[&str]) -> (CredentialStore, Vec<i64>) {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let ids = labels
            .iter()
            // refresh_token 有 UNIQUE 约束，按 label 取值保证互不相同。
            .map(|l| {
                store
                    .insert(l, None, &format!("tok-{l}"), &format!("refresh-{l}"), 0, None, None)
                    .unwrap()
                    .id
            })
            .collect();
        (store, ids)
    }

    /// 导入的匹配顺序：`account_uuid` 优先，`refresh_token` 兜底。
    ///
    /// 第一条是这套东西的关键——账号在源站重新授权过之后 refresh_token 已经换了值，只按 token
    /// 认就会把同一个账号在目标库里变成两行，两行还各自去刷新同一个上游账号。
    #[test]
    fn import_matches_by_account_uuid_then_refresh_token() {
        let (store, _) = store_with(&["a"]);
        let base = PortableCredential {
            label: "acct".into(),
            tier: Some("max".into()),
            org_type: None,
            access_token: "at-1".into(),
            refresh_token: "rt-1".into(),
            expires_at: 100,
            priority: 2,
            disabled: false,
            device_limit: 3,
            rpm_limit: 7,
            ban_reason: None,
            account_uuid: Some("uuid-1".into()),
            resume_at: None,
            proxy: None,
        };
        assert_eq!(store.import_credential(&base).unwrap(), ImportOutcome::Added);
        let before = store.list().unwrap().len();

        // 同一个账号、新的 refresh_token（源站重新授权过）→ 覆盖那一行，不新增。
        let reauthed = PortableCredential {
            refresh_token: "rt-2".into(),
            label: "acct-renamed".into(),
            priority: 5,
            ..base.clone()
        };
        assert_eq!(store.import_credential(&reauthed).unwrap(), ImportOutcome::Updated);
        assert_eq!(store.list().unwrap().len(), before, "同一个账号不该变成两行");
        let got = store
            .list()
            .unwrap()
            .into_iter()
            .find(|c| c.account_uuid.as_deref() == Some("uuid-1"))
            .unwrap();
        assert_eq!(got.refresh_token, "rt-2");
        assert_eq!(got.label, "acct-renamed", "命中后是整行覆盖");
        assert_eq!(got.priority, 5);

        // 没有 uuid 的号（profile 没拉到）仍能按 refresh_token 认出来——这条兜底不能少。
        let no_uuid =
            PortableCredential { account_uuid: None, refresh_token: "rt-9".into(), ..base.clone() };
        assert_eq!(store.import_credential(&no_uuid).unwrap(), ImportOutcome::Added);
        let again = PortableCredential { label: "by-token".into(), ..no_uuid.clone() };
        assert_eq!(store.import_credential(&again).unwrap(), ImportOutcome::Updated);

        // 空 token 的记录直接报错：让调用方把它计进 failed，而不是写一行用不了的号进去。
        let empty = PortableCredential { access_token: "".into(), ..base.clone() };
        assert!(store.import_credential(&empty).is_err());
    }

    /// 导出的设置快照与导入都**绕开管理密码**：那是目标机器自己的门锁，不该被一次导入换掉。
    #[test]
    fn admin_password_never_travels_with_settings() {
        let (store, _) = store_with(&["a"]);
        store.set_setting(ADMIN_PASSWORD, "hash-of-source-box").unwrap();
        store.set_setting(CLIENT_API_KEY, "key-from-source").unwrap();

        let snapshot = store.settings_snapshot();
        assert!(!snapshot.contains_key(ADMIN_PASSWORD), "导出不带管理密码");
        assert_eq!(
            snapshot.get(CLIENT_API_KEY).map(String::as_str),
            Some("key-from-source"),
            "接入 key 要带上：不跟着走的话所有客户端都得重配"
        );

        // 手工把管理密码塞回文件里也不认。
        let (target, _) = store_with(&["b"]);
        target.set_setting(ADMIN_PASSWORD, "hash-of-target-box").unwrap();
        let mut incoming = snapshot.clone();
        incoming.insert(ADMIN_PASSWORD.into(), "hash-of-source-box".into());
        target.import_settings(&incoming).unwrap();
        assert_eq!(
            target.get_setting(ADMIN_PASSWORD).unwrap().as_deref(),
            Some("hash-of-target-box"),
            "目标机器的管理密码不该被导入改掉"
        );
        assert_eq!(target.get_setting(CLIENT_API_KEY).unwrap().as_deref(), Some("key-from-source"));
    }

    /// 导出的每一项都要能原样导回来：迁移文件就是「导出的响应原样喂给导入」，
    /// 中间掉一个字段（曾经掉过 priority）就是操作者在新机器上发现配置不对，而且很难看出来。
    #[test]
    fn export_round_trips_every_field() {
        let (src, _) = store_with(&["a"]);
        let full = PortableCredential {
            label: "full".into(),
            tier: Some("pro".into()),
            org_type: Some("claude_team".into()),
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 1_800_000_000,
            priority: 4,
            disabled: true,
            device_limit: 6,
            rpm_limit: -1,
            ban_reason: Some("banned upstream".into()),
            account_uuid: Some("uuid".into()),
            resume_at: Some(1_900_000_000),
            proxy: Some("socks5://127.0.0.1:1080".into()),
        };
        src.import_credential(&full).unwrap();
        let exported = src.export_credentials().unwrap();
        let out = exported.iter().find(|c| c.label == "full").expect("导出里该有它");

        let (dst, _) = store_with(&[]);
        dst.import_credential(out).unwrap();
        let back = &dst.export_credentials().unwrap()[0];
        assert_eq!(back.label, full.label);
        assert_eq!(back.tier, full.tier);
        assert_eq!(back.org_type, full.org_type);
        assert_eq!(back.access_token, full.access_token);
        assert_eq!(back.refresh_token, full.refresh_token);
        assert_eq!(back.expires_at, full.expires_at);
        assert_eq!(back.priority, full.priority);
        assert_eq!(back.disabled, full.disabled);
        assert_eq!(back.device_limit, full.device_limit);
        assert_eq!(back.rpm_limit, full.rpm_limit);
        assert_eq!(back.ban_reason, full.ban_reason);
        assert_eq!(back.account_uuid, full.account_uuid);
        assert_eq!(back.resume_at, full.resume_at);
        // 代理串在读出来时会归一化（socks5 → socks5h），两侧同样处理过，故比的是归一化后的值。
        assert_eq!(back.proxy, out.proxy);
    }

    /// 裸请求速率上限：单号发满后自动分流到下一个号，全部发满才 429（[`BareRateLimited`]）。
    /// 带 device_id 的请求不受此限——那条路由设备绑定 + `device_limit` 管着。
    #[test]
    fn bare_rate_limit_spills_to_next_credential_then_rejects() {
        let (store, ids) = store_with(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        store.set_setting(BARE_RATE_LIMIT, "2").unwrap();

        // 前两条落在 a（同优先级、设备数都是 0 时 id 小者先中），第 3、4 条 a 已满 → 溢到 b。
        let picked: Vec<i64> = (0..4)
            .map(|_| {
                store
                    .select_for_device(Select {
                        ttl_secs: 0,
                        rate_limited: true,
                        exclude: &[],
                        ..Default::default()
                    })
                    .unwrap()
                    .id
            })
            .collect();
        assert_eq!(picked, vec![a, a, b, b], "满了应换号而不是直接拒");

        // 两个号都满 → 拒绝，且带得出重试间隔（默认窗口 60s）。
        let err = store
            .select_for_device(Select {
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .unwrap_err();
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
                .is_ok()
        );

        // 上限设回 0（不限）即刻恢复，计数不再拦。
        store.set_setting(BARE_RATE_LIMIT, "0").unwrap();
        assert!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .is_ok()
        );
    }

    /// 生效的 RPM 上限与设备上限共用一套三态语义，改了一处另一处不能悄悄漂开。
    #[test]
    fn effective_rpm_limit_matches_the_device_limit_tri_state() {
        assert_eq!(effective_rpm_limit(30, 60), 30, "账号独立上限覆盖全局");
        assert_eq!(effective_rpm_limit(0, 60), 60, "未配置则跟随全局默认");
        assert_eq!(effective_rpm_limit(0, 0), 0, "全局也不限时不限");
        assert_eq!(effective_rpm_limit(-1, 60), 0, "账号明确不限，忽略全局默认");
    }

    /// 账号 RPM 上限：还没定下号的请求撞到上限时溢到下一个号，全部发满才 429
    /// （[`RpmLimited`]，且 `sticky` 为假）。账号自己配的上限盖过全局默认。
    ///
    /// 全程 `rate_limited: false`——RPM 与裸请求上限不同，它不看这个标志，每次选号都计。
    #[test]
    fn rpm_limit_spills_to_next_credential_then_rejects() {
        let (store, ids) = store_with(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        store.set_setting(DEFAULT_RPM_LIMIT, "2").unwrap();
        let sel = Select { ttl_secs: 0, ..Default::default() };

        // 前两条落在 a（同优先级、设备数都是 0 时 id 小者先中），第 3、4 条 a 已满 → 溢到 b。
        let picked: Vec<i64> = (0..4).map(|_| store.select_for_device(sel).unwrap().id).collect();
        assert_eq!(picked, vec![a, a, b, b], "发满了应换号而不是直接拒");

        let err = store.select_for_device(sel).unwrap_err();
        let rl = err.downcast_ref::<RpmLimited>().expect("应是 RPM 限流错误");
        assert!(!rl.sticky, "没有设备绑定，拒的是整个候选池而不是某个号");
        assert!(
            (1..=RPM_WINDOW_SECS).contains(&rl.retry_after_secs),
            "重试间隔应落在一个窗口之内，实际 {}",
            rl.retry_after_secs
        );

        // 账号独立上限盖过全局默认：a 单独放宽到 5，窗口里已有的 2 条不妨碍它继续接。
        store.set_rpm_limit(a, 5).unwrap();
        assert_eq!(store.select_for_device(sel).unwrap().id, a);

        // 「明确不限」（-1）同样盖过全局默认：a 收紧到发不出，只剩 b 可选。
        store.set_rpm_limit(a, 1).unwrap();
        store.set_rpm_limit(b, -1).unwrap();
        assert_eq!(store.select_for_device(sel).unwrap().id, b, "-1 即不限，全局默认不再生效");
    }

    /// 粘性命中的号撞到 RPM 上限时**直接拒**，不改选别的号——改绑会让这条会话之后每一轮都
    /// 先撞一次 thinking 签名 400。窗口松了之后这台设备还在原来那个号上。
    #[test]
    fn rpm_limit_rejects_the_bound_credential_instead_of_rebinding() {
        let (store, ids) = store_with(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        store.set_rpm_limit(a, 1).unwrap();
        let sel = Select { device_id: Some("dev-1"), ttl_secs: 0, ..Default::default() };

        assert_eq!(store.select_for_device(sel).unwrap().id, a, "新设备先落在 a");

        let err = store.select_for_device(sel).unwrap_err();
        let rl = err.downcast_ref::<RpmLimited>().expect("应是 RPM 限流错误");
        assert!(rl.sticky, "拒的是这台设备绑定的那个号");
        assert!(rl.retry_after_secs >= 1, "retry-after 不得为 0，否则客户端立刻再撞一次");
        assert_eq!(store.device_count(b).unwrap(), 0, "b 空着也不该被改绑过去");

        // 放宽上限即刻恢复，且仍是原来那个号——绑定自始至终没被动过。
        store.set_rpm_limit(a, 10).unwrap();
        assert_eq!(store.select_for_device(sel).unwrap().id, a);
    }

    /// 窗口滚过去之后名额自己回来（口径同裸请求那条，只是窗口固定 60 秒）。
    #[test]
    fn rpm_window_expires_and_frees_the_slot() {
        let (store, ids) = store_with(&["a"]);
        let a = ids[0];
        store.set_rpm_limit(a, 1).unwrap();
        let sel = Select { ttl_secs: 0, ..Default::default() };

        assert!(store.select_for_device(sel).is_ok());
        assert!(store.select_for_device(sel).is_err(), "同一窗口内第二条应被拦");

        // 直接把窗口内的那条时间戳推到过期，等价于等了一个窗口。
        {
            let mut hits = store.rpm_rate.hits.lock();
            for q in hits.values_mut() {
                for t in q.iter_mut() {
                    *t -= Duration::from_secs(RPM_WINDOW_SECS as u64 + 1);
                }
            }
        }
        assert!(store.select_for_device(sel).is_ok(), "过期后名额应回收");
    }

    /// 每设备 RPM：各设备各算各的，打满的那台被拒并拿到 retry-after，其余设备不受影响；
    /// 窗口滚过去后名额自己回来。上限未配置时一条都不记（也就永远不拒）。
    #[test]
    fn device_rpm_limit_is_per_device_and_expires() {
        let (store, _) = store_with(&["a"]);

        // 没配上限 → 恒放行，且窗口表里一条都不该有（记了只会无界增长）。
        for _ in 0..5 {
            assert_eq!(store.take_device_rpm_slot("dev-1"), None, "未配置上限就是不限");
        }
        assert!(store.device_rate.hits.lock().is_empty(), "不限时不该记账");

        store.set_setting(DEVICE_RPM_LIMIT, "2").unwrap();
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
    #[test]
    fn session_rpm_limit_is_per_session_and_independent_of_device() {
        let (store, _) = store_with(&["a"]);

        for _ in 0..5 {
            assert_eq!(store.take_session_rpm_slot("sess-1"), None, "未配置上限就是不限");
        }
        assert!(store.session_rate.hits.lock().is_empty(), "不限时不该记账");

        store.set_setting(SESSION_RPM_LIMIT, "2").unwrap();
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
        store.set_setting(DEVICE_RPM_LIMIT, "1").unwrap();
        assert_eq!(store.take_device_rpm_slot("dev-1"), None, "设备的桶此刻还是空的");

        {
            let mut hits = store.session_rate.hits.lock();
            for t in hits.get_mut("sess-1").expect("sess-1 该有窗口").iter_mut() {
                *t -= Duration::from_secs(RPM_WINDOW_SECS as u64 + 1);
            }
        }
        assert_eq!(store.take_session_rpm_slot("sess-1"), None, "过期后名额应回收");
    }

    /// 设备窗口表的清扫：device_id 是客户端自报的，乱编 id 能把 map 撑大；超过阈值时清掉
    /// 空窗口，但**窗口内还有记录的键一个都不能丢**——丢了等于给那台设备白送一轮名额。
    #[test]
    fn crowded_device_windows_are_swept_without_losing_live_ones() {
        let (store, _) = store_with(&["a"]);
        store.set_setting(DEVICE_RPM_LIMIT, "1").unwrap();

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

    /// 上游 429 打过冷却的号在选号时让位；绑定到它的设备**改绑**到新号（这正是 429 换号
    /// 重试要的语义）；冷却结束后不自动回迁——粘性以最后一次选择为准。
    #[test]
    fn cooldown_makes_device_rebind_to_another_credential() {
        let (store, ids) = store_with(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);

        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .unwrap()
                .id,
            a
        );
        store.mark_rate_limited(a, None, Duration::from_secs(300));
        assert!(store.rate_limited_secs(a) > 0, "应处于冷却中");

        // 绑定还在 a 上，但 a 在冷却 → 改选 b，并把绑定迁过去。
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .unwrap()
                .id,
            b
        );
        assert_eq!(store.list_devices(b).unwrap().len(), 1, "设备应已改绑到 b");
        assert!(store.list_devices(a).unwrap().is_empty(), "a 上不该再留着这台设备");
    }

    /// 模型级冷却只挡那一个模型：fable 被容量限制时，同一个号的 sonnet/opus 照常可用。
    /// 这是「窗口没跑满却 429」那种情况的正解——号是好的，赶走整个号纯属自伤。
    #[test]
    fn model_scoped_cooldown_only_blocks_that_model() {
        let (store, ids) = store_with(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        let pick = |model| {
            store.select_for_device(Select { model: Some(model), ..Default::default() }).unwrap().id
        };

        store.mark_rate_limited(a, Some("claude-fable-5"), Duration::from_secs(300));
        assert_eq!(pick("claude-fable-5"), b, "fable 应让位给 b");
        assert_eq!(pick("claude-sonnet-5"), a, "同一个号的其它模型不该被牵连");
        // 模型级冷却不算「账号被限流」，控制台不该显示成账号出了问题。
        assert_eq!(store.rate_limited_secs(a), 0, "模型级冷却不计入账号级展示");

        // 账号级冷却则对所有模型生效。
        store.mark_rate_limited(a, None, Duration::from_secs(300));
        assert_eq!(pick("claude-sonnet-5"), b, "账号级冷却应挡下所有模型");
        assert!(store.rate_limited_secs(a) > 0);

        // 连通性测试成功那种「带模型」的解除：清账号级 + 被测模型格，别的模型格不动。
        store.clear_rate_limited(a, Some("claude-sonnet-5"));
        assert_eq!(store.rate_limited_secs(a), 0, "账号级冷却应已解除");
        assert_eq!(pick("claude-sonnet-5"), a, "被测模型应立即可用");
        assert_eq!(pick("claude-fable-5"), b, "sonnet 通了证明不了 fable 通，那一格要留着");

        // 手动解除：全部格一起清。
        store.clear_rate_limited(a, None);
        assert_eq!(pick("claude-fable-5"), a, "手动解除后所有模型都该回来");
    }

    /// 瞬时限速（容量 / 请求速率）也走选号门禁，gate 时长由 ladder 退避值决定（起步 2s）。
    ///
    /// 与额度池满那档的区别只在持续时间：瞬时 gate 很短，避免同一个号被反复轰出 429；
    /// 但不至于像 30s/60s 门禁那样让整池级联封死——ladder 每个号独立从 2s 起步，交错过期。
    #[test]
    fn transient_rate_limit_gates_selection_with_short_cooldown() {
        let (store, ids) = store_with(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        let pick = |model| {
            store.select_for_device(Select { model: Some(model), ..Default::default() }).unwrap().id
        };

        // 瞬时限速走 gate：被标记的号不参与选号。
        store.mark_rate_limited(a, Some("claude-opus-5"), Duration::from_secs(2));
        assert_eq!(pick("claude-opus-5"), b, "瞬时限速的短 gate 也应挡住选号");
        assert_eq!(store.rate_limited_secs(a), 0, "不该冒充账号级限流");

        let models = store.rate_limited_models(a);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].0, "claude-opus-5");
        assert!(models[0].2, "瞬时限速现在也走 gate，gated 应为 true");

        // 额度那档叠上去：长 gate 覆盖短 gate（取较晚的截止时刻）。
        store.mark_rate_limited(a, Some("claude-opus-5"), Duration::from_secs(300));
        assert_eq!(pick("claude-opus-5"), b, "额度池满那档仍是硬门禁");
        let models = store.rate_limited_models(a);
        assert_eq!(models.len(), 1, "同一个模型只该出现一行");
        assert!(models[0].2, "此刻挂着门禁，gated 应为 true");
        assert!(models[0].1 > 290, "展示的剩余时间应反映较长的那个门禁：{models:?}");
    }

    /// 账号级限流把调度开关**落库关掉**，到点惰性自动打开；人工关的号不会被自动打开。
    #[test]
    fn rate_limit_pause_persists_and_auto_resumes_when_due() {
        let (store, ids) = store_with(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        let pick = || store.select_for_device(Select::default()).map(|c| c.id);
        let now = crate::credentials::now_secs();

        // a 被限流暂停（还有一小时才到点）→ 落库停用，选号自然落到 b。
        store.pause_for_rate_limit(a, "上游限流：约 1 小时后自动恢复调度", now + 3600).unwrap();
        let paused = store.get(a).unwrap().unwrap();
        assert!(paused.disabled && paused.resume_at == Some(now + 3600));
        assert_eq!(pick().unwrap(), b, "被限流暂停的号不该再被选中");

        // 到点：不需要任何后台任务，下一次选号顺手把它放回来。
        store.pause_for_rate_limit(a, "已到点", now - 1).unwrap();
        assert_eq!(pick().unwrap(), a, "到点应自动回到调度池并按 (priority, id) 重新胜出");
        let back = store.get(a).unwrap().unwrap();
        assert!(!back.disabled && back.resume_at.is_none() && back.ban_reason.is_none());

        // 人工停用没有 resume_at，怎么等都不会自己打开。
        store.set_disabled(a, true).unwrap();
        assert_eq!(pick().unwrap(), b, "人工停用的号不参与调度");
        assert_eq!(store.get(a).unwrap().unwrap().resume_at, None, "人工停用不该有恢复时刻");
        assert_eq!(CredentialStore::resume_due(&store.conn.lock()).unwrap(), 0, "没有该恢复的号");
        assert!(store.get(a).unwrap().unwrap().disabled, "惰性恢复不该越过管理员的决定");
    }

    /// 连通性测试通过 → 自动回调度池；但只对**被限流暂停**的号生效。
    #[test]
    fn probe_success_resumes_only_rate_limited_pauses() {
        let (store, ids) = store_with(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        let now = crate::credentials::now_secs();

        store.pause_for_rate_limit(a, "上游限流", now + 7 * 24 * 3600).unwrap();
        assert!(store.resume_if_rate_limited(a).unwrap(), "限流暂停的号该被测试结果放回来");
        let back = store.get(a).unwrap().unwrap();
        assert!(!back.disabled && back.resume_at.is_none());

        // 人工停用 / 封号：测试通过也不动它——那是管理员的决定，或需要人工介入的终态。
        store.set_disabled(b, true).unwrap();
        assert!(!store.resume_if_rate_limited(b).unwrap());
        assert!(store.get(b).unwrap().unwrap().disabled, "人工停用不该被一次连通性测试打开");
        store.mark_banned(b, "封号").unwrap();
        assert!(!store.resume_if_rate_limited(b).unwrap());
        assert!(store.get(b).unwrap().unwrap().disabled, "封号更不该被测试打开");
    }

    /// 全部号都因限流暂停时，回的是 429 + 最早恢复时刻，而不是「没有可用凭证，请先登录」——
    /// 后者会把人引去查登录，实际上号都在，只是在等额度回血。
    #[test]
    fn all_paused_reports_rate_limit_not_missing_credentials() {
        let (store, ids) = store_with(&["a", "b"]);
        let now = crate::credentials::now_secs();
        store.pause_for_rate_limit(ids[0], "限流", now + 5 * 3600).unwrap();
        store.pause_for_rate_limit(ids[1], "限流", now + 2 * 3600).unwrap();

        let err = store.select_for_device(Select::default()).expect_err("全员暂停应报错");
        let rl = err.downcast_ref::<AllRateLimited>().expect("应是限流错误而非「没有可用凭证」");
        assert!(
            (2 * 3600 - 5..=2 * 3600).contains(&rl.retry_after_secs),
            "应给出最早恢复的那个，实得 {}",
            rl.retry_after_secs
        );
    }

    /// 冷却是**硬门禁**：全部号都在冷却时直接拒（429 + retry-after），不再退回照常选。
    /// 另外「本次已试过的号」（换号重试传进来的排除集）一律出局，重试不会再撞同一个号。
    #[test]
    fn cooldown_is_a_hard_gate_and_exclusions_are_hard() {
        let (store, ids) = store_with(&["a", "b"]);
        let (a, b) = (ids[0], ids[1]);
        store.mark_rate_limited(a, None, Duration::from_secs(300));
        store.mark_rate_limited(b, None, Duration::from_secs(600));

        // 都在冷却 → 拒绝调度，并给出最早解冻那个号（a，300s）的剩余时间。
        let err = store
            .select_for_device(Select {
                ttl_secs: 0,
                rate_limited: true,
                exclude: &[],
                ..Default::default()
            })
            .expect_err("全员冷却时不该选出任何号");
        let rl = err.downcast_ref::<AllRateLimited>().expect("应是冷却硬门禁错误");
        assert!(
            (290..=300).contains(&rl.retry_after_secs),
            "retry-after 应取最早解冻的那个号，实得 {}",
            rl.retry_after_secs
        );

        // 逃生口：手动解除 a 的冷却后立刻可用。
        store.clear_rate_limited(a, None);
        assert_eq!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .unwrap()
                .id,
            a
        );
        // 排除集是硬的：a 已试过 → 只能是 b……但 b 还在冷却，硬门禁下同样拒绝。
        assert!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[a],
                    ..Default::default()
                })
                .is_err(),
            "唯一剩下的候选在冷却中 → 拒绝"
        );
        store.clear_rate_limited(b, None);
        assert_eq!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[a],
                    ..Default::default()
                })
                .unwrap()
                .id,
            b
        );
        // 两个都试过 → 明确报错，让调用方把最初那条 429 透传回去。
        assert!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[a, b],
                    ..Default::default()
                })
                .is_err()
        );
    }

    /// 不计费的路径（`count_tokens` 等，`rate_limited = false`）不占名额：它不产生 usage、
    /// 不消耗额度，拿它占名额只会把真正的请求挤掉，而客户端的 token 预估全靠它。
    #[test]
    fn non_billable_paths_do_not_consume_rate_slots() {
        let (store, _) = store_with(&["a"]);
        store.set_setting(BARE_RATE_LIMIT, "1").unwrap();

        // 不计入的路径打多少条都不占名额。
        for _ in 0..5 {
            assert!(
                store
                    .select_for_device(Select {
                        ttl_secs: 0,
                        rate_limited: false,
                        exclude: &[],
                        ..Default::default()
                    })
                    .is_ok()
            );
        }
        // 名额仍是满的一格：计费路径的第一条照常放行，第二条才被拦。
        assert!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .is_ok()
        );
        assert!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .is_err(),
            "计费路径应照常受限"
        );
        // 被拦之后，不计费的路径依然畅通。
        assert!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: false,
                    exclude: &[],
                    ..Default::default()
                })
                .is_ok()
        );
    }

    /// 窗口过期后名额自动回收；窗口取值非法（0/负数）时退回默认，不会把人永久锁死。
    #[test]
    fn bare_rate_window_expires_and_rejects_bad_config() {
        let (store, _) = store_with(&["a"]);
        store.set_setting(BARE_RATE_LIMIT, "1").unwrap();
        assert!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .is_ok()
        );
        assert!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .is_err(),
            "同一窗口内第二条应被拦"
        );

        // 直接把窗口内的那条时间戳推到过期，等价于等了一个窗口。
        {
            let mut hits = store.bare_rate.hits.lock();
            for q in hits.values_mut() {
                for t in q.iter_mut() {
                    *t -= Duration::from_secs(DEFAULT_BARE_RATE_WINDOW_SECS as u64 + 1);
                }
            }
        }
        assert!(
            store
                .select_for_device(Select {
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .is_ok(),
            "过期后名额应回收"
        );

        store.set_setting(BARE_RATE_WINDOW_SECS, "0").unwrap();
        assert_eq!(
            store.bare_rate_window_secs(),
            DEFAULT_BARE_RATE_WINDOW_SECS,
            "非法窗口退回默认"
        );
    }

    const REVOKED: &str = "[refresh 400] invalid_grant";

    /// 刷新失败要自动换号：坏号被停用、设备改绑到下一个可用号，请求正常拿到 token。
    ///
    /// 这是本次修复的核心——此前刷新失败直接抛错，而设备绑定在选号时就已写库，
    /// 导致该设备永远选回同一个坏号、永远 503。
    #[tokio::test]
    async fn refresh_failure_fails_over_to_next_credential() {
        let (store, ids) = store_with(&["a", "b", "c"]);
        let tried = std::cell::RefCell::new(Vec::new());

        // a、b 的 refresh_token 已作废，c 正常。
        let (token, cred) = select_with_refresh_failover(
            &store,
            Select { device_id: Some("dev-1"), rate_limited: true, ..Default::default() },
            |c| {
                tried.borrow_mut().push(c.id);
                Box::pin(async move {
                    Ok(if c.label == "c" {
                        TokenAttempt::Ready("good-token".into())
                    } else {
                        TokenAttempt::Revoked(REVOKED.into())
                    })
                })
            },
        )
        .await
        .unwrap();

        assert_eq!(token, "good-token");
        assert_eq!(cred.id, ids[2], "应换到第一个刷新得动的号");
        assert_eq!(*tried.borrow(), ids, "应按优先级依次试过 a、b、c");

        // a、b 被停用并记了原因；c 不受影响。
        for id in &ids[..2] {
            let c = store.get(*id).unwrap().unwrap();
            assert!(c.disabled, "作废的号应被停用");
            assert_eq!(c.ban_reason.as_deref(), Some(REVOKED));
        }
        assert!(!store.get(ids[2]).unwrap().unwrap().disabled);

        // 设备最终绑在 c 上，后续请求直接命中它，不再重走换号。
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .unwrap()
                .id,
            ids[2]
        );
    }

    /// 可重试错误（网络抖动、5xx、限流）**不得**停用凭证——误停一个健康账号的代价，
    /// 远高于让客户端重试一次。
    #[tokio::test]
    async fn transient_refresh_error_does_not_disable() {
        let (store, ids) = store_with(&["a", "b"]);
        let calls = std::cell::Cell::new(0);

        let e = select_with_refresh_failover(
            &store,
            Select { device_id: Some("dev-1"), rate_limited: true, ..Default::default() },
            |_| {
                calls.set(calls.get() + 1);
                Box::pin(async { anyhow::bail!("请求 token 端点失败: connection reset") })
            },
        )
        .await
        .unwrap_err();

        assert!(e.to_string().contains("connection reset"), "应原样抛出底层错误: {e}");
        assert_eq!(calls.get(), 1, "可重试错误应立即返回，不该继续换号");
        for id in &ids {
            assert!(!store.get(*id).unwrap().unwrap().disabled, "可重试错误不得停用凭证");
        }
        // 绑定保留，客户端重试时仍落回同一个号。
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .unwrap()
                .id,
            ids[0]
        );
    }

    /// 所有号的 refresh_token 都作废时要报错收场，不能死循环、也不能返回停用的号。
    #[tokio::test]
    async fn all_credentials_revoked_gives_up() {
        let (store, ids) = store_with(&["a", "b"]);
        let tried = std::cell::RefCell::new(Vec::new());

        let e = select_with_refresh_failover(
            &store,
            Select { device_id: Some("dev-1"), rate_limited: true, ..Default::default() },
            |c| {
                tried.borrow_mut().push(c.id);
                Box::pin(async { Ok(TokenAttempt::Revoked(REVOKED.into())) })
            },
        )
        .await
        .unwrap_err();

        // 号用完后是 select_for_device 先报「没有可用凭证」，而不是转满 MAX_REFRESH_FAILOVER 圈。
        assert!(
            e.to_string().contains("no available credentials"),
            "error message should identify the root cause: {e}"
        );
        assert_eq!(*tried.borrow(), ids, "每个号都应被试过一次，且只试一次");
        assert!(store.list().unwrap().iter().all(|c| c.disabled));
    }

    /// 走真实写入口落一条带限流头的流水（ts / 费用 / 两个 reset 由调用方指定）。
    /// 刻意不裸 INSERT：快照与费用如今是写时落账（credential_stats），绕过写入口
    /// 的行只进流水不进账本，测出来的就不是线上那条路径了。
    ///
    /// 每条顺带记 10 个 token（输入/输出/缓存写/缓存读 各 1 + 3 + 2 + 4），于是窗口 token 数
    /// 恒为「窗口内条数 × 10」，费用与请求数怎么断，token 就该怎么断。
    fn log_row(
        store: &CredentialStore,
        cred_id: i64,
        ts: i64,
        cost: f64,
        r5: Option<i64>,
        r7: Option<i64>,
    ) {
        let rec = UsageRecord {
            cred_id: Some(cred_id),
            cost_usd: Some(cost),
            has_usage: true,
            input_tokens: Some(1),
            output_tokens: Some(3),
            cache_creation_tokens: Some(2),
            cache_read_tokens: Some(4),
            rl_5h_utilization: r5.map(|_| 0.5),
            rl_5h_reset: r5,
            rl_7d_utilization: r7.map(|_| 0.25),
            rl_7d_reset: r7,
            ..Default::default()
        };
        store.insert_usage_log_at(&rec, Some(ts)).unwrap();
    }

    /// 额度快照取「最新一条带限流信息的行」，窗口费用和请求数只算 `reset - 窗口` 之后的日志，
    /// 且不串号。这条 SQL 从 1+2N 条查询合成了一条，口径必须逐项对上。
    #[test]
    fn latest_quotas_sums_only_current_window() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap().id;
        let b = store.insert("b", None, "tb", "rb", 0, None, None).unwrap().id;

        // 账号 a：reset=100_000，故 5h 窗口起点 82_000、7d 窗口起点 -504_800（含全部行）。
        let r5 = 100_000;
        log_row(&store, a, 10_000, 1.0, Some(r5), Some(r5)); // 5h 窗口外
        log_row(&store, a, 90_000, 2.0, Some(r5), Some(r5)); // 窗口内
        log_row(&store, a, 95_000, 4.0, Some(r5), Some(r5)); // 窗口内，且是最新快照行
        // 更晚但不带限流头的行：不该覆盖快照，费用仍要计入窗口。
        store
            .insert_usage_log_at(
                &UsageRecord { cred_id: Some(a), cost_usd: Some(8.0), ..Default::default() },
                Some(99_000),
            )
            .unwrap();
        log_row(&store, b, 95_000, 16.0, Some(r5), Some(r5)); // 他号，不得混入

        let q = store.latest_quotas().unwrap();
        let qa = q.get(&a).expect("a 应有快照");
        assert_eq!(qa.ts, 95_000, "快照应取最新一条带限流信息的行");
        assert_eq!(qa.cost_5h, Some(14.0), "只应含 ts >= reset-5h 的 2+4+8");
        assert_eq!(qa.cost_7d, Some(15.0), "7d 窗口覆盖全部 1+2+4+8");
        assert_eq!(qa.requests_5h, Some(3), "5h 窗口应计入 3 次请求");
        assert_eq!(qa.requests_7d, Some(4), "7d 窗口应计入 4 次请求");
        // token 与费用/请求数同窗口同断点：窗口内两条带 token 的流水各 10 个，那条没嗅探到
        // usage 的（各列 NULL）按 0 计而不是把整个和抹成 NULL。
        assert_eq!(qa.tokens_5h, Some(20), "5h 窗口内两条 ×10，无 usage 的那条按 0");
        assert_eq!(qa.tokens_7d, Some(30), "7d 窗口覆盖三条带 token 的流水");
        assert_eq!(q.get(&b).unwrap().cost_5h, Some(16.0), "费用不得跨账号串");
        assert_eq!(q.get(&b).unwrap().requests_5h, Some(1), "请求数不得跨账号串");
        assert_eq!(q.get(&b).unwrap().tokens_5h, Some(10), "token 不得跨账号串");

        // 单账号入口与批量入口必须给出同一份结果。
        assert_eq!(store.latest_quota(a).unwrap().unwrap().cost_5h, qa.cost_5h);
        assert_eq!(store.latest_quota(a).unwrap().unwrap().ts, qa.ts);
        assert!(store.latest_quota(999).unwrap().is_none(), "不存在的账号应为 None");
    }

    /// RPM 只数最近 60 秒，且不跨账号；窗口外的老流水与从未发过请求的号都不得混进来。
    ///
    /// 时间基准取的是库里的 `unixepoch()`（与写入侧同源），所以这条用例也顺带钉住
    /// 「两边同一个时钟」：若哪天读侧改用 Rust 的系统时间，边界上的行就会时有时无。
    #[test]
    fn recent_rpm_counts_only_the_last_minute() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap().id;
        let b = store.insert("b", None, "tb", "rb", 0, None, None).unwrap().id;
        let now: i64 = store.conn.lock().query_row("SELECT unixepoch()", [], |r| r.get(0)).unwrap();
        let hit = |cred_id, ts| {
            let rec = UsageRecord { cred_id: Some(cred_id), ..Default::default() };
            store.insert_usage_log_at(&rec, Some(ts)).unwrap();
        };
        hit(a, now);
        hit(a, now - 30);
        hit(a, now - 120); // 窗口外
        hit(b, now - 5);

        let rpm = store.recent_rpm().unwrap();
        assert_eq!(rpm.get(&a).copied(), Some(2), "两分钟前那条不在 60 秒窗口内");
        assert_eq!(rpm.get(&b).copied(), Some(1), "RPM 不得跨账号串");
        assert_eq!(store.recent_rpm_of(a).unwrap(), 2, "单账号入口须与批量口径一致");
        assert_eq!(store.recent_rpm_of(b).unwrap(), 1);

        // 从未发过请求的号压根不进 map（调用方按 0 处理），单账号入口直接给 0。
        let c = store.insert("c", None, "tc", "rc", 0, None, None).unwrap().id;
        assert_eq!(store.recent_rpm().unwrap().get(&c), None);
        assert_eq!(store.recent_rpm_of(c).unwrap(), 0);

        // 全局 RPM 必须恰好是各账号之和：两个数会并排显示在同一屏上，对不上比看不到更糟。
        assert_eq!(store.total_rpm().unwrap(), 3);
        assert_eq!(
            store.total_rpm().unwrap(),
            store.recent_rpm().unwrap().values().sum::<i64>(),
            "全局与逐账号必须同口径（同一张表、同一个窗口）"
        );

        // 没落到任何账号头上的流水（选号前就失败的那些）不计入——它们压根没发出去。
        store
            .insert_usage_log_at(&UsageRecord { cred_id: None, ..Default::default() }, Some(now))
            .unwrap();
        assert_eq!(store.total_rpm().unwrap(), 3, "无账号的流水不进全局 RPM");
    }

    /// 只有一个窗口带 `reset` 时，窗口统计不得被连接条件的下界误伤成 0。
    ///
    /// 这条护栏针对的是那个下界本身：它取「两个窗口起点里更早的那个」，而 SQLite 的
    /// `min(NULL, x)` 是 **NULL**——不加 COALESCE 兜底的话，缺一个 reset 就会让整个
    /// ON 条件恒假、一行流水都连不上，窗口费用与请求数齐刷刷变成 0。
    #[test]
    fn window_stats_survive_a_missing_reset() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap().id;
        let b = store.insert("b", None, "tb", "rb", 0, None, None).unwrap().id;

        // a：只有 5h 有 reset（窗口起点 82_000），7d 一直为空。
        log_row(&store, a, 10_000, 1.0, Some(100_000), None); // 5h 窗口外
        log_row(&store, a, 90_000, 2.0, Some(100_000), None); // 窗口内
        log_row(&store, a, 95_000, 4.0, Some(100_000), None); // 窗口内
        let qa = store.latest_quota(a).unwrap().unwrap();
        assert_eq!(qa.cost_5h, Some(6.0), "缺 7d reset 不该把 5h 窗口打成 0");
        assert_eq!(qa.requests_5h, Some(2));
        assert_eq!(qa.tokens_5h, Some(20));
        assert_eq!(qa.cost_7d, None, "没有 7d reset 就没有 7d 窗口可算");
        assert_eq!(qa.requests_7d, None);
        assert_eq!(qa.tokens_7d, None, "没有窗口就没有 token 可算，不能给 0");

        // b：反过来只有 7d 有 reset（窗口起点 100_000 - 604_800，含全部行）。
        log_row(&store, b, 90_000, 8.0, None, Some(100_000));
        log_row(&store, b, 95_000, 16.0, None, Some(100_000));
        let qb = store.latest_quota(b).unwrap().unwrap();
        assert_eq!(qb.cost_7d, Some(24.0), "缺 5h reset 不该把 7d 窗口打成 0");
        assert_eq!(qb.requests_7d, Some(2));
        assert_eq!(qb.tokens_7d, Some(20));
        assert_eq!(qb.cost_5h, None);
        assert_eq!(qb.tokens_5h, None);
    }

    /// 窗口 token 数按官方 `usage` 的四项相加，且**不看模型认不认得**。
    ///
    /// 两处容易算漏，各钉一条：
    /// - 缓存写只报了 5m/1h 细分、没报合计时要退回两档之和，否则这类响应的缓存写整段丢失；
    /// - 模型不在价目表里时 `cost_usd` 为 NULL（费用算不出），但 token 是上游实报的，
    ///   该照数——把它跟着费用一起吞掉，卡片上就会出现「有请求、0 token」。
    #[test]
    fn window_tokens_sum_official_usage_fields() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap().id;
        let reset = 100_000; // 5h 窗口起点 82_000

        // 只有 5m/1h 细分的一条：输入 10 + 输出 20 + 缓存写 (30+40) + 缓存读 50 = 150。
        store
            .insert_usage_log_at(
                &UsageRecord {
                    cred_id: Some(a),
                    has_usage: true,
                    input_tokens: Some(10),
                    output_tokens: Some(20),
                    cache_5m_tokens: Some(30),
                    cache_1h_tokens: Some(40),
                    cache_read_tokens: Some(50),
                    rl_5h_utilization: Some(0.5),
                    rl_5h_reset: Some(reset),
                    ..Default::default()
                },
                Some(90_000),
            )
            .unwrap();
        // 模型未知（cost_usd 为 None）但有 token 的一条：1 + 2 = 3。
        store
            .insert_usage_log_at(
                &UsageRecord {
                    cred_id: Some(a),
                    has_usage: true,
                    input_tokens: Some(1),
                    output_tokens: Some(2),
                    ..Default::default()
                },
                Some(95_000),
            )
            .unwrap();

        let q = store.latest_quota(a).unwrap().unwrap();
        assert_eq!(q.tokens_5h, Some(153), "细分缓存写与未计价的行都要计入");
        assert_eq!(q.cost_5h, Some(0.0), "两条都没有 cost_usd，费用仍是 0");
        assert_eq!(q.requests_5h, Some(2));
    }

    /// 模型级冷却必须能被后台读到。
    ///
    /// 这是一处真实的观测盲区：模型级 429（fable 撞超额池就是这一档）只写进
    /// `(cred_id, 模型)` 那些格子，而控制台读的是账号级那一格，于是选号侧明明已经跳过
    /// 这个模型、界面上却一片正常，「冷却中」那套筛选与徽章形同虚设。
    #[test]
    fn model_level_cooldown_is_visible_to_the_console() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap().id;

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

    /// 最低客户端版本：未设置、空串、纯空白都等于「不限」（`None`），其余去掉首尾空白后原样返回。
    /// 空白不归一成 `None` 的话，代理侧会拿一个空串去 `parse_version`，虽然也放行，但网页上
    /// 会显示成「已配置」——两边说法不一致比闸本身更难查。
    #[test]
    fn blank_min_client_version_means_no_limit() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);

        assert_eq!(store.min_client_version(), None, "没配就是不限");
        store.set_setting(MIN_CLIENT_VERSION, "2.1.220").unwrap();
        assert_eq!(store.min_client_version().as_deref(), Some("2.1.220"));
        store.set_setting(MIN_CLIENT_VERSION, "  2.1  ").unwrap();
        assert_eq!(store.min_client_version().as_deref(), Some("2.1"), "首尾空白不带进判定");
        store.set_setting(MIN_CLIENT_VERSION, "   ").unwrap();
        assert_eq!(store.min_client_version(), None, "只剩空白等于没配");
        store.delete_setting(MIN_CLIENT_VERSION).unwrap();
        assert_eq!(store.min_client_version(), None);
    }

    /// 登录 scope：没配 / 配了空白都退回官方默认那一串；配了就按规整后的形态原样发出去。
    /// 库里的值可能来自另一台机器的 import，故读出来还要再规整一遍（顺序不动、只去重与压空白）。
    #[test]
    fn oauth_scopes_fall_back_to_the_official_set() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);

        assert_eq!(store.oauth_scopes(), crate::config::SCOPES, "没配就是官方那一整套");
        store.set_setting(OAUTH_SCOPES, crate::config::SCOPES_MINIMAL).unwrap();
        assert_eq!(store.oauth_scopes(), crate::config::SCOPES_MINIMAL);
        store
            .set_setting(OAUTH_SCOPES, "  user:inference   user:profile  user:inference ")
            .unwrap();
        assert_eq!(
            store.oauth_scopes(),
            "user:inference user:profile",
            "压成单空格、按输入顺序去重"
        );
        store.set_setting(OAUTH_SCOPES, "   ").unwrap();
        assert_eq!(store.oauth_scopes(), crate::config::SCOPES, "只剩空白等于没配");
        store.delete_setting(OAUTH_SCOPES).unwrap();
        assert_eq!(store.oauth_scopes(), crate::config::SCOPES);
    }

    /// 设置项走内存缓存后，读写口径必须与直接查库一致（含删除与重开库）。
    #[test]
    fn settings_cache_matches_the_database() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);

        assert_eq!(store.get_setting(REQUIRE_DEVICE_ID).unwrap(), None);
        store.set_setting(REQUIRE_DEVICE_ID, "false").unwrap();
        assert_eq!(store.get_setting(REQUIRE_DEVICE_ID).unwrap().as_deref(), Some("false"));
        assert!(!store.require_device_id(), "缓存值要真的参与判定");

        // 缓存和库不能漂：直接查库应看到同一个值。
        let in_db: String = store
            .conn
            .lock()
            .query_row("SELECT value FROM settings WHERE key = ?1", [REQUIRE_DEVICE_ID], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(in_db, "false");

        store.set_setting(REQUIRE_DEVICE_ID, "true").unwrap();
        assert!(store.require_device_id(), "覆盖写要立刻生效");
        store.delete_setting(REQUIRE_DEVICE_ID).unwrap();
        assert_eq!(store.get_setting(REQUIRE_DEVICE_ID).unwrap(), None);
        assert!(store.require_device_id(), "删除后退回默认值（要求设备身份）");

        // 转发开关同样走缓存，且新键优先于旧键。
        store.set_setting(CACHE_SCOPE_GLOBAL, "false").unwrap();
        assert!(!store.forward_flags().system_shape, "旧键应在新键缺省时生效");
        store.set_setting(SYSTEM_SHAPE, "true").unwrap();
        assert!(store.forward_flags().system_shape, "新键存在就以新键为准");
    }

    /// 重开同一个库时，缓存要从库里重新装载（否则重启后设置全部凭空回到默认值）。
    #[test]
    fn settings_cache_is_reloaded_on_open() {
        let dir = std::env::temp_dir().join(format!("luban-settings-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.db");
        let _ = std::fs::remove_file(&path);

        {
            let conn = Connection::open(&path).unwrap();
            init_schema(&conn).unwrap();
            let store = CredentialStore::with_conn(conn);
            store.set_setting(BARE_RATE_LIMIT, "42").unwrap();
        }
        let conn = Connection::open(&path).unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        assert_eq!(store.bare_rate_limit(), 42, "重开库后设置应从库里装回来");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_dir(&dir);
    }

    /// 裁剪只动流水，不动账本：累计费用/最近使用/额度快照在裁剪后原样保留。
    #[test]
    fn prune_keeps_ledger() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap().id;

        // 一条早已过保留期的旧流水（带限流头，会写快照）+ 一条刚发生的新流水（无头）。
        let old_ts = 1_000;
        log_row(&store, a, old_ts, 2.0, Some(old_ts + 100), Some(old_ts + 100));
        store
            .insert_usage_log(&UsageRecord {
                cred_id: Some(a),
                cost_usd: Some(1.0),
                ..Default::default()
            })
            .unwrap();

        assert_eq!(store.prune_usage_logs().unwrap(), 1, "只裁过保留期的旧流水");
        assert_eq!(store.list_usage_logs(10).unwrap().len(), 1, "新流水应保留");
        assert_eq!(store.cost_of(a).unwrap(), 3.0, "累计费用是账本口径，不随裁剪变小");
        assert!(store.last_used_at(a).unwrap().is_some());
        let q = store.latest_quota(a).unwrap().expect("快照在账本里长存");
        assert_eq!(q.ts, old_ts, "快照仍是最后一次带限流头的那条");
        // 窗口统计只看还留着的流水：新流水 ts 在窗口起点之后，计入。
        assert_eq!(q.cost_5h, Some(1.0));
        assert_eq!(q.requests_5h, Some(1));
    }

    /// 「非流转流」标记要能落库并原样读回；旧库补出来的那一列默认 0（它们本就早于这个
    /// 功能，没有一条是聚合来的），不能是 NULL——读取侧按 `i64` 取，NULL 会直接报错。
    #[test]
    fn sse_aggregated_round_trips_and_defaults_to_false() {
        // 先用**没有**该列的旧库建表，再走 init_schema 补列，走的正是升级路径。
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE usage_logs (
                id INTEGER PRIMARY KEY,
                ts INTEGER NOT NULL DEFAULT (unixepoch()),
                cred_id INTEGER, cred_label TEXT NOT NULL DEFAULT '',
                device_id TEXT, model TEXT, path TEXT NOT NULL DEFAULT '',
                status INTEGER NOT NULL DEFAULT 0,
                has_usage INTEGER NOT NULL DEFAULT 0,
                input_tokens INTEGER, output_tokens INTEGER,
                cache_creation_tokens INTEGER, cache_read_tokens INTEGER,
                ttft_ms INTEGER, total_ms INTEGER
            ) STRICT;
             INSERT INTO usage_logs (cred_label) VALUES ('old');",
        )
        .unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let cred = store.insert("a", None, "t", "r", 0, None, None).unwrap().id;

        for aggregated in [true, false] {
            store
                .insert_usage_log(&UsageRecord {
                    cred_id: Some(cred),
                    sse_aggregated: aggregated,
                    ..Default::default()
                })
                .unwrap();
        }

        let logs =
            store.query_usage_logs(UsageLogQuery { limit: 10, ..Default::default() }).unwrap();
        // 倒序：最新写入的（false）在前，然后是 true，最后是升级前就存在的那条。
        assert_eq!(
            logs.iter().map(|l| l.sse_aggregated).collect::<Vec<_>>(),
            vec![false, true, false],
            "标记要原样读回，且旧记录退化成 false 而不是读取失败"
        );
    }

    /// 请求明细的筛选与分页：按账号只出该账号的记录，页码不重叠，且**锚点之后新写入的记录
    /// 不得挤动已在翻的页**——这正是页码翻页要带 `until_id` 的理由。
    #[test]
    fn usage_logs_filter_by_credential_and_paginate() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap().id;
        let b = store.insert("b", None, "tb", "rb", 0, None, None).unwrap().id;
        let log = |cred: i64, cost: f64| {
            store
                .insert_usage_log(&UsageRecord {
                    cred_id: Some(cred),
                    cost_usd: Some(cost),
                    ..Default::default()
                })
                .unwrap()
        };
        // a 四条、b 一条，交替写入，确保筛选不是靠「恰好连续」蒙对的。
        for (cred, cost) in [(a, 1.0), (a, 2.0), (b, 100.0), (a, 4.0), (a, 8.0)] {
            log(cred, cost);
        }

        let all =
            store.query_usage_logs(UsageLogQuery { limit: 10, ..Default::default() }).unwrap();
        assert_eq!(all.len(), 5, "不筛时是全部");

        // 统计与记录同一套条件：a 的四条、花费合计 15，最大 id 即锚点。
        let only_a = UsageLogQuery { cred_id: Some(a), ..Default::default() };
        let stats = store.usage_log_stats(only_a).unwrap();
        assert_eq!(stats.total, 4, "b 的那条不该计入");
        assert_eq!(stats.cost_usd, 15.0);
        let anchor = stats.max_id.expect("有记录就有锚点");

        let page = |n: i64| {
            store
                .query_usage_logs(UsageLogQuery {
                    cred_id: Some(a),
                    until_id: Some(anchor),
                    offset: n * 3,
                    limit: 3,
                })
                .unwrap()
        };
        let first = page(0);
        assert_eq!(first.len(), 3);
        assert!(first.iter().all(|l| l.cred_id == Some(a)), "b 的那条不该出现");
        assert!(first.windows(2).all(|w| w[0].id > w[1].id), "按 id 倒序");

        let second = page(1);
        assert_eq!(second.len(), 1, "a 共 4 条，第二页只剩 1 条");
        assert!(second[0].id < first[2].id, "第二页不得与第一页重叠");
        assert!(page(2).is_empty(), "翻到底为空");

        // 翻页途中来了新请求：锚点之下的两页一字不变，锚点之上的统计才会长。
        let ids = |logs: &[UsageLog]| logs.iter().map(|l| l.id).collect::<Vec<_>>();
        log(a, 16.0);
        assert_eq!(ids(&page(0)), ids(&first), "新记录不得把第一页往后挤");
        assert_eq!(ids(&page(1)), ids(&second));
        let pinned = store.usage_log_stats(UsageLogQuery { until_id: Some(anchor), ..only_a });
        assert_eq!(pinned.unwrap().total, 4, "钉在锚点上的统计不动");
        assert_eq!(store.usage_log_stats(only_a).unwrap().total, 5, "不带锚点才看得到新记录");
    }

    /// overage-in-use 标记随快照落账：带限流头的响应写入即更新，后续不带头的响应不得抹掉，
    /// 下一条带头的响应按新值覆盖。这是「额度满但不 429（usage credits 在放行）」唯一的外显信号。
    #[test]
    fn overage_marker_lands_in_snapshot() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap().id;

        store
            .insert_usage_log_at(
                &UsageRecord {
                    cred_id: Some(a),
                    rl_5h_utilization: Some(1.02),
                    rl_5h_reset: Some(9_000),
                    rl_overage_in_use: Some(true),
                    ..Default::default()
                },
                Some(1_000),
            )
            .unwrap();
        assert_eq!(store.latest_quota(a).unwrap().unwrap().overage_in_use, Some(true));

        // 不带限流头的响应（CDN 拦截页之类）不动快照。
        store
            .insert_usage_log_at(
                &UsageRecord { cred_id: Some(a), cost_usd: Some(1.0), ..Default::default() },
                Some(2_000),
            )
            .unwrap();
        let q = store.latest_quota(a).unwrap().unwrap();
        assert_eq!(q.overage_in_use, Some(true), "无头响应不得抹掉标记");
        assert_eq!(q.ts, 1_000);

        // 额度恢复后上游不再报 overage → 按新值覆盖。
        store
            .insert_usage_log_at(
                &UsageRecord {
                    cred_id: Some(a),
                    rl_5h_utilization: Some(0.3),
                    rl_5h_reset: Some(20_000),
                    rl_overage_in_use: Some(false),
                    ..Default::default()
                },
                Some(3_000),
            )
            .unwrap();
        assert_eq!(store.latest_quota(a).unwrap().unwrap().overage_in_use, Some(false));
    }

    fn win(name: &str, util: f64, reset: i64, status: &str) -> QuotaWindow {
        QuotaWindow {
            name: name.into(),
            status: Some(status.into()),
            utilization: Some(util),
            reset: Some(reset),
        }
    }

    /// 全窗口快照原样落库、原样读回——含 `7d_oi` 这类**没有专用列**的窗口。
    ///
    /// 这一列存在的全部意义就是它：5h/7d 两组写死的列覆盖不到超额池，而实测里真正被拒的
    /// 正是它，缺了它后台就只能看到「两个窗口都没满」却解释不了这个号为什么在烧钱。
    #[test]
    fn snapshot_keeps_windows_without_dedicated_columns() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap().id;

        // 形态取自 proxy::rate_limit_scope 记录的那次真实 fable-5 429：基础窗口都很空，
        // 满掉的只有超额池。
        let windows = vec![
            win("5h", 0.20, 9_000, "allowed"),
            win("7d", 0.70, 90_000, "allowed"),
            win("7d_oi", 1.02, 400_000, "rejected"),
        ];
        store
            .insert_usage_log_at(
                &UsageRecord {
                    cred_id: Some(a),
                    rl_5h_utilization: Some(0.20),
                    rl_5h_reset: Some(9_000),
                    rl_7d_utilization: Some(0.70),
                    rl_7d_reset: Some(90_000),
                    rl_representative: Some("seven_day_overage_included".into()),
                    rl_overage_in_use: Some(true),
                    windows: windows.clone(),
                    ..Default::default()
                },
                Some(1_000),
            )
            .unwrap();

        let q = store.latest_quota(a).unwrap().unwrap();
        assert_eq!(q.windows, windows, "全窗口快照应原样读回");
        // 专用列不受影响：窗口内费用/请求数仍靠它们反推窗口起点。
        assert_eq!(q.rl_5h_utilization, Some(0.20));
        assert_eq!(q.rl_7d_reset, Some(90_000));
        // 批量口径与单条口径是同一条 SQL，不能只有一边带窗口。
        assert_eq!(store.latest_quotas().unwrap().get(&a).unwrap().windows, windows);
    }

    /// **只**上报没有专用列的窗口时，照样要写出快照。
    ///
    /// 旧判据是 `rl_5h_utilization.is_some() || rl_7d_utilization.is_some()`，于是这种账号
    /// 永远写不进 credential_stats，卡片恒为「暂无数据」——哪怕它此刻正靠 usage credits 放行。
    #[test]
    fn snapshot_is_written_even_without_5h_or_7d() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap().id;

        store
            .insert_usage_log_at(
                &UsageRecord {
                    cred_id: Some(a),
                    rl_overage_in_use: Some(true),
                    windows: vec![win("7d_oi", 1.02, 400_000, "rejected")],
                    ..Default::default()
                },
                Some(1_000),
            )
            .unwrap();

        let q = store.latest_quota(a).unwrap().expect("只有 7d_oi 的账号也必须有快照");
        assert_eq!(q.overage_in_use, Some(true));
        assert_eq!(q.windows.len(), 1);
        assert_eq!(q.windows[0].name, "7d_oi");
        // 没有 5h/7d 就没有窗口起点可反推，窗口内费用/请求数保持空。
        assert_eq!(q.cost_5h, None);
        assert_eq!(q.requests_7d, None);
    }

    /// 一个窗口都没有的响应（CDN 拦截页那类，只剩 unified-status）不得覆盖已有快照——
    /// 否则等于拿一条信息更少的记录抹掉信息更多的。
    #[test]
    fn windowless_response_does_not_erase_snapshot() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap().id;

        let windows = vec![win("5h", 0.5, 9_000, "allowed")];
        store
            .insert_usage_log_at(
                &UsageRecord {
                    cred_id: Some(a),
                    rl_5h_utilization: Some(0.5),
                    rl_5h_reset: Some(9_000),
                    windows: windows.clone(),
                    ..Default::default()
                },
                Some(1_000),
            )
            .unwrap();
        store
            .insert_usage_log_at(
                &UsageRecord {
                    cred_id: Some(a),
                    unified_status: Some("rejected".into()),
                    ..Default::default()
                },
                Some(2_000),
            )
            .unwrap();

        let q = store.latest_quota(a).unwrap().unwrap();
        assert_eq!(q.ts, 1_000, "无窗口的响应不该顶掉快照");
        assert_eq!(q.windows, windows);
    }

    /// 老库补出来的 `windows` 是 NULL，读回时退化成空列表（前端即按「只有 5h/7d」渲染），
    /// 而不是把整张账号列表打成 500。存进脏 JSON 同理。
    #[test]
    fn legacy_and_corrupt_windows_degrade_to_empty() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap().id;

        // 模拟老库：快照行有 5h/7d，windows 列为 NULL。
        {
            let conn = store.conn.lock();
            conn.execute(
                "INSERT INTO credential_stats
                     (cred_id, snapshot_ts, rl_5h_utilization, rl_5h_reset, windows)
                 VALUES (?1, 1000, 0.4, 9000, NULL)",
                [a],
            )
            .unwrap();
        }
        let q = store.latest_quota(a).unwrap().unwrap();
        assert!(q.windows.is_empty(), "老库的 NULL 应读成空列表");
        assert_eq!(q.rl_5h_utilization, Some(0.4), "专用列照常可用");

        store.conn.lock().execute("UPDATE credential_stats SET windows = '{oops'", []).unwrap();
        assert!(store.latest_quota(a).unwrap().unwrap().windows.is_empty(), "脏 JSON 不得报错");
    }

    /// 老库升级（账本为空、流水有历史）时 init_schema 一次性回填账本；账本非空则不重复。
    #[test]
    fn backfill_ledger_on_first_upgrade() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap().id;

        // 模拟老库形态：流水是历史攒下的（裸 INSERT，从未落过账），账本是空表。
        {
            let conn = store.conn.lock();
            conn.execute(
                "INSERT INTO usage_logs (cred_id, ts, cost_usd, device_id) \
                 VALUES (?1, 1000, 2.0, 'd1')",
                [a],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO usage_logs (cred_id, ts, cost_usd, rl_5h_utilization, rl_5h_reset) \
                 VALUES (?1, 2000, 3.0, 0.5, 9000)",
                [a],
            )
            .unwrap();
            init_schema(&conn).unwrap();
        }

        assert_eq!(store.cost_of(a).unwrap(), 5.0, "累计费用应回填齐全");
        assert_eq!(store.last_used_at(a).unwrap(), Some(2_000));
        let q = store.latest_quota(a).unwrap().expect("快照应从最新带限流头的行回填");
        assert_eq!(q.ts, 2_000);
        assert_eq!(q.rl_5h_utilization, Some(0.5));
        let dev_cost: f64 = store
            .conn
            .lock()
            .query_row(
                "SELECT cost_usd FROM device_costs WHERE device_id = 'd1' AND cred_id = ?1",
                [a],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dev_cost, 2.0, "设备费用应回填");

        // 账本已非空：再跑一遍 init_schema（每次启动都会跑）不得重复累计。
        init_schema(&store.conn.lock()).unwrap();
        assert_eq!(store.cost_of(a).unwrap(), 5.0, "重复启动不应翻倍");
    }

    /// reset 为空时对应窗口的费用与请求数留空（而非 0）：分不清「没用」和「不知道窗口起点」
    /// 会让卡片把未知显示成已用 0。
    #[test]
    fn latest_quotas_leaves_cost_none_without_reset() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap().id;
        log_row(&store, a, 40_000, 3.0, Some(50_000), None); // 在 5h 窗口(32_000 起)内

        let q = store.latest_quota(a).unwrap().unwrap();
        assert_eq!(q.cost_5h, Some(3.0));
        assert_eq!(q.requests_5h, Some(1));
        assert_eq!(q.cost_7d, None, "无 7d reset 时不应给出 0");
        assert_eq!(q.requests_7d, None, "无 7d reset 时请求数也应未知");
    }

    /// 单账号的「最近使用 / 累计费用」与全量聚合同口径，无日志时分别是 None 与 0。
    #[test]
    fn single_cred_stats_match_batch() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None, None).unwrap().id;
        let b = store.insert("b", None, "tb", "rb", 0, None, None).unwrap().id;
        log_row(&store, a, 1_000, 1.5, None, None);
        log_row(&store, a, 2_000, 2.5, None, None);
        log_row(&store, b, 3_000, 7.0, None, None);
        let c = store.insert("c", None, "tc", "rc", 0, None, None).unwrap().id; // 从未被用过

        let last = store.last_used().unwrap();
        let costs = store.cost_by_cred().unwrap();
        for id in [a, b] {
            assert_eq!(store.last_used_at(id).unwrap(), last.get(&id).copied());
            assert_eq!(store.cost_of(id).unwrap(), costs[&id]);
        }
        assert_eq!(store.last_used_at(a).unwrap(), Some(2_000));
        assert_eq!(store.cost_of(a).unwrap(), 4.0);
        assert_eq!(store.last_used_at(c).unwrap(), None, "无日志时是 None 而非 0");
        assert_eq!(store.cost_of(c).unwrap(), 0.0);
    }

    /// 转发形态开关：**未设置时必须全开**——否则升级到带开关的版本会让既有部署的转发形态
    /// 悄悄变样。只有 `"0"`/`"false"`（忽略大小写与首尾空白）算关，其余取值一律视为开。
    #[test]
    fn forward_flags_default_on_and_parse_off() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);

        assert_eq!(store.forward_flags(), ForwardFlags::default(), "空库应等于默认值");
        assert!(ForwardFlags::default().spoof_identity, "默认必须是开");
        assert!(ForwardFlags::default().system_shape);

        // 每个键各用一种「关」的写法，确认逐项独立且解析口径一致。
        for (key, off) in [
            (SPOOF_IDENTITY_ENABLED, "0"),
            (SPOOF_DEVICE_ID, "0"),
            (NORMALIZE_DEVICE_FP, "0"),
            (SPOOF_BILLING_CCH, "false"),
            (FILL_CLIENT_HEADERS, " FALSE "),
            (MERGE_BETA, "False"),
            (SYSTEM_SHAPE, "0"),
            (ORIG_HEADER_CASE, "0"),
            (THINKING_SIGNATURE_RETRY, "0"),
            (SIMULATE_CC, "0"),
            (FILL_METADATA, "0"),
            (RATE_LIMIT_RETRY, "0"),
            (SYSTEM_CACHE_SCOPE, "0"),
            (SYSTEM_CACHE_TTL, "0"),
            (NONSTREAM_AS_SSE, "0"),
            (STRIP_EXTRA_FIELDS, "0"),
            (TOOL_NAME_MIMIC, "0"),
        ] {
            store.set_setting(key, off).unwrap();
        }
        let f = store.forward_flags();
        assert_eq!(
            f,
            ForwardFlags {
                spoof_identity: false,
                spoof_device_id: false,
                normalize_device_fp: false,
                billing_cch: false,
                fill_client_headers: false,
                merge_beta: false,
                system_shape: false,
                orig_header_case: false,
                thinking_signature_retry: false,
                simulate_cc: false,
                fill_metadata: false,
                rate_limit_retry: false,
                cache_scope_global: false,
                cache_ttl_1h: false,
                nonstream_as_sse: false,
                strip_extra_fields: false,
                tool_name_mimic: false,
            }
        );

        // 只开回一项，其余保持关闭：开关之间不得互相影响。
        store.set_setting(MERGE_BETA, "true").unwrap();
        let f = store.forward_flags();
        assert!(f.merge_beta);
        assert!(!f.spoof_identity && !f.billing_cch && !f.fill_client_headers);
        assert!(!f.orig_header_case);

        // 无法识别的取值算「开」，不能因为写错字把形态悄悄关掉。
        store.set_setting(SPOOF_IDENTITY_ENABLED, "yes").unwrap();
        assert!(store.forward_flags().spoof_identity);
    }

    /// 旧库上的单列 idx_usage_logs_cred 会被换成 (cred_id, ts) 复合索引：前缀相同，
    /// 两条并存只是多一份写入开销。
    #[test]
    fn migration_replaces_cred_index_with_composite() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn.execute("CREATE INDEX idx_usage_logs_cred ON usage_logs(cred_id)", []).unwrap();
        init_schema(&conn).unwrap();

        let names: Vec<String> = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type = 'index' AND tbl_name = 'usage_logs'",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert!(names.iter().any(|n| n == "idx_usage_logs_cred_ts"), "复合索引应建好: {names:?}");
        assert!(!names.iter().any(|n| n == "idx_usage_logs_cred"), "旧单列索引应删掉: {names:?}");
    }

    /// 迁移是幂等的：对已是 AUTOINCREMENT 的库再次 init_schema 不改动、不报错。
    #[test]
    fn migration_is_idempotent_on_fresh_db() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap(); // 全新库：基表已带 AUTOINCREMENT。
        init_schema(&conn).unwrap(); // 再来一次应无副作用。
        let ddl: String = conn
            .query_row("SELECT sql FROM sqlite_master WHERE name = 'credentials'", [], |r| r.get(0))
            .unwrap();
        assert!(ddl.contains("AUTOINCREMENT"));
    }

    /// 0.2.81 之前入库的 socks5 代理，迁移时一次性归一化成 socks5h（理由见
    /// [`crate::clients::PROXY_SCHEME_UPGRADES`]）。不改写的话那些号会一直本机解析 DNS——正是
    /// 那个改动要治的故障，而网页上没有自助修复的路：代理框里的值与库里一致 → 不算改动 →
    /// 保存按钮是灰的。
    ///
    /// socks4/socks4a 那两种已经不收了（见 [`crate::clients::PROXY_SCHEMES`]），存量行原样留着：
    /// 清成直连就是拿真实 IP 打上游，比这个号不可用坏得多。
    #[test]
    fn migration_normalizes_stored_socks_schemes() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        for (id, proxy) in [
            (1, Some("socks5://u:p@example.com:1080")),
            (2, Some("socks4://10.0.0.1:1080")),
            (3, Some("socks5h://example.com:1080")),
            (4, Some("http://127.0.0.1:8080")),
            (5, None),
        ] {
            conn.execute(
                "INSERT INTO credentials (id, access_token, refresh_token, expires_at, proxy) \
                 VALUES (?1, 'a', ?3, 0, ?2)",
                params![id, proxy, format!("r{id}")], // refresh_token 有 UNIQUE 约束
            )
            .unwrap();
        }

        init_schema(&conn).unwrap(); // 改写就发生在这一次。

        let got = |id: i64| -> Option<String> {
            conn.query_row("SELECT proxy FROM credentials WHERE id = ?1", params![id], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(got(1).as_deref(), Some("socks5h://u:p@example.com:1080"), "socks5 该归一化");
        assert_eq!(got(2).as_deref(), Some("socks4://10.0.0.1:1080"), "socks4 原样留着，不清空");
        assert_eq!(got(3).as_deref(), Some("socks5h://example.com:1080"), "已是目标形态不该动");
        assert_eq!(got(4).as_deref(), Some("http://127.0.0.1:8080"), "http 不该动");
        assert_eq!(got(5), None, "直连（NULL）不该动");

        // 幂等：再跑一遍不该在 socks5h 前面再叠一层。
        init_schema(&conn).unwrap();
        assert_eq!(got(1).as_deref(), Some("socks5h://u:p@example.com:1080"));
        assert_eq!(got(2).as_deref(), Some("socks4://10.0.0.1:1080"));
    }
}
