//! 授权登录：生成授权链接（PKCE）与粘回 code 换 token。

use super::*;

/// 一次登录尝试还没换 token 之前，PKCE 上下文最多留多久。
///
/// 用户要在浏览器里完成授权再把 `code#state` 粘回来，几分钟足够；留太久只是让过期的挑战
/// 一直占着位置。到点后那次登录会被判成「尚未生成授权链接」，重新点一次即可。
pub(super) const PKCE_TTL: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// 同时最多保留几个待完成的登录尝试。纯属防御——正常同时开几个标签页也就个位数，
/// 上限只是不让反复点「添加账号」把内存撑起来。超出时丢掉最旧的那个。
pub(super) const PKCE_MAX_PENDING: usize = 32;

// ---------- 授权 ----------

#[derive(Serialize)]
pub(super) struct AuthorizeResp {
    url: String,
}

/// 生成新的 PKCE 挑战并返回授权 URL；挑战按其 `state` 暂存，供后续交换时取回。
///
/// 并发的多次登录互不干扰——每次各占一格，见 [`AppState::pkce`]。顺手清掉过期与超量的格子。
pub(super) async fn authorize(State(state): State<AppState>) -> Json<AuthorizeResp> {
    let pkce = PkceChallenge::generate();
    // 申请哪些 scope 由 settings 决定（没配就是官方那一整套），见 [`store::OAUTH_SCOPES`]。
    let scopes = state.store.oauth_scopes();
    let url = pkce.authorize_url(&scopes);
    tracing::info!(scopes = %scopes, "authorization link generated");
    remember_pkce(&mut state.pkce.lock(), pkce, std::time::Instant::now());
    Json(AuthorizeResp { url })
}

/// 进行中的登录尝试表，见 [`AppState::pkce`]。
pub(super) type PendingPkce = Vec<(String, PkceChallenge, std::time::Instant)>;

/// 记下一次新的登录尝试，顺手清掉过期与超量的格子。
///
/// 抽成自由函数是为了能直接测——它修的正是一个簿记 bug（并发登录互相顶掉），
/// 而这类 bug 只在「同时两个人操作」时才现形，靠手点几乎复现不出来。
pub(super) fn remember_pkce(
    pending: &mut PendingPkce,
    pkce: PkceChallenge,
    now: std::time::Instant,
) {
    pending.retain(|(_, _, at)| now.duration_since(*at) < PKCE_TTL);
    pending.push((pkce.state.clone(), pkce, now));
    // 超量时丢最旧的（尾插，故最旧在头部）。
    let overflow = pending.len().saturating_sub(PKCE_MAX_PENDING);
    pending.drain(..overflow);
}

/// 取出 `state` 对应的那次登录并从表中移除（一次挑战只能用一次）；过期的顺手清掉。
pub(super) fn take_pkce(
    pending: &mut PendingPkce,
    state: &str,
    now: std::time::Instant,
) -> Option<PkceChallenge> {
    pending.retain(|(_, _, at)| now.duration_since(*at) < PKCE_TTL);
    let i = pending.iter().position(|(s, _, _)| s == state)?;
    Some(pending.remove(i).1)
}

/// 自动分配时已选中、还没入库的代理：`url` → 在途几次。并发的几个脚本同时上号时，库里的
/// 挂号数还没变，只看库会全挑到同一条上；把在途的也算进去才分得开。
static PROXY_IN_FLIGHT: std::sync::LazyLock<parking_lot::Mutex<std::collections::HashMap<String, usize>>> =
    std::sync::LazyLock::new(Default::default);

/// 自动分配占着的一个名额，上号结束（成功或失败）时放掉。
pub(super) struct ProxyReservation(String);

impl Drop for ProxyReservation {
    fn drop(&mut self) {
        let mut map = PROXY_IN_FLIGHT.lock();
        if let Some(n) = map.get_mut(&self.0) {
            *n -= 1;
            if *n == 0 {
                map.remove(&self.0);
            }
        }
    }
}

