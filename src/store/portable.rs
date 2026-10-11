//! 迁移：导出 / 导入凭证、代理池、接入 Key 与设置。
//!
//! 导出与导入共用 [`PortableCredential`] 等形态：**导出的文件原样喂回来就是导入的入参**。
//! 从 SQLite 时代的旧版搬数据也走这一条路（旧版导出、新版导入）。

use super::*;

/// 迁移用的代理池条目：只保留 label 和 url，id 由目标库自己发。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PortableProxy {
    #[serde(default)]
    pub label: String,
    pub url: String,
}

impl From<&SavedProxy> for PortableProxy {
    fn from(p: &SavedProxy) -> Self {
        Self { label: p.label.clone(), url: p.url.clone() }
    }
}

/// 迁移用的一把接入 Key：名称、明文、停用状态与范围。
///
/// 范围按**分组名**带：分组 id 由各库自己发，分组本身也不随迁移走，导入时按名字对上目标库
/// 里的分组。对不全（或是不带范围的旧版文件）就以停用状态导入，等 admin 在目标站重新绑——
/// 绝不退成「全部号」：原本只限几个分组的 Key 迁移一趟就能用全部号，等于凭空放大了权限。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PortableApiKey {
    #[serde(default)]
    pub label: String,
    pub key: String,
    #[serde(default)]
    pub disabled: bool,
    /// 可用全部号。`None` 是不带范围的旧版导出文件。
    #[serde(default)]
    pub all_groups: Option<bool>,
    /// 只限这些分组（按优先顺序），`all_groups == Some(false)` 时才看。
    #[serde(default)]
    pub groups: Vec<String>,
}

/// 迁移用的一条凭证：导出与导入**共用同一个形态**，导出的文件原样喂回来就是导入的入参。
///
/// 刻意不带的三类字段：
/// - `id` / `created_at` / `updated_at`：id 由目标库自己发（[`CredentialStore::import_credential`]
///   按账号身份匹配，不认 id），时间戳属于「这条记录在这个库里的历史」，搬过去只会造出一份
///   假的过去；
/// - 用量、绑定、账本（`usage_logs`/`device_bindings`/`credential_stats`/`device_costs`）：
///   费用与额度快照是**按 cred_id 关联**的历史，跟着账号搬过去会与目标库自己的流水混在一起，
///   而设备绑定压根是「哪台机器绑在哪个号上」的本机状态，换台机器毫无意义；
/// - 管理密码：见 [`CredentialStore::settings_snapshot`]。
///
/// 全字段都给了 `#[serde(default)]`：迁移文件是会被人手改的（删掉几个号、改个优先级），
/// 少一个字段就整份导入失败太脆。缺 `expires_at` 退化成 0，即「已过期」——首次使用时用
/// refresh_token 换一份新的，正是想要的行为。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PortableCredential {
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub tier: Option<String>,
    #[serde(default)]
    pub org_type: Option<String>,
    /// 额度档原值（`default_claude_max_5x`）；statsig eval 的 `rateLimitTier` 要它。
    /// 不带上的话，迁移后到下一次成功拉 profile 之前，那个字段会一直缺着。
    #[serde(default)]
    pub rate_limit_tier: Option<String>,
    pub access_token: String,
    pub refresh_token: String,
    #[serde(default)]
    pub expires_at: u64,
    /// 缺省（`None`）落默认档 [`PRIORITY_DEFAULT`]，不能让 serde 填 0 变成最高档 P0。
    #[serde(default)]
    pub priority: Option<i64>,
    #[serde(default)]
    pub disabled: bool,
    #[serde(default)]
    pub device_limit: i64,
    #[serde(default)]
    pub rpm_limit: i64,
    #[serde(default)]
    pub ban_reason: Option<String>,
    #[serde(default)]
    pub account_uuid: Option<String>,
    /// 组织 UUID 与订阅创建时刻原串，同 `rate_limit_tier` 的道理：不带上的话，迁移后到
    /// 下一次成功拉 profile 之前，遥测 `auth.organization_uuid` 与 eval 的
    /// `subscriptionCreatedAt` 会一直缺着。
    #[serde(default)]
    pub org_uuid: Option<String>,
    #[serde(default)]
    pub subscription_created_at: Option<String>,
    /// 组织名称、席位档、订阅状态、超额用量开关：只给后台看，带上免得迁移后到下次刷新前空着。
    #[serde(default)]
    pub org_name: Option<String>,
    #[serde(default)]
    pub seat_tier: Option<String>,
    #[serde(default)]
    pub subscription_status: Option<String>,
    #[serde(default)]
    pub extra_usage_enabled: Option<bool>,
    #[serde(default)]
    pub resume_at: Option<u64>,
    #[serde(default)]
    pub proxy: Option<String>,
    /// 逐账号的提前停调度阈值（5h / 7d 两档）；`None` 跟随全局。见
    /// [`Credential::quota_pause_pct`]。
    #[serde(default)]
    pub quota_pause_pct: Option<i64>,
    #[serde(default)]
    pub quota_pause_pct_7d: Option<i64>,
    /// 模拟会话数上限，三态同 `device_limit`；旧导出没有这一项时按 0（跟随全局）。
    #[serde(default)]
    pub session_limit: i64,
}

