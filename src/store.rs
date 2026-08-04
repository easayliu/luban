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
     created_at, updated_at, device_limit, ban_reason, account_uuid, resume_at";

/// 凭证 SQLite 存储。
pub struct CredentialStore {
    conn: Mutex<Connection>,
    /// 每凭证一把刷新锁，串行化 token 刷新，见 [`valid_access_token_for_device`]。
    /// 上游刷新会**轮换 refresh_token**：并发刷新时后完成的那次会把已被作废的 token 写回库，
    /// 该凭证之后所有刷新都 `invalid_grant`，等于账号被自己废掉。
    refresh_locks: Mutex<HashMap<i64, std::sync::Arc<tokio::sync::Mutex<()>>>>,
    /// 裸请求的每凭证限流窗口（进程内），见 [`BareRateWindow`]。
    bare_rate: BareRateWindow,
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
    /// luban 是单进程本地代理，没有这个场景（同 [`BareRateWindow`] 的取舍）。
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

/// 每凭证的裸请求滑动窗口计数器（**进程内，不落库**）。
///
/// 只统计**无 `metadata.user_id` 的请求**：带设备身份的那些已由设备绑定 + `device_limit`
/// 约束着，而裸请求既不写绑定也不占名额，`device_limit` 对它们完全不生效——这个计数器补的
/// 正是那个口子（见 [`CredentialStore::select_for_device`]）。
///
/// **不落库是有意的**：短窗口限流本来就不该跨重启（重启后放行几条远好于把人锁在门外），
/// 而每请求一次 `usage_logs` 聚合查询的代价，比一把内存锁高一个数量级。代价是多实例部署时
/// 各限各的——luban 是单进程本地代理，没有这个场景；真有了再换成落库的实现。
///
/// 内存占用有上限：每个凭证最多存 `limit` 个时间戳（超限时不再追加），过期的在每次检查时
/// 顺手清掉。
#[derive(Default)]
struct BareRateWindow {
    /// cred_id → 窗口内每条裸请求的时刻（升序，用单调时钟，不受系统时间调整影响）。
    hits: Mutex<HashMap<i64, VecDeque<Instant>>>,
}

impl BareRateWindow {
    /// 该凭证在窗口内是否还有名额；有就**当场记一条**并返回 `true`。
    ///
    /// 检查与记账合在一起（而不是先问后记），是因为选号那步一旦选中就必然要发出去，
    /// 中间没有可回退的位置；拆成两步只会多出一个「问过了但没发」的窗口。
    fn try_take(&self, cred_id: i64, limit: i64, window: Duration) -> bool {
        if limit <= 0 {
            return true; // 未配置上限 = 不限
        }
        let now = Instant::now();
        let mut hits = self.hits.lock();
        let q = hits.entry(cred_id).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) >= window) {
            q.pop_front();
        }
        if q.len() as i64 >= limit {
            return false;
        }
        q.push_back(now);
        true
    }

    /// 凭证被删除/停用后清掉它的窗口，免得 map 里留下永远不再访问的键。
    fn forget(&self, cred_id: i64) {
        self.hits.lock().remove(&cred_id);
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
    /// `(cred_id, 模型)` → 冷却结束时刻（单调时钟）。模型为空串表示**整个账号**。
    until: Mutex<HashMap<(i64, String), Instant>>,
}