/// 自动分配时最多试几条代理。按挂号从少到多试，通了就停；前几条都不通多半是出口整体出了
/// 问题，再往下试只是让脚本干等。
const AUTO_PROXY_MAX_TRIES: usize = 5;

/// 自动分配时每条代理测试的超时，比「测试代理」按钮短：脚本在等着，试满也不过一分钟。
const AUTO_PROXY_TEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// 自动分配用的连通性测试：与「测试代理」按钮同一份判据（[`probe_proxy`]）。
pub(super) async fn probe_pool_proxy(url: String) -> Result<(), String> {
    let client = crate::clients::upstream_client(Some(&url)).map_err(|e| format!("{e:#}"))?;
    let result = probe_proxy(&client, AUTO_PROXY_TEST_TIMEOUT).await;
    if result.ok { Ok(()) } else { Err(result.error.unwrap_or_else(|| "test failed".into())) }
}

/// 定这次上号走哪个代理：
/// - `proxy`：直接给的地址，校验后用；
/// - `proxy_id`：本人代理池里的那条，别人的按不存在回 404；
/// - 都没给：网页上号直连；上号 Key 从本人代理池里按挂号从少到多（挂号数按全部号算，同一个
///   出口不管谁的号都算；并列取 id 小的）逐条用 `probe` 测试，用第一条测通的。池子空了才
///   直连；池里有代理却一条都不通回 502，不退回直连——直连等于把服务器自己的 IP 交出去。
pub(super) async fn resolve_proxy<F, Fut>(
    state: &AppState,
    actor: &Actor,
    req: &ExchangeReq,
    auto: bool,
    probe: F,
) -> Result<(Option<String>, Option<ProxyReservation>), ApiError>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<(), String>>,
{
    let raw = req.proxy.as_deref().map(str::trim).filter(|s| !s.is_empty());
    let validate =
        |url: &str| crate::clients::validate_proxy(url).map_err(|e| bad_request(format!("{e:#}")));
    match (raw, req.proxy_id) {
        (Some(_), Some(_)) => Err(bad_request("give either proxy or proxy_id, not both")),
        (Some(raw), None) => Ok((Some(validate(raw)?), None)),
        (None, Some(id)) => {
            let not_found = || (StatusCode::NOT_FOUND, "proxy not found".to_string());
            if state.store.proxy_owner(id).map_err(internal)? != Some(actor.id) {
                return Err(not_found());
            }
            let saved = state.store.get_proxy(id).map_err(internal)?.ok_or_else(not_found)?;
            Ok((Some(validate(&saved.url)?), None))
        }
        (None, None) if auto => {
            // 池里的地址按校验后的样子比：迁移文件导入的条目没经过校验，可能是归一化之前的写法
            // （号上存的是归一化之后的），也可能根本建不出客户端。后者一个号都挂不上、挂号数恒为
            // 0，不跳过的话每次都排在最前面。
            let mut pool: Vec<(i64, String)> = state
                .store
                .list_proxies(Scope::Owner(actor.id))
                .map_err(internal)?
                .into_iter()
                .filter_map(|p| match crate::clients::validate_proxy(&p.url) {
                    Ok(url) => Some((p.id, url)),
                    Err(e) => {
                        tracing::warn!(proxy_id = p.id, error = %e, "auto-assign: skipped an invalid pool proxy");
                        None
                    }
                })
                .collect();
            if pool.is_empty() {
                return Ok((None, None));
            }
            let used = state.store.proxy_usage_counts(Scope::All).map_err(internal)?;
            {
                let in_flight = PROXY_IN_FLIGHT.lock();
                let load = |url: &str| {
                    used.get(url).copied().unwrap_or(0) as usize
                        + in_flight.get(url).copied().unwrap_or(0)
                };
                pool.sort_by_key(|(id, url)| (load(url), *id));
            }
            let mut failures = Vec::new();
            for (id, url) in pool.into_iter().take(AUTO_PROXY_MAX_TRIES) {
                match probe(url.clone()).await {
                    Ok(()) => {
                        *PROXY_IN_FLIGHT.lock().entry(url.clone()).or_default() += 1;
                        tracing::info!(proxy_id = id, owner = %actor.username, "provision key: proxy auto-assigned");
                        return Ok((Some(url.clone()), Some(ProxyReservation(url))));
                    }
                    Err(e) => {
                        tracing::warn!(proxy_id = id, error = %e, "auto-assign: proxy failed the connectivity test");
                        failures.push(format!("proxy #{id}: {e}"));
                    }
                }
            }
            Err((
                StatusCode::BAD_GATEWAY,
                format!(
                    "no proxy in your pool passed the connectivity test (tried the {} least used): {}",
                    failures.len(),
                    failures.join("; ")
                ),
            ))
        }
        (None, None) => Ok((None, None)),
    }
}