impl From<&Credential> for PortableCredential {
    fn from(c: &Credential) -> Self {
        Self {
            label: c.label.clone(),
            tier: c.tier.clone(),
            org_type: c.org_type.clone(),
            rate_limit_tier: c.rate_limit_tier.clone(),
            access_token: c.access_token.clone(),
            refresh_token: c.refresh_token.clone(),
            expires_at: c.expires_at,
            priority: Some(c.priority),
            disabled: c.disabled,
            device_limit: c.device_limit,
            rpm_limit: c.rpm_limit,
            ban_reason: c.ban_reason.clone(),
            account_uuid: c.account_uuid.clone(),
            org_uuid: c.org_uuid.clone(),
            subscription_created_at: c.subscription_created_at.clone(),
            org_name: c.org_name.clone(),
            seat_tier: c.seat_tier.clone(),
            subscription_status: c.subscription_status.clone(),
            extra_usage_enabled: c.extra_usage_enabled,
            resume_at: c.resume_at,
            proxy: c.proxy.clone(),
            quota_pause_pct: c.quota_pause_pct,
            quota_pause_pct_7d: c.quota_pause_pct_7d,
            session_limit: c.session_limit,
        }
    }
}

/// 导入一条凭证的结果：目标库里原本没有这个账号（`Added`），还是已经有、被这条覆盖了
/// （`Updated`）。调用方据此报「新增 N 个、更新 M 个」——迁移最想知道的就是这两个数。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImportOutcome {
    Added,
    Updated,
}

use std::collections::HashMap;

use anyhow::{Context, Result};
use sqlx::PgConnection;

use super::CredentialStore;
use super::{
    CLIENT_API_KEY, CONSOLE_AUTH_KEYS, DEPLOYMENT_ONLY_KEYS, Scope, seal, token_fingerprint,
};
use crate::credentials::{PRIORITY_DEFAULT, PRIORITY_MAX, PRIORITY_MIN};

/// 导入时按账号找目标行：同一个账号 UUID 下按组织 UUID 认。
///
/// - 组织 UUID 两边都有且相等 → 就是它；
/// - 导入的有组织 UUID、库里没有同组织的 → 退回库里这个账号下**唯一一条**组织 UUID 还空着的
///   行（旧号还没回填），有多条说不清是哪个就不认；
/// - 导入的没有组织 UUID → 库里这个账号只有一行才认它，多行（个人 + 团队）说不清就不认。
///
/// 不认的交给调用方按 refresh_token 兜底，再不中就新增——宁可多一行，也不要拿一个订阅的
/// 状态覆盖掉另一个订阅。
async fn match_import_target(
    conn: &mut PgConnection,
    account_uuid: &str,
    org_uuid: Option<&str>,
) -> Result<Option<i64>> {
    let rows: Vec<(i64, Option<String>)> = sqlx::query_as(
        "SELECT id, NULLIF(TRIM(COALESCE(org_uuid, '')), '') FROM credentials \
          WHERE account_uuid = $1",
    )
    .bind(account_uuid)
    .fetch_all(conn)
    .await?;
    let only = |v: Vec<i64>| if v.len() == 1 { Some(v[0]) } else { None };
    Ok(match org_uuid {
        Some(org) => {
            if let Some((id, _)) = rows.iter().find(|(_, o)| o.as_deref() == Some(org)) {
                Some(*id)
            } else {
                only(rows.iter().filter(|(_, o)| o.is_none()).map(|(id, _)| *id).collect())
            }
        }
        None => only(rows.iter().map(|(id, _)| *id).collect()),
    })
}

impl CredentialStore {
    /// 导出全部凭证的可迁移形态，顺序同 `list`（priority, id）。
    ///
    /// **含明文 access/refresh token**——迁移要的就是它们，脱敏过的导出等于没导。谁能调到
    /// 这个口子就等于拿到了这些账号，故接口侧另加了一道闸（见 `crate::web` 的 `export`）。
    pub async fn export_credentials(&self) -> Result<Vec<PortableCredential>> {
        Ok(self.list().await?.iter().map(PortableCredential::from).collect())
    }

    /// 导出代理池的可迁移形态。
    pub async fn export_proxies(&self) -> Result<Vec<PortableProxy>> {
        Ok(self.list_proxies(Scope::All).await?.iter().map(PortableProxy::from).collect())
    }

