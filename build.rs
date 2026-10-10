//! `sqlx::migrate!` 在编译期把 `migrations/` 编进二进制，但 cargo 不知道这个依赖：只加了一个
//! 新的迁移文件、Rust 代码没动的话，增量编译不会重新展开宏，产出的二进制里缺这条迁移。
//! 这里告诉 cargo 目录变了就重编。
fn main() {
    println!("cargo:rerun-if-changed=migrations");
}
