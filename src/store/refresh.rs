//! access token 的取用、刷新与刷新失败时的换号。
//!
//! **刷新（上游网络往返）一律不在事务里、也不占着连接**：选号的写事务在
//! [`CredentialStore::select_with_slot`] 返回前就已提交，之后才去刷新；刷新期间只持有该凭证的进程内
//! 刷新锁（[`CredentialStore::refresh_lock`]），刷完再用连接池里的连接回写。

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

use anyhow::Result;

use super::CredentialStore;
use super::{Credential, Select};

/// 代理转发使用：按 device_id 粘性选出凭证并返回 (access_token, 该凭证, 会话槽位)（必要时刷新）。
///
/// 选择见 [`CredentialStore::select_with_slot`]。若命中的凭证进入刷新窗口，则调用 OAuth 刷新并回写。
/// 刷新是异步 IO，不持有任何事务或连接。
///
/// **刷新失败要自动换号**：选号在返回前就写好了设备绑定，之后才轮到刷新。若刷新失败直接把错误
/// 抛出去，这个设备就被钉死在坏号上。故这里在「refresh_token 已被作废」时停用该凭证
/// （`record_ban` 会连带清掉它的设备绑定），再重选一个号继续。网络抖动 / 5xx 这类刷新失败
/// 暂停一小会再换号，见 [`select_with_refresh_failover`]。
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
/// 连通性测试用（见 [`crate::proxy::probe`]）：测试是指名道姓要测这一个。**失败停用的口径与
/// 转发一致**：`refresh_token` 已被作废这个结论不因「是测试触发的」就打折扣。区别只在**不换号**：
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
            if let Err(e) = store.record_ban(cred.id, &refresh_ban(&reason)).await {
                tracing::warn!(error = %e, "failed to auto-disable the credential");
            }
            anyhow::bail!("{reason}")
        }
    }
}