    /// 导入一条代理：admin 名下已有这个 URL 则更新 label，没有则新增。返回是 Added 还是 Updated。
    pub async fn import_proxy(&self, p: &PortableProxy) -> Result<ImportOutcome> {
        anyhow::ensure!(!p.url.is_empty(), "proxy URL must not be empty");
        let mut tx = self.begin_write().await?;
        let existing: Option<i64> = sqlx::query_scalar(
            "SELECT id FROM proxies WHERE url = $1 \
                AND owner_id = (SELECT id FROM users WHERE role = 'admin')",
        )
        .bind(&p.url)
        .fetch_optional(&mut *tx)
        .await?;
        let outcome = match existing {
            Some(id) => {
                sqlx::query("UPDATE proxies SET label = $2 WHERE id = $1")
                    .bind(id)
                    .bind(&p.label)
                    .execute(&mut *tx)
                    .await?;
                ImportOutcome::Updated
            }
            None => {
                sqlx::query(
                    "INSERT INTO proxies (label, url, owner_id) \
                     VALUES ($1, $2, (SELECT id FROM users WHERE role = 'admin'))",
                )
                .bind(&p.label)
                .bind(&p.url)
                .execute(&mut *tx)
                .await?;
                ImportOutcome::Added
            }
        };
        tx.commit().await?;
        Ok(outcome)
    }

    /// 可迁移的设置快照（`settings` 全表），**去掉管理密码**与只属于本部署的键。
    ///
    /// 管理密码是「谁能进这台机器的控制台」，属于部署本身而不是被迁移的配置：把源站的口令
    /// 悄悄盖到目标站上，等于一次导入顺手改掉了目标站的登录方式，而做导入的人未必知道自己
    /// 改了这个。接入 key（[`CLIENT_API_KEY`]）反过来**要带**：它是客户端侧配好的东西，
    /// 迁移后不跟着走，所有客户端都得重配一遍。
    pub fn settings_snapshot(&self) -> HashMap<String, String> {
        let mut out = self.settings.read().clone();
        for k in CONSOLE_AUTH_KEYS.iter().chain(DEPLOYMENT_ONLY_KEYS) {
            out.remove(*k);
        }
        out
    }

