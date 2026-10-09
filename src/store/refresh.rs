//! access token 的取用、刷新与刷新失败时的换号。

use super::*;

/// 刷新失败后最多改选几个凭证。
///
/// 每失败一轮就停用一个凭证（可用池严格变小），循环必然收敛；这个上限只是防御性兜底，
/// 免得停用没生效时打成死循环。也顺带给单次请求的耗时封了顶——每一轮都是一次上游往返。
pub(super) const MAX_REFRESH_FAILOVER: usize = 5;

/// 刷新没拿到结果（网络 / 代理 / 超时 / 5xx / 非作废的 4xx）后，这个号暂停调度多久。
///
/// 这类失败多半是这个号的出口出了问题（代理挂了、过期了），不停的话绑在它上面的设备每条
/// 请求都要白等一次刷新超时再吃 503。写的是 `resume_at`，与限流暂停同一套恢复：到点
/// 惰性放回、连通性测试通过当场放回。取得短，网络抖一下的号很快就回来了。
pub(super) const REFRESH_FAIL_PAUSE_SECS: u64 = 120;

/// 刷新失败暂停写进 `ban_reason` 的开头标签，前端 `isRefreshFailurePause` 按它认。
pub const REFRESH_FAIL_PAUSE_TAG: &str = "[refresh-failed]";

/// [`ensure_fresh_token`] 刷新这一步失败、且不是 refresh_token 被作废（那条走
/// [`TokenAttempt::Revoked`]）。带上是哪个号，转发那边据此把流水记到这个号名下；
/// `detail` 是完整错误链（`{:#}`），只进日志、流水与后台，不回给客户端——代理报错里
/// 可能带着代理地址。
#[derive(Debug, Clone)]
pub struct RefreshFailed {
    pub cred_id: i64,
    pub cred_label: String,
    pub detail: String,
    /// 号已经因为刷新失败暂停着（别的请求刚停的），这次没有真去刷：换号循环不再写一遍暂停，
    /// 免得把恢复时刻一次次往后推。
    pub already_paused: bool,
}

impl RefreshFailed {
    /// 库里已有的刷新失败暂停，`detail` 取暂停原因（去掉标签）。
    pub(super) fn paused(cred_id: i64, cred_label: String, detail: &str) -> Self {
        Self { cred_id, cred_label, detail: detail.to_string(), already_paused: true }
    }
}

/// `ban_reason` 是刷新失败暂停写的，就返回标签后面的原因原文。
pub(super) fn refresh_pause_detail(reason: &str) -> Option<&str> {
    reason.strip_prefix(REFRESH_FAIL_PAUSE_TAG).map(str::trim_start)
}

/// 一次请求里因刷新失败最多换几次号。刷新一次最多等 [`crate::oauth`] 的 15 秒超时，
/// 本机网络或共用代理挂了的时候每个号都会失败——不封顶的话一条请求要白等
/// [`MAX_REFRESH_FAILOVER`] 轮、再连带停掉那么多个号。作废（`Revoked`）换号不算在内。
pub(super) const MAX_REFRESH_FAIL_SWAPS: usize = 2;

impl std::fmt::Display for RefreshFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "credential #{} token refresh failed: {}", self.cred_id, self.detail)
    }
}

impl std::error::Error for RefreshFailed {}

/// 一次「拿到该凭证可用 access_token」的尝试结果。可重试的错误（网络抖动、5xx、限流）
/// 走 `Err` 直接冒泡，不在这里表达。
pub enum TokenAttempt {
    /// 拿到可用 access_token。
    Ready(String),
    /// 该凭证的 refresh_token 已被上游永久作废，重试没有意义——外层会停用它并改选其它号。
    /// 携带写入 `ban_reason` 的原因。
    Revoked(String),
}

