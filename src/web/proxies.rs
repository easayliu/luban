//! 代理池接口：增删改、批量导入、连通测试与批量指派。

use super::*;

// ---------- 代理池 ----------

/// 代理池条目的对外视图，附带使用该代理的凭证数量。
#[derive(Serialize)]
pub(super) struct SavedProxyView {
    id: i64,
    label: String,
    url: String,
    created_at: u64,
    credential_count: i64,
    /// 使用该代理的凭证标签列表。
    credential_labels: Vec<String>,
}

/// 列出代理池中所有记录，附带每条代理的使用量与使用者。
///
/// 访客看到的地址由鉴权中间件统一去掉密码，见 [`auth::redact_url_credentials`]。
pub(super) async fn list_saved_proxies(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
) -> Result<Json<Vec<SavedProxyView>>, ApiError> {
    let scope = actor.scope();
    let proxies = state.store.list_proxies(scope).map_err(internal)?;
    let counts = state.store.proxy_usage_counts(scope).map_err(internal)?;
    let mut labels = state.store.proxy_usage_labels(scope).map_err(internal)?;
    let views = proxies
        .into_iter()
        .map(|p| {
            let credential_labels = labels.remove(&p.url).unwrap_or_default();
            SavedProxyView {
                credential_count: counts.get(&p.url).copied().unwrap_or(0),
                id: p.id,
                label: p.label,
                url: p.url,
                created_at: p.created_at,
                credential_labels,
            }
        })
        .collect();
    Ok(Json(views))
}

#[derive(Deserialize)]
pub(super) struct AddProxyReq {
    /// 留空时按地址自动起名，见 [auto_proxy_label]。
    #[serde(default)]
    label: String,
    url: String,
}

/// 名称留空时的自动名：`host:port`，与池里已有名称撞了就依次加 ` #2`、` #3`。
///
/// 取 `host:port` 而不是「代理 N」：列表里一眼能认出是哪台机器，而且不带 user:pass，
/// 名称是明文展示的，不能把凭据带出来。同一个网关靠用户名区分线路的（住宅代理常见）
/// 会撞名，所以要去重。
pub(super) fn auto_proxy_label(url: &str, existing: &[String]) -> String {
    let base = url
        .parse::<axum::http::Uri>()
        .ok()
        .and_then(|u| {
            let host = u.host()?.to_string();
            Some(match u.port_u16() {
                Some(port) => format!("{host}:{port}"),
                None => host,
            })
        })
        .unwrap_or_else(|| "proxy".to_string());
    if !existing.iter().any(|l| l == &base) {
        return base;
    }
    (2..)
        .map(|n| format!("{base} #{n}"))
        .find(|candidate| !existing.iter().any(|l| l == candidate))
        .expect("unbounded range always yields a free name")
}

/// 向代理池中添加一条新记录。
pub(super) async fn add_saved_proxy(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(req): Json<AddProxyReq>,
) -> Result<Json<SavedProxyView>, ApiError> {
    let url =
        crate::clients::validate_proxy(&req.url).map_err(|e| bad_request(format!("{e:#}")))?;
    let label = match req.label.trim() {
        "" => {
            let existing: Vec<String> = state
                .store
                .list_proxies(Scope::Owner(actor.id))
                .map_err(internal)?
                .into_iter()
                .map(|p| p.label)
                .collect();
            auto_proxy_label(&url, &existing)
        }
        given => given.to_string(),
    };
    let p = state.store.add_proxy(actor.id, &label, &url).map_err(internal)?;
    tracing::info!(proxy_id = p.id, label = %p.label, url = %p.url, "proxy added to pool");
    Ok(Json(SavedProxyView {
        id: p.id,
        label: p.label,
        url: p.url,
        created_at: p.created_at,
        credential_count: 0,
        credential_labels: vec![],
    }))
}

/// 一次批量导入最多几条：几千条贴进来多半是贴错了文件，而且单事务持锁太久。
const MAX_PROXY_BATCH: usize = 1000;

