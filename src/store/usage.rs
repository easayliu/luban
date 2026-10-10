//! 用量流水：落库、翻页查询与裁剪。

use super::*;

/// 待写入的一条用量日志（代理层组装后交给 [`CredentialStore::insert_usage_log`]）。
#[derive(Debug, Default, Clone)]
pub struct UsageRecord {
    pub cred_id: Option<i64>,
    /// 来访用的哪把接入 Key（`None` = 环境变量那把，或一把都没配时放行）。只进费用汇总，
    /// 见 `billing` 模块。
    pub key_id: Option<i64>,
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
    /// 只看这个人名下的号的流水（代理和用户查请求时由接口强制带上，见 `web::usage`）；
    /// `None` 为不限。
    pub owner_id: Option<i64>,
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

/// 用量日志流水的保留时长：8 天。必须大于最长的统计窗口（7 天）：7d 窗口起点是 reset 往前推
/// 7 天，封号取证要回看 7 天加 10 分钟（[`FREEZE_WINDOW_SECS`]），正好 7 天会在边界上少算，
/// cost_7d 平白变小。再往前的流水没人看——终身口径都在账本里——留着只是让表、索引和每条
/// 按时间扫的查询跟着变大（线上 30 天时 180 万行、5GB 多）。
pub const USAGE_LOG_RETENTION_SECS: i64 = 8 * 24 * 3600;

/// `usage_logs` 与 `usage_logs_frozen` 共用的列清单（不含各自的主键）。**读与写都用它**：
/// 冻结是 `INSERT … SELECT` 逐列照搬，两张表的列必须一一对齐，清单只此一份才不会漂。
/// 顺序即 [`usage_log_from_row`] 的下标顺序（从 1 起，0 号是主键）。
pub(super) const USAGE_LOG_COLS: &str =
    "ts, cred_id, cred_label, device_id, model, path, status, has_usage,
        input_tokens, output_tokens, cache_creation_tokens, cache_5m_tokens,
        cache_1h_tokens, cache_read_tokens, ttft_ms, total_ms,
        unified_status, rl_5h_status, rl_5h_reset, rl_5h_utilization,
        rl_7d_status, rl_7d_reset, rl_7d_utilization, rl_representative, ratelimit_raw,
        cost_usd, rl_overage_in_use, ua, ua_out, sse_aggregated,
        request_id, upstream_request_id,
        proxy, simulated, shape, session_id, error_type, error_message, third_party, rewrites,
        device_id_out, response_excerpt, sim_reason, session_key, session_id_in";

/// 按字符截断（不是按字节：文案里有中文，按字节切会切在多字节中间）。
pub(super) fn head_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n).collect() }
}

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::Result;
use sqlx::postgres::{PgArguments, PgRow};
use sqlx::{Arguments, Row};

use super::FREEZE_TAIL_SECS;
use super::billing::billing_record;
use super::rollup::rollup_record;
use super::{CredentialStore, strip_nul};

/// 流水里全部 `Option<String>` 文本字段；第二个参数给 `mut` 时借成可写的。
macro_rules! opt_texts {
    ($r:ident $(, $m:tt)?) => {
        [
            &$($m)? $r.device_id,
            &$($m)? $r.model,
            &$($m)? $r.ua,
            &$($m)? $r.ua_out,
            &$($m)? $r.unified_status,
            &$($m)? $r.rl_5h_status,
            &$($m)? $r.rl_7d_status,
            &$($m)? $r.rl_representative,
            &$($m)? $r.ratelimit_raw,
            &$($m)? $r.request_id,
            &$($m)? $r.upstream_request_id,
            &$($m)? $r.forensics.proxy,
            &$($m)? $r.forensics.sim_reason,
            &$($m)? $r.forensics.shape,
            &$($m)? $r.forensics.session_id,
            &$($m)? $r.forensics.session_id_in,
            &$($m)? $r.forensics.session_key,
            &$($m)? $r.forensics.device_id_out,
            &$($m)? $r.forensics.error_type,
            &$($m)? $r.forensics.error_message,
            &$($m)? $r.forensics.rewrites,
            &$($m)? $r.forensics.response_excerpt,
        ]
    };
}

/// 流水里有文本字段带 NUL 时，回一份去掉 NUL 的拷贝（见 [`super::nul_free`]）；都不带回
/// `None`，不拷贝。限流窗口（`windows`）取自响应头，HTTP 头里本来就不会有 NUL，不查。
fn without_nul(rec: &UsageRecord) -> Option<UsageRecord> {
    let has = |s: &str| s.contains('\0');
    let dirty = opt_texts!(rec).into_iter().flatten().any(|s| has(s))
        || has(&rec.cred_label)
        || has(&rec.path);
    if !dirty {
        return None;
    }
    let mut out = rec.clone();
    let r = &mut out;
    for s in opt_texts!(r, mut).into_iter().flatten() {
        strip_nul(s);
    }
    strip_nul(&mut r.cred_label);
    strip_nul(&mut r.path);
    Some(out)
}

/// 往动态拼的参数表里追加一个值。
fn push<'q, T>(args: &mut PgArguments, v: T) -> Result<usize>
where
    T: 'q + sqlx::Encode<'q, sqlx::Postgres> + sqlx::Type<sqlx::Postgres>,
{
    args.add(v).map_err(anyhow::Error::from_boxed)?;
    Ok(args.len())
}

/// 把 [`UsageLogQuery`] 的筛选条件拼成 `WHERE …`（可能为空串）与对应的绑定参数，口径同
/// `UsageLogQuery::where_clause`。
///
/// **按条件动态拼而不是写 `($1 IS NULL OR col = $1)`**：那种写法规划器得为「参数可能是 NULL」
/// 留后路，按号翻页与按请求 id 查就用不上各自的索引了。统计与取页共用这一份，「共 N 条」与
/// 翻得到的记录永远是同一个集合。
pub(super) fn where_clause(q: &UsageLogQuery) -> Result<(String, PgArguments)> {
    let mut clauses: Vec<String> = Vec::new();
    let mut args = PgArguments::default();
    if let Some(c) = q.cred_id {
        let n = push(&mut args, c)?;
        clauses.push(format!("cred_id = ${n}"));
    }
    if let Some(o) = q.owner_id {
        let n = push(&mut args, o)?;
        clauses.push(format!("cred_id IN (SELECT id FROM credentials WHERE owner_id = ${n})"));
    }
    if let Some(u) = q.until_id {
        let n = push(&mut args, u)?;
        clauses.push(format!("id <= ${n}"));
    }
    if let Some(r) = q.request_id.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
        let n = push(&mut args, r.to_string())?;
        clauses.push(format!("request_id = ${n}"));
    }
    if let Some(m) = q.model.as_deref().map(str::trim).filter(|m| !m.is_empty()) {
        let n = push(&mut args, m.to_string())?;
        clauses.push(format!("model = ${n}"));
    }
    if let Some(s) = q.since {
        let n = push(&mut args, s)?;
        clauses.push(format!("ts >= ${n}"));
    }
    if let Some(k) = q.session_key.as_deref().map(str::trim).filter(|k| !k.is_empty()) {
        let n = push(&mut args, k.to_string())?;
        clauses.push(format!("session_key = ${n}"));
    }
    // 出站与来访任一命中。两个 OR 分支各有自己的部分索引，PG 走 BitmapOr，不会退成整表扫。
    if let Some(sid) = q.session_id.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        let n = push(&mut args, sid.to_string())?;
        clauses.push(format!("(session_id = ${n} OR session_id_in = ${n})"));
    }
    let sql = if clauses.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", clauses.join(" AND "))
    };
    Ok((sql, args))
}