/// 代理转发使用：按 device_id 粘性选出凭证并返回 (access_token, 该凭证)（必要时刷新）。
///
/// 选择见 [`CredentialStore::select_for_device`]。若命中的凭证进入刷新窗口，
/// 则调用 OAuth 刷新并回写。注意刷新是异步 IO，不持有 DB 锁。
///
/// **刷新失败要自动换号**：`select_for_device` 在返回前就写好了设备绑定，之后才轮到刷新。
/// 若刷新失败直接把错误抛出去，这个设备就被钉死在坏号上——绑定还在，下一次请求照样选中它，
/// 永远 503 直到人工介入。故这里在「refresh_token 已被作废」时停用该凭证
/// （[`CredentialStore::record_ban`] 会连带清掉它的设备绑定），再重选一个号继续。
/// 网络抖动/5xx 这类可重试错误**不**停用，原样抛出，让客户端重试时还落回同一个号。
pub async fn valid_access_token_for_device(
    store: &CredentialStore,
    clients: &crate::clients::ClientPool,
    sel: Select<'_>,
) -> Result<(String, Credential, Option<i64>)> {
    select_with_refresh_failover(store, sel, |cred| {
        Box::pin(async move { fresh_token(store, clients, &cred, true).await })
    })
    .await
}

/// 取**指定**凭证的可用 access_token（必要时刷新），不选号、不写设备绑定。
///
/// 连通性测试用（见 [`crate::proxy::probe`]）。转发那条路走
/// [`valid_access_token_for_device`]：它会按负载均衡挑号，而测试是指名道姓要测这一个，
/// 挑到别的号上去测出来的结论就不是这个号的。
///
/// **失败停用的口径与转发一致**：这里发生的刷新是一次真实的上游往返，`refresh_token`
/// 已被作废这个结论不因「是测试触发的」就打折扣——不停用的话，卡片上一切如常，
/// 只有点过测试的人知道这个号其实已经死了。区别只在**不换号**：测试指名要测这一个，
/// 停用之后如实把原因抛出去即可。网络抖动/5xx 这类可重试错误照旧不停用。
pub async fn access_token_of(
    store: &CredentialStore,
    clients: &crate::clients::ClientPool,
    cred: &Credential,
) -> Result<String> {
    match ensure_fresh_token(store, clients, cred).await? {
        TokenAttempt::Ready(token) => Ok(token),
        TokenAttempt::Revoked(reason) => {
            tracing::warn!(
                cred_id = cred.id, cred = %cred.label,
                reason = %reason,
                "refresh_token revoked upstream, disabling the credential"
            );
            if let Err(e) = store.record_ban(cred.id, &refresh_ban(&reason)) {
                tracing::warn!(error = %e, "failed to auto-disable the credential");
            }
            anyhow::bail!("{reason}")
        }
    }
}

/// 刷新 token 被上游作废时的封号上下文：没有 HTTP 往返可记，来源标成 `refresh`。
pub fn refresh_ban(reason: &str) -> BanContext {
    BanContext {
        reason: reason.to_string(),
        source: "refresh",
        error_message: Some(reason.to_string()),
        ..Default::default()
    }
}

/// [`select_with_refresh_failover`] 注入的「取一次 token」返回的 future。
///
/// 写成显式 boxed future 而不是 `impl AsyncFn`：后者的 `CallRefFuture` 带高阶生命周期，
/// 会让捕获了 `&CredentialStore`/`&wreq::Client` 的闭包推不出 `Send`
/// （报 `implementation of Send is not general enough`），而这条链最终要塞进 axum handler。
/// 固定成单个 `'a` 就没有这个问题；代价是每轮一次 Box 分配，紧挨着一次上游往返，可忽略。
pub(super) type AttemptFut<'a> =
    std::pin::Pin<Box<dyn std::future::Future<Output = Result<TokenAttempt>> + Send + 'a>>;

