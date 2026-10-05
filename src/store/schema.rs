//! 建表与迁移。

use super::*;

pub(super) fn init_schema(conn: &Connection) -> Result<()> {
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
        -- slot_lost：休眠期间槽位已被别的会话接手（记过 evicted），重新拿到槽位时清零；找
        -- 「这次让出槽位的前任」只认还没丢的那条（`store::session_events::note_slot_takeover`）。
        -- last_model：最近一轮请求的模型，**只记不参与键**（键里带模型会把同一条对话换模型
        -- 的那一轮劈成两条会话、占两份名额，见 `session_binding_key` 的记述）；给后台列。
        CREATE TABLE IF NOT EXISTS session_bindings (
            session_key   TEXT    PRIMARY KEY,
            cred_id       INTEGER NOT NULL,
            slot          INTEGER NOT NULL DEFAULT 0,
            request_count INTEGER NOT NULL DEFAULT 0,
            last_model    TEXT,
            slot_lost     INTEGER NOT NULL DEFAULT 0,
            created_at    INTEGER NOT NULL DEFAULT (unixepoch()),
            last_seen_at  INTEGER NOT NULL DEFAULT (unixepoch())
        ) STRICT;
        CREATE INDEX IF NOT EXISTS idx_session_bindings_cred
            ON session_bindings(cred_id);
        -- 同 device_bindings：给保留期清理用。这张表还更容易长——键是每条对话一个，
        -- 默认保留 24 小时，多客户端时攒到几万行不稀奇。
        CREATE INDEX IF NOT EXISTS idx_session_bindings_seen
            ON session_bindings(last_seen_at);

        -- 会话绑定的历史事件（`store::session_events`）：只在状态变化时记，保留 7 天。
        -- event：bound 新建 / slot_taken 接手休眠绑定的槽位 / evicted 槽位被接手 /
        --        resumed 休眠后恢复 / rebound 改绑 / unbound 解绑 / expired 过期清理。
        -- cred_id/slot：当前（或新落）的账号与槽位；prev_cred_id/prev_slot：改绑或恢复前的。
        -- other_key：slot_taken 的前任会话键、evicted 的接手者；idle_secs：那条绑定闲置了多久；
        -- reason：rebound 的原因、unbound 的方式。
        CREATE TABLE IF NOT EXISTS session_binding_events (
            id            INTEGER PRIMARY KEY,
            ts            INTEGER NOT NULL DEFAULT (unixepoch()),
            session_key   TEXT    NOT NULL,
            event         TEXT    NOT NULL,
            cred_id       INTEGER,
            prev_cred_id  INTEGER,
            slot          INTEGER,
            prev_slot     INTEGER,
            other_key     TEXT,
            idle_secs     INTEGER,
            reason        TEXT
        ) STRICT;
        -- 会话详情按键看历史；槽位历史按「账号 + 槽位」看，离开的那一侧记在 prev 两列上。
        CREATE INDEX IF NOT EXISTS idx_session_events_key
            ON session_binding_events(session_key, id);
        CREATE INDEX IF NOT EXISTS idx_session_events_slot
            ON session_binding_events(cred_id, slot, id);
        CREATE INDEX IF NOT EXISTS idx_session_events_prev_slot
            ON session_binding_events(prev_cred_id, prev_slot, id) WHERE prev_cred_id IS NOT NULL;
        CREATE INDEX IF NOT EXISTS idx_session_events_ts
            ON session_binding_events(ts);

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
    // 槽位被接手的标记（见建表处）。补列成功（旧库第一次升级）时按现有数据初始化归属，见
    // [`init_slot_owners`]；列已在什么都不动。
    if conn
        .execute("ALTER TABLE session_bindings ADD COLUMN slot_lost INTEGER NOT NULL DEFAULT 0", [])
        .is_ok()
    {
        init_slot_owners(conn)?;
    }
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
pub(super) const PRIORITY_SCALE_MIGRATED: &str = "priority_scale_p50";

/// 标记优先级已换到 P0..=P4、默认 P2 的档位口径；有这一行就不再迁移。
pub(super) const PRIORITY_TIERS_MIGRATED: &str = "priority_tiers_p2";

/// 优先级换成 5 档：按名次压档（见 [`priority_tiers_by_rank`]），旧默认档落 P2，
/// 两侧最靠近的各占 P1/P3，再往外的并进 P0/P4，先后顺序不变。旧口径有两种：带
/// [`PRIORITY_SCALE_MIGRATED`] 标记的是 P1..=P100（默认 50），没有的是最早的口径（默认 0）。
/// **只跑一次**：压档不是幂等的，靠 settings 里的标记挡住重复执行；新库空表上跑一遍只是落下标记。
///
/// 查标记、读旧值、写新档必须在同一个 `BEGIN IMMEDIATE` 事务里：服务和 `luban status`
/// 可能同时打开同一个库，两边若都在事务外读到「未迁移」，后提交的那个会拿已压过档的数据再压
/// 一遍（P2 → P1）。IMMEDIATE 一开头就占写锁，另一边在 busy_timeout 内排队，轮到它时已能看到标记。
pub(super) fn migrate_priority_tiers(conn: &Connection) -> Result<()> {
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
pub(super) fn purge_orphan_rows(conn: &Connection) -> Result<()> {
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

/// 旧库补 `session_bindings.slot_lost` 列之后初始化槽位归属：同一账号同一槽位上有好几条绑定时
/// （休眠绑定的槽位会被新对话接手，被接手的那条行还留着、记着原槽位号），只有**最近活跃**的
/// 那条是当前持有者，其余标成已丢槽位。全标 0 的话，解绑当前持有者之后再分给别人，会把早就
/// 被接手过的那条认成前任，记出一条错的「接手」。
///
/// 「最近活跃 = 当前持有者」成立的理由：活跃绑定的槽位不会被分出去，两条活跃绑定不可能共用
/// 一个槽位；丢了槽位的绑定只有重新选号拿到槽位时才会刷新 `last_seen_at`，那时它就是新的
/// 持有者。同一秒的并列按会话键取一条，结果确定。沿用来访 ID 的（`slot = -1`）不占槽位，不动。
pub(super) fn init_slot_owners(conn: &Connection) -> Result<()> {
    conn.execute(
        "UPDATE session_bindings SET slot_lost = 1 \
          WHERE slot >= 0 AND EXISTS ( \
                SELECT 1 FROM session_bindings o \
                 WHERE o.cred_id = session_bindings.cred_id \
                   AND o.slot = session_bindings.slot \
                   AND o.session_key <> session_bindings.session_key \
                   AND (o.last_seen_at > session_bindings.last_seen_at \
                        OR (o.last_seen_at = session_bindings.last_seen_at \
                            AND o.session_key > session_bindings.session_key)))",
        [],
    )?;
    Ok(())
}
