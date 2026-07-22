//! 凭证记录模型与刷新判定。持久化在 SQLite，见 [`crate::store`]。

use std::time::{SystemTime, UNIX_EPOCH};

use crate::config;

/// 一条 Claude OAuth 凭证（对应 SQLite 一行）。
#[derive(Debug, Clone)]
pub struct Credential {
    pub id: i64,
    /// 用户可编辑的显示名（如账号备注）。
    pub label: String,
    /// 账号等级（Max / Pro / Free 等），可能未知。
    pub tier: Option<String>,
    pub access_token: String,
    pub refresh_token: String,
    /// access_token 过期的 Unix 时间戳（秒）。
    pub expires_at: u64,
    /// 优先级：数值小者优先（供后续代理轮换选择）。
    pub priority: i64,
    /// 是否停用（停用的凭证不参与转发）。
    pub disabled: bool,
    /// 允许绑定的设备数上限；`<= 0` 表示不限。见 [`crate::store`] 的粘性绑定选择。
    pub device_limit: i64,
    /// 自动检测到的上游账号级错误原因（如封号）；`None` 表示未被自动停用
    /// （手动停用或未停用皆为 `None`）。见 [`crate::store::CredentialStore::mark_banned`]。
    pub ban_reason: Option<String>,
    pub created_at: u64,
    pub updated_at: u64,
}

impl Credential {
    /// 距离过期的剩余秒数（已过期返回 0）。
    pub fn expires_in_secs(&self) -> u64 {
        self.expires_at.saturating_sub(now_secs())
    }

    /// 是否已过期或即将过期（进入刷新窗口）。
    pub fn needs_refresh(&self) -> bool {
        self.expires_in_secs() <= config::REFRESH_LEEWAY_SECS
    }
}

/// 当前 Unix 时间戳（秒）。
pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
