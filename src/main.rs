//! luban —— Claude Code 授权代理。
//!
//! 当前实现「登录授权 + 多凭证管理」：通过 Claude Code 的 OAuth 流程用订阅账号登录，
//! 多个账号的 access/refresh token 存于 SQLite。后续在此基础上加转发代理（`serve`）。

mod admin_ui;
mod auth;
mod clients;
mod config;
mod config_file;
mod credentials;
mod oauth;
mod pricing;
mod proxy;
mod store;
mod telemetry;
mod web;

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use store::CredentialStore;

/// 命令行参数。每一项也能写在数据目录的 `config.toml` 里（见 [`config_file`]），
/// 命令行 > 环境变量 > 配置文件。
#[derive(Parser)]
#[command(name = "luban", version, about = "Claude Code authorization proxy")]
struct Cli {
    /// Web service bind address (0.0.0.0 is reachable from the network; use 127.0.0.1 for local-only).
    #[arg(long, default_value = "0.0.0.0")]
    host: String,
    /// Web service port (used when running without a subcommand).
    #[arg(long, default_value_t = 4600)]
    port: u16,
    /// PostgreSQL connection URL, e.g. postgres://user:password@localhost/luban; also available
    /// through LUBAN_DATABASE_URL or `database_url` in the config file.
    #[arg(long, env = "LUBAN_DATABASE_URL")]
    database_url: Option<String>,
    /// API key used by clients such as Claude Code; also available through LUBAN_API_KEY or `api_key` in the config file.
    /// If unset, only access keys created in the console are accepted; with none, every forwarded request is rejected.
    #[arg(long, env = "LUBAN_API_KEY")]
    api_key: Option<String>,
    /// Admin console password; also available through LUBAN_ADMIN_PASSWORD or `admin_password` in the config file.
    /// Admin APIs reject every request until a password is set, either here or in the console with the setup token printed in the log.
    /// A value set here, in the environment or in the config file takes precedence and makes the web setting read-only.
    #[arg(long, env = "LUBAN_ADMIN_PASSWORD")]
    admin_password: Option<String>,
    /// Read-only viewer password; also available through LUBAN_VIEWER_PASSWORD or `viewer_password` in the config file.
    /// Viewers can browse the console but cannot change anything. Only takes effect once an admin password is set.
    /// A value set here, in the environment or in the config file takes precedence and makes the web setting read-only.
    #[arg(long, env = "LUBAN_VIEWER_PASSWORD")]
    viewer_password: Option<String>,
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
    let file = config_file::ConfigFile::load()?;
    let non_blank = |v: Option<String>| v.filter(|k| !k.trim().is_empty());
    let database_url =
        non_blank(cli.database_url).or(non_blank(file.database_url)).with_context(|| {
            format!(
                "no PostgreSQL connection URL: set database_url in ./luban.toml or {} (e.g. \
             database_url = \"postgres://user:password@localhost/luban\"), or pass \
             --database-url / LUBAN_DATABASE_URL. With Docker, use the docker-compose.yml from \
             the repository (or rerun install.sh), which adds a postgres service",
                config_file::ConfigFile::path()
                    .map(|p| p.display().to_string())
                    .unwrap_or_default()
            )
        })?;
    let store = Arc::new(store::db::open(&database_url).await?);

    match cli.command {
        // 不带子命令：直接启动网页服务 + 转发代理。
        None => {
            let api_key = non_blank(cli.api_key).or(non_blank(file.api_key));
            let admin_password = non_blank(cli.admin_password).or(non_blank(file.admin_password));
            let viewer_password =
                non_blank(cli.viewer_password).or(non_blank(file.viewer_password));
            web::run(&cli.host, cli.port, cli.open, store, api_key, admin_password, viewer_password)
                .await
        }
        Some(Command::Status) => status(&store, &database_url).await,
        Some(Command::Logout) => logout(&store).await,
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
async fn status(store: &CredentialStore, database_url: &str) -> Result<()> {
    let list = store.list().await?;
    if list.is_empty() {
        println!(
            "No credentials saved. Run `luban` without a subcommand to open the web UI and add an account."
        );
        return Ok(());
    }
    println!(
        "Saved credentials ({}; database: {}):",
        list.len(),
        store::db::describe_url(database_url)
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
async fn logout(store: &CredentialStore) -> Result<()> {
    let n = store.clear().await?;
    if n > 0 {
        let noun = if n == 1 { "credential" } else { "credentials" };
        println!("Cleared {n} {noun}, including associated device bindings and usage history.");
    } else {
        println!("No credentials to clear.");
    }
    Ok(())
}
