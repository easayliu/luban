-- unixepoch() 改取语句开始的时刻，不用 now()（事务开始的时刻）：事务里等锁、跑长语句的那段
-- 时间用 now() 全算不进去，写下的时间偏旧、保留期按旧时钟判。
-- 不用 clock_timestamp()：它是 VOLATILE，`ts >= unixepoch() - N` 这类条件就走不了索引。
CREATE OR REPLACE FUNCTION unixepoch() RETURNS BIGINT
    LANGUAGE sql STABLE
    AS $$ SELECT extract(epoch FROM statement_timestamp())::BIGINT $$;
