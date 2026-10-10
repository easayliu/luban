//! 凭证的 token 维护：手动刷新、重新授权、连通性测试与解除冷却。

use super::*;

/// 手动刷新一条凭证的 token。
pub(super) async fn refresh_credential(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<CredentialView>, ApiError> {
    // 走与自动刷新同一条路：拿刷新锁、按库里最新的凭证刷（refresh_token 与出站代理都在拿到锁
    // 之后重读），刷新与落库在脱离请求的任务里做完（见 `store::force_refresh`），浏览器断开也
    // 不会丢掉轮换后的新 token。手动刷新同样走这个号自己的代理；代理坏掉时如实报错，不退回直连。
    let (label, http, result) =
        match store::force_refresh(&state.store, &state.clients, id).await.map_err(internal)? {
            store::ManualRefresh::Gone => return Err(not_found()),
            store::ManualRefresh::Proxy(e) => return Err(bad_request(format!("{e:#}"))),
            store::ManualRefresh::Refreshed { label, http, result } => (label, http, result),
        };
    let tokens = match result {
        Ok(t) => t,
        Err(e) => {
            // refresh_token 被永久作废（invalid_grant）→ 标记封禁，与 keepalive / 转发路径口径一致。
            if let Some(te) = e.downcast_ref::<oauth::TokenEndpointError>()
                && te.is_grant_revoked()
            {
                let reason = te.ban_reason();
                tracing::warn!(cred_id = id, cred = %label, %reason, "manual refresh: grant revoked, disabling");
                let _ = state
                    .store
                    .detached(
                        |s| async move { s.record_ban(id, &store::refresh_ban(&reason)).await },
                    )
                    .await;
            }
            return Err(bad_request(e.to_string()));
        }
    };
    // 顺带把 profile 那几列写回（等级、账号 UUID、组织类型、额度档、组织 UUID、订阅创建时刻）：
    // 旧库里的号是在这些列存在之前加的，只有刷新才补得上。失败忽略、不影响 token 刷新结果，
    // 但要留一行——否则「刷新成功了但等级还是旧的」在日志里毫无痕迹。
    match oauth::fetch_profile(&http, &tokens.access_token).await {
        Ok(profile) => {
            if let Err(e) =
                state.store.apply_profile(id, &profile, tokens.organization_uuid.as_deref()).await
            {
                tracing::warn!(cred_id = id, error = %e, "failed to write back the profile fields (the refresh itself succeeded)");
            }
        }
        Err(e) => {
            tracing::warn!(cred_id = id, error = %e, "fetching the profile after refresh failed, profile fields left unchanged");
        }
    }
    view_of(&state, id).await
}

/// 重新授权拿到的身份与这一行对不上的情形，见 [`reauth_mismatch`]。括号里是给人看的名字
/// （邮箱 / 组织名，拉不到就用 UUID）。
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ReauthMismatch {
    Account(String),
    Organization(String),
}

impl ReauthMismatch {
    /// 前端 `localizeBackendMessage` 按原文匹配这两句，改措辞要同步。
    pub(super) fn message(&self) -> String {
        match self {
            Self::Account(who) => format!(
                "the authorized account ({who}) is not this account; sign in with the original account and try again"
            ),
            Self::Organization(org) => format!(
                "the authorized organization ({org}) is not this account's organization; sign in and choose the original organization, then try again"
            ),
        }
    }
}

/// 重新授权的同号校验：先比账号 UUID，再比组织 UUID。
///
/// 这次拿到的值 profile 优先，拉不到用交换响应里那个。两边都有且不等才算对不上；任一边缺
/// （旧号还没回填、profile 与交换响应都没给）无从比对，放行。组织要单独比：同一个人可以既有
/// 个人订阅又占一个团队席位，账号 UUID 相同，登录时选错组织就会把另一个订阅的 token 塞进来。
pub(super) fn reauth_mismatch(
    known_account: Option<&str>,
    known_org: Option<&str>,
    profile: Option<&oauth::Profile>,
    tokens: &oauth::TokenSet,
) -> Option<ReauthMismatch> {
    fn present(v: Option<&str>) -> Option<&str> {
        v.map(str::trim).filter(|s| !s.is_empty())
    }
    let got_account =
        present(profile.and_then(|p| p.account_uuid.as_deref()).or(tokens.account_uuid.as_deref()));
    if let (Some(known), Some(got)) = (present(known_account), got_account)
        && known != got
    {
        let who =
            profile.and_then(|p| p.email.as_deref()).or(tokens.account.as_deref()).unwrap_or(got);
        return Some(ReauthMismatch::Account(who.to_string()));
    }
    let got_org = present(
        profile.and_then(|p| p.org_uuid.as_deref()).or(tokens.organization_uuid.as_deref()),
    );
    if let (Some(known), Some(got)) = (present(known_org), got_org)
        && known != got
    {
        let org = profile.and_then(|p| p.org_name.as_deref()).unwrap_or(got);
        return Some(ReauthMismatch::Organization(org.to_string()));
    }
    None
}

#[derive(Deserialize)]
pub(super) struct ReauthorizeReq {
    /// 用户从授权回调页粘贴的 `code#state`，授权链接同样取自 `GET /authorize`。
    code: String,
}

/// 重新授权：对**已有**的号重走一遍 OAuth 登录，用新换到的 token 覆盖这一行。
///
/// 给 refresh_token 被作废（`invalid_grant`）的号用：删了重加会丢掉优先级、上限、代理、
/// 用量流水这些，这里只换 token 与 profile，其余原样保留。换码与拉 profile 走这个号自己的
/// 代理，与刷新同一出口。
///
/// 两道把关：
/// - 持有该号的刷新锁再写——等锁的自动刷新拿锁后会重读库，看到新 token 就直接复用，
///   不会把旧那一族的结果写回来；
/// - 账号 UUID、组织 UUID 与库里的对不上就拒绝（见 [`reauth_mismatch`]）：登错号或选错组织会把
///   另一个账号 / 订阅的 token 塞进这一行，而标签、代理、会话槽位都还是原来那个号的。
///
/// 号是因为 token 问题被自动停用的（见 [`is_token_pause`]），换好 token 就顺手启用；封号、
/// 订阅未生效、手动停用、额度暂停与 token 无关，保持原状。
pub(super) async fn reauthorize_credential(
    State(state): State<AppState>,
    Extension(actor): Extension<Actor>,
    Path(id): Path<i64>,
    Json(req): Json<ReauthorizeReq>,
) -> Result<Json<CredentialView>, ApiError> {
    state.store.get(id).await.map_err(internal)?.ok_or_else(not_found)?;
    let returned_state = oauth::state_of(&req.code).map_err(|e| bad_request(e.to_string()))?;

    // 先拿锁再读号：等锁期间自动刷新可能刚把号停掉、或刚换了代理，下面的启用判断与出口
    // 都得按拿锁之后的状态来。
    let lock = state.store.refresh_lock(id);
    let _guard = lock.lock().await;
    let cred = state.store.get(id).await.map_err(internal)?.ok_or_else(not_found)?;
    // 代理建不出来放在取挑战之前：挑战取出即作废，代理错了改好再试还能用同一个授权结果。
    let http = state.clients.for_credential(&cred).map_err(|e| bad_request(format!("{e:#}")))?;
    let pkce = take_pkce(&mut state.pkce.lock(), actor.id, &returned_state, std::time::Instant::now())
        .ok_or_else(|| bad_request("this login attempt expired or was not found; generate a new authorization link and try again"))?;

    let tokens = oauth::exchange_code(&http, &pkce, &req.code)
        .await
        .map_err(|e| bad_request(e.to_string()))?;
    let profile = match oauth::fetch_profile(&http, &tokens.access_token).await {
        Ok(p) => Some(p),
        Err(e) => {
            tracing::warn!(cred_id = id, error = %e, "reauthorize: fetching the account profile failed, skipping the same-account check");
            None
        }
    };
    if let Some(mismatch) = reauth_mismatch(
        cred.account_uuid.as_deref(),
        cred.org_uuid.as_deref(),
        profile.as_ref(),
        &tokens,
    ) {
        tracing::warn!(cred_id = id, cred = %cred.label, ?mismatch, "reauthorize: authorized a different account or organization, rejected");
        return Err(bad_request(mismatch.message()));
    }

    state
        .store
        .update_tokens(id, &tokens.access_token, &tokens.refresh_token, tokens.expires_at)
        .await
        .map_err(internal)?;
    if let Some(profile) = &profile
        && let Err(e) =
            state.store.apply_profile(id, profile, tokens.organization_uuid.as_deref()).await
    {
        tracing::warn!(cred_id = id, error = %e, "reauthorize: failed to store the profile fields");
    }
    let token_pause = cred.disabled
        && cred.ban_reason.as_deref().is_some_and(|r| is_token_pause(r, cred.resume_at.is_some()));
    if token_pause {
        state.store.set_disabled(id, false).await.map_err(internal)?;
    }
    tracing::info!(cred_id = id, cred = %cred.label, re_enabled = token_pause, "credential reauthorized");
    view_of(&state, id).await
}

/// 停用原因是不是「token 出了问题」——重新授权换好 token 就该回来的那几种：
///
/// - `[refresh …]`：刷新被作废（`invalid_grant`），或 `[refresh-failed]` 刷新失败的限时暂停；
/// - `[proxy]`：代理建不出来——重新授权走的就是这个号的代理，能走到这里说明代理已好；
/// - 转发 / 保活撞上 401 这类 token 被吊销、过期的报错（`OAuth token has been revoked` 等）。
///
/// 最后一条与前端 `isAccountBan` 同口径：带封号特征（账号停用、暂挂、违规）的不算，那是
/// 号本身废了，换 token 也回不来。其余限时暂停（额度、限流）只看 `[refresh-failed]` 那一档。
/// 改口径须前后端一起改。
pub(super) fn is_token_pause(reason: &str, has_resume_at: bool) -> bool {
    if reason.starts_with(store::REFRESH_FAIL_PAUSE_TAG) {
        return true;
    }
    if has_resume_at {
        return false;
    }
    if reason.starts_with("[refresh ") || reason.starts_with("[proxy]") {
        return true;
    }
    let l = reason.to_ascii_lowercase();
    let near = |word: &str, within: usize, targets: &[&str]| {
        l.match_indices(word).any(|(i, _)| {
            let tail = &l.as_bytes()[i + word.len()..(i + word.len() + within).min(l.len())];
            targets.iter().any(|t| tail.windows(t.len()).any(|w| w == t.as_bytes()))
        })
    };
    let account_ban = ["account_on_hold", "violat", "/restricted"].iter().any(|p| l.contains(p))
        || near(
            "account",
            5 + "deactivat".len(),
            &["suspend", "disable", "ban", "terminat", "deactivat"],
        );
    if account_ban {
        return false;
    }
    l.contains("invalid_grant")
        || ["token", "grant"].iter().any(|w| {
            near(w, 15 + "not found".len(), &["revoked", "expired", "invalid", "not found"])
        })
}

#[derive(Deserialize)]
pub(super) struct TestReq {
    /// 要测的模型名（如 `claude-opus-5`）。原样发给上游，不做白名单校验——模型名会随官方
    /// 上新变化，写死一份清单只会在下次上新时把新模型挡在外面，而「模型名不对」上游本来就
    /// 会回一条清清楚楚的 404/400，那正是这个功能要展示的东西。
    model: String,
}

/// 连通性测试：用**指定**账号向上游发一条最小请求，看这个号能不能用这个模型。
///
/// 停用/封禁的号也允许测——「它是不是已经恢复了」正是要问的问题，所以这里只校验凭证存在。
/// 测试的副作用与代价见 [`proxy::probe`]：不选号，但账号状态按真实流量的口径更新（429 打
/// 冷却、命中封号特征自动停用），会写一条用量日志（卡片上的额度与花费据此更新），也真的会
/// 消耗一点点订阅额度。上游拒绝（4xx/5xx）不是本接口的错误，照样 200 返回一份结果，
/// 由前端展示状态码与原因；只有「凭证不存在」「模型名没填」才是 4xx。
pub(super) async fn test_credential(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(req): Json<TestReq>,
) -> Result<Json<proxy::ProbeReport>, ApiError> {
    let model = req.model.trim();
    if model.is_empty() {
        return Err(bad_request("specify the model name to test"));
    }
    let cred = state.store.get(id).await.map_err(internal)?.ok_or_else(not_found)?;
    Ok(Json(proxy::probe(&state, &cred, model).await))
}

/// 手动解除该凭证的限流状态：进程内的模型级冷却全清，且若它是被账号级限流**自动停用**的，
/// 一并重新启用（等价于手动打开启用开关，只是不会误碰人工停用/封号的号）。学到的「套餐不含
/// 某模型」记录也一并清掉——管理员刚给这个组织开了 extra usage 时，这就是让它立刻回去试的入口。
///
/// 解除错了，下一条请求撞上 429 会重新打上，最坏多一次往返——所以这里不做任何「确认上游真的
/// 恢复了」的前置校验，想稳妥的话入口旁边就是连通性测试（它通过时也会自动恢复调度）。
pub(super) async fn clear_cooldown(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<CredentialView>, ApiError> {
    state.store.get(id).await.map_err(internal)?.ok_or_else(not_found)?;
    state.store.clear_rate_limited(id, None);
    state.store.clear_model_denials(id, None).await.map_err(internal)?;
    state.store.resume_if_rate_limited(id).await.map_err(internal)?;
    view_of(&state, id).await
}
