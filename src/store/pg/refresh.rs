//! access token 的取用、刷新与刷新失败时的换号（PG 版），对应 `store::refresh`。
//!
//! 类型（[`TokenAttempt`]、[`RefreshFailed`]）、常量与 [`refresh_ban`] 都用 `store::refresh` 的；
//! 这里只是把几个自由函数换成接收 [`PgStore`]。
//!
//! **刷新（上游网络往返）一律不在事务里、也不占着连接**：选号的写事务在
//! [`PgStore::select_with_slot`] 返回前就已提交，之后才去刷新；刷新期间只持有该凭证的进程内
//! 刷新锁（[`PgStore::refresh_lock`]），刷完再用连接池里的连接回写。

use anyhow::Result;

use super::super::{
    AttemptFut, Credential, MAX_REFRESH_FAIL_SWAPS, MAX_REFRESH_FAILOVER, REFRESH_FAIL_PAUSE_SECS,
    REFRESH_FAIL_PAUSE_TAG, RefreshFailed, Select, TokenAttempt, refresh_ban, refresh_pause_detail,
};
use super::PgStore;

/// 代理转发使用：按 device_id 粘性选出凭证并返回 (access_token, 该凭证, 会话槽位)（必要时刷新）。
///
/// 选择见 [`PgStore::select_with_slot`]。若命中的凭证进入刷新窗口，则调用 OAuth 刷新并回写。
/// 刷新是异步 IO，不持有任何事务或连接。
///
/// **刷新失败要自动换号**：选号在返回前就写好了设备绑定，之后才轮到刷新。若刷新失败直接把错误
/// 抛出去，这个设备就被钉死在坏号上。故这里在「refresh_token 已被作废」时停用该凭证
/// （`record_ban` 会连带清掉它的设备绑定），再重选一个号继续。网络抖动 / 5xx 这类刷新失败
/// 暂停一小会再换号，见 [`select_with_refresh_failover`]。
pub async fn valid_access_token_for_device(
    store: &PgStore,
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
    store: &PgStore,
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
    store: &PgStore,
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
    store: &PgStore,
    clients: &crate::clients::ClientPool,
    cred: &Credential,
) -> Result<TokenAttempt> {
    fresh_token(store, clients, cred, false).await
}

/// [`ensure_fresh_token`] 的实现。`skip_refresh_paused` 只有转发选号那条传 `true`：等锁期间
/// 别的请求刚刷失败、把号停了，就直接报那次失败，不再白等一次超时。连通性测试与保活传
/// `false`——测试正是要真刷一次看号回没回来。
pub(super) async fn fresh_token(
    store: &PgStore,
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
    let err = match crate::oauth::refresh(&http, &cred.refresh_token).await {
        Ok(tokens) => {
            store
                .update_tokens(
                    cred.id,
                    &tokens.access_token,
                    &tokens.refresh_token,
                    tokens.expires_at,
                )
                .await?;
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

#[cfg(test)]
mod tests {
    use sqlx::PgPool;

    use super::super::super::*;
    use super::super::select::tests::store_with;
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
}
