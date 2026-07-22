//! luban —— Claude Code 授权代理。
//!
//! 当前实现「登录授权 + 多凭证管理」：通过 Claude Code 的 OAuth 流程用订阅账号登录，
//! 多个账号的 access/refresh token 存于 SQLite。后续在此基础上加转发代理（`serve`）。

mod admin_ui;
mod config;
mod credentials;
mod oauth;
mod store;
mod web;

use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};

use store::CredentialStore;

#[derive(Parser)]
#[command(name = "luban", version, about = "Claude Code 授权代理")]
struct Cli {
    /// 网页服务监听地址（容器内用 0.0.0.0）
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    /// 网页服务监听端口（默认命令时生效）
    #[arg(long, default_value_t = 4600)]
    port: u16,
    /// 不自动打开浏览器
    #[arg(long)]
    no_open: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// 列出所有已保存的凭证
    Status,
    /// 清空所有已保存的凭证
    Logout,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let store = Arc::new(CredentialStore::open_default()?);

    match cli.command {
        // 不带子命令：直接启动网页服务。
        None => web::run(&cli.host, cli.port, !cli.no_open, store).await,
        Some(Command::Status) => status(&store),
        Some(Command::Logout) => logout(&store),
    }
}

/// 列出所有凭证。
fn status(store: &CredentialStore) -> Result<()> {
    let list = store.list()?;
    if list.is_empty() {
        println!("暂无凭证，运行 `luban`（无参数）打开网页添加账号。");
        return Ok(());
    }
    println!("共 {} 个凭证（库：{}）：", list.len(), CredentialStore::db_path()?.display());
    for c in &list {
        let state = if c.disabled {
            "已停用".to_string()
        } else if c.expires_in_secs() == 0 {
            "已过期(将自动刷新)".to_string()
        } else {
            format!("有效 剩余 {} 分钟", c.expires_in_secs() / 60)
        };
        println!("  #{:<3} [P{}] {:<16} {}", c.id, c.priority, c.label, state);
    }
    Ok(())
}

/// 清空所有凭证。
fn logout(store: &CredentialStore) -> Result<()> {
    let n = store.clear()?;
    if n > 0 {
        println!("已清空 {} 个凭证。", n);
    } else {
        println!("当前没有凭证，无需操作。");
    }
    Ok(())
}
