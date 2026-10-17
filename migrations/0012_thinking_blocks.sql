-- 经 luban 转发的回复里，每个 thinking 块的签名、redacted_thinking 块的密文由哪个号签发。
--
-- 上游以「签名 / 密文无效」拒绝历史思考块时，拿请求里那些块的指纹来查签发者：签发自别的号
-- 即会话中途换过号，降级重试救活；全部签发自当前号则是客户端自己改坏了，原样回 400（与 API
-- Key 直连一致）。按会话键找换号痕迹不可靠——不带会话 id 的来访键是前缀指纹，system 里的时间
-- 一变键就变，新键下查不到旧号。
--
-- fp 是签名 / 密文串的 SHA-256 前 32 个十六进制字符；同一指纹只记首次签发者。cred_id 为 NULL
-- 表示「外来」：签发者不是现有任何一个号（降级重试救活过的那段历史就这么记，之后续轮照常恢复）。
-- 外键级联：号删了它签发过的登记一并没了，重新导入拿到新 id 不会把旧登记认成「别的号」；
-- 后台异步登记撞上刚删掉的号会被外键拒掉，不会写回已删除的 id。保留 90 天（比流水的 30 天长）。
CREATE TABLE thinking_blocks (
    fp      TEXT   PRIMARY KEY,
    cred_id BIGINT REFERENCES credentials(id) ON DELETE CASCADE,
    ts      BIGINT NOT NULL
);
CREATE INDEX idx_thinking_blocks_ts ON thinking_blocks (ts);
CREATE INDEX idx_thinking_blocks_cred ON thinking_blocks (cred_id);
