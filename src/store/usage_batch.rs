//! 流水批量落库：转发路径收尾时交出的用量日志与 thinking 签发登记先进队列，由一个后台写入任务
//! 攒一小批（[`LINGER`] 内、至多 [`MAX_BATCH`] 条）在**一个事务**里写完。
//!
//! 逐条写时每条请求一个事务、七八次往返，且每条都要改一遍全库最热的几行（预聚合「全部」那一格、
//! 当前小时的费用汇总、号的账本），几条并发事务就在这些行锁上排队，每次改动还各留一个死元组与一份
//! WAL。攒批后同一行一批只改一次：账本、费用汇总、预聚合在内存里按各自的键先合并好再 upsert，
//! 流水多行一条 INSERT，往返与热行改动都按批摊薄；也只剩这一个写者，热行上不再有锁争用。
//!
//! 与逐条写（[`CredentialStore::insert_usage_log_at`]、[`CredentialStore::remember_thinking_blocks`]）
//! 落库结果相同（`batched_writes_match_one_by_one` 逐表比对），口径差别：
//!
//! - 时刻在入队时取（应用时钟，按 `unixepoch()` 的四舍五入取整），不是落库那条语句的时刻——晚落库
//!   的那一两秒不该算进流水的时刻；
//! - thinking 登记随流水不等 WAL 落盘（逐条写时它是单独一笔同步提交），PG 崩溃时与流水一起丢最后
//!   几百毫秒；还没落库的那段时间由进程内的待写表（[`CredentialStore::pending_thinking`]）补上；
//! - 取证里的 `inbound_shape` / `inbound_headers` 超过 [`INBOUND_FIELD_MAX`] 的记成一个只带原长的
//!   占位 JSON（错误文案、响应开头在入队时就按落库上限截好，结果与落库时截相同）。
//!
//! 写失败按原因处理（[`classify`]）：数据本身有问题（22xxx / 23xxx 之类）退回逐条写，一条坏数据不连累
//! 整批；连接 / 超时 / 串行化冲突这类临时故障整批退避重试（[`RETRY_FIRST`] 起翻倍到 [`RETRY_MAX`]），
//! 不拆成逐条——库挂着时逐条只会每条再等一遍取连接超时；提交那一步报错时结果不确定，先按事务号查它
//! 到底提交了没有（`pg_xact_status`），提交了就算完，没提交再重试，不会把整批记两遍。
//!
//! 队列有上限（[`QUEUE_CAP`]）：库长时间写不进去时满了就丢新来的、计数并限频告警，内存不会一直涨。

use std::borrow::Cow;
use std::collections::{BTreeMap, HashMap};
use std::panic::AssertUnwindSafe;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::Result;
use futures_util::FutureExt as _;
use sqlx::{PgConnection, Postgres, QueryBuilder};
use tokio::sync::mpsc;

use super::billing::BILLING_BUCKET_SECS;
use super::rollup::{RollupAgg, RollupInput, keys, rollup_bucket};
use super::usage::{
    ERROR_MESSAGE_MAX, INSERT_USAGE_LOG_SQL, RESPONSE_EXCERPT_MAX, SET_SYNC_COMMIT_SQL, head_chars,
    split_tool_names, without_nul,
};
use super::{CredentialStore, PendingWrite, UsageRecord, strip_nul};

/// 一批从第一条入队起最多再等这么久。500 rpm 下一批七八条：事务数与热行改动降到约八分之一，
/// 流水最多晚一秒可见。
const LINGER: Duration = Duration::from_secs(1);

/// 一批最多这么多条（流水 48 列，一条 INSERT 的绑定参数远低于 65535 的上限）。
const MAX_BATCH: usize = 200;

/// 队列上限：500 rpm 下约 20 分钟的量。满了丢新来的（[`BatchStats::dropped`]）。
const QUEUE_CAP: usize = 10_000;

/// 临时故障整批重试的退避：从这里起每次翻倍，封顶 [`RETRY_MAX`]。
#[cfg(not(test))]
const RETRY_FIRST: Duration = Duration::from_millis(500);
#[cfg(test)]
const RETRY_FIRST: Duration = Duration::from_millis(1);
#[cfg(not(test))]
const RETRY_MAX: Duration = Duration::from_secs(10);
#[cfg(test)]
const RETRY_MAX: Duration = Duration::from_millis(4);

/// `inbound_shape` / `inbound_headers` 入队时的上限（字节）。正常的摘要几 KB；来访顶层带了大字段时
/// 摘要会把它原样带上（见 `proxy::logging::inbound_shape_summary`），超了只记原长。
const INBOUND_FIELD_MAX: usize = 16 * 1024;

/// 待写表里的 thinking 登记多久不落库就当作没了（写入任务丢了那批、或一直写不进去）。
const PENDING_THINKING_TTL: Duration = Duration::from_secs(30);

/// 观测：多久汇总打一行写入情况。
const REPORT_EVERY: Duration = Duration::from_secs(60);

/// 单批写了这么久就当场告警。
const SLOW_BATCH: Duration = Duration::from_secs(5);

/// 队列里的一笔。
enum Item {
    Usage { rec: Box<UsageRecord>, ts: i64 },
    Thinking { cred_id: Option<i64>, fps: Vec<String>, ts: i64, queued_at: Instant },
}

/// 带着在途计数的一笔：落库（或放弃）之后才减，[`CredentialStore::drain_writes`] 据此等它。
struct Queued {
    item: Item,
    _pending: PendingWrite,
}

/// 写入任务的发送端，见 [`CredentialStore::queue_usage_log`]。
pub(super) struct Batcher(mpsc::Sender<Queued>);

/// 批量写入的丢弃计数（写入任务定期汇总时一并打出，见 [`Report`]）。
#[derive(Default)]
pub(super) struct BatchStats {
    /// 队列满了丢掉的笔数（累计）。
    dropped: AtomicU64,
    /// 上次为丢弃告警的时刻：一分钟最多一行。
    last_drop_warn: parking_lot::Mutex<Option<Instant>>,
}

/// 现在的 unix 秒，取整口径同库里的 `unixepoch()`（`extract(epoch …)::BIGINT` 是四舍五入）。
fn now_secs() -> i64 {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64;
    (ms + 500) / 1000
}

/// 入队前把大字段收住：错误文案与响应开头按落库上限截（先去 NUL 再截，与落库时 [`without_nul`] 后再
/// [`head_chars`] 的结果相同），来访摘要超了 [`INBOUND_FIELD_MAX`] 记成占位。
fn trim_for_queue(rec: &mut UsageRecord) {
    for (field, max) in [
        (&mut rec.forensics.error_message, ERROR_MESSAGE_MAX),
        (&mut rec.forensics.response_excerpt, RESPONSE_EXCERPT_MAX),
    ] {
        if let Some(s) = field
            && s.len() > max
        {
            strip_nul(s);
            *s = head_chars(s, max);
        }
    }
    for field in [&mut rec.forensics.inbound_shape, &mut rec.forensics.inbound_headers] {
        if let Some(s) = field
            && s.len() > INBOUND_FIELD_MAX
        {
            *s = serde_json::json!({ "truncated_bytes": s.len() }).to_string();
        }
    }
}

impl CredentialStore {
    /// 一条用量日志（连同账本、费用汇总、预聚合）交给批量写入，见模块说明。
    pub fn queue_usage_log(self: &Arc<Self>, mut rec: UsageRecord) {
        trim_for_queue(&mut rec);
        self.queue(Item::Usage { rec: Box::new(rec), ts: now_secs() });
    }

    /// 一条回复签发的 thinking 签名 / 密文指纹（[`Self::remember_thinking_blocks`]）交给批量写入。
    /// 入队时同步记进待写表（[`Self::pending_thinking`]），落库前查签发者也看得见。
    pub fn queue_thinking_blocks(self: &Arc<Self>, cred_id: Option<i64>, fps: Vec<String>) {
        if fps.is_empty() {
            return;
        }
        let queued_at = Instant::now();
        {
            let mut pending = self.pending_thinking.lock();
            if pending.len() > 1024 {
                pending.retain(|_, (_, at)| queued_at.duration_since(*at) < PENDING_THINKING_TTL);
            }
            for fp in &fps {
                pending.entry(fp.clone()).or_insert((cred_id, queued_at));
            }
        }
        let item = Item::Thinking { cred_id, fps, ts: now_secs(), queued_at };
        if let Some(Item::Thinking { fps, queued_at, .. }) = self.queue(item) {
            self.forget_pending_thinking(&fps, queued_at);
        }
    }