#[derive(Deserialize)]
pub(super) struct ExchangeReq {
    /// 用户从授权回调页粘贴的 `code#state`。
    pub(super) code: String,
    /// 可选的显示名；留空则自动命名。
    #[serde(default)]
    pub(super) label: Option<String>,
    /// 可选的出站代理——登录换码和拉 profile 都走它，入库后自动存为该凭证的逐账号代理。
    #[serde(default)]
    pub(super) proxy: Option<String>,
    /// 改从代理池里选：只认本人池里的那条。与 `proxy` 二选一。
    #[serde(default)]
    pub(super) proxy_id: Option<i64>,
    /// 放进哪些号池分组（至少一个）。不传或为空时放进默认分组。代理和用户只能选开放给
    /// 自己的分组。
    #[serde(default)]
    pub(super) group_ids: Vec<i64>,
}

/// 用粘贴的 `code#state` 交换 token，并新增一条凭证。
pub(super) async fn exchange(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    via_key: Option<Extension<auth::ViaProvisionKey>>,
    Json(req): Json<ExchangeReq>,
) -> Result<Json<CredentialView>, ApiError> {
    // 先从粘贴内容里取出 state，据此找到**它自己那次**登录的挑战——不能拿「最后一次生成的
    // 那个」，否则并发登录会互相顶掉（见 [`AppState::pkce`]）。取出即移除：一次挑战只能用一次。
    // 分组先核对：换码会作废这次授权，等换完才发现分组选错了就得让用户重新授权一遍。
    let group_ids = if req.group_ids.is_empty() {
        vec![state.store.default_group_id().map_err(internal)?]
    } else {
        check_selectable(&state, &actor, &req.group_ids)?;
        req.group_ids.clone()
    };
    // 代理也在取挑战之前定：选错了代理（不是本人池里的、地址不合法）、池里的代理都不通时，
    // 这次授权还能重试。
    let (proxy, _reservation) =
        resolve_proxy(&state, &actor, &req, via_key.is_some(), probe_pool_proxy).await?;
    let returned_state = oauth::state_of(&req.code).map_err(|e| bad_request(e.to_string()))?;
    let pkce = take_pkce(&mut state.pkce.lock(), &returned_state, std::time::Instant::now())
        .ok_or_else(|| bad_request("this login attempt expired or was not found; click 'Add account' again to generate a new authorization link"))?;

    // 有代理就临时建一个走代理的客户端——换码和拉 profile 都走它。
    let tmp_client;
    let client: &wreq::Client = match proxy.as_deref() {
        Some(url) => {
            tmp_client = crate::clients::upstream_client(Some(url))
                .map_err(|e| bad_request(format!("{e:#}")))?;
            &tmp_client
        }
        None => state.clients.direct(),
    };

    // exchange_code 内部会再比一次 state。冗余是有意的：这里是「按 state 找挑战」，那里是
    // 「确认挑战与粘贴内容配套」，万一将来查找逻辑改错了，那道校验还在。
    let tokens = oauth::exchange_code(client, &pkce, &req.code)
        .await
        .map_err(|e| bad_request(e.to_string()))?;

    // 拉取账号 profile 拿邮箱/姓名/等级（失败不阻断，用兜底）。不阻断不等于不留痕：
    // 悄悄吞掉的话，账号加进来标签是「账号 N」、等级空着，看不出是 profile 没拉到。
    let profile = match oauth::fetch_profile(client, &tokens.access_token).await {
        Ok(p) => p,
        Err(e) => {
            tracing::warn!(error = %e, "add credential: fetching the account profile failed, falling back for label and tier");
            oauth::Profile::default()
        }
    };

    // 显示名优先级：用户填写 > profile 邮箱 > profile 姓名 > 交换响应邮箱 > 「账号 N」。
    let label = match req.label.map(|s| s.trim().to_string()) {
        Some(s) if !s.is_empty() => s,
        _ => profile
            .email
            .clone()
            .or_else(|| profile.name.clone())
            .or_else(|| tokens.account.clone())
            .unwrap_or_else(|| {
                let n = state.store.list().map(|v| v.len()).unwrap_or(0) + 1;
                format!("Account {}", n)
            }),
    };

    let cred = state
        .store
        .insert(
            &label,
            profile.tier.as_deref(),
            &tokens.access_token,
            &tokens.refresh_token,
            tokens.expires_at,
            profile.account_uuid.as_deref(),
            profile.org_type.as_deref(),
            actor.id,
        )
        .map_err(|e| {
            if e.downcast_ref::<store::OwnerGone>().is_some() {
                bad_request("the account adding this credential no longer exists")
            } else {
                internal(e)
            }
        })?;

    // 新号落库时先进了默认分组（触发器），这里换成所选的分组。分组在核对之后被删了的话
    // 号就留在默认分组里，不回滚这次上号。
    match state.store.set_credential_groups(&[cred.id], &group_ids) {
        Ok(Ok(())) => {}
        Ok(Err(e)) => {
            tracing::warn!(cred_id = cred.id, error = %e, "add credential: kept in the default group")
        }
        Err(e) => {
            tracing::warn!(cred_id = cred.id, error = %e, "add credential: failed to set its groups")
        }
    }

    // 额度档原值、组织 UUID、订阅创建时刻不在 `insert` 的参数里（那串已经够长了），入库后
    // 走与刷新同一份 `apply_profile` 写；profile 拉不到时组织 id 退回交换响应里那个
    // （官方也是这个兜底次序：profile → tokenAccount）。
    if let Err(e) =
        state.store.apply_profile(cred.id, &profile, tokens.organization_uuid.as_deref())
    {
        tracing::warn!(cred_id = cred.id, error = %e, "failed to store the profile fields");
    }

    // 登录时带了代理的，入库后顺手存上——后续刷新、转发自动走它，不用再手动配一次。
    // 凭证已入库，代理存不进去时不回滚凭证（手动配一次也行），但必须如实报错让人知道。
    if let Some(ref url) = proxy {
        state.store.set_proxy(cred.id, Some(url)).map_err(internal)?;
        // 顺手把代理加进代理池——下次添加账号时直接从池里选，不必再手打一遍。
        // 已存在的自动忽略（URL 有唯一索引）。
        state.store.ensure_proxy_in_pool(actor.id, url);
    }

    // 用掉的挑战在取出时就已经从表里移除了，这里无需再清——其余进行中的登录不受影响。
    tracing::info!(
        cred_id = cred.id, cred = %cred.label, owner = %actor.username,
        tier = ?cred.tier, org_type = ?cred.org_type,
        proxy = %proxy.as_deref().unwrap_or("<direct>"),
        "credential added"
    );
    // 从库里重新读：insert 返回的那份还没带代理与分组。
    view_of(&state, cred.id).await
}
