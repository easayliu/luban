-- eval 回包里决定 `/v1/messages` 带不带 `x-claude-code-sandy-plover`（用过的工具）的两个开关，按号记最近一份。
--
-- 官方客户端把 eval 整份特性缓存存在本机配置里，进程一启动就读得到；luban 原先只记在内存，重启后到这个号下一次
-- eval 回来之前都按没下发算。会话启动握手里额度探测不等 eval 回包就发，重启后每个号第一个会话的那条探测因此漏带
-- 这个头。启动时整表读回内存，值与上次记下的不同才写。
--
-- snug_mitten 是 `tengu_snug_mitten`（下发 true 才算开），silly_shore 是 `tengu_silly_shore`（读不到按开）。
-- 外键级联：号删了一并没了，后台写入撞上刚删的号会被外键拒掉。
CREATE TABLE tools_used_flags (
    cred_id     BIGINT  PRIMARY KEY REFERENCES credentials(id) ON DELETE CASCADE,
    snug_mitten BOOLEAN NOT NULL,
    silly_shore BOOLEAN NOT NULL,
    updated_at  BIGINT  NOT NULL
);