/// [`valid_access_token_for_device`] 的重选循环本体。把「取 token」这一步抽成参数注入，
/// 是为了让换号逻辑本身能脱离网络被测到——这段逻辑此前不存在（刷新失败直接抛错），
/// 设备会被钉死在坏号上，属于只在生产才暴露的那类 bug，必须有回归测试盯着。
///
/// `attempt` 收 `Credential` 而非 `&Credential`：按值传就不会让返回的 future 借用参数，
/// `AttemptFut<'a>` 里那个 `'a` 才能是固定的。
pub(super) async fn select_with_refresh_failover<'a>(
    store: &CredentialStore,
    sel: Select<'_>,
    attempt: impl Fn(Credential) -> AttemptFut<'a>,
) -> Result<(String, Credential, Option<i64>)> {
    let sel = Select {
        ttl_secs: store.device_binding_ttl(),
        retention_secs: store.device_binding_retention(),
        session_ttl_secs: store.session_binding_ttl(),
        session_retention_secs: store.session_binding_retention(),
        ..sel
    };

    // 本次请求里最近一次「刷新没拿到结果」的错误：后面换不到号时报它——它才是这条请求
    // 失败的原因，也带着是哪个号（转发那边据此把流水记到这个号名下）。
    let mut refresh_failure: Option<anyhow::Error> = None;
    let mut refresh_fails = 0;
    for round in 0..MAX_REFRESH_FAILOVER {
        // 每轮都重新选：上一轮停用的那个已被排除，且它的设备绑定已清，这里才会换到新号。
        let (cred, slot) = match store.select_with_slot(sel) {
            Ok(v) => v,
            Err(e) => {
                let Some(rf) = refresh_failure else { return Err(e) };
                // 选号那句（刚暂停的号在池外，多半是「全员冷却」）只进日志。
                tracing::warn!(error = %e, "no other credential to switch to after a token refresh failure");
                return Err(rf);
            }
        };
        match attempt(cred.clone()).await {
            Ok(TokenAttempt::Ready(token)) => return Ok((token, cred, slot)),
            Ok(TokenAttempt::Revoked(reason)) => {
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    round,
                    reason = %reason,
                    "refresh_token revoked upstream, disabling the credential and selecting another"
                );
                // 停用没生效就必须中止：否则下一轮还会选中同一个号，白转满 MAX_REFRESH_FAILOVER 圈。
                if !store.record_ban(cred.id, &refresh_ban(&reason))? {
                    anyhow::bail!(
                        "credential #{} refresh failed and could not be disabled: {reason}",
                        cred.id
                    );
                }
            }
            Err(e) => {
                let Some(rf) = e.downcast_ref::<RefreshFailed>() else { return Err(e) };
                // 刷新没拿到结果：暂停一小会、换号。号本身可能是好的，所以走限时暂停而不是封号。
                // 已经暂停着的（别的请求刚停的）不再写，恢复时刻不往后推。
                if !rf.already_paused {
                    let reason = format!(
                        "{REFRESH_FAIL_PAUSE_TAG} {}; scheduling resumes automatically in about {} minutes",
                        rf.detail,
                        REFRESH_FAIL_PAUSE_SECS / 60
                    );
                    let resume_at = crate::credentials::now_secs() + REFRESH_FAIL_PAUSE_SECS;
                    // 没写进去（号刚被人工停用 / 封掉）就不再换号，照实报这次失败。
                    if !store.pause_for_rate_limit(cred.id, &reason, resume_at)? {
                        return Err(e);
                    }
                }
                refresh_fails += 1;
                if refresh_fails >= MAX_REFRESH_FAIL_SWAPS {
                    tracing::warn!(
                        cred_id = cred.id, cred = %cred.label,
                        round,
                        "token refresh failed again, giving up instead of trying more credentials"
                    );
                    return Err(e);
                }
                tracing::warn!(
                    cred_id = cred.id, cred = %cred.label,
                    round,
                    "token refresh failed, pausing the credential and selecting another"
                );
                refresh_failure = Some(e);
            }
        }
    }

    if let Some(rf) = refresh_failure {
        return Err(rf);
    }
    anyhow::bail!(
        "all {MAX_REFRESH_FAILOVER} credential refresh attempts failed; no credentials are available"
    )
}

/// 取该凭证的可用 access_token，未进入刷新窗口就直接复用，否则刷新并回写。
///
/// 刷新走该凭证的专属锁 + 双重检查：上游刷新会轮换 refresh_token，并发刷新中后完成的那次
/// 会把已作废的 token 写回库，导致该凭证之后所有刷新都 `invalid_grant`（账号被自己废掉）。
/// 拿到锁后重新读库，若他人已刷好则直接复用，不再多打一次刷新。
pub async fn ensure_fresh_token(
    store: &CredentialStore,
    clients: &crate::clients::ClientPool,
    cred: &Credential,
) -> Result<TokenAttempt> {
    fresh_token(store, clients, cred, false).await
}

