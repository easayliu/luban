-- 排查 Key（MCP）与上号 Key 同表，按 kind 区分：鉴权、作废、随人删除走同一套（见 store::provision）。
ALTER TABLE provision_keys
    ADD COLUMN kind TEXT NOT NULL DEFAULT 'provision' CHECK (kind IN ('provision', 'mcp'));
