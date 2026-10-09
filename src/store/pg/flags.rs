//! 转发形态开关的 PG 版，对应 `store::flags`。
//!
//! 旧模块里只有 [`super::super::ForwardFlags`] 这个纯内存类型（字段、默认值与两个派生判定），
//! 没有任何碰库的代码，直接复用；从设置缓存读齐全部开关的 `forward_flags` 在 `pg/settings.rs`。