/// [`valid_access_token_for_device`] 的重选循环本体。把「取 token」这一步抽成参数注入，
/// 是为了让换号逻辑本身能脱离网络被测到。
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
        let (cred, slot) = match store.select_with_slot(sel).await {
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
                if !store.record_ban(cred.id, &refresh_ban(&reason)).await? {
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
                    if !store.pause_for_rate_limit(cred.id, &reason, resume_at).await? {
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
///
/// 刷新锁是进程内的：多个进程共用一个库时各刷各的，仍可能互相作废 refresh_token。
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

    let guard = store.refresh_lock(cred.id).lock_owned().await;
    // 双重检查：等锁期间可能已被其它请求刷新过。
    let cred = store.get(cred.id).await?.unwrap_or_else(|| cred.clone());
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
    // 都会有一次带真实 IP 的请求打到上游。取的是双重检查之后那份 `cred`——等锁期间代理可能
    // 刚被改过。代理建不出来是永久配置错误，走 Revoked 让上层停用、踢出调度池。
    let http = match clients.for_credential(&cred) {
        Ok(c) => c,
        Err(e) => return Ok(TokenAttempt::Revoked(format!("[proxy] {e:#}"))),
    };
    let err = match refresh_detached(store, &http, cred.id, &cred.refresh_token, guard).await? {
        Ok(tokens) => {
            // profile 字段还缺着的号（旧库、或登录时 profile 没拉到）顺手补一次。只在缺项时拉，
            // 故每个号至多多一次往返；失败只记日志，不影响刷新结果。
            if cred.profile_incomplete() {
                match crate::oauth::fetch_profile(&http, &tokens.access_token).await {
                    Ok(profile) => {
                        if let Err(e) = store
                            .apply_profile(cred.id, &profile, tokens.organization_uuid.as_deref())
                            .await
                        {
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

    // 无论是否判定为永久失效，都把失败原文打出来：线上真出现一次就能据此收紧 `is_grant_revoked`。
    tracing::warn!(cred_id = cred.id, cred = %cred.label, error = %format!("{err:#}"), "token refresh failed");
    match err.downcast_ref::<crate::oauth::TokenEndpointError>() {
        Some(te) if te.is_grant_revoked() => Ok(TokenAttempt::Revoked(te.ban_reason())),
        // 网络抖动 / 5xx / 限流 / 非 invalid_grant 的 4xx：凭证本身可能是好的，不停用。
        // 带上完整错误链：最外层只有一句「request to the token endpoint failed」。
        _ => Err(RefreshFailed {
            cred_id: cred.id,
            cred_label: cred.label.clone(),
            detail: format!("{err:#}"),
            already_paused: false,
        }
        .into()),
    }
}

/// [`force_refresh`] 的结果。
pub enum ManualRefresh {
    /// 号已不存在。
    Gone,
    /// 号的出站代理建不出来（永久配置错误），没有发刷新。
    Proxy(anyhow::Error),
    /// 发了刷新。`label` 是重读到的号名；`http` 是按最新凭证建的客户端，刷新后的后续请求
    /// （拉 profile）接着用它；`result` 的错误是上游刷新失败，原样交回，调用方按
    /// [`crate::oauth::TokenEndpointError`] 判。
    Refreshed { label: String, http: wreq::Client, result: Result<crate::oauth::TokenSet> },
}

/// 手动刷新（控制台的「刷新 token」）：不看是否到期，拿着刷新锁、按库里**最新**的凭证刷一次，
/// 落库同自动刷新（见 [`refresh_detached`]）。
///
/// refresh_token 与出站代理都在拿到锁之后重读：等锁期间自动刷新可能刚轮换过 refresh_token
/// （用旧的必吃 `invalid_grant`，号被当成封禁停掉），代理也可能刚被改过（用旧客户端会从
/// 改掉的那个出口发出去）。外层错误 = 读库 / 落库失败。
pub async fn force_refresh(
    store: &CredentialStore,
    clients: &crate::clients::ClientPool,
    cred_id: i64,
) -> Result<ManualRefresh> {
    let guard = store.refresh_lock(cred_id).lock_owned().await;
    let Some(cred) = store.get(cred_id).await? else {
        return Ok(ManualRefresh::Gone);
    };
    let http = match clients.for_credential(&cred) {
        Ok(c) => c,
        Err(e) => return Ok(ManualRefresh::Proxy(e)),
    };
    let result = refresh_detached(store, &http, cred.id, &cred.refresh_token, guard).await?;
    Ok(ManualRefresh::Refreshed { label: cred.label, http, result })
}

/// 用 `refresh_token` 向上游换新 token 并落库，整段放进脱离调用方的任务、连同刷新锁一起
/// 交过去：上游一刷就**轮换 refresh_token**，新 token 要是因为客户端断开（handler 的 future
/// 被丢掉）或一次取连接超时没写进库，库里只剩已作废的旧 token，之后每次刷新都
/// `invalid_grant`，号就废了。锁要跟着任务走，否则调用方被取消后锁先放了，下一个请求读到
/// 旧 token 又去刷一次。任务算进 [`CredentialStore::drain_writes`] 要等的笔数，关停时也等它。
///
/// 外层错误 = 落库失败（已重试过）；内层错误 = 上游刷新失败。
async fn refresh_detached(
    store: &CredentialStore,
    http: &wreq::Client,
    cred_id: i64,
    refresh_token: &str,
    guard: tokio::sync::OwnedMutexGuard<()>,
) -> Result<Result<crate::oauth::TokenSet>> {
    let (pool, http, refresh_token) = (store.pool.clone(), http.clone(), refresh_token.to_string());
    store
        .run_tracked(async move {
            let _guard = guard;
            let tokens = match crate::oauth::refresh(&http, &refresh_token).await {
                Ok(t) => t,
                Err(e) => return Ok(Err(e)),
            };
            persist_tokens(&pool, cred_id, &tokens).await?;
            Ok(Ok(tokens))
        })
        .await
}

/// 刷新拿到的新 token 落库。取连接超时之类的一过性错误重试几次：这一笔丢了，号就废了，
/// 见 [`fresh_token`]。
async fn persist_tokens(
    pool: &sqlx::PgPool,
    id: i64,
    tokens: &crate::oauth::TokenSet,
) -> Result<()> {
    const ATTEMPTS: u32 = 5;
    let mut attempt = 1;
    loop {
        let r = super::credential::write_tokens(
            pool,
            id,
            &tokens.access_token,
            &tokens.refresh_token,
            tokens.expires_at,
        )
        .await;
        match r {
            Ok(_) => return Ok(()),
            Err(e) if attempt < ATTEMPTS => {
                tracing::warn!(cred_id = id, attempt, error = %format!("{e:#}"), "failed to store refreshed tokens, retrying");
                tokio::time::sleep(std::time::Duration::from_millis(200 << attempt)).await;
                attempt += 1;
            }
            Err(e) => return Err(e.context("failed to store refreshed tokens")),
        }
    }
}

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::super::select::tests::store_with;
    use super::super::*;
    use super::{fresh_token, select_with_refresh_failover};

    const REVOKED: &str = "[refresh 400] invalid_grant";

    fn refresh_failed(cred: &Credential) -> anyhow::Error {
        RefreshFailed {
            cred_id: cred.id,
            cred_label: cred.label.clone(),
            detail: "request to the token endpoint failed: connection refused".into(),
            already_paused: false,
        }
        .into()
    }

    /// 刷新失败要自动换号：坏号被停用、设备改绑到下一个可用号，请求正常拿到 token。
    ///
    /// 这是本次修复的核心——此前刷新失败直接抛错，而设备绑定在选号时就已写库，
    /// 导致该设备永远选回同一个坏号、永远 503。
    #[sqlx::test]
    async fn refresh_failure_fails_over_to_next_credential(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b", "c"]).await;
        let tried = std::cell::RefCell::new(Vec::new());

        // a、b 的 refresh_token 已作废，c 正常。
        let (token, cred, _) = select_with_refresh_failover(
            &store,
            Select { device_id: Some("dev-1"), rate_limited: true, ..Default::default() },
            |c| {
                tried.borrow_mut().push(c.id);
                Box::pin(async move {
                    Ok(if c.label == "c" {
                        TokenAttempt::Ready("good-token".into())
                    } else {
                        TokenAttempt::Revoked(REVOKED.into())
                    })
                })
            },
        )
        .await
        .unwrap();

        assert_eq!(token, "good-token");
        assert_eq!(cred.id, ids[2], "应换到第一个刷新得动的号");
        assert_eq!(*tried.borrow(), ids, "应按优先级依次试过 a、b、c");

        // a、b 被停用并记了原因；c 不受影响。
        for id in &ids[..2] {
            let c = store.get(*id).await.unwrap().unwrap();
            assert!(c.disabled, "作废的号应被停用");
            assert_eq!(c.ban_reason.as_deref(), Some(REVOKED));
        }
        assert!(!store.get(ids[2]).await.unwrap().unwrap().disabled);

        // 设备最终绑在 c 上，后续请求直接命中它，不再重走换号。
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .unwrap()
                .id,
            ids[2]
        );
    }

    /// 可重试错误（网络抖动、5xx、限流）**不得**停用凭证——误停一个健康账号的代价，
    /// 远高于让客户端重试一次。
    #[sqlx::test]
    async fn transient_refresh_error_does_not_disable(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let calls = std::cell::Cell::new(0);

        let e = select_with_refresh_failover(
            &store,
            Select { device_id: Some("dev-1"), rate_limited: true, ..Default::default() },
            |_| {
                calls.set(calls.get() + 1);
                Box::pin(async { anyhow::bail!("请求 token 端点失败: connection reset") })
            },
        )
        .await
        .unwrap_err();

        assert!(e.to_string().contains("connection reset"), "应原样抛出底层错误: {e}");
        assert_eq!(calls.get(), 1, "可重试错误应立即返回，不该继续换号");
        for id in &ids {
            assert!(!store.get(*id).await.unwrap().unwrap().disabled, "可重试错误不得停用凭证");
        }
        // 绑定保留，客户端重试时仍落回同一个号。
        assert_eq!(
            store
                .select_for_device(Select {
                    device_id: Some("dev-1"),
                    ttl_secs: 0,
                    rate_limited: true,
                    exclude: &[],
                    ..Default::default()
                })
                .await
                .unwrap()
                .id,
            ids[0]
        );
    }

    /// 刷新没拿到结果（网络 / 代理）：这个号限时暂停、原因带完整错误链，当场换下一个号。
    #[sqlx::test]
    async fn refresh_failure_pauses_the_credential_and_fails_over(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;

        let (token, cred, _) = select_with_refresh_failover(
            &store,
            Select { device_id: Some("dev-1"), rate_limited: true, ..Default::default() },
            |c| {
                let first = c.id == ids[0];
                Box::pin(async move {
                    if first {
                        Err(refresh_failed(&c))
                    } else {
                        Ok(TokenAttempt::Ready("t".into()))
                    }
                })
            },
        )
        .await
        .unwrap();

        assert_eq!((token.as_str(), cred.id), ("t", ids[1]));
        let paused = store.get(ids[0]).await.unwrap().unwrap();
        assert!(paused.disabled);
        assert!(paused.resume_at.is_some(), "限时暂停，不是封号");
        let reason = paused.ban_reason.unwrap();
        assert!(reason.starts_with(&format!("{REFRESH_FAIL_PAUSE_TAG} ")), "{reason}");
        assert!(reason.contains("connection refused"), "原因要带底层错误: {reason}");
    }

    /// 换不到别的号时报的是那次刷新失败（带号），不是选号那句「全员冷却」。
    #[sqlx::test]
    async fn refresh_failure_without_another_credential_reports_the_credential(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a"]).await;

        let e = select_with_refresh_failover(
            &store,
            Select { device_id: Some("dev-1"), rate_limited: true, ..Default::default() },
            |c| Box::pin(async move { Err(refresh_failed(&c)) }),
        )
        .await
        .unwrap_err();

        let rf = e.downcast_ref::<RefreshFailed>().expect("应报 RefreshFailed");
        assert_eq!(rf.cred_id, ids[0]);
        assert!(store.get(ids[0]).await.unwrap().unwrap().resume_at.is_some());
    }

    /// 刷新失败换号有上限：本机网络挂了时每个号都会失败，不能一条请求挨个试完、停掉一整池。
    #[sqlx::test]
    async fn refresh_failure_swaps_are_capped(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b", "c"]).await;
        let tried = std::cell::RefCell::new(Vec::new());

        let e = select_with_refresh_failover(
            &store,
            Select { device_id: Some("dev-1"), rate_limited: true, ..Default::default() },
            |c| {
                tried.borrow_mut().push(c.id);
                Box::pin(async move { Err(refresh_failed(&c)) })
            },
        )
        .await
        .unwrap_err();

        assert!(e.downcast_ref::<RefreshFailed>().is_some(), "{e}");
        assert_eq!(tried.borrow().len(), MAX_REFRESH_FAIL_SWAPS);
        assert!(!store.get(ids[2]).await.unwrap().unwrap().disabled, "没试到的号不该被停");
    }

    /// 已经因刷新失败暂停着的号（等锁期间别的请求刚停的）：直接报那次失败、不再真刷，
    /// 换号循环也不再写暂停，恢复时刻不被往后推。
    #[sqlx::test]
    async fn refresh_paused_credential_is_not_refreshed_again(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a"]).await;
        let reason = format!("{REFRESH_FAIL_PAUSE_TAG} connection refused");
        let resume_at = crate::credentials::now_secs() + 60;
        assert!(store.pause_for_rate_limit(ids[0], &reason, resume_at).await.unwrap());
        let cred = store.get(ids[0]).await.unwrap().unwrap();
        let clients = crate::clients::ClientPool::new().unwrap();

        let Err(e) = fresh_token(&store, &clients, &cred, true).await else {
            panic!("暂停中的号不该再刷")
        };
        let rf = e.downcast_ref::<RefreshFailed>().expect("应报 RefreshFailed");
        assert!(rf.already_paused);
        assert_eq!(rf.detail, "connection refused");

        let e = select_with_refresh_failover(
            &store,
            Select { device_id: Some("dev-1"), rate_limited: true, ..Default::default() },
            |c| Box::pin(async move { Err(RefreshFailed::paused(c.id, c.label, "x").into()) }),
        )
        .await
        .unwrap_err();
        // 池里只剩这个暂停的号：选号直接报全池暂停，且认得出是刷新失败停的。
        let rl = e.downcast_ref::<AllRateLimited>().expect("应报 AllRateLimited");
        let rf = rl.refresh_failed.as_ref().expect("要带上刷新失败的号");
        assert_eq!(rf.cred_id, ids[0]);
        assert!(e.to_string().contains("token refresh failure"), "{e}");
        assert_eq!(store.get(ids[0]).await.unwrap().unwrap().resume_at, Some(resume_at));
    }

    /// 所有号的 refresh_token 都作废时要报错收场，不能死循环、也不能返回停用的号。
    #[sqlx::test]
    async fn all_credentials_revoked_gives_up(pool: PgPool) {
        let (store, ids) = store_with(pool, &["a", "b"]).await;
        let tried = std::cell::RefCell::new(Vec::new());

        let e = select_with_refresh_failover(
            &store,
            Select { device_id: Some("dev-1"), rate_limited: true, ..Default::default() },
            |c| {
                tried.borrow_mut().push(c.id);
                Box::pin(async { Ok(TokenAttempt::Revoked(REVOKED.into())) })
            },
        )
        .await
        .unwrap_err();

        // 号用完后是 select_for_device 先报「没有可用凭证」，而不是转满 MAX_REFRESH_FAILOVER 圈。
        assert!(
            e.to_string().contains("no available credentials"),
            "error message should identify the root cause: {e}"
        );
        assert_eq!(*tried.borrow(), ids, "每个号都应被试过一次，且只试一次");
        assert!(store.list().await.unwrap().iter().all(|c| c.disabled));
    }

    /// 刷新落库的任务算进关停等待：调用方被取消、落库又被行锁卡着时，`drain_writes` 要等它
    /// 写完，不能立刻返回让运行时把它丢掉（库里留下已作废的旧 refresh_token）。
    #[sqlx::test]
    async fn detached_token_write_is_awaited_on_shutdown(pool: PgPool) {
        use std::sync::atomic::Ordering;
        let (store, ids) = store_with(pool.clone(), &["a"]).await;
        let id = ids[0];

        let mut blocker = pool.begin().await.unwrap();
        sqlx::query("SELECT id FROM credentials WHERE id = $1 FOR UPDATE")
            .bind(id)
            .execute(&mut *blocker)
            .await
            .unwrap();

        let tokens = crate::oauth::TokenSet {
            access_token: "new-access".into(),
            refresh_token: "new-refresh".into(),
            expires_at: 1,
            account: None,
            account_uuid: None,
            organization_uuid: None,
        };
        let p = pool.clone();
        let write = store.run_tracked(async move { super::persist_tokens(&p, id, &tokens).await });
        // 调用方等不及、被取消。
        assert!(tokio::time::timeout(std::time::Duration::from_millis(200), write).await.is_err());
        assert_eq!(store.pending_writes.load(Ordering::SeqCst), 1, "被取消后任务仍在途");

        blocker.commit().await.unwrap();
        store.drain_writes(std::time::Duration::from_secs(5)).await;
        assert_eq!(store.pending_writes.load(Ordering::SeqCst), 0);
        assert_eq!(store.get(id).await.unwrap().unwrap().refresh_token, "new-refresh");
    }
}