impl RateLimitCooldown {
    /// 打上冷却。`model` 为 `None` 即账号级（所有模型）。
    /// 同一格重复命中时取**较晚**的那个结束时刻，不让新的短冷却缩短旧的长冷却。
    fn mark(&self, cred_id: i64, model: Option<&str>, dur: Duration) {
        let deadline = Instant::now() + dur;
        let mut until = self.until.lock();
        let slot =
            until.entry((cred_id, model.unwrap_or_default().to_string())).or_insert(deadline);
        if *slot < deadline {
            *slot = deadline;
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
                Some(t) if *t > now => hit = true,
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
            .filter_map(|key| until.get(&(cred_id, key)).copied())
            .filter(|t| *t > now)
            .map(|t| t.duration_since(now).as_secs() as i64)
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
        self.until
            .lock()
            .get(&(cred_id, String::new()))
            .filter(|t| **t > now)
            .map(|t| t.duration_since(now).as_secs() as i64)
            .unwrap_or(0)
    }

    /// 该凭证**模型级**冷却的明细：`(模型名, 剩余秒数)`，按剩余时间倒序。未冷却时为空。
    ///
    /// 补的是一个真实的观测盲区：模型级 429（实测里 fable 撞超额池就是这一档）只写进
    /// `(cred_id, 模型)` 那些格子，而后台读的是账号级那一格，于是选号侧明明已经跳过这个
    /// 模型、界面上却什么都看不到——「冷却中」那套筛选与徽章形同虚设。
    fn model_remaining(&self, cred_id: i64) -> Vec<(String, i64)> {
        let now = Instant::now();
        let mut out: Vec<(String, i64)> = self
            .until
            .lock()
            .iter()
            .filter(|((id, model), t)| *id == cred_id && !model.is_empty() && **t > now)
            .map(|((_, model), t)| (model.clone(), t.duration_since(now).as_secs() as i64))
            .collect();
        // 剩得最久的排前面；同秒数按模型名，保证展示顺序稳定（HashMap 迭代序是随机的）。
        out.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        out
    }

    fn forget(&self, cred_id: i64) {
        self.until.lock().retain(|(id, _), _| *id != cred_id);
    }
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
            bare_rate: BareRateWindow::default(),
            cooldown: RateLimitCooldown::default(),
            settings: parking_lot::RwLock::new(settings),
        }
    }

    /// 取该凭证的刷新锁（不存在则创建）。
    fn refresh_lock(&self, cred_id: i64) -> std::sync::Arc<tokio::sync::Mutex<()>> {
        self.refresh_locks.lock().entry(cred_id).or_default().clone()
    }

    /// 插入一条新凭证，返回带 id 的完整记录。
    pub fn insert(
        &self,
        label: &str,
        tier: Option<&str>,
        access_token: &str,
        refresh_token: &str,
        expires_at: u64,
        account_uuid: Option<&str>,
    ) -> Result<Credential> {
        let conn = self.conn.lock();
        // 新凭证一律落在默认档 P0：同档内按设备数负载均衡，新账号立刻参与分摊。
        // 需要瀑布式（榨干一个再用下一个）时，手动/批量把账号调到不同优先级即可。
        conn.execute(
            "INSERT INTO credentials
                 (label, tier, access_token, refresh_token, expires_at, account_uuid)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![label, tier, access_token, refresh_token, expires_at as i64, account_uuid],
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
        self.bare_rate.forget(id);
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

    /// 该凭证**模型级**冷却的明细 `(模型名, 剩余秒数)`，未冷却为空。
    /// 这一档不影响账号整体调度：其余模型照常可用，见 [`RateLimitCooldown`]。
    pub fn rate_limited_models(&self, cred_id: i64) -> Vec<(String, i64)> {
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

    /// 单条凭证当前**有效**绑定的设备数：已排除超过 TTL 未活跃的绑定（与选路时的惰性
    /// 过期口径一致），故后台显示会随时间自然回落，不必等下一次请求触发 sweep。
    /// TTL `<= 0`（永不过期）时按全量计。
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
    /// 不受绑定 TTL 影响：TTL 外的残行本就不占名额，顺手删掉也无害；而明细按 TTL 过滤，
    /// 后台能点到的必然是有效绑定。
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

/// 是否改写 `metadata.user_id` 的 account_uuid/device_id；`"0"`/`"false"` 关闭，缺省视为开启。
pub const SPOOF_IDENTITY_ENABLED: &str = "spoof_identity_enabled";

/// 来访自带 `device_id` 时，要不要把它换成本凭证派生的那个。缺省视为开启（即既有行为）。
///
/// 与 [`REQUIRE_DEVICE_ID`] 无关：那个管「没带身份的请求放不放行」，这个管「带了身份的
/// 请求要不要改写其中的设备段」。
pub const SPOOF_DEVICE_ID: &str = "spoof_device_id";

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
}

impl Default for ForwardFlags {
    fn default() -> Self {
        Self {
            spoof_identity: true,
            spoof_device_id: true,
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

/// 全局默认设备数上限的 settings 键名；`<= 0` 表示默认不限。
/// 账号自身 `device_limit == 0`（默认值）时套用它，无需逐个账号配置。
pub const DEFAULT_DEVICE_LIMIT: &str = "default_device_limit";

/// 单凭证裸请求速率上限的 settings 键名；`<= 0` 表示不限（默认）。见
/// [`CredentialStore::bare_rate_limit`]。
pub const BARE_RATE_LIMIT: &str = "bare_rate_limit";

/// 裸请求速率窗口（秒）的 settings 键名；`<= 0` 时退回 [`DEFAULT_BARE_RATE_WINDOW_SECS`]。
pub const BARE_RATE_WINDOW_SECS: &str = "bare_rate_window_secs";

/// 裸请求速率窗口默认值：60 秒（即上限的语义是「每分钟多少条」）。
pub const DEFAULT_BARE_RATE_WINDOW_SECS: i64 = 60;

/// 上游 429 时最多换几个号重试的 settings 键名；`0` 表示不重试。
pub const RATE_LIMIT_RETRY_MAX: &str = "rate_limit_retry_max";

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

/// 待写入的一条用量日志（代理层组装后交给 [`CredentialStore::insert_usage_log`]）。
#[derive(Debug, Default)]
pub struct UsageRecord {
    pub cred_id: Option<i64>,
    pub cred_label: String,
    /// 完整 device_id（供本地分析；对外展示可自行截断）。
    pub device_id: Option<String>,
    pub model: Option<String>,
    pub path: String,
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
    pub status: u16,
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
                 rl_overage_in_use, ratelimit_raw, cost_usd)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                     ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27)",
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

    /// 最近的用量日志，按时间倒序，最多 `limit` 条。
    pub fn list_usage_logs(&self, limit: i64) -> Result<Vec<UsageLog>> {
        let conn = self.conn.lock();
        let mut stmt = conn.prepare(
            "SELECT id, ts, cred_id, cred_label, device_id, model, path, status, has_usage,
                    input_tokens, output_tokens, cache_creation_tokens, cache_5m_tokens,
                    cache_1h_tokens, cache_read_tokens, ttft_ms, total_ms,
                    unified_status, rl_5h_status, rl_5h_reset, rl_5h_utilization,
                    rl_7d_status, rl_7d_reset, rl_7d_utilization, rl_representative, ratelimit_raw,
                    cost_usd, rl_overage_in_use
               FROM usage_logs ORDER BY id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map([limit], |r| {
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
            cost_usd           REAL
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
        ) STRICT, WITHOUT ROWID;",
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
    /// 设备绑定有效期（秒）；`<= 0` 表示永不过期。
    pub ttl_secs: i64,
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
    /// 1. 已有绑定且该凭证仍启用 → 复用（更新 last_seen / request_count），已绑定设备不受限。
    /// 2. 绑定的凭证已停用或删除 → 清除陈旧绑定，作为新设备重新选择。
    /// 3. 新设备 → 在仍有名额的启用凭证中做负载均衡：选“当前设备数最少”者并绑定；
    ///    同数时按 (priority, id) 决定，保持确定性。
    /// 4. 所有启用凭证均达设备上限 → 硬性拒绝，返回 [`DeviceLimitReached`]（代理映射为 429）。
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
    /// `rate_limited` 由调用方判定——代理只对**真正消耗额度的**路径置真
    /// （`/v1/messages`，见 `crate::proxy::is_billable_messages`）。`count_tokens` 这类
    /// 既不产生 usage、也不消耗额度的路径不计：拿它占名额只会把真正的请求挤掉，
    /// 而客户端的 `/context` 显示与压缩前预估全靠它。
    ///
    /// `ttl_secs > 0` 时先清除超时未活跃的绑定（惰性过期）；`<= 0` 表示永不过期。
    /// 全部操作在单次持锁内完成，避免与其它写入竞态。
    ///
    /// **限流按「选一次号」计，不是按「客户端请求」计**：刷新失败换号那条路
    /// （[`select_with_refresh_failover`]）每轮都会重选，故一次客户端请求最多可能扣掉几个
    /// 名额。那条路只在凭证被上游作废时才走（罕见），宁可多扣也好过给它开一个绕过限流的口子。
    pub fn select_for_device(&self, sel: Select<'_>) -> Result<Credential> {
        let Select { device_id, ttl_secs, rate_limited, exclude, model } = sel;
        // 这几项须在取锁前读（内部自己会取锁，parking_lot 不可重入）。
        let default_limit = self.default_device_limit();
        let (rate_limit, rate_window) = (self.bare_rate_limit(), self.bare_rate_window_secs());
        let conn = self.conn.lock();

        // 惰性过期：清掉超过 TTL 未活跃的绑定，释放其占用的设备名额。
        if ttl_secs > 0 {
            conn.execute(
                "DELETE FROM device_bindings WHERE last_seen_at < unixepoch() - ?1",
                [ttl_secs],
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

        // 1/2) 命中既有绑定。
        if let Some(did) = device_id {
            let bound: Option<i64> = conn
                .query_row("SELECT cred_id FROM device_bindings WHERE device_id = ?1", [did], |r| {
                    r.get(0)
                })
                .optional()?;
            if let Some(cid) = bound {
                if let Some(c) = creds.iter().find(|c| c.id == cid) {
                    conn.execute(
                        "UPDATE device_bindings
                            SET last_seen_at = unixepoch(), request_count = request_count + 1
                          WHERE device_id = ?1",
                        [did],
                    )?;
                    return Ok(c.clone());
                }
                // 绑定的凭证已停用/删除，或正在冷却/本轮已试过：清除绑定后重新选择，
                // 下面选中谁就**改绑**到谁（`INSERT … ON CONFLICT DO UPDATE cred_id`）。
                // 冷却结束后这台设备不会自己回到原号——粘性以最后一次选择为准，
                // 这正是「429 换号重试要改绑」想要的语义。
                conn.execute("DELETE FROM device_bindings WHERE device_id = ?1", [did])?;
            }
        }

        // 各凭证当前设备数。
        let mut counts: HashMap<i64, i64> = HashMap::new();
        {
            let mut cstmt =
                conn.prepare("SELECT cred_id, COUNT(*) FROM device_bindings GROUP BY cred_id")?;
            let rows = cstmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))?;
            for row in rows {
                let (cid, n) = row?;
                counts.insert(cid, n);
            }
        }

        // 当前设备数（惰性过期后已排除超时项）。
        let used = |c: &Credential| counts.get(&c.id).copied().unwrap_or(0);
        // 生效上限：账号未单独配置（device_limit == 0）时套用全局默认。
        let limit_of = |c: &Credential| effective_device_limit(c.device_limit, default_limit);

        // 3/4) 优先级分档调度：优先级为主键（数值小者优先），同一档内再按设备数
        //      负载均衡，最后 id 兜底。低优先级档仅在高优先级档全部占满/不可用后才触及。
        let chosen = if device_id.is_some() {
            // 硬限制：仅在仍有名额者（生效上限 <=0 不限，或 used<上限）中选；
            // 当前优先级档全满时其成员被过滤掉，min 自然溢出到下一档；全部满则拒绝。
            match creds
                .iter()
                .filter(|c| limit_of(c) <= 0 || used(c) < limit_of(c))
                .min_by_key(|c| (c.priority, used(c), c.id))
            {
                Some(c) => c,
                None => return Err(DeviceLimitReached.into()),
            }
        } else {
            // 无 device_id：不占设备名额，但要过裸请求速率上限。按同一套 (priority, used, id)
            // 排好序后逐个试，第一个还有名额的即中。
            //
            // 拿 `try_take` 直接当 `find` 的谓词是安全的：它只在**放行**时才记一条，被跳过的
            // （已满的）那些不留痕，而 `find` 命中即短路，故一次选号最多记一条。
            // 不计入限流（或未配置上限）时谓词恒真，等价于原来的 `min_by_key`，零额外开销。
            let mut ordered: Vec<&Credential> = creds.iter().collect();
            ordered.sort_by_key(|c| (c.priority, used(c), c.id));
            let window = Duration::from_secs(rate_window.max(1) as u64);
            match ordered
                .into_iter()
                .find(|c| !rate_limited || self.bare_rate.try_take(c.id, rate_limit, window))
            {
                Some(c) => c,
                None => return Err(BareRateLimited { retry_after_secs: rate_window }.into()),
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
enum TokenAttempt {
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
    http: &wreq::Client,
    sel: Select<'_>,
) -> Result<(String, Credential)> {
    select_with_refresh_failover(store, sel, |cred| {
        Box::pin(async move { ensure_fresh_token(store, http, &cred).await })
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
    http: &wreq::Client,
    cred: &Credential,
) -> Result<String> {
    match ensure_fresh_token(store, http, cred).await? {
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
    let sel = Select { ttl_secs: store.device_binding_ttl(), ..sel };

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
async fn ensure_fresh_token(
    store: &CredentialStore,
    http: &wreq::Client,
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
    let err = match crate::oauth::refresh(http, &cred.refresh_token).await {
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
        let a = store.insert("a", None, "ta", "ra", 0, None).unwrap();
        let b = store.insert("b", None, "tb", "rb", 0, None).unwrap();
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
        let a = store.insert("a", None, "ta", "ra", 0, None).unwrap();
        let b = store.insert("b", None, "tb", "rb", 0, None).unwrap();
        let c = store.insert("c", None, "tc", "rc", 0, None).unwrap();
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
        let a = store.insert("a", None, "ta", "ra", 0, None).unwrap();
        let b = store.insert("b", None, "tb", "rb", 0, None).unwrap();
        let c = store.insert("c", None, "tc", "rc", 0, None).unwrap();
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
        let a = store.insert("a", None, "ta", "ra", 0, None).unwrap();
        let b = store.insert("b", None, "tb", "rb", 0, None).unwrap();

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
                    .insert(l, None, &format!("tok-{l}"), &format!("refresh-{l}"), 0, None)
                    .unwrap()
                    .id
            })
            .collect();
        (store, ids)
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
        let a = store.insert("a", None, "ta", "ra", 0, None).unwrap().id;
        let b = store.insert("b", None, "tb", "rb", 0, None).unwrap().id;

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
        assert_eq!(q.get(&b).unwrap().cost_5h, Some(16.0), "费用不得跨账号串");
        assert_eq!(q.get(&b).unwrap().requests_5h, Some(1), "请求数不得跨账号串");

        // 单账号入口与批量入口必须给出同一份结果。
        assert_eq!(store.latest_quota(a).unwrap().unwrap().cost_5h, qa.cost_5h);
        assert_eq!(store.latest_quota(a).unwrap().unwrap().ts, qa.ts);
        assert!(store.latest_quota(999).unwrap().is_none(), "不存在的账号应为 None");
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
        let a = store.insert("a", None, "ta", "ra", 0, None).unwrap().id;
        let b = store.insert("b", None, "tb", "rb", 0, None).unwrap().id;

        // a：只有 5h 有 reset（窗口起点 82_000），7d 一直为空。
        log_row(&store, a, 10_000, 1.0, Some(100_000), None); // 5h 窗口外
        log_row(&store, a, 90_000, 2.0, Some(100_000), None); // 窗口内
        log_row(&store, a, 95_000, 4.0, Some(100_000), None); // 窗口内
        let qa = store.latest_quota(a).unwrap().unwrap();
        assert_eq!(qa.cost_5h, Some(6.0), "缺 7d reset 不该把 5h 窗口打成 0");
        assert_eq!(qa.requests_5h, Some(2));
        assert_eq!(qa.cost_7d, None, "没有 7d reset 就没有 7d 窗口可算");
        assert_eq!(qa.requests_7d, None);

        // b：反过来只有 7d 有 reset（窗口起点 100_000 - 604_800，含全部行）。
        log_row(&store, b, 90_000, 8.0, None, Some(100_000));
        log_row(&store, b, 95_000, 16.0, None, Some(100_000));
        let qb = store.latest_quota(b).unwrap().unwrap();
        assert_eq!(qb.cost_7d, Some(24.0), "缺 5h reset 不该把 7d 窗口打成 0");
        assert_eq!(qb.requests_7d, Some(2));
        assert_eq!(qb.cost_5h, None);
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
        let a = store.insert("a", None, "ta", "ra", 0, None).unwrap().id;

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
        let a = store.insert("a", None, "ta", "ra", 0, None).unwrap().id;

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

    /// overage-in-use 标记随快照落账：带限流头的响应写入即更新，后续不带头的响应不得抹掉，
    /// 下一条带头的响应按新值覆盖。这是「额度满但不 429（usage credits 在放行）」唯一的外显信号。
    #[test]
    fn overage_marker_lands_in_snapshot() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        let store = CredentialStore::with_conn(conn);
        let a = store.insert("a", None, "ta", "ra", 0, None).unwrap().id;

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
        let a = store.insert("a", None, "ta", "ra", 0, None).unwrap().id;

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
        let a = store.insert("a", None, "ta", "ra", 0, None).unwrap().id;

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
        let a = store.insert("a", None, "ta", "ra", 0, None).unwrap().id;

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
        let a = store.insert("a", None, "ta", "ra", 0, None).unwrap().id;

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
        let a = store.insert("a", None, "ta", "ra", 0, None).unwrap().id;

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
        let a = store.insert("a", None, "ta", "ra", 0, None).unwrap().id;
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
        let a = store.insert("a", None, "ta", "ra", 0, None).unwrap().id;
        let b = store.insert("b", None, "tb", "rb", 0, None).unwrap().id;
        log_row(&store, a, 1_000, 1.5, None, None);
        log_row(&store, a, 2_000, 2.5, None, None);
        log_row(&store, b, 3_000, 7.0, None, None);
        let c = store.insert("c", None, "tc", "rc", 0, None).unwrap().id; // 从未被用过

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
        ] {
            store.set_setting(key, off).unwrap();
        }
        let f = store.forward_flags();
        assert_eq!(
            f,
            ForwardFlags {
                spoof_identity: false,
                spoof_device_id: false,
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
}