/// 流水统计（条数、花费合计、最大 id）的 SQL，见 [`CredentialStore::usage_log_stats`]。
pub(super) fn usage_log_stats_sql(where_sql: &str) -> String {
    format!("SELECT COUNT(*), COALESCE(SUM(cost_usd), 0), MAX(id) FROM usage_logs{where_sql}")
}

/// 流水取页的 SQL，见 [`CredentialStore::query_usage_logs`]。`n` 是参数个数，最后两个是
/// LIMIT / OFFSET。
///
/// **分两步**：子查询只按筛选取出这一页的 id，外层再按 id 读整行。子查询只碰 id，各条筛选
/// 索引都能覆盖它；OFFSET 跳过的那些行只在索引里跳，不回表读整行。
pub(super) fn usage_log_page_sql(where_sql: &str, n: usize) -> String {
    format!(
        "SELECT id, {USAGE_LOG_COLS}
           FROM usage_logs
          WHERE id IN (SELECT id FROM usage_logs{where_sql}
                        ORDER BY id DESC LIMIT ${} OFFSET ${})
          ORDER BY id DESC",
        n - 1,
        n
    )
}

/// 写一条流水并顺手补冻结的那条 SQL，见 [`CredentialStore::insert_usage_log_at`]。
///
/// 写流水与「刚封的号补进冻结表」合成一条：CTE 里插流水、`RETURNING` 整行，外层按这一行的
/// 号与时刻找封后 [`FREEZE_TAIL_SECS`] 内最近的那条封号事件，有就把这一行照搬进冻结表。
static INSERT_USAGE_LOG_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        "WITH ins AS (
             INSERT INTO usage_logs
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
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                     $16, $17, $18, $19, $20, $21, $22, $23, $24, $25, $26, $27, $28, $29,
                     $30, $31, $32, $33, $34, $35, $36, $37, $38, $39, $40, $41, $42, $43, $44,
                     $45)
             RETURNING id, {USAGE_LOG_COLS})
         INSERT INTO usage_logs_frozen (ban_event_id, src_id, {USAGE_LOG_COLS})
         SELECT b.id, ins.id, {USAGE_LOG_COLS}
           FROM ins
           JOIN LATERAL (SELECT id FROM ban_events
                          WHERE cred_id = ins.cred_id AND ts >= ins.ts - $46
                          ORDER BY ts DESC, id DESC LIMIT 1) b ON TRUE"
    )
});

/// 落账（`credential_stats` 与 `device_costs`）的那条 SQL：不带额度快照的。设备账本在 CTE 里，
/// `$4`（device_id）为 NULL 时不记。
const LEDGER_SQL: &str = "WITH dev AS (
         INSERT INTO device_costs AS d (device_id, cred_id, cost_usd, request_count)
         SELECT $4, $1, COALESCE($3, 0), 1 WHERE $4::TEXT IS NOT NULL
         ON CONFLICT (device_id, cred_id) DO UPDATE
                SET cost_usd = d.cost_usd + excluded.cost_usd,
                    request_count = d.request_count + 1)
     INSERT INTO credential_stats AS s (cred_id, last_used_at, cost_total_usd)
     VALUES ($1, $2, COALESCE($3, 0))
     ON CONFLICT (cred_id) DO UPDATE SET
         last_used_at   = excluded.last_used_at,
         cost_total_usd = s.cost_total_usd + excluded.cost_total_usd";

/// 同上，带额度快照（`$5` 起），整份覆盖账本里的快照列。
const LEDGER_SNAPSHOT_SQL: &str = "WITH dev AS (
         INSERT INTO device_costs AS d (device_id, cred_id, cost_usd, request_count)
         SELECT $4, $1, COALESCE($3, 0), 1 WHERE $4::TEXT IS NOT NULL
         ON CONFLICT (device_id, cred_id) DO UPDATE
                SET cost_usd = d.cost_usd + excluded.cost_usd,
                    request_count = d.request_count + 1)
     INSERT INTO credential_stats AS s
         (cred_id, last_used_at, cost_total_usd, snapshot_ts, unified_status,
          rl_5h_utilization, rl_5h_reset, rl_7d_utilization, rl_7d_reset, rl_representative,
          overage_in_use, windows)
     VALUES ($1, $2, COALESCE($3, 0), $2, $5, $6, $7, $8, $9, $10, $11, $12)
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
         windows           = excluded.windows";