    /// 入队；没进得去（队列满、没有运行时）就把那笔还回来。
    fn queue(self: &Arc<Self>, item: Item) -> Option<Item> {
        let queued = Queued { item, _pending: PendingWrite::new(&self.pending_writes) };
        let mut slot = self.usage_batch.lock();
        // 头一笔时起写入任务；它所在的运行时没了（测试里换运行时）就重起一个。
        if slot.as_ref().is_none_or(|b| b.0.is_closed()) {
            let Ok(handle) = tokio::runtime::Handle::try_current() else {
                tracing::warn!("no tokio runtime; dropping the usage write");
                return Some(queued.item);
            };
            let (tx, rx) = mpsc::channel(QUEUE_CAP);
            handle.spawn(writer(self.clone(), rx));
            *slot = Some(Batcher(tx));
        }
        let tx = slot.as_ref()?.0.clone();
        drop(slot);
        match tx.try_send(queued) {
            Ok(()) => None,
            Err(mpsc::error::TrySendError::Full(q)) => {
                let dropped = self.batch_stats.dropped.fetch_add(1, Ordering::Relaxed) + 1;
                let now = Instant::now();
                let mut last = self.batch_stats.last_drop_warn.lock();
                if last.is_none_or(|t| now.duration_since(t) >= REPORT_EVERY) {
                    *last = Some(now);
                    tracing::warn!(
                        dropped,
                        cap = QUEUE_CAP,
                        "the usage write queue is full (the database is not keeping up); dropping new usage writes"
                    );
                }
                Some(q.item)
            }
            Err(mpsc::error::TrySendError::Closed(q)) => {
                tracing::warn!("the usage writer is gone; dropping the usage write");
                Some(q.item)
            }
        }
    }

    /// 某笔 thinking 登记已落库（或放弃了）：从待写表里拿掉它记下的那些（别的笔后来记的同一指纹不动）。
    fn forget_pending_thinking(&self, fps: &[String], queued_at: Instant) {
        let mut pending = self.pending_thinking.lock();
        for fp in fps {
            if pending.get(fp).is_some_and(|(_, at)| *at == queued_at) {
                pending.remove(fp);
            }
        }
    }

    /// 待写表里这个指纹的签发者（还没落库、也没过期的），见 [`Self::pending_thinking`]。
    pub(super) fn pending_thinking_issuer(&self, fp: &str) -> Option<Option<i64>> {
        let pending = self.pending_thinking.lock();
        let (cred, at) = pending.get(fp)?;
        (at.elapsed() < PENDING_THINKING_TTL).then_some(*cred)
    }
}

/// 唯一的写入任务：攒一批、写一批。[`CredentialStore::drain_writes`] 敲 `batch_flush` 时不再等满
/// [`LINGER`]，手里有的当场写。写一批时 panic 只丢这一批（打一行 error 写明条数），任务照常往下跑，
/// 队列里排着的不受连累。
async fn writer(store: Arc<CredentialStore>, mut rx: mpsc::Receiver<Queued>) {
    let mut report = Report::new();
    while let Some(first) = rx.recv().await {
        let mut batch = vec![first];
        let deadline = tokio::time::Instant::now() + LINGER;
        while batch.len() < MAX_BATCH {
            tokio::select! {
                biased;
                next = rx.recv() => match next {
                    Some(q) => batch.push(q),
                    None => break,
                },
                () = store.batch_flush.notified() => {
                    while batch.len() < MAX_BATCH && let Ok(q) = rx.try_recv() {
                        batch.push(q);
                    }
                    break;
                }
                () = tokio::time::sleep_until(deadline) => break,
            }
        }
        let started = Instant::now();
        let n = batch.len();
        if AssertUnwindSafe(write_batch(&store, &batch)).catch_unwind().await.is_err() {
            tracing::error!(rows = n, "the usage batch writer panicked; this batch is lost");
        }
        for q in &batch {
            if let Item::Thinking { fps, queued_at, .. } = &q.item {
                store.forget_pending_thinking(fps, *queued_at);
            }
        }
        drop(batch);
        report.note(&store, n, started.elapsed(), rx.len());
    }
}

/// 写入情况的定期汇总（[`REPORT_EVERY`] 一行），慢批当场告警。
struct Report {
    since: Instant,
    batches: u64,
    rows: u64,
    total: Duration,
    max: Duration,
}

impl Report {
    fn new() -> Self {
        Self {
            since: Instant::now(),
            batches: 0,
            rows: 0,
            total: Duration::ZERO,
            max: Duration::ZERO,
        }
    }

    fn note(&mut self, store: &CredentialStore, rows: usize, took: Duration, queued: usize) {
        self.batches += 1;
        self.rows += rows as u64;
        self.total += took;
        self.max = self.max.max(took);
        if took >= SLOW_BATCH {
            tracing::warn!(
                rows,
                took_ms = took.as_millis() as u64,
                queued,
                "slow usage batch write"
            );
        }
        if self.since.elapsed() >= REPORT_EVERY {
            tracing::info!(
                batches = self.batches,
                rows = self.rows,
                avg_ms = (self.total.as_millis() as u64) / self.batches.max(1),
                max_ms = self.max.as_millis() as u64,
                queued,
                dropped = store.batch_stats.dropped.load(Ordering::Relaxed),
                "usage batch writer"
            );
            *self = Self::new();
        }
    }
}

/// 一批写失败的原因，决定怎么补救（见模块说明）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Failure {
    /// 数据本身有问题：逐条重写，把坏的那条隔离出来。
    Data,
    /// 临时故障：整批退避重试。
    Transient,
}

/// 按错误归类。认不出的按数据问题算（逐条重写一遍，每条各自报错），不会无限重试一个写不进去的批。
fn classify(e: &anyhow::Error) -> Failure {
    let Some(e) = e.chain().find_map(|c| c.downcast_ref::<sqlx::Error>()) else {
        return Failure::Data;
    };
    match e {
        sqlx::Error::PoolTimedOut
        | sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::Protocol(_)
        | sqlx::Error::WorkerCrashed => Failure::Transient,
        sqlx::Error::Database(db) => {
            let code = db.code().unwrap_or_default();
            // 08 连接异常、53 资源不足、40001 串行化冲突、40P01 死锁、57P01~03 库在关 / 重启。
            if code.starts_with("08")
                || code.starts_with("53")
                || matches!(code.as_ref(), "40001" | "40P01" | "57P01" | "57P02" | "57P03")
            {
                Failure::Transient
            } else {
                Failure::Data
            }
        }
        _ => Failure::Data,
    }
}

/// 一次整批写的失败：哪一步、它的事务号（拿到了的话）、原因。
struct FlushError {
    /// 提交那一步报的错：结果不确定，事务可能已经提交了。
    at_commit: bool,
    /// 本批事务的事务号（`pg_current_xact_id()`），提交结果不确定时拿它查。
    xid: Option<String>,
    err: anyhow::Error,
}

/// 写一批：成功即完；数据问题逐条重写；临时故障与提交结果不确定的整批退避重试，见模块说明。
async fn write_batch(store: &CredentialStore, batch: &[Queued]) {
    let mut usage: Vec<(&UsageRecord, i64)> = Vec::new();
    let mut thinking: Vec<(Option<i64>, &[String], i64)> = Vec::new();
    for q in batch {
        match &q.item {
            Item::Usage { rec, ts } => usage.push((rec, *ts)),
            Item::Thinking { cred_id, fps, ts, .. } => thinking.push((*cred_id, fps, *ts)),
        }
    }
    settle(store, &usage, &thinking, || flush(store, &usage, &thinking)).await;
}

