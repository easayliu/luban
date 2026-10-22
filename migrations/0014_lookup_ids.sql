-- 按上游的两种 id 查流水：上游 request-id（`req_011C…`）与回复的 Message id（`msg_…`）。
--
-- 来查的人手里常常只有这两个：Claude Code 本地对话记录里每条回复存的是 `requestId`（上游
-- request-id 响应头，luban 原样透传）与 `message.id`，上游报错体里的 `request_id` 也是前者。
-- 此前只有 luban 自己的请求 id 查得到。
--
-- message_id：可空、无默认值，加列不重写表；加列之前的旧记录一律为空。
-- 两条部分索引建时各扫一遍流水表。两种 id 都以毫秒时刻打头，之后的写入集中在索引里最新的一小段。
ALTER TABLE usage_logs ADD COLUMN message_id TEXT;
CREATE INDEX idx_usage_logs_upstream_request_id
    ON usage_logs (upstream_request_id) WHERE upstream_request_id IS NOT NULL;
CREATE INDEX idx_usage_logs_message_id ON usage_logs (message_id) WHERE message_id IS NOT NULL;
