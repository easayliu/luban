//! luban —— Claude Code 授权代理。
//!
//! 当前实现「登录授权 + 多凭证管理」：通过 Claude Code 的 OAuth 流程用订阅账号登录，
//! 多个账号的 access/refresh token 存于 SQLite。后续在此基础上加转发代理（`serve`）。

mod admin_ui;
mod auth;
mod config;
mod credentials;
mod oauth;
mod pricing;
mod proxy;
mod store;
mod web;

use std::sync::Arc;

use anyhow::Result;
use clap::{Parser, Subcommand};

use store::CredentialStore;

#[derive(Parser)]
#[command(name = "luban", version, about = "Claude Code 授权代理")]
struct Cli {
    /// 网页服务监听地址（默认 0.0.0.0 对外可达；仅本机可设 127.0.0.1）
    #[arg(long, default_value = "0.0.0.0")]
    host: String,
    /// 网页服务监听端口（默认命令时生效）
    #[arg(long, default_value_t = 4600)]
    port: u16,
    /// 接入用的 API Key（Claude Code 侧填此值）；也可用环境变量 LUBAN_API_KEY。
    /// 不设置则代理不校验来访身份（请仅本机使用）。
    #[arg(long, env = "LUBAN_API_KEY")]
    api_key: Option<String>,
    /// 管理界面登录密码；也可用环境变量 LUBAN_ADMIN_PASSWORD。
    /// 设置后（含网页设置）管理接口需鉴权；此参数会接管、网页只读。
    #[arg(long, env = "LUBAN_ADMIN_PASSWORD")]
    admin_password: Option<String>,
    /// 启动后自动打开浏览器（默认不打开）
    #[arg(long)]
    open: bool,
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
    init_logging();
    let cli = Cli::parse();
    let store = Arc::new(CredentialStore::open_default()?);

    match cli.command {
        // 不带子命令：直接启动网页服务 + 转发代理。
        None => {
            let api_key = cli.api_key.filter(|k| !k.trim().is_empty());
            let admin_password = cli.admin_password.filter(|k| !k.trim().is_empty());
            web::run(&cli.host, cli.port, cli.open, store, api_key, admin_password).await
        }
        Some(Command::Status) => status(&store),
        Some(Command::Logout) => logout(&store),
    }
}

/// 初始化日志：本地时间、干净格式、非终端自动关 ANSI 颜色。
/// 默认 info 级，`RUST_LOG` 可覆盖（如 `RUST_LOG=luban=debug`）。
fn init_logging() {
    use std::io::IsTerminal;
    use tracing_subscriber::{EnvFilter, fmt::time::ChronoLocal};
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_timer(ChronoLocal::new("%Y-%m-%d %H:%M:%S%.3f".to_owned()))
        .with_target(false)
        .with_ansi(std::io::stdout().is_terminal())
        .init();
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