impl CredentialStore {
    /// 最近 `days` 天流水里出现过的模型（去重），附最后一次出现的时刻，按时刻倒序。
    /// 连通性测试自己打的那些（`device_id = 'probe'`）不算——它们是人挑的，不是客户端在用的。
    ///
    /// 不按 ts 扫窗口再 GROUP BY：那样要把窗口内的全部流水逐行回表读 model / device_id。改成在
    /// `idx_usage_logs_model_ts` 上跳着走（递归 CTE 模拟 loose index scan）：先逐个取下一个
    /// 不同的模型名，再对每个模型从最新一条往回找第一条非 probe 的——模型就十来个。
    pub async fn recent_models(&self, days: i64) -> Result<Vec<(String, i64)>> {
        Ok(sqlx::query_as(
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
                            AND u.ts >= unixepoch() - $1 * 86400
                            AND (u.device_id IS NULL OR u.device_id <> 'probe')) AS last_ts
                   FROM m WHERE model IS NOT NULL
             ) s
             WHERE last_ts IS NOT NULL
             ORDER BY last_ts DESC",
        )
        .bind(days)
        .fetch_all(&self.pool)
        .await?)
    }

    /// 写入一条用量日志。
    pub async fn insert_usage_log(&self, rec: &UsageRecord) -> Result<()> {
        self.insert_usage_log_at(rec, None).await
    }

    /// 写入一条用量日志，并在**同一事务**里把账本（credential_stats / device_costs）、预聚合
    /// （usage_rollup）与费用汇总（billing_hourly）记上。
    ///
    /// 账本承接三个终身口径：最近使用、累计费用、最新额度快照。流水只保留近期（见
    /// [`Self::prune_usage_logs`]），这些口径若继续从流水聚合，裁剪一跑数字就会跟着变小；写时
    /// 落账之后，读路径不再依赖流水的历史深度。同一事务保证几边不漂移。
    ///
    /// `ts` 为 `None` 时取库的时钟（`unixepoch()`，即这条语句开始的时刻）；拆出这个参数是给测试
    /// 用的——窗口/裁剪相关的用例需要指定「这条流水发生在何时」。
    ///
    /// 每条转发请求后都跑：普通事务（不拿全局写锁），语句能合的都合了。并发正确性靠行锁：
    ///
    /// - 第一步拿这个号的冻结锁（advisory 共享锁，键见 [`super::freeze_lock_key`]），同时给账号行
    ///   上 `FOR KEY SHARE`、顺手读出号主（费用汇总的号主就定在这一刻）。
    ///   - 冻结锁和别的流水写入不冲突，只和封号取证（`bans` 模块）的排它锁互斥：封号事件落地与这条
    ///     流水的写入一定分得出先后——先封的，这里后面的语句看得到那条事件、把这一行补进冻结表；
    ///     先写的，取证那边等它提交后再冻结、冻得到它。SQLite 版靠全局写锁串行化天然如此，PG 版
    ///     不加这一步，两边各看各的快照就会漏掉触发封号的那一发。用 advisory 锁而不是账号行的
    ///     行锁：号被删掉之后没有行可锁，删号时还在途的流水照样要和补做的取证分出先后。
    ///   - 账号行读不到说明号已被 [`Self::remove`] 删掉（删号时还在途的请求）：流水照记（删号本来
    ///     就保留历史流水），账本与费用汇总跳过，不把刚删掉的账本行又建回来、记出号主为 0 的费用。
    ///     `FOR KEY SHARE` 让删号等这条流水提交（删号先锁账号行再删账本，见 [`Self::remove`]）；
    /// - 账本、预聚合、费用汇总都是 `INSERT … ON CONFLICT DO UPDATE` 原地累加，并发写同一行
    ///   由行锁排队，不会丢；预聚合的直方图读改写见 [`rollup_record`]。
    pub(super) async fn insert_usage_log_at(
        &self,
        rec: &UsageRecord,
        ts: Option<i64>,
    ) -> Result<()> {
        let cleaned = without_nul(rec);
        let rec = cleaned.as_ref().unwrap_or(rec);
        let mut tx = self.pool.begin().await?;
        let (ts, owner) = if ts.is_some() && rec.cred_id.is_none() {
            (ts.unwrap_or_default(), None)
        } else {
            // 号主列为空时记 0（同 SQLite 版），这样 None 只表示「没有这一行」。
            // 先拿这个号的冻结锁（共享），见下面的文档；号为空时参数为 NULL，函数不加锁。
            let (now, owner): (i64, Option<i64>) = sqlx::query_as(
                "SELECT unixepoch(),
                        (SELECT COALESCE(owner_id, 0) FROM credentials WHERE id = $1 FOR KEY SHARE)
                   FROM (SELECT pg_advisory_xact_lock_shared($2)) freeze_lock",
            )
            .bind(rec.cred_id)
            .bind(rec.cred_id.map(super::freeze_lock_key))
            .fetch_one(&mut *tx)
            .await?;
            (ts.unwrap_or(now), owner)
        };
        // 工具名清单拆出去按 sha 存一份（见 [`split_tool_names`]），流水里只留 sha。
        let (shape, tool_set) = split_tool_names(rec.forensics.shape.as_deref());
        let new_tool_set = tool_set.filter(|(sha, _)| !self.known_tool_sets.lock().contains(sha));
        if let Some((sha, names)) = &new_tool_set {
            sqlx::query(
                "INSERT INTO tool_sets (sha, names) VALUES ($1, $2) ON CONFLICT DO NOTHING",
            )
            .bind(sha)
            .bind(names)
            .execute(&mut *tx)
            .await?;
        }
        // 刚封的号：封号事件落地时冻结的是**当时已有**的流水，而触发封号的那一发（以及同时
        // 在途的几发）要等响应流结束才落库，冻结时还不存在。故封后 FREEZE_TAIL_SECS 内到达
        // 的这个号的流水，写入时顺手补进冻结表——不然最要紧的那一条恰好缺席。
        sqlx::query(sqlx::AssertSqlSafe(INSERT_USAGE_LOG_SQL.as_str()))
            .bind(ts)
            .bind(rec.cred_id)
            .bind(&rec.cred_label)
            .bind(&rec.device_id)
            .bind(&rec.model)
            .bind(&rec.path)
            .bind(rec.status as i64)
            .bind(rec.has_usage as i64)
            .bind(rec.input_tokens)
            .bind(rec.output_tokens)
            .bind(rec.cache_creation_tokens)
            .bind(rec.cache_5m_tokens)
            .bind(rec.cache_1h_tokens)
            .bind(rec.cache_read_tokens)
            .bind(rec.ttft_ms)
            .bind(rec.total_ms)
            .bind(&rec.unified_status)
            .bind(&rec.rl_5h_status)
            .bind(rec.rl_5h_reset)
            .bind(rec.rl_5h_utilization)
            .bind(&rec.rl_7d_status)
            .bind(rec.rl_7d_reset)
            .bind(rec.rl_7d_utilization)
            .bind(&rec.rl_representative)
            .bind(rec.rl_overage_in_use.map(i64::from))
            .bind(&rec.ratelimit_raw)
            .bind(rec.cost_usd)
            .bind(&rec.ua)
            .bind(&rec.ua_out)
            .bind(rec.sse_aggregated as i64)
            .bind(&rec.request_id)
            .bind(&rec.upstream_request_id)
            .bind(&rec.forensics.proxy)
            .bind(rec.forensics.simulated as i64)
            .bind(shape.as_deref())
            .bind(&rec.forensics.session_id)
            .bind(&rec.forensics.error_type)
            .bind(rec.forensics.error_message.as_deref().map(|m| head_chars(m, ERROR_MESSAGE_MAX)))
            .bind(rec.forensics.third_party as i64)
            .bind(&rec.forensics.rewrites)
            .bind(&rec.forensics.device_id_out)
            .bind(
                rec.forensics
                    .response_excerpt
                    .as_deref()
                    .map(|m| head_chars(m, RESPONSE_EXCERPT_MAX)),
            )
            .bind(&rec.forensics.sim_reason)
            .bind(&rec.forensics.session_key)
            .bind(&rec.forensics.session_id_in)
            .bind(FREEZE_TAIL_SECS)
            .execute(&mut *tx)
            .await?;
        // 落账与费用汇总。cred_id 为空的流水（还没选到凭证就失败的请求）无处归属、号已删掉的
        // 也无处可记，都只记日志不记账。
        if let (Some(cid), Some(owner)) = (rec.cred_id, owner) {
            // 费用汇总：号主、分组、接入 Key 在这一刻定死，见 `billing` 模块。
            billing_record(&mut tx, ts, rec, owner).await?;
            // 快照只在响应带**窗口级**限流信息时覆盖，口径同旧版「最新一条带限流信息的行」
            // ——更晚的普通响应不能把快照抹掉。
            //
            // 判据里的 `!rec.windows.is_empty()` 不是冗余：一个只上报 `7d_oi` 之类窗口的账号
            // 若只认 5h/7d 两个专用字段就**永远写不进快照**。窗口种类是上游说了算的。
            //
            // 仍然不认「只有 unified_status / overage_in_use、一个窗口都没有」的响应：
            // 那种覆盖会把已有的窗口列一并抹成空，拿一条信息更少的快照换掉信息更多的。
            //
            // 设备账本只要认得出设备就记一笔：**请求数无条件 +1**，费用取不到时按 0 计——
            // 4xx/429 这些没有 usage 的请求同样是这台设备打出去的，排查限流恰恰要看这些。
            let snapshot = rec.rl_5h_utilization.is_some()
                || rec.rl_7d_utilization.is_some()
                || !rec.windows.is_empty();
            let q = sqlx::query(if snapshot { LEDGER_SNAPSHOT_SQL } else { LEDGER_SQL })
                .bind(cid)
                .bind(ts)
                .bind(rec.cost_usd)
                .bind(&rec.device_id);
            let q = if snapshot {
                // 序列化失败在这里不可达（定长结构），真失败也只是少存这一列，不该把整条
                // 用量日志连坐掉。
                q.bind(&rec.unified_status)
                    .bind(rec.rl_5h_utilization)
                    .bind(rec.rl_5h_reset)
                    .bind(rec.rl_7d_utilization)
                    .bind(rec.rl_7d_reset)
                    .bind(&rec.rl_representative)
                    .bind(rec.rl_overage_in_use.map(i64::from))
                    .bind(serde_json::to_string(&rec.windows).ok())
            } else {
                q
            };
            q.execute(&mut *tx).await?;
        }
        // 预聚合：延迟 / 缓存趋势与拆分表读它，不再按时间扫流水，见 `rollup` 模块。
        //
        // 放在最后、紧挨着提交：它要改的「全部」维度那一行（当前 15 分钟桶）每条流水都要改，
        // 是全库最热的一行，行锁一直持到提交。排在前面的话，后面每一次往返都压着它，并发写
        // 流水全在这一行上排队。各事务改行的顺序一致（账本 → 汇总），不会互相死锁。
        rollup_record(&mut tx, ts, rec).await?;
        tx.commit().await?;
        if let Some((sha, _)) = new_tool_set {
            let mut known = self.known_tool_sets.lock();
            // 只是省掉重复的 upsert，丢了无妨；防着工具集千变万化时无限长。
            if known.len() >= KNOWN_TOOL_SETS_MAX {
                known.clear();
            }
            known.insert(sha);
        }
        Ok(())
    }

    /// 裁掉超过保留期（[`USAGE_LOG_RETENTION_SECS`]）的用量日志流水，返回删除条数。汇总另有
    /// 保留期（90 天，比流水长），随这里一起裁。
    ///
    /// 流水裁剪不影响任何终身口径——最近使用/累计费用/最新快照都在账本里（写时落账）；还要读
    /// 流水的只剩两处：5h/7d 窗口统计（最多回看 7 天多）和请求日志页（只翻近期），8 天都覆盖
    /// 得住。
    ///
    /// 分批删、批间歇一下：日志表可能积了几百万行，一条大 DELETE 是一个长事务，产生的 WAL
    /// 与死元组一次性压上来，复制与 autovacuum 都跟着抖；分批让在线写入平稳穿插。
    pub async fn prune_usage_logs(&self) -> Result<usize> {
        const BATCH: i64 = 500;
        const PAUSE: Duration = Duration::from_millis(50);
        let mut total = 0;
        loop {
            // 还有封号取证待办（`ban_pending`）要冻结的那段窗口不裁：取证可能因为停机或一直出错
            // 拖过了保留期，先裁掉的话补出来的统计与冻结就残缺了，而且再也补不回来。窗口口径同
            // `bans` 模块的补做：[封号时刻 - FREEZE_WINDOW_SECS, 封号时刻 + FREEZE_TAIL_SECS]。
            let n = sqlx::query(
                "DELETE FROM usage_logs WHERE id IN (
                     SELECT u.id FROM usage_logs u
                      WHERE u.ts < unixepoch() - $1
                        AND NOT EXISTS (
                            SELECT 1 FROM ban_pending p
                             WHERE p.cred_id = u.cred_id
                               AND u.ts >= p.ban_ts - $3 AND u.ts <= p.ban_ts + $4)
                      LIMIT $2)",
            )
            .bind(USAGE_LOG_RETENTION_SECS)
            .bind(BATCH)
            .bind(FREEZE_WINDOW_SECS)
            .bind(FREEZE_TAIL_SECS)
            .execute(&self.pool)
            .await?
            .rows_affected() as usize;
            total += n;
            if (n as i64) < BATCH {
                break;
            }
            tokio::time::sleep(PAUSE).await;
        }
        self.prune_rollup().await?;
        Ok(total)
    }

    /// 最近的用量日志，按时间倒序，最多 `limit` 条。测试用；线上那两条路径都带筛选，
    /// 直接走 [`Self::query_usage_logs`]。
    #[cfg(test)]
    pub async fn list_usage_logs(&self, limit: i64) -> Result<Vec<UsageLog>> {
        self.query_usage_logs(UsageLogQuery { limit, ..Default::default() }).await
    }

    /// 同一批筛选条件下的条数、花费合计与最大 id。见 [`UsageLogStats`]。
    ///
    /// `q` 里的 `limit`/`offset` **不参与**——统计的是整个集合，不是当前这一页。
    pub async fn usage_log_stats(&self, q: UsageLogQuery) -> Result<UsageLogStats> {
        let (where_sql, args) = where_clause(&q)?;
        let row = sqlx::query_with(sqlx::AssertSqlSafe(usage_log_stats_sql(&where_sql)), args)
            .fetch_one(&self.pool)
            .await?;
        Ok(UsageLogStats {
            total: row.try_get(0)?,
            cost_usd: row.try_get(1)?,
            max_id: row.try_get(2)?,
        })
    }

    /// 按条件查用量流水，恒按 `id` 倒序。见 [`UsageLogQuery`] 与 [`usage_log_page_sql`]。
    pub async fn query_usage_logs(&self, q: UsageLogQuery) -> Result<Vec<UsageLog>> {
        let (where_sql, mut args) = where_clause(&q)?;
        push(&mut args, q.limit)?;
        let n = push(&mut args, q.offset)?;
        let rows = sqlx::query_with(sqlx::AssertSqlSafe(usage_log_page_sql(&where_sql, n)), args)
            .fetch_all(&self.pool)
            .await?;
        let mut logs = rows.iter().map(usage_log_from_row).collect::<Result<Vec<_>>>()?;
        self.fill_tool_names(&mut logs).await?;
        Ok(logs)
    }

    /// 给读出来的流水把工具名清单按 sha 补回 shape（[`split_tool_names`] 的逆操作）。旧流水
    /// 里本来就带着清单的不动；查不到 sha 的（不该发生）原样留着只有 sha 的那份。
    pub(super) async fn fill_tool_names(&self, logs: &mut [UsageLog]) -> Result<()> {
        let mut parsed: Vec<(usize, serde_json::Value, String)> = Vec::new();
        for (i, log) in logs.iter().enumerate() {
            let Some(shape) = log.forensics.shape.as_deref() else { continue };
            let Ok(v) = serde_json::from_str::<serde_json::Value>(shape) else { continue };
            let tools = v.get("tools");
            if let Some(sha) = tools.and_then(|t| t.get("sha")).and_then(|s| s.as_str())
                && tools.and_then(|t| t.get("names")).is_none()
                && tools.and_then(|t| t.get("count")).and_then(|c| c.as_u64()) != Some(0)
            {
                let sha = sha.to_string();
                parsed.push((i, v, sha));
            }
        }
        if parsed.is_empty() {
            return Ok(());
        }
        let mut shas: Vec<&str> = parsed.iter().map(|p| p.2.as_str()).collect();
        shas.sort_unstable();
        shas.dedup();
        let found: HashMap<String, String> =
            sqlx::query_as("SELECT sha, names FROM tool_sets WHERE sha = ANY($1)")
                .bind(&shas)
                .fetch_all(&self.pool)
                .await?
                .into_iter()
                .collect();
        for (i, mut v, sha) in parsed {
            let Some(names) = found.get(&sha) else { continue };
            let Ok(names) = serde_json::from_str::<serde_json::Value>(names) else { continue };
            if let Some(tools) = v.get_mut("tools").and_then(|t| t.as_object_mut()) {
                tools.insert("names".into(), names);
                logs[i].forensics.shape = Some(v.to_string());
            }
        }
        Ok(())
    }
}