#[derive(Deserialize)]
pub(super) struct AddProxiesReq {
    items: Vec<AddProxyReq>,
    /// 只校验、归一化、查重并起好名字，不写库——前端导入前的预览就用它，预览与真正导入走的是
    /// 同一套判据，不会出现「预览说能导、导进去却报错」。
    #[serde(default)]
    dry_run: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum BatchProxyStatus {
    /// 可导入（dry_run）/ 已导入。
    Added,
    /// 地址已在代理池里。
    Exists,
    /// 与本批前面某一条是同一个地址（归一化之后），见 `duplicate_of`。
    Duplicate,
    /// 地址校验不通过，见 `error`。
    Invalid,
}

#[derive(Serialize)]
struct BatchProxyItem {
    status: BatchProxyStatus,
    /// 归一化后的地址；校验不通过时为空。
    url: Option<String>,
    /// 最终名称（留空时已自动起好）；只有 `added` 才有。
    label: Option<String>,
    error: Option<String>,
    /// `duplicate` 时指向本批中第一次出现这个地址的下标。
    duplicate_of: Option<usize>,
    /// 真正导入后的记录 id。
    id: Option<i64>,
}

/// 批量添加代理：逐条校验、归一化、与代理池及本批内部查重，名称留空的按 [auto_proxy_label] 起名
/// （本批内自动起的名字也互相去重）。结果与入参按下标一一对应，有问题的条目只跳过，不让整批失败；
/// 能导入的在一个事务里写入。
pub(super) async fn add_saved_proxies(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(req): Json<AddProxiesReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if req.items.is_empty() {
        return Err(bad_request("enter at least one proxy"));
    }
    if req.items.len() > MAX_PROXY_BATCH {
        return Err(bad_request(format!("at most {MAX_PROXY_BATCH} proxies per import")));
    }
    // 查重只看本人的池子：不同的人各存一条同样的地址是允许的。
    let pool = state.store.list_proxies(Scope::Owner(actor.id)).map_err(internal)?;
    let pool_urls: std::collections::HashSet<&str> = pool.iter().map(|p| p.url.as_str()).collect();
    let mut labels: Vec<String> = pool.iter().map(|p| p.label.clone()).collect();
    let mut seen: std::collections::HashMap<String, usize> = Default::default();
    let mut results = Vec::with_capacity(req.items.len());
    let mut to_insert: Vec<(usize, String, String)> = Vec::new();
    for (i, item) in req.items.iter().enumerate() {
        let url = match crate::clients::validate_proxy(&item.url) {
            Ok(url) => url,
            Err(e) => {
                results.push(BatchProxyItem {
                    status: BatchProxyStatus::Invalid,
                    url: None,
                    label: None,
                    error: Some(format!("{e:#}")),
                    duplicate_of: None,
                    id: None,
                });
                continue;
            }
        };
        let (status, duplicate_of, label) = if pool_urls.contains(url.as_str()) {
            (BatchProxyStatus::Exists, None, None)
        } else if let Some(&first) = seen.get(&url) {
            (BatchProxyStatus::Duplicate, Some(first), None)
        } else {
            seen.insert(url.clone(), i);
            let label = match item.label.trim() {
                "" => auto_proxy_label(&url, &labels),
                given => given.to_string(),
            };
            labels.push(label.clone());
            to_insert.push((i, label.clone(), url.clone()));
            (BatchProxyStatus::Added, None, Some(label))
        };
        results.push(BatchProxyItem {
            status,
            url: Some(url),
            label,
            error: None,
            duplicate_of,
            id: None,
        });
    }
    if !req.dry_run && !to_insert.is_empty() {
        let pairs: Vec<(String, String)> =
            to_insert.iter().map(|(_, l, u)| (l.clone(), u.clone())).collect();
        let inserted = state.store.add_proxies(actor.id, &pairs).map_err(internal)?;
        for ((i, _, _), row) in to_insert.iter().zip(inserted) {
            match row {
                Some(p) => results[*i].id = Some(p.id),
                // 查重之后、写库之前别处加进了同一个地址。
                None => {
                    results[*i].status = BatchProxyStatus::Exists;
                    results[*i].label = None;
                }
            }
        }
        let added = results.iter().filter(|r| r.id.is_some()).count();
        tracing::info!(requested = req.items.len(), added, "proxies added to pool in bulk");
    }
    Ok(Json(serde_json::json!({ "items": results })))
}

#[derive(Deserialize)]
pub(super) struct UpdateProxyReq {
    label: String,
    url: String,
}

/// 更新代理池中一条记录。
pub(super) async fn update_saved_proxy(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(req): Json<UpdateProxyReq>,
) -> Result<Json<SavedProxyView>, ApiError> {
    let label = req.label.trim();
    if label.is_empty() {
        return Err(bad_request("the proxy name must not be empty"));
    }
    let url =
        crate::clients::validate_proxy(&req.url).map_err(|e| bad_request(format!("{e:#}")))?;
    if !state.store.update_proxy(id, label, &url).map_err(internal)? {
        return Err((StatusCode::NOT_FOUND, "proxy not found".into()));
    }
    let p = state
        .store
        .get_proxy(id)
        .map_err(internal)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "proxy not found".to_string()))?;
    let count = state
        .store
        .proxy_usage_counts(actor.scope())
        .map_err(internal)?
        .get(&p.url)
        .copied()
        .unwrap_or(0);
    let credential_labels = state
        .store
        .proxy_usage_labels(actor.scope())
        .map_err(internal)?
        .remove(&p.url)
        .unwrap_or_default();
    tracing::info!(proxy_id = id, label = %p.label, url = %p.url, "proxy updated in pool");
    Ok(Json(SavedProxyView {
        id: p.id,
        label: p.label,
        url: p.url,
        created_at: p.created_at,
        credential_count: count,
        credential_labels,
    }))
}