/// [`write_batch`] 的重试与补救部分（`attempt` 是整批写一次，测试里换成会出错的）。
async fn settle<F, Fut>(
    store: &CredentialStore,
    usage: &[(&UsageRecord, i64)],
    thinking: &[(Option<i64>, &[String], i64)],
    mut attempt: F,
) where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = std::result::Result<(), FlushError>>,
{
    let mut delay = RETRY_FIRST;
    // 上一次提交结果不确定时的事务号：重试前先查它提交了没有。
    let mut unsure: Option<String> = None;
    loop {
        if let Some(xid) = &unsure {
            match xact_status(store, xid).await {
                Ok(Some(s)) if s == "committed" => return,
                // 回滚了（或事务号已太老查不到）：没写进去，重试。
                Ok(Some(s)) if s == "aborted" => unsure = None,
                Ok(None) => unsure = None,
                // 还在进行中、或查不了（库还没恢复）：等一会儿再查。
                Ok(Some(_)) | Err(_) => {
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(RETRY_MAX);
                    continue;
                }
            }
        }
        let Err(FlushError { at_commit, xid, err }) = attempt().await else { return };
        let failure = classify(&err);
        if at_commit && let Some(xid) = xid {
            tracing::warn!(
                usage = usage.len(),
                thinking = thinking.len(),
                error = %format!("{err:#}"),
                "committing a usage batch failed; checking whether it landed before retrying"
            );
            unsure = Some(xid);
        } else if failure == Failure::Data {
            tracing::warn!(
                usage = usage.len(),
                thinking = thinking.len(),
                error = %format!("{err:#}"),
                "batched usage write failed on the data; retrying one by one"
            );
            one_by_one(store, usage, thinking).await;
            return;
        } else {
            tracing::warn!(
                usage = usage.len(),
                thinking = thinking.len(),
                retry_in_ms = delay.as_millis() as u64,
                error = %format!("{err:#}"),
                "batched usage write hit a transient failure; retrying the whole batch"
            );
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(RETRY_MAX);
    }
}

/// 事务号 `xid` 的结局（`committed` / `aborted` / `in progress`；太老查不到为 `None`）。
async fn xact_status(store: &CredentialStore, xid: &str) -> Result<Option<String>> {
    Ok(sqlx::query_scalar("SELECT pg_xact_status($1::xid8)")
        .bind(xid)
        .fetch_one(&store.pool)
        .await?)
}

/// 整批里有坏数据：逐条写，坏的那条自己报错，别的照常落库。
async fn one_by_one(
    store: &CredentialStore,
    usage: &[(&UsageRecord, i64)],
    thinking: &[(Option<i64>, &[String], i64)],
) {
    for (rec, ts) in usage {
        if let Err(e) = store.insert_usage_log_at(rec, Some(*ts)).await {
            tracing::warn!(what = "usage log", error = %format!("{e:#}"), "background store write failed");
        }
    }
    for (cred_id, fps, _) in thinking {
        if let Err(e) = store.remember_thinking_blocks(*cred_id, fps).await {
            tracing::warn!(what = "thinking blocks", error = %format!("{e:#}"), "background store write failed");
        }
    }
}

/// 一批在一个事务里写完。各步与逐条写的那一笔同序（锁号行 → 工具集 → 流水 → 费用汇总 → 账本 →
/// 预聚合），再补 thinking 登记。
async fn flush(
    store: &CredentialStore,
    usage: &[(&UsageRecord, i64)],
    thinking: &[(Option<i64>, &[String], i64)],
) -> std::result::Result<(), FlushError> {
    let mut xid = None;
    let at = |at_commit: bool, xid: &Option<String>| {
        let xid = xid.clone();
        move |err: anyhow::Error| FlushError { at_commit, xid, err }
    };
    let tx = write_tx(store, usage, thinking, &mut xid).await.map_err(at(false, &xid))?;
    tx.commit().await.map_err(|e| at(true, &xid)(e.into()))?;
    note_tool_sets(store, usage);
    Ok(())
}

/// 提交成功后把这批的新工具集记进 `known_tool_sets`，往后不再补写。
fn note_tool_sets(store: &CredentialStore, usage: &[(&UsageRecord, i64)]) {
    let mut known = store.known_tool_sets.lock();
    for (rec, _) in usage {
        if let (_, Some((sha, _))) = split_tool_names(rec.forensics.shape.as_deref())
            && !known.contains(&sha)
        {
            if known.len() >= super::usage::KNOWN_TOOL_SETS_MAX {
                known.clear();
            }
            known.insert(sha);
        }
    }
}

/// 开事务、把一批全写进去，交回待提交的事务（`xid` 填上它的事务号）。
async fn write_tx(
    store: &CredentialStore,
    usage: &[(&UsageRecord, i64)],
    thinking: &[(Option<i64>, &[String], i64)],
    xid: &mut Option<String>,
) -> Result<sqlx::Transaction<'static, Postgres>> {
    let cleaned: Vec<Cow<'_, UsageRecord>> = usage
        .iter()
        .map(|(rec, _)| without_nul(rec).map_or(Cow::Borrowed(*rec), Cow::Owned))
        .collect();
    let splits: Vec<_> =
        cleaned.iter().map(|r| split_tool_names(r.forensics.shape.as_deref())).collect();
    let new_sets: BTreeMap<&str, &str> = {
        let known = store.known_tool_sets.lock();
        splits
            .iter()
            .filter_map(|(_, set)| set.as_ref())
            .filter(|(sha, _)| !known.contains(sha))
            .map(|(sha, names)| (sha.as_str(), names.as_str()))
            .collect()
    };
    // 提交不等 WAL 落盘，例外同逐条写：要插新工具集的那批照库的默认同步提交。
    let sync_commit = if new_sets.is_empty() { Some("off") } else { None };
    let mut tx = store.pool.begin().await?;
    // 事务号顺手取出来：提交报错时据此查它到底提交了没有。
    let (_, id): (String, String) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "{SET_SYNC_COMMIT_SQL}, pg_current_xact_id()::TEXT"
    )))
    .bind(sync_commit)
    .fetch_one(&mut *tx)
    .await?;
    *xid = Some(id);

    // 号行上 `FOR KEY SHARE`（删号等这批提交）、读出号主；读不到的号已删，流水照记、账目跳过。
    let mut cred_ids: Vec<i64> = cleaned
        .iter()
        .filter_map(|r| r.cred_id)
        .chain(thinking.iter().filter_map(|(c, _, _)| *c))
        .collect();
    cred_ids.sort_unstable();
    cred_ids.dedup();
    let owners: HashMap<i64, i64> = if cred_ids.is_empty() {
        HashMap::new()
    } else {
        sqlx::query_as::<_, (i64, i64)>(
            "SELECT id, COALESCE(owner_id, 0) FROM credentials WHERE id = ANY($1)
              ORDER BY id FOR KEY SHARE",
        )
        .bind(&cred_ids)
        .fetch_all(&mut *tx)
        .await?
        .into_iter()
        .collect()
    };

    if !new_sets.is_empty() {
        let (shas, names): (Vec<&str>, Vec<&str>) = new_sets.iter().map(|(s, n)| (*s, *n)).unzip();
        sqlx::query(
            "INSERT INTO tool_sets (sha, names)
             SELECT * FROM unnest($1::TEXT[], $2::TEXT[]) ON CONFLICT DO NOTHING",
        )
        .bind(&shas)
        .bind(&names)
        .execute(&mut *tx)
        .await?;
    }

    if !cleaned.is_empty() {
        insert_usage_rows(&mut tx, &cleaned, usage, &splits).await?;
        // 有号主的才记账（号已删 / 还没选到号的只留流水），同逐条写。
        let billed: Vec<(&UsageRecord, i64, i64)> = cleaned
            .iter()
            .zip(usage)
            .filter_map(|(r, (_, ts))| {
                let owner = *owners.get(&r.cred_id?)?;
                Some((r.as_ref(), *ts, owner))
            })
            .collect();
        billing_rows(&mut tx, &billed).await?;
        ledger_rows(&mut tx, &billed).await?;
        rollup_rows(&mut tx, &cleaned, usage).await?;
    }
    thinking_rows(&mut tx, thinking, &owners).await?;
    Ok(tx)
}

/// 一条流水的 shape 拆掉工具名清单之后的样子与拆出来的清单，见 [`split_tool_names`]。
type ToolSplit<'a> = (Option<Cow<'a, str>>, Option<(String, String)>);