/// 进程内记着已经落进 `tool_sets` 的 sha 最多几条，见 [`CredentialStore::insert_usage_log_at`]。
const KNOWN_TOOL_SETS_MAX: usize = 10_000;

/// 把 shape 摘要（`proxy::logging` 的 `shape_summary`）里的工具名清单 `tools.names` 拆出来：
/// 回（去掉清单后的 shape, Some((tools.sha, 清单的 JSON))）。没有清单的原样返回、第二项为
/// `None`；不是合法 JSON 的也原样返回。
///
/// 同一个客户端每条请求带的工具集都一样，清单却有几十个工具名，原来每条流水各存一份，占了
/// shape 列的大头；拆出去按 sha 只存一份，读的时候补回（[`CredentialStore::fill_tool_names`]）。
/// sha 本来就是按完整工具名列表算的，同 sha 即同一份清单。
pub(super) fn split_tool_names(
    shape: Option<&str>,
) -> (Option<Cow<'_, str>>, Option<(String, String)>) {
    let Some(raw) = shape else { return (None, None) };
    let Ok(mut v) = serde_json::from_str::<serde_json::Value>(raw) else {
        return (Some(Cow::Borrowed(raw)), None);
    };
    let Some(tools) = v.get_mut("tools").and_then(|t| t.as_object_mut()) else {
        return (Some(Cow::Borrowed(raw)), None);
    };
    let Some(sha) = tools.get("sha").and_then(|s| s.as_str()).map(str::to_string) else {
        return (Some(Cow::Borrowed(raw)), None);
    };
    let Some(names) = tools.remove("names") else {
        return (Some(Cow::Borrowed(raw)), None);
    };
    (Some(Cow::Owned(v.to_string())), Some((sha, names.to_string())))
}