/// 从代理池中删除一条记录（不影响已配置该地址的凭证）。
pub(super) async fn delete_saved_proxy(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if !state.store.delete_proxy(id).map_err(internal)? {
        return Err((StatusCode::NOT_FOUND, "proxy not found".into()));
    }
    tracing::info!(proxy_id = id, "proxy deleted from pool");
    Ok(Json(serde_json::json!({ "ok": true })))
}

#[derive(Deserialize)]
pub(super) struct DeleteProxiesReq {
    ids: Vec<i64>,
}

/// 批量删除代理池记录（不影响已配置这些地址的凭证），返回实际删掉的条数。
pub(super) async fn delete_saved_proxies(
    State(state): State<AppState>,
    Json(req): Json<DeleteProxiesReq>,
) -> Result<Json<serde_json::Value>, ApiError> {
    if req.ids.is_empty() {
        return Err(bad_request("select at least one proxy"));
    }
    let deleted = state.store.delete_proxies(&req.ids).map_err(internal)?;
    tracing::info!(requested = req.ids.len(), deleted, "proxies deleted from pool in bulk");
    Ok(Json(serde_json::json!({ "deleted": deleted })))
}

#[derive(Deserialize)]
pub(super) struct TestProxyReq {
    url: String,
}

#[derive(Serialize)]
pub(super) struct TestProxyResult {
    ok: bool,
    ip: Option<String>,
    country: Option<String>,
    city: Option<String>,
    region: Option<String>,
    org: Option<String>,
    latency_ms: u128,
    error: Option<String>,
}

/// 测试代理连通性：通过指定代理访问 ip-api.com 获取出口 IP 和地理信息。
pub(super) async fn test_proxy(
    Json(req): Json<TestProxyReq>,
) -> Result<Json<TestProxyResult>, ApiError> {
    let started = std::time::Instant::now();
    let url =
        crate::clients::validate_proxy(&req.url).map_err(|e| bad_request(format!("{e:#}")))?;
    let client =
        crate::clients::upstream_client(Some(&url)).map_err(|e| bad_request(format!("{e:#}")))?;
    let resp = match tokio::time::timeout(
        std::time::Duration::from_secs(15),
        client
            .get("http://ip-api.com/json/?fields=query,country,regionName,city,org,status")
            .send(),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return Ok(Json(TestProxyResult {
                ok: false,
                ip: None,
                country: None,
                city: None,
                region: None,
                org: None,
                latency_ms: started.elapsed().as_millis(),
                error: Some(format!("{e:#}")),
            }));
        }
        Err(_) => {
            return Ok(Json(TestProxyResult {
                ok: false,
                ip: None,
                country: None,
                city: None,
                region: None,
                org: None,
                latency_ms: started.elapsed().as_millis(),
                error: Some("proxy test timed out (15s)".into()),
            }));
        }
    };
    let latency_ms = started.elapsed().as_millis();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    let ok = body.get("status").and_then(|s| s.as_str()) == Some("success");
    let str_field = |k: &str| body.get(k).and_then(|v| v.as_str()).map(str::to_string);
    Ok(Json(TestProxyResult {
        ok,
        ip: str_field("query"),
        country: str_field("country"),
        city: str_field("city"),
        region: str_field("regionName"),
        org: str_field("org"),
        latency_ms,
        error: if ok { None } else { Some(body.to_string()) },
    }))
}

#[derive(Deserialize)]
pub(super) struct SetProxiesReq {
    ids: Vec<i64>,
    proxy: Option<String>,
}

/// 批量设置出站代理。
pub(super) async fn set_proxies(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(req): Json<SetProxiesReq>,
) -> Result<Json<Vec<CredentialView>>, ApiError> {
    check_ids(&req.ids)?;
    let proxy = match req.proxy.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => {
            Some(crate::clients::validate_proxy(raw).map_err(|e| bad_request(format!("{e:#}")))?)
        }
        None => None,
    };
    let n = state.store.set_proxies(&req.ids, proxy.as_deref()).map_err(internal)?;
    tracing::info!(
        count = n,
        proxy = %proxy.as_deref().unwrap_or("<direct>"),
        "proxy set in bulk"
    );
    list_credentials(State(state), Extension(actor)).await
}
