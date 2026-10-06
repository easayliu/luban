//! 用量流水：落库、翻页查询与裁剪。

use super::*;

impl CredentialStore {
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
    pub(super) fn where_clause(&self) -> (String, Vec<rusqlite::types::Value>) {
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

/// 流水统计（条数、花费合计、最大 id）的 SQL，见 [`CredentialStore::usage_log_stats`]。
/// 拎成函数是为了让测试对**实际执行的这条**跑 EXPLAIN QUERY PLAN。
pub(super) fn usage_log_stats_sql(where_sql: &str) -> String {
    format!("SELECT COUNT(*), COALESCE(SUM(cost_usd), 0), MAX(id) FROM usage_logs{where_sql}")
}

/// 流水取页的 SQL，见 [`CredentialStore::query_usage_logs`]。`n` 是参数个数，最后两个是
/// LIMIT / OFFSET。
///
/// **分两步**：子查询只按筛选取出这一页的 id，外层再按 id 读整行。子查询只碰 id，各条筛选
/// 索引都能覆盖它，不回表；一步到位的写法里，规划器若按 `(model, ts)` 圈窗口，就得先把窗口
/// 内每一行整行读出来排序再丢掉，一页 50 条要几百毫秒。OFFSET 跳过的那些行同理，只在
/// 索引里跳。外层的 `id IN (…)` 按主键逐条取，IN 列表本身有序，不再排序。
pub(super) fn usage_log_page_sql(where_sql: &str, n: usize) -> String {
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

impl CredentialStore {
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
    pub(super) fn insert_usage_log_at(&self, rec: &UsageRecord, ts: Option<i64>) -> Result<()> {
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
        // 预聚合：延迟 / 缓存趋势与拆分表读它，不再按时间扫流水，见 `rollup` 模块。
        rollup_record(&tx, ts, rec)?;
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
        // 汇总另有保留期（90 天，比流水长），随这里一起裁。
        self.prune_rollup()?;
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
}

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

/// 按 [`USAGE_LOG_COLS`] 的顺序把一行读成 [`UsageLog`]（0 号列是主键）。
pub(super) fn usage_log_from_row(r: &Row<'_>) -> rusqlite::Result<UsageLog> {
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
pub(super) fn head_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { s.chars().take(n).collect() }
}
