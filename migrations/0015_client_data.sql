-- 服务端 bootstrap（`GET /api/claude_cli/bootstrap?model=…`）回包里的 `client_data`，按（号, 模型）记最近一份。
--
-- 官方客户端把它按槽位存在本机配置里（`clientDataCacheSlots`），进程一启动就读得到；luban 原先只记在内存，
-- 重启后到这个号这个模型下一次握手之前都是空的：`<total_tokens>` 那档（`tengu_lapis_anchor`）退回默认，
-- 进行中的会话中途换档、整段缓存重写。启动时整表读回内存，回包与上次记下的不同才写。
--
-- model 是 bootstrap 的 `model` 参数（规范名，小写）；data 是 `client_data` 那个对象的 JSON 文本（回包里没有
-- 这一项记 `{}`）。外键级联：号删了一并没了，后台写入撞上刚删的号会被外键拒掉。
CREATE TABLE client_data (
    cred_id    BIGINT NOT NULL REFERENCES credentials(id) ON DELETE CASCADE,
    model      TEXT   NOT NULL,
    data       TEXT   NOT NULL,
    updated_at BIGINT NOT NULL,
    PRIMARY KEY (cred_id, model)
);