    /// 导入一条凭证：目标库已有这个账号就整行覆盖，没有就新增。
    ///
    /// **匹配顺序是「账号 UUID + 组织 UUID」优先、`refresh_token` 兜底**，这个先后有实际后果：同一个
    /// 账号在源站重新授权过之后 refresh_token 已经是新值，只按 token 匹配会把它当成一个新
    /// 账号插进去，目标库里同一个账号出现两行（两行还会各自去刷新同一个上游账号）。反过来，
    /// 老库里可能有 `account_uuid` 还没拉到的号（profile 没取成功），故 token 这条兜底不能去。
    ///
    /// 只按账号 UUID 不够：同一个人可以既有个人订阅、又在团队里占一个席位，两次授权拿到的是
    /// **同一个** `account_uuid`、不同的 `org_uuid`，在库里是两行。只认账号的话，导入时后一行会
    /// 把前一行整行覆盖掉。见 [`match_import_target`]。
    ///
    /// 命中后是**整行覆盖**而不是只更新 token：迁移文件是源站此刻的完整状态，优先级、设备
    /// 上限、代理这些都是操作者在源站上调好的。
    ///
    /// 「找目标行、再决定插还是改」在串行化事务里做：两条同账号的导入并发进来时不会各插一行。
    pub async fn import_credential(&self, c: &PortableCredential) -> Result<ImportOutcome> {
        if c.access_token.trim().is_empty() || c.refresh_token.trim().is_empty() {
            anyhow::bail!("credential has an empty access_token or refresh_token");
        }
        // 空串的 uuid 当没有：老库里存过空串，拿它去匹配会把所有这类号连成一个。
        let uuid = c.account_uuid.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let org = c.org_uuid.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let proxy = c.proxy.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let priority = c.priority.map_or(PRIORITY_DEFAULT, |p| p.clamp(PRIORITY_MIN, PRIORITY_MAX));
        let mut tx = self.begin_write().await?;
        let by_account = match uuid {
            Some(u) => match_import_target(&mut tx, u, org).await?,
            None => None,
        };
        let existing: Option<i64> = match by_account {
            Some(id) => Some(id),
            None => {
                sqlx::query_scalar("SELECT id FROM credentials WHERE refresh_token_hash = $1")
                    .bind(token_fingerprint(&c.refresh_token))
                    .fetch_optional(&mut *tx)
                    .await?
            }
        };
        let outcome = match existing {
            Some(id) => {
                sqlx::query(
                    "UPDATE credentials SET
                         label = $2, tier = $3, org_type = $4, access_token = $5,
                         refresh_token = $6, expires_at = $7, priority = $8, disabled = $9,
                         device_limit = $10, rpm_limit = $11, ban_reason = $12,
                         account_uuid = $13, resume_at = $14, proxy = $15,
                         rate_limit_tier = $16, org_uuid = $17, subscription_created_at = $18,
                         quota_pause_pct = $19, quota_pause_pct_7d = $20, session_limit = $21,
                         org_name = $22, seat_tier = $23, subscription_status = $24,
                         extra_usage_enabled = $25, refresh_token_hash = $26,
                         updated_at = unixepoch()
                     WHERE id = $1",
                )
                .bind(id)
                .bind(&c.label)
                .bind(&c.tier)
                .bind(&c.org_type)
                .bind(seal(&c.access_token))
                .bind(seal(&c.refresh_token))
                .bind(c.expires_at as i64)
                .bind(priority)
                .bind(c.disabled as i64)
                .bind(c.device_limit)
                .bind(c.rpm_limit)
                .bind(&c.ban_reason)
                .bind(uuid)
                .bind(c.resume_at.map(|t| t as i64))
                .bind(proxy)
                .bind(&c.rate_limit_tier)
                .bind(&c.org_uuid)
                .bind(&c.subscription_created_at)
                .bind(c.quota_pause_pct)
                .bind(c.quota_pause_pct_7d)
                .bind(c.session_limit)
                .bind(&c.org_name)
                .bind(&c.seat_tier)
                .bind(&c.subscription_status)
                .bind(c.extra_usage_enabled.map(i64::from))
                .bind(token_fingerprint(&c.refresh_token))
                .execute(&mut *tx)
                .await
                .context("failed to update the existing credential")?;
                ImportOutcome::Updated
            }
            None => {
                sqlx::query(
                    "INSERT INTO credentials
                         (label, tier, org_type, access_token, refresh_token, expires_at,
                          priority, disabled, device_limit, rpm_limit, ban_reason,
                          account_uuid, resume_at, proxy, rate_limit_tier, org_uuid,
                          subscription_created_at, quota_pause_pct, quota_pause_pct_7d,
                          session_limit, org_name, seat_tier, subscription_status,
                          extra_usage_enabled, refresh_token_hash, owner_id)
                     VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12, $13, $14, $15,
                             $16, $17, $18, $19, $20, $21, $22, $23, $24, $25,
                             (SELECT id FROM users WHERE role = 'admin'))",
                )
                .bind(&c.label)
                .bind(&c.tier)
                .bind(&c.org_type)
                .bind(seal(&c.access_token))
                .bind(seal(&c.refresh_token))
                .bind(c.expires_at as i64)
                .bind(priority)
                .bind(c.disabled as i64)
                .bind(c.device_limit)
                .bind(c.rpm_limit)
                .bind(&c.ban_reason)
                .bind(uuid)
                .bind(c.resume_at.map(|t| t as i64))
                .bind(proxy)
                .bind(&c.rate_limit_tier)
                .bind(&c.org_uuid)
                .bind(&c.subscription_created_at)
                .bind(c.quota_pause_pct)
                .bind(c.quota_pause_pct_7d)
                .bind(c.session_limit)
                .bind(&c.org_name)
                .bind(&c.seat_tier)
                .bind(&c.subscription_status)
                .bind(c.extra_usage_enabled.map(i64::from))
                .bind(token_fingerprint(&c.refresh_token))
                .execute(&mut *tx)
                .await
                .context("failed to insert the credential (its refresh_token may already exist)")?;
                ImportOutcome::Added
            }
        };
        tx.commit().await?;
        Ok(outcome)
    }

    /// 导入设置：逐项写库并同步内存镜像，返回实际写入的项数。
    ///
    /// 管理密码一律跳过（口径同 [`Self::settings_snapshot`]，导出不带、导入也不认——万一有人
    /// 手工把它塞回文件里）。**只写文件里有的键**：目标库里多出来的设置保持原值，不做「以文件
    /// 为准清空其余」——那样一份手改过的、只留了几项的文件会把目标站其余配置全部重置成默认。
    pub async fn import_settings(&self, settings: &HashMap<String, String>) -> Result<usize> {
        let mut n = 0;
        for (k, v) in settings {
            if CONSOLE_AUTH_KEYS.contains(&k.as_str()) || DEPLOYMENT_ONLY_KEYS.contains(&k.as_str())
            {
                continue;
            }
            // 旧版导出文件里的全局接入 Key：转成一把不绑定分组的接入 Key（设置项本身已不再使用）。
            if k == CLIENT_API_KEY {
                if !v.trim().is_empty() {
                    // 旧版只有这一把全局 Key，本来就能用全部号，范围照实写上。
                    let key = PortableApiKey {
                        label: "导入的 Key".into(),
                        key: v.trim().into(),
                        disabled: false,
                        all_groups: Some(true),
                        groups: Vec::new(),
                    };
                    self.import_api_key(&key).await?;
                    n += 1;
                }
                continue;
            }
            self.set_setting(k, v).await?;
            n += 1;
        }
        Ok(n)
    }

    /// 导出全部接入 Key（含明文与范围）。
    pub async fn export_api_keys(&self) -> Result<Vec<PortableApiKey>> {
        let names: HashMap<i64, String> =
            self.list_groups().await?.into_iter().map(|g| (g.id, g.name)).collect();
        let mut out = Vec::new();
        for k in self.list_api_keys().await? {
            let key = self.reveal_api_key(k.id).await?.unwrap_or_default();
            let groups = k.groups.iter().filter_map(|id| names.get(id).cloned()).collect();
            out.push(PortableApiKey {
                label: k.label,
                key,
                disabled: k.disabled,
                all_groups: Some(k.all_groups),
                groups,
            });
        }
        Ok(out)
    }

    /// 导入一把接入 Key：同一把（明文相同）已在就跳过，回 `Updated`；否则按文件里的范围新增，
    /// 范围还原不全就停用（见 [`PortableApiKey`]）。停用状态与范围同一个事务落库。
    pub async fn import_api_key(&self, k: &PortableApiKey) -> Result<ImportOutcome> {
        anyhow::ensure!(!k.key.trim().is_empty(), "API key must not be empty");
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM api_keys WHERE key_hash = $1)")
                .bind(token_fingerprint(k.key.trim()))
                .fetch_one(&self.pool)
                .await?;
        if exists {
            return Ok(ImportOutcome::Updated);
        }
        let (all_groups, group_ids, complete) = match k.all_groups {
            Some(true) => (true, Vec::new(), true),
            Some(false) => {
                let ids: HashMap<String, i64> =
                    self.list_groups().await?.into_iter().map(|g| (g.name, g.id)).collect();
                let found: Vec<i64> = k.groups.iter().filter_map(|n| ids.get(n).copied()).collect();
                let complete = found.len() == k.groups.len();
                (false, found, complete)
            }
            None => (false, Vec::new(), false),
        };
        if !complete {
            tracing::warn!(
                label = %k.label.trim(), groups = ?k.groups,
                "import: the API key's group scope could not be restored; imported it disabled"
            );
        }
        self.insert_api_key(
            k.label.trim(),
            k.key.trim(),
            k.disabled || !complete,
            all_groups,
            &group_ids,
        )
        .await?
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(ImportOutcome::Added)
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::*;

    /// 旧测试里的 `store_with(&[])` 是一个全新的空库；`#[sqlx::test]` 每个测试只给一个库，
    /// 要第二个「空的目标库」时就把号清空——导入只看 `credentials`，效果相同。
    async fn wipe_credentials(store: &CredentialStore) {
        sqlx::query("DELETE FROM credential_groups").execute(&store.pool).await.unwrap();
        sqlx::query("DELETE FROM credentials").execute(&store.pool).await.unwrap();
    }

    fn base(rt: &str) -> PortableCredential {
        PortableCredential {
            label: "acct".into(),
            tier: Some("max".into()),
            org_type: None,
            rate_limit_tier: None,
            access_token: "at-1".into(),
            refresh_token: rt.into(),
            expires_at: 100,
            priority: Some(2),
            disabled: false,
            device_limit: 3,
            session_limit: 0,
            rpm_limit: 7,
            quota_pause_pct: None,
            quota_pause_pct_7d: None,
            ban_reason: None,
            account_uuid: Some("uuid-1".into()),
            org_uuid: None,
            subscription_created_at: None,
            org_name: None,
            seat_tier: None,
            subscription_status: None,
            extra_usage_enabled: None,
            resume_at: None,
            proxy: None,
        }
    }

    /// 导入时缺 priority 落默认档 P2，不被当成 0 变成 P0；带了的截在 P0..=P4。
    #[sqlx::test]
    async fn import_without_priority_lands_on_default(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let raw = |rt: &str, extra: &str| -> PortableCredential {
            serde_json::from_str(&format!(
                r#"{{"access_token":"at-{rt}","refresh_token":"{rt}"{extra}}}"#
            ))
            .unwrap()
        };
        store.import_credential(&raw("rt-a", "")).await.unwrap();
        store.import_credential(&raw("rt-b", r#","priority":0"#)).await.unwrap();
        store.import_credential(&raw("rt-c", r#","priority":7"#)).await.unwrap();
        let by_rt: HashMap<String, i64> = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .map(|c| (c.refresh_token, c.priority))
            .collect();
        assert_eq!(by_rt["rt-a"], PRIORITY_DEFAULT);
        assert_eq!(by_rt["rt-b"], PRIORITY_MIN);
        assert_eq!(by_rt["rt-c"], PRIORITY_MAX);
    }

    /// 导入的匹配顺序：`account_uuid` 优先，`refresh_token` 兜底。
    ///
    /// 第一条是这套东西的关键——账号在源站重新授权过之后 refresh_token 已经换了值，只按 token
    /// 认就会把同一个账号在目标库里变成两行，两行还各自去刷新同一个上游账号。
    #[sqlx::test]
    async fn import_matches_by_account_uuid_then_refresh_token(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        // 旧测试库里先有一个不相干的号 "a"。
        store
            .import_credential(&PortableCredential {
                account_uuid: None,
                label: "a".into(),
                ..base("refresh-a")
            })
            .await
            .unwrap();
        let base = base("rt-1");
        assert_eq!(store.import_credential(&base).await.unwrap(), ImportOutcome::Added);
        let before = store.list().await.unwrap().len();

        // 同一个账号、新的 refresh_token（源站重新授权过）→ 覆盖那一行，不新增。
        let reauthed = PortableCredential {
            refresh_token: "rt-2".into(),
            label: "acct-renamed".into(),
            priority: Some(3),
            ..base.clone()
        };
        assert_eq!(store.import_credential(&reauthed).await.unwrap(), ImportOutcome::Updated);
        assert_eq!(store.list().await.unwrap().len(), before, "同一个账号不该变成两行");
        let got = store
            .list()
            .await
            .unwrap()
            .into_iter()
            .find(|c| c.account_uuid.as_deref() == Some("uuid-1"))
            .unwrap();
        assert_eq!(got.refresh_token, "rt-2");
        assert_eq!(got.label, "acct-renamed", "命中后是整行覆盖");
        assert_eq!(got.priority, 3);

        // 没有 uuid 的号（profile 没拉到）仍能按 refresh_token 认出来——这条兜底不能少。
        let no_uuid =
            PortableCredential { account_uuid: None, refresh_token: "rt-9".into(), ..base.clone() };
        assert_eq!(store.import_credential(&no_uuid).await.unwrap(), ImportOutcome::Added);
        let again = PortableCredential { label: "by-token".into(), ..no_uuid.clone() };
        assert_eq!(store.import_credential(&again).await.unwrap(), ImportOutcome::Updated);

        // 空 token 的记录直接报错：让调用方把它计进 failed，而不是写一行用不了的号进去。
        let empty = PortableCredential { access_token: "".into(), ..base.clone() };
        assert!(store.import_credential(&empty).await.is_err());
    }

    /// 同一个人既有个人订阅又占一个团队席位：同一个账号 UUID、两个组织 UUID，导入后得是两行，
    /// 各自按组织认回自己那一行；组织 UUID 说不清时不乱认。
    #[sqlx::test]
    async fn import_matches_by_account_and_org_uuid(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let personal = PortableCredential {
            label: "personal".into(),
            tier: Some("Max 20x".into()),
            org_type: Some("claude_max".into()),
            access_token: "at-p".into(),
            refresh_token: "rt-p".into(),
            priority: Some(0),
            device_limit: 0,
            rpm_limit: 0,
            account_uuid: Some("acct".into()),
            org_uuid: Some("org-personal".into()),
            ..base("unused")
        };
        let team = PortableCredential {
            label: "team".into(),
            tier: Some("Team Standard".into()),
            org_type: Some("claude_team".into()),
            access_token: "at-t".into(),
            refresh_token: "rt-t".into(),
            org_uuid: Some("org-team".into()),
            ..personal.clone()
        };
        assert_eq!(store.import_credential(&personal).await.unwrap(), ImportOutcome::Added);
        assert_eq!(
            store.import_credential(&team).await.unwrap(),
            ImportOutcome::Added,
            "另一个组织是另一行"
        );

        // 团队那行在源站重新授权过（新 refresh_token），按组织认回团队那行，个人那行不动。
        let team2 = PortableCredential { refresh_token: "rt-t2".into(), ..team.clone() };
        assert_eq!(store.import_credential(&team2).await.unwrap(), ImportOutcome::Updated);
        let rows = store.list().await.unwrap();
        assert_eq!(rows.len(), 2);
        let by_label = |l: &str| rows.iter().find(|c| c.label == l).unwrap().clone();
        assert_eq!(by_label("team").refresh_token, "rt-t2");
        assert_eq!(by_label("personal").refresh_token, "rt-p");

        // 没带组织 UUID、账号下又有两行：说不清是哪个，不认，新增一行。
        let ambiguous =
            PortableCredential { org_uuid: None, refresh_token: "rt-x".into(), ..team.clone() };
        assert_eq!(store.import_credential(&ambiguous).await.unwrap(), ImportOutcome::Added);

        // 库里是还没回填组织 UUID 的旧号：账号下只有它一条空着的，认它。
        wipe_credentials(&store).await;
        let legacy = PortableCredential { org_uuid: None, ..personal.clone() };
        assert_eq!(store.import_credential(&legacy).await.unwrap(), ImportOutcome::Added);
        let fresh = PortableCredential { refresh_token: "rt-new".into(), ..personal.clone() };
        assert_eq!(store.import_credential(&fresh).await.unwrap(), ImportOutcome::Updated);
        assert_eq!(store.list().await.unwrap().len(), 1);
    }

    /// 导出的每一项都要能原样导回来：迁移文件就是「导出的响应原样喂给导入」，
    /// 中间掉一个字段（曾经掉过 priority）就是操作者在新机器上发现配置不对，而且很难看出来。
    #[sqlx::test]
    async fn export_round_trips_every_field(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        store
            .import_credential(&PortableCredential {
                account_uuid: None,
                label: "a".into(),
                ..base("refresh-a")
            })
            .await
            .unwrap();
        let full = PortableCredential {
            label: "full".into(),
            tier: Some("pro".into()),
            org_type: Some("claude_team".into()),
            rate_limit_tier: Some("default_claude_max_5x".into()),
            access_token: "at".into(),
            refresh_token: "rt".into(),
            expires_at: 1_800_000_000,
            priority: Some(4),
            disabled: true,
            device_limit: 6,
            session_limit: 0,
            rpm_limit: -1,
            quota_pause_pct: Some(95),
            quota_pause_pct_7d: Some(0),
            ban_reason: Some("banned upstream".into()),
            account_uuid: Some("uuid".into()),
            org_uuid: Some("09520b85-f6b6-432f-97e2-6ecb804a083f".into()),
            subscription_created_at: Some("2026-04-15T13:03:55.239Z".into()),
            org_name: Some("Acme".into()),
            seat_tier: Some("team_standard".into()),
            subscription_status: Some("active".into()),
            extra_usage_enabled: Some(false),
            resume_at: Some(1_900_000_000),
            proxy: Some("socks5://127.0.0.1:1080".into()),
        };
        store.import_credential(&full).await.unwrap();
        let exported = store.export_credentials().await.unwrap();
        let out = exported.iter().find(|c| c.label == "full").expect("导出里该有它").clone();

        // 序列化成迁移文件再读回来，才是真正的「原样喂回来」。
        let file = serde_json::to_string(&out).unwrap();
        let out: PortableCredential = serde_json::from_str(&file).unwrap();
        wipe_credentials(&store).await;
        store.import_credential(&out).await.unwrap();
        let back = &store.export_credentials().await.unwrap()[0];
        assert_eq!(back.label, full.label);
        assert_eq!(back.tier, full.tier);
        assert_eq!(back.org_type, full.org_type);
        assert_eq!(back.org_name, full.org_name);
        assert_eq!(back.seat_tier, full.seat_tier);
        assert_eq!(back.subscription_status, full.subscription_status);
        assert_eq!(back.extra_usage_enabled, full.extra_usage_enabled);
        assert_eq!(back.rate_limit_tier, full.rate_limit_tier);
        assert_eq!(back.access_token, full.access_token);
        assert_eq!(back.refresh_token, full.refresh_token);
        assert_eq!(back.expires_at, full.expires_at);
        assert_eq!(back.priority, full.priority);
        assert_eq!(back.disabled, full.disabled);
        assert_eq!(back.device_limit, full.device_limit);
        assert_eq!(back.rpm_limit, full.rpm_limit);
        // `Some(0)`（这一档不停）与 `None`（跟随全局）是两个不同的值，往返不能抹掉。
        assert_eq!(back.quota_pause_pct, Some(95));
        assert_eq!(back.quota_pause_pct_7d, Some(0));
        assert_eq!(back.ban_reason, full.ban_reason);
        assert_eq!(back.account_uuid, full.account_uuid);
        assert_eq!(back.org_uuid, full.org_uuid);
        assert_eq!(back.subscription_created_at, full.subscription_created_at);
        assert_eq!(back.resume_at, full.resume_at);
        assert_eq!(back.proxy, out.proxy);
    }

    /// 代理池与接入 Key 的往返：导出原样导回，已有的认作 Updated、不重复插。
    #[sqlx::test]
    async fn proxies_and_api_keys_round_trip(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let p = PortableProxy { label: "p".into(), url: "http://h:1".into() };
        assert_eq!(store.import_proxy(&p).await.unwrap(), ImportOutcome::Added);
        let renamed = PortableProxy { label: "p2".into(), ..p.clone() };
        assert_eq!(store.import_proxy(&renamed).await.unwrap(), ImportOutcome::Updated);
        let proxies = store.export_proxies().await.unwrap();
        assert_eq!(proxies.len(), 1);
        assert_eq!(proxies[0].label, "p2");

        let k = PortableApiKey {
            label: "k".into(),
            key: " key-1 ".into(),
            disabled: true,
            all_groups: Some(true),
            groups: vec![],
        };
        assert_eq!(store.import_api_key(&k).await.unwrap(), ImportOutcome::Added);
        assert_eq!(store.import_api_key(&k).await.unwrap(), ImportOutcome::Updated);
        let keys = store.export_api_keys().await.unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!((keys[0].key.as_str(), keys[0].disabled), ("key-1", true));
        assert!(store.api_key_access("key-1").await.unwrap().is_none(), "停用状态跟着走");
    }

    /// 接入 Key 的范围随迁移走：按分组名还原；对不全、或是不带范围的旧版文件，以停用状态导入，
    /// 绝不退成全部号。
    #[sqlx::test]
    async fn api_key_scope_survives_migration(pool: PgPool) {
        let store = CredentialStore::for_test(pool).await;
        let vip = store.create_group("vip", "").await.unwrap().unwrap();
        let gone = store.create_group("gone", "").await.unwrap().unwrap();
        store.create_api_key("all", "key-all", &[]).await.unwrap().unwrap();
        store.create_api_key("vip", "key-vip", &[vip]).await.unwrap().unwrap();
        store.create_api_key("orphan", "key-orphan", &[gone]).await.unwrap().unwrap();
        store.delete_group(gone).await.unwrap().unwrap();
        let mut exported = store.export_api_keys().await.unwrap();
        exported.sort_by(|a, b| a.label.cmp(&b.label));
        let by = |l: &str| exported.iter().find(|k| k.label == l).unwrap().clone();
        assert_eq!((by("all").all_groups, by("all").groups.len()), (Some(true), 0));
        assert_eq!(
            (by("vip").all_groups, by("vip").groups.clone()),
            (Some(false), vec!["vip".into()])
        );
        assert_eq!((by("orphan").all_groups, by("orphan").groups.len()), (Some(false), 0));

        // 目标库：有同名的 vip，没有 other。
        for id in store.list_api_keys().await.unwrap().iter().map(|k| k.id) {
            store.delete_api_key(id).await.unwrap();
        }
        let missing = PortableApiKey {
            label: "missing".into(),
            key: "key-missing".into(),
            disabled: false,
            all_groups: Some(false),
            groups: vec!["vip".into(), "other".into()],
        };
        let legacy = PortableApiKey {
            label: "legacy".into(),
            key: "key-legacy".into(),
            disabled: false,
            all_groups: None,
            groups: vec![],
        };
        for k in exported.iter().chain([&missing, &legacy]) {
            assert_eq!(store.import_api_key(k).await.unwrap(), ImportOutcome::Added);
        }
        let groups_of = |key: &'static str| {
            let store = &store;
            async move { store.api_key_access(key).await.unwrap().map(|a| a.groups) }
        };
        assert_eq!(groups_of("key-all").await, Some(None), "全部号照旧");
        assert_eq!(groups_of("key-vip").await, Some(Some(vec![vip])), "按名字对回 vip");
        assert_eq!(groups_of("key-orphan").await, Some(Some(vec![])), "原本就一个号都选不到");
        assert_eq!(groups_of("key-missing").await, None, "分组对不全：停用");
        assert_eq!(groups_of("key-legacy").await, None, "旧版文件不知道范围：停用");
        let legacy_row =
            store.list_api_keys().await.unwrap().into_iter().find(|k| k.label == "legacy").unwrap();
        assert!(!legacy_row.all_groups, "停用的也不留成全部号，免得一启用就放大");
    }

    /// 设置导入只写文件里有的键、跳过管理密码；旧版的全局接入 Key 变成一把接入 Key。
    #[sqlx::test]
    async fn settings_import_skips_console_auth_and_converts_the_legacy_key(pool: PgPool) {
        let store = CredentialStore::for_test(pool.clone()).await;
        let mut incoming = HashMap::new();
        incoming.insert(CONSOLE_AUTH_KEYS[0].to_string(), "hash-of-source-box".to_string());
        incoming.insert(CLIENT_API_KEY.to_string(), "legacy-key".to_string());
        incoming.insert("some_setting".to_string(), "v".to_string());
        assert_eq!(store.import_settings(&incoming).await.unwrap(), 2);
        let snapshot = store.settings_snapshot();
        assert_eq!(snapshot.get("some_setting").map(String::as_str), Some("v"));
        assert!(!snapshot.contains_key(CONSOLE_AUTH_KEYS[0]));
        assert!(!snapshot.contains_key(CLIENT_API_KEY), "设置项本身不落库");
        let access = store.api_key_access("legacy-key").await.unwrap().expect("转成了接入 Key");
        assert!(access.groups.is_none(), "不绑定分组，用全部号");
        // 落了库：重新打开（重读设置）还在。
        let reopened = CredentialStore::for_test(pool).await;
        assert_eq!(reopened.settings_snapshot().get("some_setting").map(String::as_str), Some("v"));
    }
}
