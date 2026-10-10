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
    let proxies = state.store.list_proxies(scope).await.map_err(internal)?;
    let counts = state.store.proxy_usage_counts(scope).await.map_err(internal)?;
    let mut labels = state.store.proxy_usage_labels(scope).await.map_err(internal)?;
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

/// 代理和用户填的出口代理不能指向本机或内网（回环、私网、链路本地、CGNAT、ULA 等），
/// 域名先解析、逐个地址核对。admin 不受限（同机或内网的代理网关是正经用法）。
///
/// 为什么拦：服务端会真的去连这个地址——「测试代理」直接连，号的转发、刷新也走它。不拦的话
/// 代理和用户就能借服务端探测内网端口（耗时、错误文案都回给前端）。只在写入 / 测试时核对：
/// 域名之后改解析到内网（DNS rebinding）挡不住，那要在建连时再拦一道。
pub(super) async fn check_proxy_target(actor: &Actor, url: &str) -> Result<(), ApiError> {
    if actor.is_admin() {
        return Ok(());
    }
    let uri: axum::http::Uri =
        url.parse().map_err(|_| bad_request(format!("invalid proxy URL: {url}")))?;
    let host =
        uri.host().ok_or_else(|| bad_request(format!("the proxy URL has no host: {url}")))?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let addrs: Vec<std::net::IpAddr> = match host.parse() {
        Ok(ip) => vec![ip],
        Err(_) => tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::net::lookup_host((host, 0)),
        )
        .await
        .map_err(|_| bad_request(format!("resolving the proxy host {host} timed out")))?
        .map_err(|e| bad_request(format!("could not resolve the proxy host {host}: {e}")))?
        .map(|a| a.ip())
        .collect(),
    };
    match addrs.into_iter().find(|ip| is_internal_ip(*ip)) {
        Some(ip) => Err(bad_request(format!(
            "the proxy host {host} points to a local or private address ({ip}); only the admin can use such proxies"
        ))),
        None => Ok(()),
    }
}

/// [`check_proxy_target`]，但已在 `actor` 自己代理池里的地址放过：池里的条目入池时核对过，
/// 或是 admin 给这个人的号配代理时随之入池的（admin 可以用内网代理）。不放过的话，这类条目
/// 号主改个名、批量指派到自己别的号上都会被拒，而上号按 `proxy_id` 选同一条却能用。
pub(super) async fn check_proxy_for(
    state: &AppState,
    actor: &Actor,
    url: &str,
) -> Result<(), ApiError> {
    if actor.is_admin() {
        return Ok(());
    }
    let pooled = state
        .store
        .list_proxies(Scope::Owner(actor.id))
        .await
        .map_err(internal)?
        .iter()
        .any(|p| p.url == url);
    if pooled { Ok(()) } else { check_proxy_target(actor, url).await }
}

/// 本机、内网与保留地址：服务端不该替代理和用户去连的那些。
pub(super) fn is_internal_ip(ip: std::net::IpAddr) -> bool {
    use std::net::IpAddr;
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.is_multicast()
                || a == 0
                // CGNAT 100.64.0.0/10：云上常拿它做内网（含部分厂商的元数据服务）。
                || (a == 100 && (b & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            // 内嵌 IPv4 的几种写法，取出来按 IPv4 的口径判：映射 `::ffff:a.b.c.d` 与兼容
            // `::a.b.c.d`（`to_ipv4` 两种都认）、NAT64 `64:ff9b::/96` 与 `64:ff9b:1::/48`
            // （取末 32 位）、6to4 `2002::/16`（取第 2、3 段）。有 NAT64 的部署里，
            // `64:ff9b::a00:5` 就是 10.0.0.5。
            let v4 =
                |hi: u16, lo: u16| std::net::Ipv4Addr::from((u32::from(hi) << 16) | u32::from(lo));
            let embedded = v6
                .to_ipv4()
                .or_else(|| (seg[0] == 0x64 && seg[1] == 0xff9b).then(|| v4(seg[6], seg[7])))
                .or_else(|| (seg[0] == 0x2002).then(|| v4(seg[1], seg[2])));
            if embedded.is_some_and(|ip| is_internal_ip(IpAddr::V4(ip))) {
                return true;
            }
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg[0] & 0xfe00) == 0xfc00 // ULA fc00::/7
                || (seg[0] & 0xffc0) == 0xfe80 // 链路本地 fe80::/10
        }
    }
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
    check_proxy_target(&actor, &url).await?;
    let label = match req.label.trim() {
        "" => {
            let existing: Vec<String> = state
                .store
                .list_proxies(Scope::Owner(actor.id))
                .await
                .map_err(internal)?
                .into_iter()
                .map(|p| p.label)
                .collect();
            auto_proxy_label(&url, &existing)
        }
        given => given.to_string(),
    };
    let p = state.store.add_proxy(actor.id, &label, &url).await.map_err(internal)?;
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

