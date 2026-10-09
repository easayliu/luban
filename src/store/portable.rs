//! 迁移：导出 / 导入凭证、代理池与设置。

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

/// 迁移用的一把接入 Key：名称、明文、停用状态。分组绑定不带——分组 id 由目标库自己发，
/// 导进去的 Key 一律不绑定分组（用全部号），需要的话在目标站重新绑。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PortableApiKey {
    #[serde(default)]
    pub label: String,
    pub key: String,
    #[serde(default)]
    pub disabled: bool,
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

impl CredentialStore {
    /// 导出全部凭证的可迁移形态，顺序同 [`Self::list`]（priority, id）。
    ///
    /// **含明文 access/refresh token**——迁移要的就是它们，脱敏过的导出等于没导。谁能调到
    /// 这个口子就等于拿到了这些账号，故接口侧另加了一道闸（见 `crate::web` 的 `export`）。
    pub fn export_credentials(&self) -> Result<Vec<PortableCredential>> {
        Ok(self.list()?.iter().map(PortableCredential::from).collect())
    }

    /// 导出代理池的可迁移形态。
    pub fn export_proxies(&self) -> Result<Vec<PortableProxy>> {
        Ok(self.list_proxies(Scope::All)?.iter().map(PortableProxy::from).collect())
    }

    /// 导入一条代理：URL 已存在则更新 label，不存在则新增。返回是 Added 还是 Updated。
    pub fn import_proxy(&self, p: &PortableProxy) -> Result<ImportOutcome> {
        anyhow::ensure!(!p.url.is_empty(), "proxy URL must not be empty");
        let conn = self.conn.lock();
        let existing: Option<i64> = conn
            .query_row(
                "SELECT id FROM proxies WHERE url = ?1 \
                    AND owner_id = (SELECT id FROM users WHERE role = 'admin')",
                [&p.url],
                |r| r.get(0),
            )
            .optional()?;
        match existing {
            Some(id) => {
                conn.execute("UPDATE proxies SET label = ?2 WHERE id = ?1", params![id, p.label])?;
                Ok(ImportOutcome::Updated)
            }
            None => {
                conn.execute(
                    "INSERT INTO proxies (label, url, owner_id) \
                     VALUES (?1, ?2, (SELECT id FROM users WHERE role = 'admin'))",
                    params![p.label, p.url],
                )?;
                Ok(ImportOutcome::Added)
            }
        }
    }

    /// 可迁移的设置快照（`settings` 全表），**去掉管理密码**。
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

