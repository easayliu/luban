-- 流水 shape 里的工具名清单按 tools.sha 去重存这里（见 store::usage 的 split_tool_names）。
-- 同一个客户端每条请求带的工具集都一样，原来每条流水各存一份几十个工具名，占 shape 列的大头。
-- 流水只留 sha，读出来时按 sha 补回。只增不删：一种工具集一行，量很小，冻结流水也要靠它补。
CREATE TABLE tool_sets (
    sha        TEXT   PRIMARY KEY,
    names      TEXT   NOT NULL,
    created_at BIGINT NOT NULL DEFAULT unixepoch()
);

-- 流水按小时裁一次，每次只删掉表的一小部分，默认 20% 的阈值很久才触发一次 autovacuum。
ALTER TABLE usage_logs SET (autovacuum_vacuum_scale_factor = 0.05);