/// 批量导入时内网地址核对的并发路数，见 [`add_saved_proxies`]。
const PROXY_CHECK_CONCURRENCY: usize = 16;

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
    let pool = state.store.list_proxies(Scope::Owner(actor.id)).await.map_err(internal)?;
    let pool_urls: std::collections::HashSet<&str> = pool.iter().map(|p| p.url.as_str()).collect();
    let mut labels: Vec<String> = pool.iter().map(|p| p.label.clone()).collect();
    let mut seen: std::collections::HashMap<String, usize> = Default::default();
    let mut results = Vec::with_capacity(req.items.len());
    let mut to_insert: Vec<(usize, String, String)> = Vec::new();
    // 每条只校验、归一化一次，后面都用这份。
    let validated: Vec<Result<String, String>> = req
        .items
        .iter()
        .map(|item| crate::clients::validate_proxy(&item.url).map_err(|e| format!("{e:#}")))
        .collect();
    // 内网地址核对（见 [`check_proxy_target`]）要解析域名：一批几百条逐条等太慢，一下全发出去
    // 又会把阻塞线程池（解析走 getaddrinfo）占满，故限 [`PROXY_CHECK_CONCURRENCY`] 路并发。
    // 已在本人池里的不核对（反正只会落成「已存在」），admin 整批不核对。
    let blocked: std::collections::HashMap<String, String> = if actor.is_admin() {
        Default::default()
    } else {
        use futures_util::StreamExt;
        // 拿自有的 String 进流：借用的 &str 过不了 handler 那道 Send 的高阶生命周期检查。
        let urls: std::collections::HashSet<String> = validated
            .iter()
            .filter_map(|r| r.as_deref().ok())
            .filter(|url| !pool_urls.contains(url))
            .map(str::to_owned)
            .collect();
        let actor = actor.clone();
        futures_util::stream::iter(urls)
            .map(|url| {
                let actor = actor.clone();
                async move {
                    let res = check_proxy_target(&actor, &url).await;
                    (url, res)
                }
            })
            .buffer_unordered(PROXY_CHECK_CONCURRENCY)
            .filter_map(|(url, res)| async move { res.err().map(|(_, e)| (url, e)) })
            .collect()
            .await
    };
    for (i, item) in req.items.iter().enumerate() {
        let checked = validated[i].clone().and_then(|url| match blocked.get(&url) {
            Some(e) => Err(e.clone()),
            None => Ok(url),
        });
        let url = match checked {
            Ok(url) => url,
            Err(e) => {
                results.push(BatchProxyItem {
                    status: BatchProxyStatus::Invalid,
                    url: None,
                    label: None,
                    error: Some(e),
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
        let inserted = state.store.add_proxies(actor.id, &pairs).await.map_err(internal)?;
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
    pub(super) label: String,
    pub(super) url: String,
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
    check_proxy_for(&state, &actor, &url).await?;
    if !state.store.update_proxy(id, label, &url).await.map_err(internal)? {
        return Err((StatusCode::NOT_FOUND, "proxy not found".into()));
    }
    let p = state
        .store
        .get_proxy(id)
        .await
        .map_err(internal)?
        .ok_or_else(|| (StatusCode::NOT_FOUND, "proxy not found".to_string()))?;
    let count = state
        .store
        .proxy_usage_counts(actor.scope())
        .await
        .map_err(internal)?
        .get(&p.url)
        .copied()
        .unwrap_or(0);
    let credential_labels = state
        .store
        .proxy_usage_labels(actor.scope())
        .await
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
    if !state.store.delete_proxy(id).await.map_err(internal)? {
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
    let deleted = state.store.delete_proxies(&req.ids).await.map_err(internal)?;
    tracing::info!(requested = req.ids.len(), deleted, "proxies deleted from pool in bulk");
    Ok(Json(serde_json::json!({ "deleted": deleted })))
}

#[derive(Deserialize)]
pub(super) struct TestProxyReq {
    url: String,
}

#[derive(Serialize)]
pub(super) struct TestProxyResult {
    pub(super) ok: bool,
    ip: Option<String>,
    country: Option<String>,
    city: Option<String>,
    region: Option<String>,
    org: Option<String>,
    latency_ms: u128,
    pub(super) error: Option<String>,
}

/// 测试代理连通性：通过指定代理访问 ip-api.com 获取出口 IP 和地理信息。
pub(super) async fn test_proxy(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Json(req): Json<TestProxyReq>,
) -> Result<Json<TestProxyResult>, ApiError> {
    let url =
        crate::clients::validate_proxy(&req.url).map_err(|e| bad_request(format!("{e:#}")))?;
    check_proxy_for(&state, &actor, &url).await?;
    let client =
        crate::clients::upstream_client(Some(&url)).map_err(|e| bad_request(format!("{e:#}")))?;
    Ok(Json(probe_proxy(&client, PROXY_TEST_TIMEOUT).await))
}

/// 「测试代理」的超时。
pub(super) const PROXY_TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// 经 `client`（已配好代理）打一次 ip-api.com，看代理通不通。「测试代理」按钮与上号 Key
/// 自动分配代理共用这一份判据。
pub(super) async fn probe_proxy(
    client: &wreq::Client,
    timeout: std::time::Duration,
) -> TestProxyResult {
    let started = std::time::Instant::now();
    let resp = match tokio::time::timeout(
        timeout,
        client
            .get("http://ip-api.com/json/?fields=query,country,regionName,city,org,status")
            .send(),
    )
    .await
    {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            return TestProxyResult {
                ok: false,
                ip: None,
                country: None,
                city: None,
                region: None,
                org: None,
                latency_ms: started.elapsed().as_millis(),
                error: Some(format!("{e:#}")),
            };
        }
        Err(_) => {
            return TestProxyResult {
                ok: false,
                ip: None,
                country: None,
                city: None,
                region: None,
                org: None,
                latency_ms: started.elapsed().as_millis(),
                error: Some(format!("proxy test timed out ({}s)", timeout.as_secs())),
            };
        }
    };
    let latency_ms = started.elapsed().as_millis();
    let body: serde_json::Value = resp.json().await.unwrap_or_default();
    let ok = body.get("status").and_then(|s| s.as_str()) == Some("success");
    let str_field = |k: &str| body.get(k).and_then(|v| v.as_str()).map(str::to_string);
    TestProxyResult {
        ok,
        ip: str_field("query"),
        country: str_field("country"),
        city: str_field("city"),
        region: str_field("regionName"),
        org: str_field("org"),
        latency_ms,
        error: if ok { None } else { Some(body.to_string()) },
    }
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
    if let Some(url) = &proxy {
        check_proxy_for(&state, &actor, url).await?;
    }
    let n = state.store.set_proxies(&req.ids, proxy.as_deref()).await.map_err(internal)?;
    tracing::info!(
        count = n,
        proxy = %proxy.as_deref().unwrap_or("<direct>"),
        "proxy set in bulk"
    );
    list_credentials(State(state), Extension(actor)).await
}
