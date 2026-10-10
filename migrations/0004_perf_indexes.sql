-- 补几个按号查的索引，删一个没人用的。

-- 账单按号筛（只带 cred_id 时主键 (hour, ...) 只能扫整段时间范围）。
CREATE INDEX idx_billing_cred_hour ON billing_hourly (cred_id, hour);

-- 设备账本按号读（设备列表）与删号时按号删，原来只有主键 (device_id, cred_id)，全表扫。
CREATE INDEX idx_device_costs_cred ON device_costs (cred_id);

-- 会话槽位：取空槽位、记槽位接手都按 (号, 槽位) 找，原来只能按号回表读全部会话行。
-- 前导列就是 cred_id，原来那个单列索引被它覆盖，删掉省一份维护。
CREATE INDEX idx_session_bindings_cred_slot ON session_bindings (cred_id, slot);
DROP INDEX idx_session_bindings_cred;

-- 流水按设备查：线上代码没有这种查询，每条插入白维护一个索引。
DROP INDEX idx_usage_logs_device;

-- 删成员时漏删的分组授权（已补），清掉已经留下的孤儿行。
DELETE FROM group_grants WHERE user_id NOT IN (SELECT id FROM users);
