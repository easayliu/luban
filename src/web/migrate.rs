//! 迁移：导出 / 导入凭证、代理池与设置。

use super::*;

// 错误详情在这两个构造器里记，而不是让每个 handler 自己写一行：管理接口的失败此前**只**
// 回给客户端、服务端一行不留，出了 500 在日志里根本查不到。方法与路径由
// [`log_api_failures`] 那层补，两边合起来才是完整的一次失败。
//
// 没用 `#[track_caller]` 带出调用位置：这两个函数大量以 `.map_err(internal)` 的函数指针形式
// 传递，reify 出的 shim 不透传 caller location，记出来的位置是错的，不如不记。
// ---------- 迁移：导出 / 导入 ----------

/// 迁移文件的信封。导出写它、导入读它，两个方向同一个结构——导出的文件原样喂回去就能导入。
///
/// `kind`/`version` 是给导入侧的**防误投**：迁移文件和别的 JSON（备份脚本的输出、随手存的
/// 接口响应）长得都差不多，认错了会把一堆垃圾写进凭证表。校验见 [`import`]。
#[derive(Serialize, Deserialize)]
struct ExportFile {
    /// 恒为 [`EXPORT_KIND`]。
    kind: String,
    /// 文件格式版本，当前 [`EXPORT_VERSION`]。
    version: u32,
    /// 导出时刻（Unix 秒）。只作人看的信息，导入侧不据此做任何判断。
    exported_at: i64,
    /// 导出这份文件的 luban 版本，排查「哪个版本导出的」时有用。
    luban_version: String,
    /// 全部凭证，**含明文 token**。
    credentials: Vec<store::PortableCredential>,
    /// `settings` 全表（不含管理密码，见 [`store::CredentialStore::settings_snapshot`]）。
    /// 用 `BTreeMap` 而不是 `HashMap`：导出文件是会被人 diff、被存进版本库的，键序必须稳定。
    settings: std::collections::BTreeMap<String, String>,
    /// 代理池：凭证的 `proxy` 字段只存 URL，池里还有 label 这类管理信息。
    /// 旧版导出文件没有此字段，`#[serde(default)]` 让导入侧拿到空 Vec，不会坏。
    #[serde(default)]
    proxies: Vec<store::PortableProxy>,
}

/// 迁移文件的 `kind` 标记。
const EXPORT_KIND: &str = "luban-export";

/// 迁移文件的格式版本。加字段不必动它（导入侧全字段 `#[serde(default)]`）；
/// 只有**改变已有字段含义**时才需要 +1。
///
/// - 2：`priority` 换到 1..=100、默认 P50 的口径。
/// - 3：`priority` 换到 P0..=P4、默认 P2 的档位口径；1、2 版文件导入时按
///   [`priority_tiers_by_rank`] 在文件内按名次压档（旧默认档 → P2）。
const EXPORT_VERSION: u32 = 3;

/// 导出全部账号与设置，供迁移到另一台机器。
///
/// **未设管理密码时拒绝**：这个口子返回的是明文 access/refresh token，等于把全部账号交出去。
/// 未设密码时 [`auth::require_admin`] 已经拦下所有管理接口，这里是兜底：其余管理接口顶多
/// 是改配置，这条不一样，所以它自己再确认一次门锁着。
pub(super) async fn export(State(state): State<AppState>) -> Result<Response, ApiError> {
    if !auth::admin_configured(&state) {
        return Err((
            StatusCode::FORBIDDEN,
            "set an admin password before exporting: this file contains plaintext account tokens"
                .into(),
        ));
    }
    let credentials = state.store.export_credentials().map_err(internal)?;
    let proxies = state.store.export_proxies().map_err(internal)?;
    let exported_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let file = ExportFile {
        kind: EXPORT_KIND.to_string(),
        version: EXPORT_VERSION,
        exported_at,
        luban_version: env!("CARGO_PKG_VERSION").to_string(),
        credentials,
        settings: state.store.settings_snapshot().into_iter().collect(),
        proxies,
    };
    tracing::info!(
        credentials = file.credentials.len(),
        settings = file.settings.len(),
        proxies = file.proxies.len(),
        "exported"
    );
    // 文件名带时间戳：迁移时常常连着导好几份（改完再导一次），同名会互相覆盖。
    let filename = format!("luban-export-{exported_at}.json");
    Ok((
        [(header::CONTENT_DISPOSITION, format!("attachment; filename=\"{filename}\""))],
        Json(file),
    )
        .into_response())
}

/// 导入的两种模式。
#[derive(Deserialize, PartialEq, Eq, Clone, Copy)]
#[serde(rename_all = "snake_case")]
enum ImportMode {
    /// 合并：文件里的号按账号身份覆盖同一个号，其余保留。目标库里有、文件里没有的号不动。
    Merge,
    /// 先清空再导入：目标库变成文件的样子。**连用量历史一起清**（同
    /// [`store::CredentialStore::clear`]），故只在「这台机器就是要接管源站」时用。
    Replace,
}

#[derive(Deserialize)]
pub(super) struct ImportReq {
    /// 迁移文件本体（导出接口的响应原文）。
    payload: ExportFile,
    /// 合并还是先清空，缺省合并——两者里只有它是不会丢东西的那个。
    #[serde(default = "default_import_mode")]
    mode: ImportMode,
    /// 要不要一并导入设置。缺省 `false`：账号是迁移的主体，而设置里含接入 key 这类
    /// 会立刻改变客户端能不能连上的项，得由操作者明确点头。
    #[serde(default)]
    import_settings: bool,
}

