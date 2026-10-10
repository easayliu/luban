-- 封号取证整套下线：复盘直接查 usage_logs（保留期随之放宽到 30 天）。
DROP TABLE IF EXISTS usage_logs_frozen;
DROP TABLE IF EXISTS ban_pending;
DROP TABLE IF EXISTS ban_events;
