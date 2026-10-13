-- 刷新 token 时要不要省掉 `scope`：登录只授了一小撮的号，刷新带官方那五项会被上游回
-- `invalid_scope`，luban 回退成不带 scope 的一条（沿用原授权）。回退成功一次就记在这里，
-- 之后这个号刷新直接发不带 scope 的，不再每次先吃一个 400。重新登录（换了授权）时复位。
ALTER TABLE credentials ADD COLUMN refresh_omit_scope BOOLEAN NOT NULL DEFAULT FALSE;