fn default_import_mode() -> ImportMode {
    ImportMode::Merge
}

#[derive(Serialize)]
pub(super) struct ImportResp {
    /// 新增的账号数。
    added: usize,
    /// 覆盖了已有账号的条数。
    updated: usize,
    /// 导入失败的条数（每条的原因都在日志里；整份文件不会因为一条坏行全废）。
    failed: usize,
    /// `replace` 模式下被清掉的原有账号数。
    cleared: usize,
    /// 实际写入的设置项数（未勾选导入设置时为 0）。
    settings_applied: usize,
    /// 代理池：新增 + 更新的条数。
    proxies_added: usize,
    proxies_updated: usize,
}

/// 导入账号与设置。
///
/// **逐条导入、失败不回滚**：一条坏记录（token 空、refresh_token 撞上另一个号）不该让另外
/// 二十个号也进不来，故每条各自成事务，失败的计数并记日志。迁移场景下「进来 19 个、1 个报错」
/// 远好于「全都没进来，你自己找是哪条」。
pub(super) async fn import(
    State(state): State<AppState>,
    Json(req): Json<ImportReq>,
) -> Result<Json<ImportResp>, ApiError> {
    if req.payload.kind != EXPORT_KIND {
        return Err(bad_request(format!(
            "not a luban export file (kind = {:?})",
            req.payload.kind
        )));
    }
    if req.payload.version > EXPORT_VERSION {
        return Err(bad_request(format!(
            "this file was written by a newer luban (format version {}, this build understands {EXPORT_VERSION}); upgrade first",
            req.payload.version
        )));
    }
    if req.payload.credentials.is_empty() && req.payload.proxies.is_empty() && !req.import_settings
    {
        return Err(bad_request("nothing to import: the file has no credentials or proxies"));
    }
    // 清空放在导入之前、且只在 replace 下做：先清后导意味着导入失败时库是空的，
    // 所以这个模式在界面上要单独确认（见前端的 ImportDialog）。
    let cleared = if req.mode == ImportMode::Replace {
        let n = state.store.clear().map_err(internal)?;
        tracing::warn!(cleared = n, "import: cleared all existing credentials first");
        n
    } else {
        0
    };
    let mut resp = ImportResp {
        added: 0,
        updated: 0,
        failed: 0,
        cleared,
        settings_applied: 0,
        proxies_added: 0,
        proxies_updated: 0,
    };
    // 代理池先于凭证导入：凭证的 `proxy` 字段引用池里的 URL，先建好池条目在管理界面上更直观。
    for (i, p) in req.payload.proxies.iter().enumerate() {
        match state.store.import_proxy(p) {
            Ok(store::ImportOutcome::Added) => resp.proxies_added += 1,
            Ok(store::ImportOutcome::Updated) => resp.proxies_updated += 1,
            Err(e) => {
                tracing::warn!(index = i, label = %p.label, url = %p.url, error = %e, "import: skipped one proxy");
            }
        }
    }
    // 旧版文件的优先级按名次压到 5 档：1 版默认档是 0，2 版是 50。
    let legacy_tiers = match req.payload.version {
        0 | 1 => Some(0),
        2 => Some(50),
        _ => None,
    }
    .map(|mid| {
        priority_tiers_by_rank(req.payload.credentials.iter().filter_map(|c| c.priority), mid)
    });
    for (i, c) in req.payload.credentials.iter().enumerate() {
        let converted;
        let c = match &legacy_tiers {
            Some(tiers) => {
                converted = store::PortableCredential {
                    // 缺字段的留 None，导入时落默认档 P2。
                    priority: c.priority.map(|p| tiers[&p]),
                    ..c.clone()
                };
                &converted
            }
            None => c,
        };
        match state.store.import_credential(c) {
            Ok(store::ImportOutcome::Added) => resp.added += 1,
            Ok(store::ImportOutcome::Updated) => resp.updated += 1,
            Err(e) => {
                resp.failed += 1;
                tracing::warn!(index = i, label = %c.label, error = %e, "import: skipped one credential");
            }
        }
    }
    if req.import_settings {
        let settings: std::collections::HashMap<String, String> =
            req.payload.settings.into_iter().collect();
        // 文件里带 `latest_cc_release` 时，整个导入放进缓存的串行锁里做，写完再以库为准同步缓存：
        // 否则库是一个数、进程里认另一个（重启才暴露），或者后台一笔迟到的落库把刚导入的盖掉。
        resp.settings_applied = if settings.contains_key(store::LATEST_CC_RELEASE) {
            let mut applied = 0;
            oauth::LATEST_RELEASE
                .sync_from_store(|| {
                    applied = state.store.import_settings(&settings)?;
                    read_latest_release_setting(&state.store)
                })
                .map_err(internal)?;
            applied
        } else {
            state.store.import_settings(&settings).map_err(internal)?
        };
    }
    tracing::info!(
        added = resp.added,
        updated = resp.updated,
        failed = resp.failed,
        cleared = resp.cleared,
        settings = resp.settings_applied,
        proxies_added = resp.proxies_added,
        proxies_updated = resp.proxies_updated,
        "imported"
    );
    Ok(Json(resp))
}
