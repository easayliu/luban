//! 本地配置文件。按顺序找，用找到的第一个（不合并）：
//!
//! 1. 当前目录的 `luban.toml`：在仓库里 `cargo run` 用的就是它（已在 `.gitignore` 里，不入库）；
//! 2. 数据目录的 `config.toml`（默认 `~/.luban/config.toml`，跟着 `LUBAN_HOME` 走）：装好的
//!    `luban` 在哪个目录下启动都读得到。
//!
//! 省得每次都在 shell 里 export 一串环境变量。可选：都不存在就当全空。每一项都和同名的命令行
//! 参数、环境变量对应，**命令行 > 环境变量 > 配置文件**；Docker 部署照旧用环境变量。
//!
//! ```toml
//! database_url = "postgres://postgres@localhost/luban"
//! # api_key = "..."
//! # admin_password = "..."
//! # viewer_password = "..."
//! ```
//!
//! 文件里写了密码与连库地址，权限按 0600 处理：别人读得到时启动日志里提醒一句。

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

/// 当前目录下的配置文件名。
const LOCAL_FILE_NAME: &str = "luban.toml";

/// 数据目录下的配置文件名。
const FILE_NAME: &str = "config.toml";

/// 配置文件里能写的项。未知的键直接报错：拼错了的键悄悄不生效，比报错更难查。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    /// 同 `--database-url` / `LUBAN_DATABASE_URL`。
    pub database_url: Option<String>,
    /// 同 `--api-key` / `LUBAN_API_KEY`。
    pub api_key: Option<String>,
    /// 同 `--admin-password` / `LUBAN_ADMIN_PASSWORD`。
    pub admin_password: Option<String>,
    /// 同 `--viewer-password` / `LUBAN_VIEWER_PASSWORD`。
    pub viewer_password: Option<String>,
}

impl ConfigFile {
    /// 要读的配置文件：当前目录有 `luban.toml` 就是它，否则是数据目录的 `config.toml`（不论
    /// 存在与否，报错时告诉人该往哪写）。
    pub fn path() -> Result<PathBuf> {
        let local = PathBuf::from(LOCAL_FILE_NAME);
        if local.is_file() {
            return Ok(local);
        }
        Ok(crate::store::db::data_dir()?.join(FILE_NAME))
    }

    /// 读配置文件（见 [`Self::path`]）；不存在回全空。
    pub fn load() -> Result<Self> {
        let path = Self::path()?;
        let file = Self::load_from(&path)?;
        if path.is_file() {
            tracing::info!(path = %path.display(), "loaded the config file");
        }
        Ok(file)
    }

    fn load_from(path: &Path) -> Result<Self> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e).with_context(|| format!("failed to read {}", path.display())),
        };
        warn_if_readable_by_others(path);
        toml::from_str(&raw).with_context(|| format!("invalid config file {}", path.display()))
    }
}

#[cfg(unix)]
fn warn_if_readable_by_others(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    if let Ok(meta) = std::fs::metadata(path)
        && meta.permissions().mode() & 0o077 != 0
    {
        tracing::warn!(
            path = %path.display(),
            "the config file may hold passwords and is readable by other users; run chmod 600 on it"
        );
    }
}

#[cfg(not(unix))]
fn warn_if_readable_by_others(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_empty_and_keys_parse() {
        let dir = std::env::temp_dir().join(format!("luban-config-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        let _ = std::fs::remove_file(&path);
        assert!(ConfigFile::load_from(&path).unwrap().database_url.is_none());

        std::fs::write(&path, "database_url = \"postgres://localhost/luban\"\napi_key = \"k\"\n")
            .unwrap();
        let c = ConfigFile::load_from(&path).unwrap();
        assert_eq!(c.database_url.as_deref(), Some("postgres://localhost/luban"));
        assert_eq!(c.api_key.as_deref(), Some("k"));
        assert!(c.admin_password.is_none());

        std::fs::write(&path, "databse_url = \"x\"\n").unwrap();
        assert!(ConfigFile::load_from(&path).is_err(), "拼错的键要报错，不能悄悄不生效");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