/// [`ensure_fresh_token`] 的实现。`skip_refresh_paused` 只有转发选号那条传 `true`：等锁期间
/// 别的请求刚刷失败、把号停了，就直接报那次失败，不再白等一次超时。连通性测试与保活传
/// `false`——测试正是要真刷一次看号回没回来。
pub(super) async fn fresh_token(
    store: &CredentialStore,
    clients: &crate::clients::ClientPool,
    cred: &Credential,
    skip_refresh_paused: bool,
) -> Result<TokenAttempt> {
    if !cred.needs_refresh() {
        return Ok(TokenAttempt::Ready(cred.access_token.clone()));
    }

    let lock = store.refresh_lock(cred.id);
    let _guard = lock.lock().await;
    // 双重检查：等锁期间可能已被其它请求刷新过。
    let cred = store.get(cred.id)?.unwrap_or_else(|| cred.clone());
    if !cred.needs_refresh() {
        tracing::debug!(
            cred_id = cred.id,
            "credential was refreshed while waiting for the lock, reusing the new token"
        );
        return Ok(TokenAttempt::Ready(cred.access_token));
    }
    if skip_refresh_paused
        && cred.disabled
        && cred.resume_at.is_some_and(|at| at > crate::credentials::now_secs())
        && let Some(detail) = cred.ban_reason.as_deref().and_then(refresh_pause_detail)
    {
        tracing::debug!(
            cred_id = cred.id,
            "credential was paused after a refresh failure while waiting for the lock"
        );
        return Err(RefreshFailed::paused(cred.id, cred.label.clone(), detail).into());
    }

    tracing::info!(cred_id = cred.id, cred = %cred.label, "credential entered the refresh window, refreshing token");
    // 刷新也必须走这个号自己的代理：只把转发挂上代理、刷新走直连的话，每次 token 过期
    // 都会有一次带真实 IP 的请求打到上游，而且那条路径的失败最不容易被注意到。
    // 取的是双重检查之后那份 `cred`——等锁期间代理可能刚被改过。
    // 代理建不出来是永久配置错误，走 Revoked 让上层 mark_banned 踢出调度池。
    let http = match clients.for_credential(&cred) {
        Ok(c) => c,
        Err(e) => return Ok(TokenAttempt::Revoked(format!("[proxy] {e:#}"))),
    };
    let err = match crate::oauth::refresh(&http, &cred.refresh_token).await {
        Ok(tokens) => {
            store.update_tokens(
                cred.id,
                &tokens.access_token,
                &tokens.refresh_token,
                tokens.expires_at,
            )?;
            // profile 字段还缺着的号（旧库、或登录时 profile 没拉到）顺手补一次。官方
            // `refreshOAuthToken` 也是这样：手里已有完整资料就跳过，否则刷新后紧接着拉
            // profile。只在缺项时拉，故每个号至多多一次往返；失败只记日志，不影响刷新结果。
            if cred.profile_incomplete() {
                match crate::oauth::fetch_profile(&http, &tokens.access_token).await {
                    Ok(profile) => {
                        if let Err(e) = store.apply_profile(
                            cred.id,
                            &profile,
                            tokens.organization_uuid.as_deref(),
                        ) {
                            tracing::warn!(cred_id = cred.id, error = %e, "failed to backfill profile fields after refresh");
                        }
                    }
                    Err(e) => {
                        tracing::warn!(cred_id = cred.id, error = %e, "fetching the profile after refresh failed, profile fields left as they were");
                    }
                }
            }
            return Ok(TokenAttempt::Ready(tokens.access_token));
        }
        Err(e) => e,
    };

    // 无论是否判定为永久失效，都把失败原文打出来：这个端点的失败响应形态我们没有实测样本，
    // 线上真出现一次就能据此收紧 `is_grant_revoked`。
    tracing::warn!(cred_id = cred.id, cred = %cred.label, error = %format!("{err:#}"), "token refresh failed");
    match err.downcast_ref::<crate::oauth::TokenEndpointError>() {
        Some(te) if te.is_grant_revoked() => Ok(TokenAttempt::Revoked(te.ban_reason())),
        // 网络抖动 / 5xx / 限流 / 非 invalid_grant 的 4xx：凭证本身可能是好的，不停用。
        // 带上完整错误链：最外层只有一句「request to the token endpoint failed」，
        // 连不上、超时还是代理拒绝全在里层。
        _ => Err(RefreshFailed {
            cred_id: cred.id,
            cred_label: cred.label.clone(),
            detail: format!("{err:#}"),
            already_paused: false,
        }
        .into()),
    }
}
