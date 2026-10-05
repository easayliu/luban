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

#[derive(Deserialize)]
pub(super) struct ExchangeReq {
    /// 用户从授权回调页粘贴的 `code#state`。
    code: String,
    /// 可选的显示名；留空则自动命名。
    #[serde(default)]
    label: Option<String>,
    /// 可选的出站代理——登录换码和拉 profile 都走它，入库后自动存为该凭证的逐账号代理。
    #[serde(default)]
    proxy: Option<String>,
}

/// 用粘贴的 `code#state` 交换 token，并新增一条凭证。
pub(super) async fn exchange(
    State(state): State<AppState>,
    Json(req): Json<ExchangeReq>,
) -> Result<Json<CredentialView>, ApiError> {
    // 先从粘贴内容里取出 state，据此找到**它自己那次**登录的挑战——不能拿「最后一次生成的
    // 那个」，否则并发登录会互相顶掉（见 [`AppState::pkce`]）。取出即移除：一次挑战只能用一次。
    let returned_state = oauth::state_of(&req.code).map_err(|e| bad_request(e.to_string()))?;
    let pkce = take_pkce(&mut state.pkce.lock(), &returned_state, std::time::Instant::now())
        .ok_or_else(|| bad_request("this login attempt expired or was not found; click 'Add account' again to generate a new authorization link"))?;

    // 如果用户指定了代理，先校验、再临时建一个走代理的客户端——换码和拉 profile 都走它。
    let proxy = match req.proxy.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
        Some(raw) => {
            Some(crate::clients::validate_proxy(raw).map_err(|e| bad_request(format!("{e:#}")))?)
        }
        None => None,
    };
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
        )
        .map_err(internal)?;

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
        state.store.ensure_proxy_in_pool(url);
    }

    // 用掉的挑战在取出时就已经从表里移除了，这里无需再清——其余进行中的登录不受影响。
    tracing::info!(
        cred_id = cred.id, cred = %cred.label,
        tier = ?cred.tier, org_type = ?cred.org_type,
        proxy = %proxy.as_deref().unwrap_or("<direct>"),
        "credential added"
    );
    // 设了代理时从库里重新读——insert 返回的那份还没带 proxy，view_of 会拿到最新状态。
    if proxy.is_some() {
        return view_of(&state, cred.id).await;
    }
    Ok(Json(CredentialView::new(&cred, 0, 0, DefaultLimits::of(&state.store))))
}