    /// 导入时按账号找目标行：同一个账号 UUID 下按组织 UUID 认。
    ///
    /// - 组织 UUID 两边都有且相等 → 就是它；
    /// - 导入的有组织 UUID、库里没有同组织的 → 退回库里这个账号下**唯一一条**组织 UUID 还空着的
    ///   行（旧号还没回填），有多条说不清是哪个就不认；
    /// - 导入的没有组织 UUID → 库里这个账号只有一行才认它，多行（个人 + 团队）说不清就不认。
    ///
    /// 不认的交给调用方按 refresh_token 兜底，再不中就新增——宁可多一行，也不要拿一个订阅的
    /// 状态覆盖掉另一个订阅。
    fn match_import_target(
        tx: &rusqlite::Transaction<'_>,
        account_uuid: &str,
        org_uuid: Option<&str>,
    ) -> Result<Option<i64>> {
        let mut stmt = tx.prepare(
            "SELECT id, NULLIF(TRIM(COALESCE(org_uuid, '')), '') FROM credentials
              WHERE account_uuid = ?1",
        )?;
        let rows: Vec<(i64, Option<String>)> = stmt
            .query_map([account_uuid], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
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

    /// 导入一条凭证：目标库已有这个账号就整行覆盖，没有就新增。
    ///
    /// **匹配顺序是「账号 UUID + 组织 UUID」优先、`refresh_token` 兜底**，这个先后有实际后果：同一个
    /// 账号在源站重新授权过之后 refresh_token 已经是新值，只按 token 匹配会把它当成一个新
    /// 账号插进去，目标库里同一个账号出现两行（两行还会各自去刷新同一个上游账号）。反过来，
    /// 老库里可能有 `account_uuid` 还没拉到的号（profile 没取成功），故 token 这条兜底不能去。
    ///
    /// 只按账号 UUID 不够：同一个人可以既有个人订阅、又在团队里占一个席位，两次授权拿到的是
    /// **同一个** `account_uuid`、不同的 `org_uuid`，在库里是两行。只认账号的话，导入时后一行会
    /// 把前一行整行覆盖掉。见 [`Self::match_import_target`]。
    ///
    /// 命中后是**整行覆盖**而不是只更新 token：迁移文件是源站此刻的完整状态，优先级、设备
    /// 上限、代理这些都是操作者在源站上调好的。想保留目标站自己的调法，就别对已有的号做导入
    /// （或者导入后再调）——半覆盖半保留的规则说不清也记不住。
    pub fn import_credential(&self, c: &PortableCredential) -> Result<ImportOutcome> {
        if c.access_token.trim().is_empty() || c.refresh_token.trim().is_empty() {
            anyhow::bail!("credential has an empty access_token or refresh_token");
        }
        // 空串的 uuid 当没有：老库里存过空串，拿它去匹配会把所有这类号连成一个。
        let uuid = c.account_uuid.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let org = c.org_uuid.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let proxy = c.proxy.as_deref().map(str::trim).filter(|s| !s.is_empty());
        let priority = c.priority.map_or(PRIORITY_DEFAULT, |p| p.clamp(PRIORITY_MIN, PRIORITY_MAX));
        let conn = self.conn.lock();
        let tx = conn.unchecked_transaction()?;
        let by_account = match uuid {
            Some(u) => Self::match_import_target(&tx, u, org)?,
            None => None,
        };
        let existing: Option<i64> = match by_account {
            Some(id) => Some(id),
            None => tx
                .query_row(
                    "SELECT id FROM credentials WHERE refresh_token_hash = ?1",
                    [token_fingerprint(&c.refresh_token)],
                    |r| r.get(0),
                )
                .optional()?,
        };
        let outcome = match existing {
            Some(id) => {
                tx.execute(
                    "UPDATE credentials SET
                         label = ?2, tier = ?3, org_type = ?4, access_token = ?5,
                         refresh_token = ?6, expires_at = ?7, priority = ?8, disabled = ?9,
                         device_limit = ?10, rpm_limit = ?11, ban_reason = ?12,
                         account_uuid = ?13, resume_at = ?14, proxy = ?15,
                         rate_limit_tier = ?16, org_uuid = ?17, subscription_created_at = ?18,
                         quota_pause_pct = ?19, quota_pause_pct_7d = ?20, session_limit = ?21,
                         org_name = ?22, seat_tier = ?23, subscription_status = ?24,
                         extra_usage_enabled = ?25, refresh_token_hash = ?26,
                         updated_at = unixepoch()
                     WHERE id = ?1",
                    params![
                        id,
                        c.label,
                        c.tier,
                        c.org_type,
                        seal(&c.access_token),
                        seal(&c.refresh_token),
                        c.expires_at as i64,
                        priority,
                        c.disabled as i64,
                        c.device_limit,
                        c.rpm_limit,
                        c.ban_reason,
                        uuid,
                        c.resume_at.map(|t| t as i64),
                        proxy,
                        c.rate_limit_tier,
                        c.org_uuid,
                        c.subscription_created_at,
                        c.quota_pause_pct,
                        c.quota_pause_pct_7d,
                        c.session_limit,
                        c.org_name,
                        c.seat_tier,
                        c.subscription_status,
                        c.extra_usage_enabled.map(i64::from),
                        token_fingerprint(&c.refresh_token),
                    ],
                )
                .context("failed to update the existing credential")?;
                ImportOutcome::Updated
            }
            None => {
                tx.execute(
                    "INSERT INTO credentials
                         (label, tier, org_type, access_token, refresh_token, expires_at,
                          priority, disabled, device_limit, rpm_limit, ban_reason,
                          account_uuid, resume_at, proxy, rate_limit_tier, org_uuid,
                          subscription_created_at, quota_pause_pct, quota_pause_pct_7d,
                          session_limit, org_name, seat_tier, subscription_status,
                          extra_usage_enabled, refresh_token_hash, owner_id)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15,
                             ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25,
                             (SELECT id FROM users WHERE role = 'admin'))",
                    params![
                        c.label,
                        c.tier,
                        c.org_type,
                        seal(&c.access_token),
                        seal(&c.refresh_token),
                        c.expires_at as i64,
                        priority,
                        c.disabled as i64,
                        c.device_limit,
                        c.rpm_limit,
                        c.ban_reason,
                        uuid,
                        c.resume_at.map(|t| t as i64),
                        proxy,
                        c.rate_limit_tier,
                        c.org_uuid,
                        c.subscription_created_at,
                        c.quota_pause_pct,
                        c.quota_pause_pct_7d,
                        c.session_limit,
                        c.org_name,
                        c.seat_tier,
                        c.subscription_status,
                        c.extra_usage_enabled.map(i64::from),
                        token_fingerprint(&c.refresh_token),
                    ],
                )
                .context("failed to insert the credential (its refresh_token may already exist)")?;
                ImportOutcome::Added
            }
        };
        tx.commit()?;
        Ok(outcome)
    }

    /// 导入设置：逐项写库并同步内存镜像，返回实际写入的项数。
    ///
    /// 管理密码一律跳过（口径同 [`Self::settings_snapshot`]，导出不带、导入也不认——万一有人
    /// 手工把它塞回文件里）。**只写文件里有的键**：目标库里多出来的设置保持原值，不做「以文件
    /// 为准清空其余」——那样一份手改过的、只留了几项的文件会把目标站其余配置全部重置成默认。
    pub fn import_settings(&self, settings: &HashMap<String, String>) -> Result<usize> {
        let mut n = 0;
        for (k, v) in settings {
            if CONSOLE_AUTH_KEYS.contains(&k.as_str()) || DEPLOYMENT_ONLY_KEYS.contains(&k.as_str())
            {
                continue;
            }
            // 旧版导出文件里的全局接入 Key：转成一把不绑定分组的接入 Key（设置项本身已不再使用）。
            if k == CLIENT_API_KEY {
                if !v.trim().is_empty() {
                    let key = PortableApiKey {
                        label: "导入的 Key".into(),
                        key: v.trim().into(),
                        disabled: false,
                    };
                    self.import_api_key(&key)?;
                    n += 1;
                }
                continue;
            }
            self.set_setting(k, v)?;
            n += 1;
        }
        Ok(n)
    }

    /// 导出全部接入 Key（含明文）。
    pub fn export_api_keys(&self) -> Result<Vec<PortableApiKey>> {
        self.list_api_keys()?
            .into_iter()
            .map(|k| {
                let key = self.reveal_api_key(k.id)?.unwrap_or_default();
                Ok(PortableApiKey { label: k.label, key, disabled: k.disabled })
            })
            .collect()
    }

    /// 导入一把接入 Key：同一把（明文相同）已在就跳过，回 `Updated`；否则新增、不绑定分组。
    pub fn import_api_key(&self, k: &PortableApiKey) -> Result<ImportOutcome> {
        anyhow::ensure!(!k.key.trim().is_empty(), "API key must not be empty");
        let exists: bool = {
            let conn = self.conn.lock();
            conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM api_keys WHERE key_hash = ?1)",
                [token_fingerprint(k.key.trim())],
                |r| r.get(0),
            )?
        };
        if exists {
            return Ok(ImportOutcome::Updated);
        }
        let id = self
            .create_api_key(k.label.trim(), k.key.trim(), &[])?
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        if k.disabled {
            self.update_api_key(id, k.label.trim(), true, &[])?
                .map_err(|e| anyhow::anyhow!("{e}"))?;
        }
        Ok(ImportOutcome::Added)
    }
}