/// 按 [`USAGE_LOG_COLS`] 的顺序把一行读成 [`UsageLog`]（0 号列是主键）。
pub(super) fn usage_log_from_row(r: &PgRow) -> Result<UsageLog> {
    Ok(UsageLog {
        id: r.try_get(0)?,
        ts: r.try_get(1)?,
        cred_id: r.try_get(2)?,
        cred_label: r.try_get(3)?,
        device_id: r.try_get(4)?,
        model: r.try_get(5)?,
        path: r.try_get(6)?,
        status: r.try_get::<i64, _>(7)? as u16,
        has_usage: r.try_get::<i64, _>(8)? != 0,
        input_tokens: r.try_get(9)?,
        output_tokens: r.try_get(10)?,
        cache_creation_tokens: r.try_get(11)?,
        cache_5m_tokens: r.try_get(12)?,
        cache_1h_tokens: r.try_get(13)?,
        cache_read_tokens: r.try_get(14)?,
        ttft_ms: r.try_get(15)?,
        total_ms: r.try_get(16)?,
        unified_status: r.try_get(17)?,
        rl_5h_status: r.try_get(18)?,
        rl_5h_reset: r.try_get(19)?,
        rl_5h_utilization: r.try_get(20)?,
        rl_7d_status: r.try_get(21)?,
        rl_7d_reset: r.try_get(22)?,
        rl_7d_utilization: r.try_get(23)?,
        rl_representative: r.try_get(24)?,
        ratelimit_raw: r.try_get(25)?,
        cost_usd: r.try_get(26)?,
        rl_overage_in_use: r.try_get::<Option<i64>, _>(27)?.map(|v| v != 0),
        ua: r.try_get(28)?,
        ua_out: r.try_get(29)?,
        sse_aggregated: r.try_get::<i64, _>(30)? != 0,
        request_id: r.try_get(31)?,
        upstream_request_id: r.try_get(32)?,
        forensics: Forensics {
            proxy: r.try_get(33)?,
            simulated: r.try_get::<i64, _>(34)? != 0,
            shape: r.try_get(35)?,
            session_id: r.try_get(36)?,
            error_type: r.try_get(37)?,
            error_message: r.try_get(38)?,
            third_party: r.try_get::<i64, _>(39)? != 0,
            rewrites: r.try_get(40)?,
            device_id_out: r.try_get(41)?,
            response_excerpt: r.try_get(42)?,
            sim_reason: r.try_get(43)?,
            session_key: r.try_get(44)?,
            session_id_in: r.try_get(45)?,
        },
    })
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// 建一个测试库和若干个号（号主 admin），回号的 id。
    pub(in crate::store) async fn store_with(
        pool: sqlx::PgPool,
        labels: &[&str],
    ) -> (CredentialStore, Vec<i64>) {
        let store = CredentialStore::for_test(pool).await;
        let mut ids = Vec::new();
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

    /// 库的当前时刻。
    pub(in crate::store) async fn db_now(store: &CredentialStore) -> i64 {
        sqlx::query_scalar("SELECT unixepoch()").fetch_one(&store.pool).await.unwrap()
    }

    /// 写一条带用量与（可选）限流头的流水，走真实的写入路径（流水 + 账本同一事务）。
    /// 每条记 10 个 token（输入/输出/缓存写/缓存读 各 1 + 3 + 2 + 4）。
    pub(in crate::store) async fn log_row(
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
        store.insert_usage_log_at(&rec, Some(ts)).await.unwrap();
    }

    #[test]
    fn redact_proxy_hides_only_the_password() {
        use super::super::redact_proxy;
        assert_eq!(redact_proxy("socks5h://u:p@h:1"), "socks5h://u:***@h:1");
        assert_eq!(redact_proxy("http://h:8080"), "http://h:8080");
        assert_eq!(redact_proxy("http://u@h:8080"), "http://u@h:8080");
        assert_eq!(redact_proxy("http://u:p:q@h"), "http://u:***@h");
    }

    /// 裁剪只动流水，不动账本：累计费用/最近使用/额度快照在裁剪后原样保留。
    #[sqlx::test]
    async fn prune_keeps_ledger(pool: sqlx::PgPool) {
        let (store, ids) = store_with(pool, &["a"]).await;
        let a = ids[0];

        // 一条早已过保留期的旧流水（带限流头，会写快照）+ 一条刚发生的新流水（无头）。
        let old_ts = 1_000;
        log_row(&store, a, old_ts, 2.0, Some(old_ts + 100), Some(old_ts + 100)).await;
        store
            .insert_usage_log(&UsageRecord {
                cred_id: Some(a),
                cost_usd: Some(1.0),
                ..Default::default()
            })
            .await
            .unwrap();

        assert_eq!(store.prune_usage_logs().await.unwrap(), 1, "只裁过保留期的旧流水");
        assert_eq!(store.list_usage_logs(10).await.unwrap().len(), 1, "新流水应保留");
        assert_eq!(store.cost_of(a).await.unwrap(), 3.0, "累计费用是账本口径，不随裁剪变小");
        assert!(store.last_used_at(a).await.unwrap().is_some());
        let q = store.latest_quota(a).await.unwrap().expect("快照在账本里长存");
        assert_eq!(q.ts, old_ts, "快照仍是最后一次带限流头的那条");
        // 窗口统计只看还留着的流水：新流水 ts 在窗口起点之后，计入。
        assert_eq!(q.cost_5h, Some(1.0));
        assert_eq!(q.requests_5h, Some(1));
    }

    /// 「非流转流」标记要能落库并原样读回；没写这一列的行（这里用裸 INSERT 造一条）取列默认
    /// 值 0，读回 false 而不是读取失败。
    ///
    /// SQLite 版测的是老库补列的升级路径，PG 版没有升级路径，只保留「默认 false」这一半。
    #[sqlx::test]
    async fn sse_aggregated_round_trips_and_defaults_to_false(pool: sqlx::PgPool) {
        let store = CredentialStore::for_test(pool).await;
        sqlx::query("INSERT INTO usage_logs (cred_label) VALUES ('old')")
            .execute(&store.pool)
            .await
            .unwrap();
        let cred = store.insert("a", None, "t", "r", 0, None, None, 1).await.unwrap().id;

        for aggregated in [true, false] {
            store
                .insert_usage_log(&UsageRecord {
                    cred_id: Some(cred),
                    sse_aggregated: aggregated,
                    ..Default::default()
                })
                .await
                .unwrap();
        }

        let logs = store
            .query_usage_logs(UsageLogQuery { limit: 10, ..Default::default() })
            .await
            .unwrap();
        // 倒序：最新写入的（false）在前，然后是 true，最后是裸插入的那条。
        assert_eq!(
            logs.iter().map(|l| l.sse_aggregated).collect::<Vec<_>>(),
            vec![false, true, false],
            "标记要原样读回，且没写这一列的记录退化成 false 而不是读取失败"
        );
    }

    /// 请求明细的筛选与分页：按账号只出该账号的记录，页码不重叠，且**锚点之后新写入的记录
    /// 不得挤动已在翻的页**——这正是页码翻页要带 `until_id` 的理由。
    #[sqlx::test]
    async fn usage_logs_filter_by_credential_and_paginate(pool: sqlx::PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        let log = async |cred: i64, cost: f64| {
            store
                .insert_usage_log(&UsageRecord {
                    cred_id: Some(cred),
                    cost_usd: Some(cost),
                    ..Default::default()
                })
                .await
                .unwrap()
        };
        // a 四条、b 一条，交替写入，确保筛选不是靠「恰好连续」蒙对的。
        for (cred, cost) in [(a, 1.0), (a, 2.0), (b, 100.0), (a, 4.0), (a, 8.0)] {
            log(cred, cost).await;
        }

        let all = store
            .query_usage_logs(UsageLogQuery { limit: 10, ..Default::default() })
            .await
            .unwrap();
        assert_eq!(all.len(), 5, "不筛时是全部");

        // 统计与记录同一套条件：a 的四条、花费合计 15，最大 id 即锚点。
        let only_a = UsageLogQuery { cred_id: Some(a), ..Default::default() };
        let stats = store.usage_log_stats(only_a.clone()).await.unwrap();
        assert_eq!(stats.total, 4, "b 的那条不该计入");
        assert_eq!(stats.cost_usd, 15.0);
        let anchor = stats.max_id.expect("有记录就有锚点");

        let page = async |n: i64| {
            store
                .query_usage_logs(UsageLogQuery {
                    cred_id: Some(a),
                    until_id: Some(anchor),
                    offset: n * 3,
                    limit: 3,
                    request_id: None,
                    model: None,
                    since: None,
                    session_key: None,
                    session_id: None,
                    owner_id: None,
                })
                .await
                .unwrap()
        };
        let first = page(0).await;
        assert_eq!(first.len(), 3);
        assert!(first.iter().all(|l| l.cred_id == Some(a)), "b 的那条不该出现");
        assert!(first.windows(2).all(|w| w[0].id > w[1].id), "按 id 倒序");

        let second = page(1).await;
        assert_eq!(second.len(), 1, "a 共 4 条，第二页只剩 1 条");
        assert!(second[0].id < first[2].id, "第二页不得与第一页重叠");
        assert!(page(2).await.is_empty(), "翻到底为空");

        // 翻页途中来了新请求：锚点之下的两页一字不变，锚点之上的统计才会长。
        let ids = |logs: &[UsageLog]| logs.iter().map(|l| l.id).collect::<Vec<_>>();
        log(a, 16.0).await;
        assert_eq!(ids(&page(0).await), ids(&first), "新记录不得把第一页往后挤");
        assert_eq!(ids(&page(1).await), ids(&second));
        let pinned =
            store.usage_log_stats(UsageLogQuery { until_id: Some(anchor), ..only_a.clone() }).await;
        assert_eq!(pinned.unwrap().total, 4, "钉在锚点上的统计不动");
        assert_eq!(store.usage_log_stats(only_a).await.unwrap().total, 5, "不带锚点才看得到新记录");
    }

    /// 请求 id 落库、可精确查。
    ///
    /// SQLite 版还用 EXPLAIN QUERY PLAN 钉住了「按号翻页走 (cred_id, id)、按请求 id 走
    /// request_id 索引、按模型走 (model, ts) 覆盖索引、按号统计走覆盖索引」；PG 的计划随统计
    /// 信息变，空表上一律顺序扫，那几条断言不移植（索引本身在 schema 里都建了）。
    #[sqlx::test]
    async fn usage_logs_are_searchable_by_request_id_using_indexes(pool: sqlx::PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        let log = async |cred: i64, rid: &str, up: Option<&str>| {
            store
                .insert_usage_log(&UsageRecord {
                    cred_id: Some(cred),
                    request_id: Some(rid.into()),
                    upstream_request_id: up.map(Into::into),
                    ..Default::default()
                })
                .await
                .unwrap()
        };
        log(a, "lb-1", Some("req_up_1")).await;
        log(a, "lb-2", None).await;
        log(b, "lb-3", Some("req_up_3")).await;

        let by = |rid: &str| UsageLogQuery {
            request_id: Some(rid.into()),
            limit: 10,
            ..Default::default()
        };
        let hit = store.query_usage_logs(by("lb-3")).await.unwrap();
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].cred_id, Some(b));
        assert_eq!(hit[0].upstream_request_id.as_deref(), Some("req_up_3"));
        assert_eq!(
            store.usage_log_stats(by("lb-3")).await.unwrap().total,
            1,
            "统计与取页同一套条件"
        );
        assert!(store.query_usage_logs(by("nope")).await.unwrap().is_empty());
        // 空白视同不筛。
        assert_eq!(store.query_usage_logs(by("   ")).await.unwrap().len(), 3);
        // 与按号筛叠加。
        let both = UsageLogQuery {
            cred_id: Some(a),
            request_id: Some("lb-3".into()),
            limit: 10,
            ..Default::default()
        };
        assert!(store.query_usage_logs(both).await.unwrap().is_empty(), "lb-3 是 b 的");
        // 按模型 + 起点（拆分表点进来）带不带锚点都能跑通。
        for until_id in [None, Some(100)] {
            let q = UsageLogQuery {
                model: Some("claude-opus-5".into()),
                since: Some(0),
                until_id,
                limit: 10,
                ..Default::default()
            };
            assert_eq!(store.usage_log_stats(q.clone()).await.unwrap().total, 0);
            assert!(store.query_usage_logs(q).await.unwrap().is_empty());
        }
    }

    /// 流水按**模拟会话键**筛：键落进流水、与按号筛可叠、空白视同不筛。（SQLite 版另用
    /// EXPLAIN QUERY PLAN 钉住走部分索引，PG 版不移植那一条。）
    #[sqlx::test]
    async fn usage_logs_filter_by_session_key_using_a_partial_index(pool: sqlx::PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let (a, b) = (ids[0], ids[1]);
        let log = async |cred: i64, key: Option<&str>| {
            store
                .insert_usage_log(&UsageRecord {
                    cred_id: Some(cred),
                    forensics: Forensics { session_key: key.map(Into::into), ..Default::default() },
                    ..Default::default()
                })
                .await
                .unwrap()
        };
        let one = "lb:v2:sid:7fe47444-c834-44e0-b568-d61e07daa35e";
        let two = "lb:v2:pfx:3f9a1c7e5b2d4680a1b2c3d4e5f60718";
        log(a, Some(one)).await;
        log(a, Some(one)).await;
        log(a, Some(two)).await;
        log(b, Some(one)).await;
        log(a, None).await; // 带设备身份的那类：这一列为空

        let by = |key: &str| UsageLogQuery {
            session_key: Some(key.into()),
            limit: 10,
            ..Default::default()
        };
        let hit = store.query_usage_logs(by(one)).await.unwrap();
        assert_eq!(hit.len(), 3, "两个号上的同键请求都算");
        assert!(
            hit.iter().all(|l| l.forensics.session_key.as_deref() == Some(one)),
            "键随流水落库"
        );
        assert_eq!(store.usage_log_stats(by(one)).await.unwrap().total, 3, "统计与取页同一套条件");
        assert_eq!(store.query_usage_logs(by(two)).await.unwrap().len(), 1);
        assert!(store.query_usage_logs(by("lb:v2:pfx:nope")).await.unwrap().is_empty());
        assert_eq!(store.query_usage_logs(by("  ")).await.unwrap().len(), 5, "空白视同不筛");
        // 与按号筛叠加——会话行点进来带的正是这两项。
        let scoped = UsageLogQuery {
            cred_id: Some(a),
            session_key: Some(one.into()),
            limit: 10,
            ..Default::default()
        };
        assert_eq!(store.query_usage_logs(scoped.clone()).await.unwrap().len(), 2, "b 上那条不算");
        assert_eq!(store.usage_log_stats(scoped).await.unwrap().total, 2);
    }

    /// 按**会话 id** 查：出站与来访两侧任一命中。（SQLite 版另用 EXPLAIN QUERY PLAN 钉住
    /// MULTI-INDEX OR，PG 版不移植那一条。）
    #[sqlx::test]
    async fn usage_logs_look_up_a_session_id_on_either_side(pool: sqlx::PgPool) {
        let (store, ids) = store_with(pool, &["a"]).await;
        let a = ids[0];
        let log = async |out: Option<&str>, inn: Option<&str>| {
            store
                .insert_usage_log(&UsageRecord {
                    cred_id: Some(a),
                    forensics: Forensics {
                        session_id: out.map(Into::into),
                        session_id_in: inn.map(Into::into),
                        ..Default::default()
                    },
                    ..Default::default()
                })
                .await
                .unwrap()
        };
        let client = "11111111-2222-4333-8444-555555555555";
        let upstream = "7fe47444-c834-44e0-b568-d61e07daa35e";
        log(Some(upstream), Some(client)).await; // 走模拟：两侧不同
        log(Some(upstream), Some(client)).await;
        log(Some(client), Some(client)).await; // 没改身份：两侧同一个
        log(None, Some(client)).await; // 本地拒绝：只有来访那侧
        log(Some("99999999-9999-4999-8999-999999999999"), None).await; // luban 自己发的

        let by = |sid: &str| UsageLogQuery {
            session_id: Some(sid.into()),
            limit: 10,
            ..Default::default()
        };
        assert_eq!(
            store.query_usage_logs(by(client)).await.unwrap().len(),
            4,
            "下游拿自己那个 uuid 来查：被改过身份的两条、没改的一条、本地拒绝的一条"
        );
        assert_eq!(
            store.query_usage_logs(by(upstream)).await.unwrap().len(),
            2,
            "从上游侧回查：只有出站是它的那两条"
        );
        assert_eq!(
            store.usage_log_stats(by(client)).await.unwrap().total,
            4,
            "统计与取页同一套条件"
        );
        assert!(
            store
                .query_usage_logs(by("00000000-0000-4000-8000-000000000000"))
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(store.query_usage_logs(by("  ")).await.unwrap().len(), 5, "空白视同不筛");
    }

    /// 「近 N 天出现过的模型」：空 / NULL 模型名不算，只被连通性测试打过的不算，窗口外的不算；
    /// 最新几条是 probe 的，退回它之前那条客户端流水的时刻。按时刻倒序。
    #[sqlx::test]
    async fn recent_models_skips_probe_empty_and_stale(pool: sqlx::PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let now = chrono::Utc::now().timestamp();
        let log = async |model: Option<&str>, device: Option<&str>, ts: i64| {
            store
                .insert_usage_log_at(
                    &UsageRecord {
                        model: model.map(Into::into),
                        device_id: device.map(Into::into),
                        ..Default::default()
                    },
                    Some(ts),
                )
                .await
                .unwrap();
        };
        log(Some("opus"), Some("d1"), now - 300).await;
        // 最新那几条是连通性测试打的：退回它之前那条客户端流水的时刻。
        log(Some("opus"), Some("probe"), now - 10).await;
        log(Some("sonnet"), None, now - 100).await;
        log(Some("probe-only"), Some("probe"), now - 50).await;
        log(Some("stale"), Some("d1"), now - 8 * 86400).await;
        log(Some(""), Some("d1"), now - 20).await;
        log(None, Some("d1"), now - 20).await;

        assert_eq!(
            store.recent_models(7).await.unwrap(),
            vec![("sonnet".to_string(), now - 100), ("opus".to_string(), now - 300)]
        );
    }

    /// 后台的各条只读报表在一个有流水、有封号事件的库上都跑得通（SQL 在 PG 上合法、聚合列的
    /// 类型解码得了）。对应 SQLite 版 `reader_sees_committed_writes_and_rejects_writes` 里
    /// 「每个读方法跑一遍」的那一半；只读连接、WAL 那一半 PG 版没有对应物。
    #[sqlx::test]
    async fn admin_reports_run_on_a_populated_store(pool: sqlx::PgPool) {
        use super::super::{BanContext, BreakdownBy};
        let (store, ids) = store_with(pool, &["a"]).await;
        let a = ids[0];
        store
            .insert_usage_log(&UsageRecord {
                cred_id: Some(a),
                cred_label: "a".into(),
                status: 200,
                ttft_ms: Some(100),
                total_ms: Some(300),
                output_tokens: Some(10),
                ..Default::default()
            })
            .await
            .unwrap();
        let ctx = BanContext { reason: "banned".into(), source: "manual", ..Default::default() };
        assert!(store.record_ban(a, &ctx).await.unwrap());
        let since = 0;
        store.recent_rpm().await.unwrap();
        store.recent_rpm_of(a).await.unwrap();
        store.total_rpm().await.unwrap();
        store.last_used().await.unwrap();
        store.cost_by_cred().await.unwrap();
        store.cache_report(since, 3600, 0).await.unwrap();
        store.ttft_report(since, 3600, 0).await.unwrap();
        store.usage_breakdown(since, BreakdownBy::Model, 12).await.unwrap();
        store.usage_breakdown(since, BreakdownBy::Account, 12).await.unwrap();
        store.credential_stats(a, since, 3600, 0, 20).await.unwrap();
        store.local_rejections(since).await.unwrap();
        store.recent_models(7).await.unwrap();
        let q = UsageLogQuery { limit: 10, ..Default::default() };
        assert_eq!(store.usage_log_stats(q.clone()).await.unwrap().total, 1);
        assert_eq!(store.query_usage_logs(q).await.unwrap().len(), 1);
        let ev = store.list_ban_events(None, 10).await.unwrap().remove(0);
        assert_eq!(store.ban_counts().await.unwrap().get(&a).copied(), Some(1));
        assert_eq!(store.frozen_usage_logs(ev.id, 10, 0).await.unwrap().0, 1);
    }

    /// 删号时还在途的请求：流水照记，账本与费用汇总不再给已删的号建行。
    #[sqlx::test]
    async fn usage_after_removal_keeps_log_but_skips_ledger(pool: sqlx::PgPool) {
        let (store, ids) = store_with(pool, &["a"]).await;
        let a = ids[0];
        let now = db_now(&store).await;
        log_row(&store, a, now, 1.0, Some(now + 60), None).await;
        store.remove(&[a]).await.unwrap();
        let rec = UsageRecord {
            cred_id: Some(a),
            device_id: Some("dev".into()),
            cost_usd: Some(1.0),
            has_usage: true,
            rl_5h_utilization: Some(0.5),
            ..Default::default()
        };
        store.insert_usage_log_at(&rec, None).await.unwrap();
        let count = |table: &str| {
            let sql = format!("SELECT COUNT(*) FROM {table} WHERE cred_id = $1");
            let pool = store.pool.clone();
            async move {
                sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(sql))
                    .bind(a)
                    .fetch_one(&pool)
                    .await
                    .unwrap()
            }
        };
        assert_eq!(count("usage_logs").await, 2);
        assert_eq!(count("credential_stats").await, 0);
        assert_eq!(count("device_costs").await, 0);
        // 删号前那条的费用汇总保留（同删号口径），删号后这条不再记。
        assert_eq!(count("billing_hourly").await, 1);
        let billed: i64 = sqlx::query_scalar(
            "SELECT SUM(requests)::BIGINT FROM billing_hourly WHERE cred_id = $1",
        )
        .bind(a)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!(billed, 1);
    }

    /// 流水与封号事件里带 NUL 的文本去掉 NUL 后照常落库，不让整条语句报错。
    #[sqlx::test]
    async fn nul_in_usage_and_ban_text_is_stripped(pool: sqlx::PgPool) {
        let (store, ids) = store_with(pool, &["a"]).await;
        let a = ids[0];
        let rec = UsageRecord {
            cred_id: Some(a),
            device_id: Some("d\0ev".into()),
            model: Some("m\0odel".into()),
            forensics: Forensics {
                error_message: Some("bad \0 input".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        store.insert_usage_log_at(&rec, None).await.unwrap();
        let (device, model, msg): (String, String, String) = sqlx::query_as(
            "SELECT device_id, model, error_message FROM usage_logs WHERE cred_id = $1",
        )
        .bind(a)
        .fetch_one(&store.pool)
        .await
        .unwrap();
        assert_eq!((device.as_str(), model.as_str(), msg.as_str()), ("dev", "model", "bad  input"));

        let ctx = super::super::BanContext {
            reason: "r\0".into(),
            error_message: Some("e\0".into()),
            ..Default::default()
        };
        assert!(store.record_ban(a, &ctx).await.unwrap());
        let ev = store.list_ban_events(None, 10).await.unwrap().remove(0);
        assert_eq!(ev.reason, "r");
        store.deny_model(a, "m\0", "why\0", None).await.unwrap();
        assert_eq!(store.denied_models(a).await.unwrap().len(), 1);
    }

    /// 工具名清单拆进 `tool_sets`：库里的 shape 不带清单，读出来（含冻结流水）补回后与写入时
    /// 逐字一致；同一份清单只存一行。
    #[sqlx::test]
    async fn tool_names_are_stored_once_and_restored_on_read(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let a = store.insert("a", None, "ta", "ra", u64::MAX, None, None, 1).await.unwrap().id;
        let shape = r#"{"keys":["model","tools"],"model":"claude-opus-5","tools":{"count":2,"sha":"0123456789abcdef","names":["Bash","Read"]}}"#;
        let rec = UsageRecord {
            cred_id: Some(a),
            cred_label: "a".into(),
            status: 200,
            forensics: Forensics { shape: Some(shape.into()), ..Default::default() },
            ..Default::default()
        };
        store.insert_usage_log_at(&rec, None).await.unwrap();
        store.insert_usage_log_at(&rec, None).await.unwrap();

        let stored: Vec<String> = sqlx::query_scalar("SELECT shape FROM usage_logs")
            .fetch_all(&store.pool)
            .await
            .unwrap();
        assert!(stored.iter().all(|s| !s.contains("names") && s.contains("0123456789abcdef")));
        let sets: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM tool_sets")
            .fetch_one(&store.pool)
            .await
            .unwrap();
        assert_eq!(sets, 1);

        let logs = store.list_usage_logs(10).await.unwrap();
        assert_eq!(logs.len(), 2);
        assert!(logs.iter().all(|l| l.forensics.shape.as_deref() == Some(shape)));

        let ctx = BanContext { reason: "[403] x".into(), source: "forward", ..Default::default() };
        assert!(store.record_ban(a, &ctx).await.unwrap());
        let ev = store.list_ban_events(Some(a), 1).await.unwrap().remove(0);
        let frozen = store.frozen_usage_logs(ev.id, 10, 0).await.unwrap().1;
        assert_eq!(frozen[0].forensics.shape.as_deref(), Some(shape));

        // 没有工具的、不是 JSON 的原样存。
        assert_eq!(split_tool_names(Some("not json")).0.as_deref(), Some("not json"));
        let plain = r#"{"keys":["model"]}"#;
        assert_eq!(split_tool_names(Some(plain)), (Some(Cow::Borrowed(plain)), None));
    }
}
