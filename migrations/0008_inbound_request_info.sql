-- 走模拟的请求的来访信息：流水的 shape 记的是出站体，来访带了什么头、体长什么样只在改写前存在，
-- 判「为什么被当成非官方」时没处可查。只给走模拟的请求记，不走模拟的留空。
-- inbound_headers：来访头（JSON 对象，去掉凭证类头）；
-- inbound_shape：来访体的结构摘要（同 shape 的格式，另带 billing header / 身份句有无）。
ALTER TABLE usage_logs ADD COLUMN inbound_headers TEXT;
ALTER TABLE usage_logs ADD COLUMN inbound_shape TEXT;
