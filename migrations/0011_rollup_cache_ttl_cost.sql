-- token 各项按官方 usage / 定价口径拆开：缓存写分 5m / 1h 两档，缓存拆分表改看输出与官方
-- API 价，不再看「节省」。
--
-- 一、预聚合 usage_rollup。
--
-- 预聚合原来只有缓存写合计与 saved_usd；加上 1h 档写入（5m 档 = 合计 − 1h）、全部请求的输出
-- token（gen_tokens 只算成功且有 TTFT 的，给吞吐用，口径不同）与按价目表估的等价费用（流水
-- cost_usd 之和，含输出），saved_usd 不再用，删掉。
--
-- 预聚合本来就是每条流水各记一笔，流水只会被裁、不会多出来：格子的请求数等于流水条数，这一格
-- 的流水就是完整的，照流水回填；对不上（流水裁掉了一部分）的格子新列留 0。界面窗口最长等于
-- 流水保留期，窗口里的格子流水都在。
ALTER TABLE usage_rollup
    ADD COLUMN written_1h_tokens BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN output_tokens BIGINT NOT NULL DEFAULT 0,
    ADD COLUMN cost_usd DOUBLE PRECISION NOT NULL DEFAULT 0,
    DROP COLUMN saved_usd;

WITH logs AS (
    SELECT ts / 900 * 900 AS bucket,
           1 AS n,
           COALESCE(model, '') AS model,
           COALESCE(cred_id::TEXT, '') AS cred,
           COALESCE(cache_1h_tokens, 0) AS w1h,
           COALESCE(output_tokens, 0) AS out,
           COALESCE(cost_usd, 0) AS cost
      FROM usage_logs
),
agg AS (
    SELECT 'all' AS dim, bucket, '' AS key, SUM(n) AS n, SUM(w1h) AS w1h, SUM(out) AS out,
           SUM(cost) AS cost
      FROM logs GROUP BY bucket
    UNION ALL
    SELECT 'model', bucket, model, SUM(n), SUM(w1h), SUM(out), SUM(cost)
      FROM logs GROUP BY bucket, model
    UNION ALL
    SELECT 'cred', bucket, cred, SUM(n), SUM(w1h), SUM(out), SUM(cost)
      FROM logs GROUP BY bucket, cred
)
UPDATE usage_rollup r
   SET written_1h_tokens = a.w1h, output_tokens = a.out, cost_usd = a.cost
  FROM agg a
 WHERE r.dim = a.dim AND r.bucket = a.bucket AND r.key = a.key
   AND r.requests = a.n;

-- 二、费用汇总 billing_hourly：加 1h 档缓存写（5m 档 = 合计 − 1h − 未分档）。
--
-- 新列可空，NULL 表示「这一格里有分不出档的历史缓存写」：升级前的格子先全是 NULL，只有能确定
-- 每一行各是哪一档的才回填，其余留 NULL，读的时候单独报「未分档」，不猜成哪一档；之后累加
-- NULL + x 仍是 NULL。
--
-- 缓存写为 0 的行，1h 档必定是 0，不管流水还在不在、号还在不在，先全部定下来（否则多 Key 的
-- 混档组里没有缓存写的那一行也留 NULL，升级后同一小时新记的 1h 写入就跟着被算成未分档）。
--
-- 其余行按「小时 + 号 + 模型」把账单与流水对账（流水里没有 Key 与分组，只能对到这一级）。
-- 账单只记写入时号还在的请求（删号后在途的请求只记流水），流水又会按保留期裁掉：
--   - 号还在：它的每条流水都入了账（号的 id 不复用），流水不会比账单多；组内请求数、缓存写
--     合计都相等，就说明一条没裁，是同一批。与裁剪按什么顺序删无关。
--   - 号已删：流水可能多出删号后在途的请求，又可能少了裁掉的，两者能凑出相等的条数；裁剪分批
--     删、不按时间顺序，现存最早一条流水也证明不了之后的小时没裁过。不回填，留 NULL。
-- 对上账的组也只在能确定各行归属时回填：组里只有一行有缓存写（就是流水那一批）；或整组全是
-- 5m / 全是 1h（每行都是同一档）。多行有缓存写且两档混着的，组内合计知道、各行各占多少不知道，
-- 留 NULL——按比例分摊会把「Key A 全是 1h、Key B 全是 5m」摊成两行各一半。
ALTER TABLE billing_hourly ADD COLUMN cache_write_1h_tokens BIGINT;

UPDATE billing_hourly SET cache_write_1h_tokens = 0 WHERE cache_write_tokens = 0;

WITH logs AS (
    SELECT ts / 3600 * 3600 AS hour, cred_id, COALESCE(model, '') AS model,
           COUNT(*) AS n,
           SUM(COALESCE(cache_creation_tokens,
                        COALESCE(cache_5m_tokens, 0) + COALESCE(cache_1h_tokens, 0))) AS write,
           SUM(COALESCE(cache_1h_tokens, 0)) AS w1h
      FROM usage_logs
     WHERE cred_id IN (SELECT id FROM credentials)
     GROUP BY 1, 2, 3
),
grouped AS (
    SELECT b.hour, b.owner_id, b.cred_id, b.key_id, b.group_id, b.model, b.cache_write_tokens,
           l.n, l.write, l.w1h,
           COUNT(*) FILTER (WHERE b.cache_write_tokens > 0) OVER w AS write_rows,
           SUM(b.requests) OVER w AS group_requests,
           SUM(b.cache_write_tokens) OVER w AS group_write
      FROM billing_hourly b
      JOIN logs l ON l.hour = b.hour AND l.cred_id = b.cred_id AND l.model = b.model
    WINDOW w AS (PARTITION BY b.hour, b.cred_id, b.model)
)
UPDATE billing_hourly b
   SET cache_write_1h_tokens = CASE
           WHEN g.write_rows = 1 THEN LEAST(g.w1h, g.cache_write_tokens)
           WHEN g.w1h = 0 THEN 0
           ELSE g.cache_write_tokens  -- 整组全是 1h
       END
  FROM grouped g
 WHERE b.hour = g.hour AND b.owner_id = g.owner_id AND b.cred_id = g.cred_id
   AND b.key_id = g.key_id AND b.group_id = g.group_id AND b.model = g.model
   AND b.cache_write_tokens > 0
   AND g.group_requests = g.n AND g.group_write = g.write
   AND (g.write_rows = 1 OR g.w1h = 0 OR g.w1h = g.group_write);
