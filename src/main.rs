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
#[command(name = "luban", version, about = "Claude Code authorization proxy")]
struct Cli {
    /// Web service bind address (0.0.0.0 is reachable from the network; use 127.0.0.1 for local-only).
    #[arg(long, default_value = "0.0.0.0")]
    host: String,
    /// Web service port (used when running without a subcommand).
    #[arg(long, default_value_t = 4600)]
    port: u16,
    /// API key used by clients such as Claude Code; also available through LUBAN_API_KEY.
    /// If unset, the proxy does not authenticate callers; use it only on a trusted local network.
    #[arg(long, env = "LUBAN_API_KEY")]
    api_key: Option<String>,
    /// Admin console password; also available through LUBAN_ADMIN_PASSWORD.
    /// Once set, admin APIs require authentication. A CLI or environment value takes precedence and makes the web setting read-only.
    #[arg(long, env = "LUBAN_ADMIN_PASSWORD")]
    admin_password: Option<String>,
    /// Open a browser after startup (off by default).
    #[arg(long)]
    open: bool,
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// List all saved credentials.
    Status,
    /// Remove all saved credentials.
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
        println!(
            "No credentials saved. Run `luban` without a subcommand to open the web UI and add an account."
        );
        return Ok(());
    }
    println!(
        "Saved credentials ({}; database: {}):",
        list.len(),
        CredentialStore::db_path()?.display()
    );
    for c in &list {
        let state = if c.disabled {
            "disabled".to_string()
        } else if c.expires_in_secs() == 0 {
            "expired (refreshes automatically)".to_string()
        } else {
            format!("active; {} min remaining", c.expires_in_secs() / 60)
        };
        println!("  #{:<3} [P{}] {:<16} {}", c.id, c.priority, c.label, state);
    }
    Ok(())
}

/// 清空所有凭证。
fn logout(store: &CredentialStore) -> Result<()> {
    let n = store.clear()?;
    if n > 0 {
        let noun = if n == 1 { "credential" } else { "credentials" };
        println!("Cleared {n} {noun}, including associated device bindings and usage history.");
    } else {
        println!("No credentials to clear.");
    }
    Ok(())
}
