-- 按号的流水统计（`COUNT(*)`、`SUM(cost_usd)`、`MAX(id)` WHERE cred_id = $1）能只扫索引。
--
-- 账号详情页「最近请求」每 30 秒取一次最新几条，同时要这个号 30 天内的总条数与总花费。原来
-- (cred_id, id) 里没有 cost_usd、(cred_id, ts) INCLUDE 里没有 id，两条哪条都得逐行回表，
-- 繁忙号一次就是几十万次随机读。键不变（翻页照旧走它），只把 cost_usd 带进叶子。
DROP INDEX idx_usage_logs_cred_id;
CREATE INDEX idx_usage_logs_cred_id ON usage_logs (cred_id, id) INCLUDE (cost_usd);