/// 流水：一条多行 INSERT，列与绑定顺序同 [`INSERT_USAGE_LOG_SQL`]，行序即入队顺序（id 照旧递增）。
async fn insert_usage_rows(
    conn: &mut PgConnection,
    cleaned: &[Cow<'_, UsageRecord>],
    usage: &[(&UsageRecord, i64)],
    splits: &[ToolSplit<'_>],
) -> Result<()> {
    let head = INSERT_USAGE_LOG_SQL.split_once("VALUES").map_or(INSERT_USAGE_LOG_SQL, |(h, _)| h);
    let rows: Vec<_> = cleaned.iter().zip(usage).zip(splits).collect();
    for chunk in rows.chunks(500) {
        let mut q: QueryBuilder<Postgres> = QueryBuilder::new(head);
        q.push_values(chunk, |mut b, ((rec, (_, ts)), (shape, _))| {
            b.push_bind(*ts)
                .push_bind(rec.cred_id)
                .push_bind(rec.cred_label.clone())
                .push_bind(rec.device_id.clone())
                .push_bind(rec.model.clone())
                .push_bind(rec.path.clone())
                .push_bind(rec.status as i64)
                .push_bind(rec.has_usage as i64)
                .push_bind(rec.input_tokens)
                .push_bind(rec.output_tokens)
                .push_bind(rec.cache_creation_tokens)
                .push_bind(rec.cache_5m_tokens)
                .push_bind(rec.cache_1h_tokens)
                .push_bind(rec.cache_read_tokens)
                .push_bind(rec.ttft_ms)
                .push_bind(rec.total_ms)
                .push_bind(rec.unified_status.clone())
                .push_bind(rec.rl_5h_status.clone())
                .push_bind(rec.rl_5h_reset)
                .push_bind(rec.rl_5h_utilization)
                .push_bind(rec.rl_7d_status.clone())
                .push_bind(rec.rl_7d_reset)
                .push_bind(rec.rl_7d_utilization)
                .push_bind(rec.rl_representative.clone())
                .push_bind(rec.rl_overage_in_use.map(i64::from))
                .push_bind(rec.ratelimit_raw.clone())
                .push_bind(rec.cost_usd)
                .push_bind(rec.ua.clone())
                .push_bind(rec.ua_out.clone())
                .push_bind(rec.sse_aggregated as i64)
                .push_bind(rec.request_id.clone())
                .push_bind(rec.upstream_request_id.clone())
                .push_bind(rec.forensics.proxy.clone())
                .push_bind(rec.forensics.simulated as i64)
                .push_bind(shape.as_deref().map(str::to_string))
                .push_bind(rec.forensics.session_id.clone())
                .push_bind(rec.forensics.error_type.clone())
                .push_bind(
                    rec.forensics
                        .error_message
                        .as_deref()
                        .map(|m| super::usage::head_chars(m, super::usage::ERROR_MESSAGE_MAX)),
                )
                .push_bind(rec.forensics.third_party as i64)
                .push_bind(rec.forensics.rewrites.clone())
                .push_bind(rec.forensics.device_id_out.clone())
                .push_bind(
                    rec.forensics
                        .response_excerpt
                        .as_deref()
                        .map(|m| super::usage::head_chars(m, super::usage::RESPONSE_EXCERPT_MAX)),
                )
                .push_bind(rec.forensics.sim_reason.clone())
                .push_bind(rec.forensics.session_key.clone())
                .push_bind(rec.forensics.session_id_in.clone())
                .push_bind(rec.forensics.inbound_headers.clone())
                .push_bind(rec.forensics.inbound_shape.clone())
                .push_bind(rec.message_id.clone());
        });
        q.build().execute(&mut *conn).await?;
    }
    Ok(())
}

/// 费用汇总（[`super::billing`]）：按（小时、号主、号、接入 Key、模型）先合并再 upsert。分组同逐条写，
/// 在 SQL 里按（号、Key）现取。`key_id` 为空记 0（逐条写也是 `COALESCE($4, 0)`；Key 从 1 编号，
/// 0 与空在取分组时同样落空）。
async fn billing_rows(conn: &mut PgConnection, billed: &[(&UsageRecord, i64, i64)]) -> Result<()> {
    #[derive(Default)]
    struct Sum {
        requests: i64,
        input: i64,
        output: i64,
        cache_write: i64,
        cache_1h: i64,
        cache_read: i64,
        cost: f64,
    }
    let mut cells: BTreeMap<(i64, i64, i64, i64, String), Sum> = BTreeMap::new();
    for (rec, ts, owner) in billed {
        let Some(cred_id) = rec.cred_id else { continue };
        let key = (
            (ts / BILLING_BUCKET_SECS) * BILLING_BUCKET_SECS,
            *owner,
            cred_id,
            rec.key_id.unwrap_or(0),
            rec.model.clone().unwrap_or_default(),
        );
        let s = cells.entry(key).or_default();
        s.requests += 1;
        s.input += rec.input_tokens.unwrap_or(0);
        s.output += rec.output_tokens.unwrap_or(0);
        s.cache_write += rec
            .cache_creation_tokens
            .unwrap_or(rec.cache_5m_tokens.unwrap_or(0) + rec.cache_1h_tokens.unwrap_or(0));
        s.cache_1h += rec.cache_1h_tokens.unwrap_or(0);
        s.cache_read += rec.cache_read_tokens.unwrap_or(0);
        s.cost += rec.cost_usd.unwrap_or(0.0);
    }
    if cells.is_empty() {
        return Ok(());
    }
    // 整数列依次是：小时、号主、号、Key、请求数、输入、输出、缓存写、其中 1h、缓存读。
    let mut ints: [Vec<i64>; 10] = Default::default();
    let mut models: Vec<String> = Vec::new();
    let mut costs: Vec<f64> = Vec::new();
    for ((hour, owner, cred, key, model), s) in cells {
        let row = [
            hour,
            owner,
            cred,
            key,
            s.requests,
            s.input,
            s.output,
            s.cache_write,
            s.cache_1h,
            s.cache_read,
        ];
        for (col, v) in ints.iter_mut().zip(row) {
            col.push(v);
        }
        models.push(model);
        costs.push(s.cost);
    }
    sqlx::query(
        "INSERT INTO billing_hourly AS b
             (hour, owner_id, cred_id, key_id, group_id, model, requests, input_tokens,
              output_tokens, cache_write_tokens, cache_write_1h_tokens, cache_read_tokens,
              cost_usd)
         SELECT t.hour, t.owner, t.cred, t.key,
                COALESCE(
                    (SELECT kg.group_id FROM api_key_groups kg
                       JOIN credential_groups cg ON cg.group_id = kg.group_id AND cg.cred_id = t.cred
                      WHERE kg.key_id = t.key ORDER BY kg.ord LIMIT 1),
                    (SELECT MIN(group_id) FROM credential_groups WHERE cred_id = t.cred),
                    0),
                t.model, t.requests, t.input, t.output, t.cache_write, t.cache_1h, t.cache_read,
                t.cost
           FROM unnest($1::BIGINT[], $2::BIGINT[], $3::BIGINT[], $4::BIGINT[], $5::TEXT[],
                       $6::BIGINT[], $7::BIGINT[], $8::BIGINT[], $9::BIGINT[], $10::BIGINT[],
                       $11::BIGINT[], $12::FLOAT8[])
                WITH ORDINALITY AS t(hour, owner, cred, key, model, requests, input, output,
                                     cache_write, cache_1h, cache_read, cost, ord)
          ORDER BY t.ord
         ON CONFLICT (hour, owner_id, cred_id, key_id, group_id, model) DO UPDATE SET
             requests = b.requests + excluded.requests,
             input_tokens = b.input_tokens + excluded.input_tokens,
             output_tokens = b.output_tokens + excluded.output_tokens,
             cache_write_tokens = b.cache_write_tokens + excluded.cache_write_tokens,
             cache_write_1h_tokens = b.cache_write_1h_tokens + excluded.cache_write_1h_tokens,
             cache_read_tokens = b.cache_read_tokens + excluded.cache_read_tokens,
             cost_usd = b.cost_usd + excluded.cost_usd",
    )
    .bind(&ints[0])
    .bind(&ints[1])
    .bind(&ints[2])
    .bind(&ints[3])
    .bind(&models)
    .bind(&ints[4])
    .bind(&ints[5])
    .bind(&ints[6])
    .bind(&ints[7])
    .bind(&ints[8])
    .bind(&ints[9])
    .bind(&costs)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

/// 账本：设备账本按（设备、号）合并；号账本里最近使用取这批最后一条、费用累加，额度快照取这批最后
/// 一条带窗口级限流信息的（判据同逐条写），与逐条依次写下去的结果相同。
async fn ledger_rows(conn: &mut PgConnection, billed: &[(&UsageRecord, i64, i64)]) -> Result<()> {
    let mut devices: BTreeMap<(String, i64), (f64, i64)> = BTreeMap::new();
    struct Cred<'a> {
        last_used: i64,
        cost: f64,
        snapshot: Option<(i64, &'a UsageRecord)>,
    }
    let mut creds: BTreeMap<i64, Cred<'_>> = BTreeMap::new();
    for (rec, ts, _) in billed {
        let Some(cid) = rec.cred_id else { continue };
        let cost = rec.cost_usd.unwrap_or(0.0);
        if let Some(dev) = &rec.device_id {
            let d = devices.entry((dev.clone(), cid)).or_default();
            d.0 += cost;
            d.1 += 1;
        }
        let c = creds.entry(cid).or_insert(Cred { last_used: *ts, cost: 0.0, snapshot: None });
        c.last_used = *ts;
        c.cost += cost;
        if rec.rl_5h_utilization.is_some()
            || rec.rl_7d_utilization.is_some()
            || !rec.windows.is_empty()
        {
            c.snapshot = Some((*ts, rec));
        }
    }
    if !devices.is_empty() {
        let mut cols: (Vec<String>, Vec<i64>, Vec<f64>, Vec<i64>) = Default::default();
        for ((dev, cid), (cost, n)) in devices {
            cols.0.push(dev);
            cols.1.push(cid);
            cols.2.push(cost);
            cols.3.push(n);
        }
        sqlx::query(
            "INSERT INTO device_costs AS d (device_id, cred_id, cost_usd, request_count)
             SELECT * FROM unnest($1::TEXT[], $2::BIGINT[], $3::FLOAT8[], $4::BIGINT[])
             ON CONFLICT (device_id, cred_id) DO UPDATE
                    SET cost_usd = d.cost_usd + excluded.cost_usd,
                        request_count = d.request_count + excluded.request_count",
        )
        .bind(&cols.0)
        .bind(&cols.1)
        .bind(&cols.2)
        .bind(&cols.3)
        .execute(&mut *conn)
        .await?;
    }
    let (plain, snap): (Vec<_>, Vec<_>) =
        creds.into_iter().partition(|(_, c)| c.snapshot.is_none());
    if !plain.is_empty() {
        let ids: Vec<i64> = plain.iter().map(|(id, _)| *id).collect();
        let last: Vec<i64> = plain.iter().map(|(_, c)| c.last_used).collect();
        let cost: Vec<f64> = plain.iter().map(|(_, c)| c.cost).collect();
        sqlx::query(
            "INSERT INTO credential_stats AS s (cred_id, last_used_at, cost_total_usd)
             SELECT * FROM unnest($1::BIGINT[], $2::BIGINT[], $3::FLOAT8[])
             ON CONFLICT (cred_id) DO UPDATE SET
                 last_used_at   = excluded.last_used_at,
                 cost_total_usd = s.cost_total_usd + excluded.cost_total_usd",
        )
        .bind(&ids)
        .bind(&last)
        .bind(&cost)
        .execute(&mut *conn)
        .await?;
    }
    if !snap.is_empty() {
        let mut ids = Vec::new();
        let mut last = Vec::new();
        let mut cost = Vec::new();
        let mut snap_ts = Vec::new();
        let mut unified: Vec<Option<String>> = Vec::new();
        let mut u5: Vec<Option<f64>> = Vec::new();
        let mut r5: Vec<Option<i64>> = Vec::new();
        let mut u7: Vec<Option<f64>> = Vec::new();
        let mut r7: Vec<Option<i64>> = Vec::new();
        let mut rep: Vec<Option<String>> = Vec::new();
        let mut over: Vec<Option<i64>> = Vec::new();
        let mut windows: Vec<Option<String>> = Vec::new();
        for (id, c) in &snap {
            let Some((ts, rec)) = c.snapshot else { continue };
            ids.push(*id);
            last.push(c.last_used);
            cost.push(c.cost);
            snap_ts.push(ts);
            unified.push(rec.unified_status.clone());
            u5.push(rec.rl_5h_utilization);
            r5.push(rec.rl_5h_reset);
            u7.push(rec.rl_7d_utilization);
            r7.push(rec.rl_7d_reset);
            rep.push(rec.rl_representative.clone());
            over.push(rec.rl_overage_in_use.map(i64::from));
            // 同逐条写：序列化失败只是少存这一列。
            windows.push(serde_json::to_string(&rec.windows).ok());
        }
        sqlx::query(
            "INSERT INTO credential_stats AS s
                 (cred_id, last_used_at, cost_total_usd, snapshot_ts, unified_status,
                  rl_5h_utilization, rl_5h_reset, rl_7d_utilization, rl_7d_reset, rl_representative,
                  overage_in_use, windows)
             SELECT * FROM unnest($1::BIGINT[], $2::BIGINT[], $3::FLOAT8[], $4::BIGINT[],
                                  $5::TEXT[], $6::FLOAT8[], $7::BIGINT[], $8::FLOAT8[],
                                  $9::BIGINT[], $10::TEXT[], $11::BIGINT[], $12::TEXT[])
             ON CONFLICT (cred_id) DO UPDATE SET
                 last_used_at      = excluded.last_used_at,
                 cost_total_usd    = s.cost_total_usd + excluded.cost_total_usd,
                 snapshot_ts       = excluded.snapshot_ts,
                 unified_status    = excluded.unified_status,
                 rl_5h_utilization = excluded.rl_5h_utilization,
                 rl_5h_reset       = excluded.rl_5h_reset,
                 rl_7d_utilization = excluded.rl_7d_utilization,
                 rl_7d_reset       = excluded.rl_7d_reset,
                 rl_representative = excluded.rl_representative,
                 overage_in_use    = excluded.overage_in_use,
                 windows           = excluded.windows",
        )
        .bind(&ids)
        .bind(&last)
        .bind(&cost)
        .bind(&snap_ts)
        .bind(&unified)
        .bind(&u5)
        .bind(&r5)
        .bind(&u7)
        .bind(&r7)
        .bind(&rep)
        .bind(&over)
        .bind(&windows)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// 预聚合（[`super::rollup`]）：按（维度、桶、键）把这批的贡献先合起来，一条 upsert 写完，再把 TTFT
/// 直方图与库里旧的合并回写（同逐条写的两步）。格子的名字取这一格这批里第一条的（新建时才写）。
async fn rollup_rows(
    conn: &mut PgConnection,
    cleaned: &[Cow<'_, UsageRecord>],
    usage: &[(&UsageRecord, i64)],
) -> Result<()> {
    let mut cells: BTreeMap<(&'static str, i64, String), (String, RollupAgg)> = BTreeMap::new();
    for (rec, (_, ts)) in cleaned.iter().zip(usage) {
        let delta = RollupAgg::of(RollupInput {
            status: i64::from(rec.status),
            ttft_ms: rec.ttft_ms,
            total_ms: rec.total_ms,
            output_tokens: rec.output_tokens,
            input_tokens: rec.input_tokens,
            cache_read_tokens: rec.cache_read_tokens,
            cache_creation_tokens: rec.cache_creation_tokens,
            cache_5m_tokens: rec.cache_5m_tokens,
            cache_1h_tokens: rec.cache_1h_tokens,
            cost_usd: rec.cost_usd,
        });
        let bucket = rollup_bucket(*ts);
        for (dim, key) in keys(rec.model.as_deref(), rec.cred_id) {
            let label = if dim == super::rollup::RollupDim::Cred {
                rec.cred_label.clone()
            } else {
                String::new()
            };
            cells
                .entry((dim.tag(), bucket, key))
                .or_insert_with(|| (label, RollupAgg::default()))
                .1
                .add(&delta);
        }
    }
    if cells.is_empty() {
        return Ok(());
    }
    let mut dims = Vec::new();
    let mut buckets = Vec::new();
    let mut ckeys = Vec::new();
    let mut labels = Vec::new();
    let mut n: [Vec<i64>; 10] = Default::default();
    let mut cost: Vec<f64> = Vec::new();
    for ((dim, bucket, key), (label, a)) in &cells {
        dims.push(*dim);
        buckets.push(*bucket);
        ckeys.push(key.as_str());
        labels.push(label.as_str());
        for (col, v) in n.iter_mut().zip([
            a.requests,
            a.input_tokens,
            a.cached_tokens,
            a.written_tokens,
            a.written_1h_tokens,
            a.output_tokens,
            a.lat_count,
            a.ttft_sum,
            a.gen_tokens,
            a.gen_ms,
        ]) {
            col.push(v);
        }
        cost.push(a.cost_usd);
    }
    let old: Vec<(String, i64, String, Option<Vec<u8>>)> = sqlx::query_as(
        "INSERT INTO usage_rollup AS r
             (dim, bucket, key, label, requests, input_tokens, cached_tokens, written_tokens,
              written_1h_tokens, output_tokens, lat_count, ttft_sum, gen_tokens, gen_ms, cost_usd)
         SELECT t.dim, t.bucket, t.key, t.label, t.requests, t.input, t.cached, t.written,
                t.written_1h, t.output, t.lat_count, t.ttft_sum, t.gen_tokens, t.gen_ms, t.cost
           FROM unnest($1::TEXT[], $2::BIGINT[], $3::TEXT[], $4::TEXT[], $5::BIGINT[],
                       $6::BIGINT[], $7::BIGINT[], $8::BIGINT[], $9::BIGINT[], $10::BIGINT[],
                       $11::BIGINT[], $12::BIGINT[], $13::BIGINT[], $14::BIGINT[], $15::FLOAT8[])
                WITH ORDINALITY AS t(dim, bucket, key, label, requests, input, cached, written,
                                     written_1h, output, lat_count, ttft_sum, gen_tokens, gen_ms,
                                     cost, ord)
          ORDER BY t.ord
         ON CONFLICT (dim, bucket, key) DO UPDATE SET
             requests = r.requests + excluded.requests,
             input_tokens = r.input_tokens + excluded.input_tokens,
             cached_tokens = r.cached_tokens + excluded.cached_tokens,
             written_tokens = r.written_tokens + excluded.written_tokens,
             written_1h_tokens = r.written_1h_tokens + excluded.written_1h_tokens,
             output_tokens = r.output_tokens + excluded.output_tokens,
             cost_usd = r.cost_usd + excluded.cost_usd,
             lat_count = r.lat_count + excluded.lat_count,
             ttft_sum = r.ttft_sum + excluded.ttft_sum,
             gen_tokens = r.gen_tokens + excluded.gen_tokens,
             gen_ms = r.gen_ms + excluded.gen_ms
         RETURNING dim, bucket, key, ttft_hist",
    )
    .bind(&dims)
    .bind(&buckets)
    .bind(&ckeys)
    .bind(&labels)
    .bind(&n[0])
    .bind(&n[1])
    .bind(&n[2])
    .bind(&n[3])
    .bind(&n[4])
    .bind(&n[5])
    .bind(&n[6])
    .bind(&n[7])
    .bind(&n[8])
    .bind(&n[9])
    .bind(&cost)
    .fetch_all(&mut *conn)
    .await?;
    let mut h_dims = Vec::new();
    let mut h_buckets = Vec::new();
    let mut h_keys = Vec::new();
    let mut hists = Vec::new();
    for (dim, bucket, key, old_hist) in old {
        let Some((_, (_, agg))) =
            cells.iter().find(|((d, b, k), _)| *d == dim && *b == bucket && *k == key)
        else {
            continue;
        };
        if agg.ttft.count() == 0 {
            continue;
        }
        let mut hist = agg.ttft.clone();
        // 坏数据当成空直方图，同逐条写。
        if let Some(old) = old_hist.as_deref().and_then(super::rollup::LatencyHist::decode) {
            hist.merge(&old);
        }
        h_dims.push(dim);
        h_buckets.push(bucket);
        h_keys.push(key);
        hists.push(hist.encode());
    }
    if !hists.is_empty() {
        sqlx::query(
            "UPDATE usage_rollup r SET ttft_hist = t.hist
               FROM unnest($1::TEXT[], $2::BIGINT[], $3::TEXT[], $4::BYTEA[]) AS t(dim, bucket, key, hist)
              WHERE r.dim = t.dim AND r.bucket = t.bucket AND r.key = t.key",
        )
        .bind(&h_dims)
        .bind(&h_buckets)
        .bind(&h_keys)
        .bind(&hists)
        .execute(&mut *conn)
        .await?;
    }
    Ok(())
}

/// thinking 登记：签发号已删的那笔整笔跳过（`owners` 里没有它），余下同一指纹先到的那笔为准
/// （`ON CONFLICT DO NOTHING` 逐条写时也是先写的留下），同逐条写。号行已在本事务开头锁住，外键不会
/// 撞上并发删号。
async fn thinking_rows(
    conn: &mut PgConnection,
    thinking: &[(Option<i64>, &[String], i64)],
    owners: &HashMap<i64, i64>,
) -> Result<()> {
    let mut seen = std::collections::HashSet::new();
    let mut fps: Vec<&str> = Vec::new();
    let mut creds: Vec<Option<i64>> = Vec::new();
    let mut tss: Vec<i64> = Vec::new();
    for (cred_id, list, ts) in thinking {
        if cred_id.is_some_and(|id| !owners.contains_key(&id)) {
            continue;
        }
        for fp in list.iter() {
            if seen.insert(fp.as_str()) {
                fps.push(fp);
                creds.push(*cred_id);
                tss.push(*ts);
            }
        }
    }
    if fps.is_empty() {
        return Ok(());
    }
    sqlx::query(
        "INSERT INTO thinking_blocks (fp, cred_id, ts)
         SELECT * FROM unnest($1::TEXT[], $2::BIGINT[], $3::BIGINT[])
         ON CONFLICT (fp) DO NOTHING",
    )
    .bind(&fps)
    .bind(&creds)
    .bind(&tss)
    .execute(&mut *conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{Forensics, QuotaWindow};
    use serde_json::Value;

    /// 一组覆盖各种形态的写入：跨小时 / 跨 15 分钟桶、带与不带号 / 设备 / Key / 快照 / 工具集 /
    /// NUL / 超长文案 / TTFT，号已删的，以及几笔 thinking 登记（含重复指纹、签发号已删）。
    type Workload = (Vec<(UsageRecord, i64)>, Vec<(Option<i64>, Vec<String>)>);

    fn workload(live: [i64; 2], gone: i64) -> Workload {
        let base = 1_790_000_000 / 3600 * 3600 + 3000;
        let models = ["claude-opus-5", "claude-sonnet-5", "claude-haiku-5-5"];
        let mut usage = Vec::new();
        for i in 0..60i64 {
            let cred = match i % 7 {
                0 => None,
                1 => Some(gone),
                n => Some(live[(n % 2) as usize]),
            };
            let snap = i % 5 == 0;
            let shape = match i % 4 {
                0 => Some(format!(
                    r#"{{"messages":{i},"tools":{{"sha":"sha{}","names":["Bash","Read{}"]}}}}"#,
                    i % 3,
                    i % 3
                )),
                1 => Some(r#"{"messages":1}"#.to_string()),
                _ => None,
            };
            usage.push((
                UsageRecord {
                    cred_id: cred,
                    key_id: (i % 3 != 0).then_some(i % 2 + 5),
                    cred_label: format!("acct{}\0", i % 3),
                    device_id: (i % 6 != 0).then(|| format!("dev{}", i % 4)),
                    model: (i % 11 != 0).then(|| models[(i % 3) as usize].to_string()),
                    path: "/v1/messages".into(),
                    ua: Some("claude-cli/2.1.318".into()),
                    status: if i % 9 == 0 { 429 } else { 200 },
                    has_usage: i % 9 != 0,
                    input_tokens: Some(100 + i),
                    output_tokens: (i % 8 != 0).then_some(10 * i),
                    cache_creation_tokens: (i % 3 == 0).then_some(1000 + i),
                    cache_5m_tokens: Some(i),
                    cache_1h_tokens: (i % 2 == 0).then_some(2 * i),
                    cache_read_tokens: Some(5000 - i),
                    ttft_ms: (i % 4 != 3).then_some(300 + 37 * i),
                    total_ms: Some(2000 + 101 * i),
                    unified_status: snap.then(|| "allowed".into()),
                    rl_5h_utilization: (snap && i % 2 == 0).then_some(0.1 + i as f64 / 100.0),
                    rl_5h_reset: snap.then_some(base + 9999),
                    rl_7d_utilization: snap.then_some(0.3),
                    rl_representative: snap.then(|| "five_hour".into()),
                    rl_overage_in_use: snap.then_some(i % 2 == 0),
                    windows: if snap && i % 3 == 0 {
                        vec![QuotaWindow {
                            name: "7d_oi".into(),
                            status: Some("allowed".into()),
                            utilization: Some(0.5),
                            reset: Some(base + 1),
                        }]
                    } else {
                        Vec::new()
                    },
                    cost_usd: (i % 10 != 0).then_some(0.0123 * i as f64 + 0.1),
                    request_id: Some(format!("req_{i}")),
                    message_id: Some(format!("msg_{i}")),
                    forensics: Forensics {
                        shape,
                        simulated: i % 2 == 0,
                        error_message: (i % 9 == 0).then(|| "e".repeat(5000)),
                        response_excerpt: (i % 13 == 0).then(|| "r\0x".repeat(3000)),
                        session_id: Some(format!("s{}", i % 5)),
                        ..Default::default()
                    },
                    ..Default::default()
                },
                // 跨过一个小时、几个 15 分钟桶。
                base + i * 23,
            ));
        }
        let thinking = vec![
            (Some(live[0]), vec!["fa".into(), "fb".into()]),
            (None, vec!["fb".into(), "fc".into()]),
            (Some(gone), vec!["fd".into(), "fe".into()]),
            (Some(live[1]), vec!["fd".into(), "fa".into(), "ff".into()]),
        ];
        (usage, thinking)
    }

    /// 写完之后几张表的全部内容（按主键排好，去掉自增 id 与 thinking 的登记时刻）。
    async fn dump(pool: &sqlx::PgPool) -> Vec<(String, Vec<Value>)> {
        let mut out = Vec::new();
        for (table, sql) in [
            ("usage_logs", "SELECT to_jsonb(t) - 'id' FROM usage_logs t ORDER BY id"),
            (
                "billing_hourly",
                "SELECT to_jsonb(t) FROM billing_hourly t ORDER BY hour, owner_id, cred_id, key_id, group_id, model",
            ),
            ("credential_stats", "SELECT to_jsonb(t) FROM credential_stats t ORDER BY cred_id"),
            ("device_costs", "SELECT to_jsonb(t) FROM device_costs t ORDER BY device_id, cred_id"),
            ("usage_rollup", "SELECT to_jsonb(t) FROM usage_rollup t ORDER BY dim, bucket, key"),
            // `created_at` 是写入时的库时间（`unixepoch()`），逐条与批量两遍跨了秒就差 1，同 `thinking_blocks` 的 `ts` 不比。
            ("tool_sets", "SELECT to_jsonb(t) - 'created_at' FROM tool_sets t ORDER BY sha"),
            ("thinking_blocks", "SELECT to_jsonb(t) - 'ts' FROM thinking_blocks t ORDER BY fp"),
        ] {
            let rows: Vec<(Value,)> = sqlx::query_as(sql).fetch_all(pool).await.unwrap();
            out.push((table.to_string(), rows.into_iter().map(|r| r.0).collect()));
        }
        out
    }

    async fn clear(pool: &sqlx::PgPool) {
        for sql in [
            "TRUNCATE usage_logs, billing_hourly, device_costs, usage_rollup, tool_sets, thinking_blocks",
            "DELETE FROM credential_stats",
        ] {
            sqlx::query(sql).execute(pool).await.unwrap();
        }
    }

    /// 数相同（浮点按相对 1e-9：费用先在批内加再累进库，与逐条累进只差舍入）。
    fn same(a: &Value, b: &Value) -> bool {
        match (a, b) {
            (Value::Number(x), Value::Number(y)) => match (x.as_f64(), y.as_f64()) {
                (Some(x), Some(y)) if x.fract() != 0.0 || y.fract() != 0.0 => {
                    (x - y).abs() <= 1e-9 * x.abs().max(y.abs()).max(1.0)
                }
                _ => x == y,
            },
            (Value::Object(x), Value::Object(y)) => {
                x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same(v, w)))
            }
            (Value::Array(x), Value::Array(y)) => {
                x.len() == y.len() && x.iter().zip(y).all(|(v, w)| same(v, w))
            }
            _ => a == b,
        }
    }

    fn assert_same(one: &[(String, Vec<Value>)], batched: &[(String, Vec<Value>)], what: &str) {
        for ((table, a), (_, b)) in one.iter().zip(batched) {
            assert_eq!(a.len(), b.len(), "{what}: {table} 行数");
            for (x, y) in a.iter().zip(b) {
                assert!(same(x, y), "{what}: {table}\n逐条 {x}\n批量 {y}");
            }
        }
    }

    /// 批量写（整批一次、或切成几批）与逐条写（[`CredentialStore::insert_usage_log_at`] /
    /// [`CredentialStore::remember_thinking_blocks`]）落库后七张表逐行相同。
    #[sqlx::test]
    async fn batched_writes_match_one_by_one(pool: sqlx::PgPool) {
        let store = CredentialStore::for_test(pool.clone()).await;
        let a = store.insert("a", None, "a", "ra", 0, None, None, 1).await.unwrap().id;
        let b = store.insert("b", None, "b", "rb", 0, None, None, 1).await.unwrap().id;
        let gone = store.insert("gone", None, "g", "rg", 0, None, None, 1).await.unwrap().id;
        store.remove(&[gone]).await.unwrap();
        let (usage, thinking) = workload([a, b], gone);
        assert!(usage.iter().any(|(r, _)| r.cred_id == Some(gone)));

        // 逐条：thinking 夹在流水之间，按入队顺序。
        clear(&pool).await;
        let one = CredentialStore::for_test(pool.clone()).await;
        for (rec, ts) in &usage {
            one.insert_usage_log_at(rec, Some(*ts)).await.unwrap();
        }
        for (cred, fps) in &thinking {
            one.remember_thinking_blocks(*cred, fps).await.unwrap();
        }
        let expect = dump(&pool).await;
        assert!(
            expect.iter().all(|(t, rows)| !rows.is_empty() || t == "device_costs"),
            "{expect:?}"
        );

        for chunk in [usage.len(), 7, 1] {
            clear(&pool).await;
            // 新开一个 store：已知工具集在进程内，别让逐条那遍的记忆省掉批量这遍的 tool_sets 写入。
            let batched = CredentialStore::for_test(pool.clone()).await;
            // 走入队时的预截断（错误文案、响应开头），落库结果应与逐条写在落库时截的相同。
            let trimmed: Vec<(UsageRecord, i64)> = usage
                .iter()
                .map(|(r, ts)| {
                    let mut r = r.clone();
                    trim_for_queue(&mut r);
                    (r, *ts)
                })
                .collect();
            let u: Vec<(&UsageRecord, i64)> = trimmed.iter().map(|(r, ts)| (r, *ts)).collect();
            let t: Vec<(Option<i64>, &[String], i64)> =
                thinking.iter().map(|(c, f)| (*c, f.as_slice(), 0)).collect();
            for (i, part) in u.chunks(chunk).enumerate() {
                flush(&batched, part, if i == 0 { &t } else { &[] })
                    .await
                    .map_err(|e| e.err)
                    .unwrap();
            }
            assert_same(&expect, &dump(&pool).await, &format!("每批 {chunk} 条"));
        }
    }

    /// 入队的流水与 thinking 登记经写入任务落库；`drain_writes` 不必等满攒批时间。
    #[sqlx::test]
    async fn queued_writes_land_and_drain_does_not_wait_for_the_linger(pool: sqlx::PgPool) {
        let store = Arc::new(CredentialStore::for_test(pool.clone()).await);
        let cred = store.insert("a", None, "a", "r", 0, None, None, 1).await.unwrap().id;
        for i in 0..5 {
            store.queue_usage_log(UsageRecord {
                cred_id: Some(cred),
                path: "/v1/messages".into(),
                status: 200,
                cost_usd: Some(0.5),
                request_id: Some(format!("req_{i}")),
                ..Default::default()
            });
        }
        store.queue_thinking_blocks(Some(cred), vec!["fp1".into()]);
        let t0 = std::time::Instant::now();
        store.drain_writes(Duration::from_secs(5)).await;
        assert!(t0.elapsed() < LINGER, "{:?}", t0.elapsed());
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM usage_logs").fetch_one(&pool).await.unwrap();
        assert_eq!(n, 5);
        let cost: f64 =
            sqlx::query_scalar("SELECT cost_total_usd FROM credential_stats WHERE cred_id = $1")
                .bind(cred)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert!((cost - 2.5).abs() < 1e-9);
        let fp: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM thinking_blocks WHERE fp = 'fp1'")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(fp, 1);
    }

    fn rec(i: i64, cred: i64) -> UsageRecord {
        UsageRecord {
            cred_id: Some(cred),
            path: "/v1/messages".into(),
            status: 200,
            cost_usd: Some(1.0),
            request_id: Some(format!("req_{i}")),
            ..Default::default()
        }
    }

    async fn counts(pool: &sqlx::PgPool, cred: i64) -> (i64, f64) {
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM usage_logs").fetch_one(pool).await.unwrap();
        let cost: f64 =
            sqlx::query_scalar("SELECT cost_total_usd FROM credential_stats WHERE cred_id = $1")
                .bind(cred)
                .fetch_one(pool)
                .await
                .unwrap();
        (n, cost)
    }

    fn io_error() -> anyhow::Error {
        sqlx::Error::Io(std::io::Error::other("connection reset")).into()
    }

    /// 提交那一步报错、但服务端其实已经提交了：按事务号查到已提交就算完，不再重写（不记两遍账）；
    /// 真没提交（回滚了）的照常重写一遍。
    #[sqlx::test]
    async fn an_uncertain_commit_is_checked_not_rewritten(pool: sqlx::PgPool) {
        let store = CredentialStore::for_test(pool.clone()).await;
        let cred = store.insert("a", None, "a", "r", 0, None, None, 1).await.unwrap().id;
        let recs: Vec<UsageRecord> = (0..5).map(|i| rec(i, cred)).collect();
        let u: Vec<(&UsageRecord, i64)> = recs.iter().map(|r| (r, 1_790_000_000)).collect();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        for landed in [true, false] {
            clear(&pool).await;
            calls.store(0, Ordering::SeqCst);
            let (store, u, calls) = (&store, &u, &calls);
            settle(store, u, &[], move || async move {
                if calls.fetch_add(1, Ordering::SeqCst) > 0 {
                    return flush(store, u, &[]).await;
                }
                let mut xid = None;
                let tx = write_tx(store, u, &[], &mut xid).await.unwrap();
                if landed {
                    tx.commit().await.unwrap();
                } else {
                    tx.rollback().await.unwrap();
                }
                Err(FlushError { at_commit: true, xid, err: io_error() })
            })
            .await;
            assert_eq!(counts(&pool, cred).await, (5, 5.0), "已提交 = {landed}");
            assert_eq!(calls.load(Ordering::SeqCst), if landed { 1 } else { 2 });
        }
    }

    /// 临时故障整批退避重试，不拆成逐条；数据问题才逐条重写。
    #[sqlx::test]
    async fn transient_failures_retry_the_whole_batch(pool: sqlx::PgPool) {
        let store = CredentialStore::for_test(pool.clone()).await;
        let cred = store.insert("a", None, "a", "r", 0, None, None, 1).await.unwrap().id;
        let recs: Vec<UsageRecord> = (0..5).map(|i| rec(i, cred)).collect();
        let u: Vec<(&UsageRecord, i64)> = recs.iter().map(|r| (r, 1_790_000_000)).collect();
        let calls = std::sync::atomic::AtomicUsize::new(0);
        {
            let (store, u, calls) = (&store, &u, &calls);
            settle(store, u, &[], move || async move {
                match calls.fetch_add(1, Ordering::SeqCst) {
                    0 => Err(FlushError {
                        at_commit: false,
                        xid: None,
                        err: sqlx::Error::PoolTimedOut.into(),
                    }),
                    1 => Err(FlushError { at_commit: false, xid: None, err: io_error() }),
                    _ => flush(store, u, &[]).await,
                }
            })
            .await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 3, "两次临时故障后整批第三次写成");
        assert_eq!(counts(&pool, cred).await, (5, 5.0));

        // 数据问题：不重试整批，逐条写（这里逐条都写得进去）。
        clear(&pool).await;
        calls.store(0, Ordering::SeqCst);
        {
            let calls = &calls;
            settle(&store, &u, &[], move || async move {
                calls.fetch_add(1, Ordering::SeqCst);
                Err(FlushError { at_commit: false, xid: None, err: anyhow::anyhow!("bad row") })
            })
            .await;
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(counts(&pool, cred).await, (5, 5.0));
    }

    #[test]
    fn failures_are_classified_by_cause() {
        assert_eq!(classify(&sqlx::Error::PoolTimedOut.into()), Failure::Transient);
        assert_eq!(classify(&io_error()), Failure::Transient);
        assert_eq!(classify(&io_error().context("insert usage_logs")), Failure::Transient);
        assert_eq!(classify(&sqlx::Error::RowNotFound.into()), Failure::Data);
        assert_eq!(classify(&anyhow::anyhow!("something else")), Failure::Data);
    }

    /// 还在队列里的 thinking 登记，查签发者时看得见、也不会被当成外来；落库后待写表清掉，库里查得到。
    #[sqlx::test]
    async fn queued_thinking_blocks_are_visible_before_they_land(pool: sqlx::PgPool) {
        let store = Arc::new(CredentialStore::for_test(pool.clone()).await);
        let cred = store.insert("a", None, "a", "r", 0, None, None, 1).await.unwrap().id;
        store.queue_thinking_blocks(Some(cred), vec!["fp_new".into()]);
        let fps = vec!["fp_new".to_string(), "fp_unknown".to_string()];
        let before = store.thinking_block_issuers(&fps).await.unwrap();
        assert_eq!((before.issuers.clone(), before.known, before.total), (vec![Some(cred)], 1, 2));
        assert_eq!(before.known_fps, vec!["fp_new".to_string()]);
        assert_eq!(store.without_pending_signed(&fps), vec!["fp_unknown".to_string()]);

        store.drain_writes(Duration::from_secs(5)).await;
        assert!(store.pending_thinking.lock().is_empty(), "落库后从待写表里拿掉");
        let after = store.thinking_block_issuers(&fps).await.unwrap();
        assert_eq!(
            (after.issuers, after.known, after.known_fps),
            (before.issuers, 1, before.known_fps)
        );
    }

    /// 入队前收住大字段：错误文案 / 响应开头按落库上限截（与落库时截的结果相同，见
    /// `batched_writes_match_one_by_one`），来访摘要超了上限记成只带原长的占位。
    #[test]
    fn oversized_fields_are_trimmed_on_the_way_in() {
        let mut r = UsageRecord::default();
        r.forensics.inbound_shape = Some("x".repeat(INBOUND_FIELD_MAX + 1));
        r.forensics.inbound_headers = Some("{}".into());
        r.forensics.error_message = Some("é".repeat(ERROR_MESSAGE_MAX + 10));
        trim_for_queue(&mut r);
        assert_eq!(
            r.forensics.inbound_shape.as_deref(),
            Some(format!(r#"{{"truncated_bytes":{}}}"#, INBOUND_FIELD_MAX + 1).as_str())
        );
        assert_eq!(r.forensics.inbound_headers.as_deref(), Some("{}"));
        assert_eq!(r.forensics.error_message.as_ref().unwrap().chars().count(), ERROR_MESSAGE_MAX);
    }
}
