//! 进程内限流：滑动窗口计数、上游 429 冷却表，以及对应的拒绝错误。

use super::*;

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
pub(super) struct RateWindow<K = i64> {
    /// 键 → 窗口内每条请求的时刻（升序，用单调时钟，不受系统时间调整影响）。
    pub(super) hits: Mutex<HashMap<K, VecDeque<Instant>>>,
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
    pub(super) fn has_room(&self, key: K, limit: i64, window: Duration) -> bool {
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
    pub(super) fn take(&self, key: K, limit: i64, window: Duration) {
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
    pub(super) fn try_take(&self, key: K, limit: i64, window: Duration) -> bool {
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
    pub(super) fn retry_after_secs(&self, key: &K, window: Duration) -> i64 {
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
    pub(super) fn forget(&self, key: &K) {
        self.hits.lock().remove(key);
    }

    /// 键数超过 `max_keys` 时清掉所有已空的窗口。
    ///
    /// 凭证维度不需要这个（键有限且删号时会 [`Self::forget`]），设备维度需要：device_id 是
    /// 客户端自报的，一个乱编 id 的脚本能往 map 里塞进无数个键。空队列（窗口内一条都没有）
    /// 是安全的清理对象——清掉与留着的判定结果完全一样。
    pub(super) fn sweep_if_crowded(&self, window: Duration, max_keys: usize) {
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
pub(super) struct RateLimitCooldown {
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
    pub(super) fn mark(&self, cred_id: i64, model: Option<&str>, dur: Duration) {
        let deadline = Instant::now() + dur;
        let mut until = self.until.lock();
        let slot = until.entry((cred_id, model.unwrap_or_default().to_string())).or_default();
        if slot.gate.is_none_or(|t| t < deadline) {
            slot.gate = Some(deadline);
        }
    }

    /// 该凭证此刻对该模型是否仍在冷却中：账号级冷却对所有模型生效，模型级只挡自己那一个。
    /// 顺手清掉已到期的项。
    pub(super) fn is_cooling(&self, cred_id: i64, model: Option<&str>) -> bool {
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
    pub(super) fn clear(&self, cred_id: i64, model: Option<&str>) {
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
    pub(super) fn remaining_for(&self, cred_id: i64, model: Option<&str>) -> i64 {
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
    pub(super) fn remaining_secs(&self, cred_id: i64) -> i64 {
        let now = Instant::now();
        self.until.lock().get(&(cred_id, String::new())).map(|c| c.gate_remaining(now)).unwrap_or(0)
    }

    /// 该凭证**模型级**冷却的明细：`(模型名, 剩余秒数)`，按剩余时间倒序。未冷却时为空。
    ///
    /// 补的是一个真实的观测盲区：模型级 429（实测里 fable 撞超额池就是这一档）只写进
    /// `(cred_id, 模型)` 那些格子，而后台读的是账号级那一格，于是选号侧明明已经跳过这个
    /// 模型、界面上却什么都看不到——「冷却中」那套筛选与徽章形同虚设。
    pub(super) fn model_remaining(&self, cred_id: i64) -> Vec<(String, i64, bool)> {
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

    pub(super) fn forget(&self, cred_id: i64) {
        self.until.lock().retain(|(id, _), _| *id != cred_id);
    }
}

impl CredentialStore {
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
}

/// 设备限流窗口表里最多留多少个键，超过就清掉空窗口，见 [`RateWindow::sweep_if_crowded`]。
/// 取 4096：比任何真实部署的设备数高一两个数量级，正常规模下这条清扫永远不会触发。
pub(super) const DEVICE_RATE_MAX_KEYS: usize = 4096;

/// 会话限流窗口表里最多留多少个键，见 [`CredentialStore::take_session_rpm_slot`]。
/// 比设备那个高一档（16384）：会话 id 正常使用下就在不断产生新值，撞上清扫的机会本就更大，
/// 而清扫要遍历全表，不该在还装得下的时候触发。
pub(super) const SESSION_RATE_MAX_KEYS: usize = 16384;

/// RPM（每分钟请求数）的统计窗口：最近 60 秒。
pub const RPM_WINDOW_SECS: i64 = 60;
