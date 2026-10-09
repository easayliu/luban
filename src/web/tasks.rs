//! 后台任务：启动时回填学到的规则，以及定时裁剪、规则重建、版本同步、会话保活与遥测发送。

use super::*;

/// 启动时回填学到的规则，并拉起各个后台循环。每个循环各自克隆要用的 `state` 字段。
pub(super) fn spawn_background_tasks(state: &AppState) {
    // 把上次运行学到的上游规则读回来（形态拒绝 / 已废弃字段 / 零输出请求类），免得每种组合
    // 重启后再白撞一次。读失败只告警：这是优化，不是启动的前提。
    match state.store.learned_rejections() {
        Ok(rows) => {
            let seeded = proxy::seed_learned_memories(
                &state.shape_rejections,
                &state.deprecated_fields,
                &state.empty_replies,
                rows,
            );
            // 按新逻辑不该存在的旧行：不回填，顺手从库里删掉。
            drop_stale_learned_rules(&state.store, &seeded.stale);
            let proxy::SeededMemories {
                shape, deprecated, empty_reply, refusal, app_refusal, ..
            } = seeded;
            if shape + deprecated + empty_reply + refusal + app_refusal > 0 {
                tracing::info!(
                    shape,
                    empty_reply,
                    refusal,
                    app_refusal,
                    deprecated,
                    "restored learned upstream rejections from the database"
                );
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to load learned rejections; starting with empty memories")
        }
    }

    // 每天裁剪一次用量日志流水：终身统计在账本里（见 store 的 credential_stats/device_costs），
    // 流水只需保留近期。interval 的首个 tick 立即触发，兼作启动清理；删除是分批短事务，
    // 走 spawn_blocking 避免拿着 SQLite 锁占住异步线程。
    {
        let store = state.store.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(24 * 3600));
            loop {
                tick.tick().await;
                let store = store.clone();
                match tokio::task::spawn_blocking(move || store.prune_usage_logs()).await {
                    Ok(Ok(n)) if n > 0 => tracing::info!(rows = n, "pruned expired usage logs"),
                    Ok(Err(e)) => tracing::warn!(error = %e, "failed to prune usage logs"),
                    _ => {}
                }
            }
        });
    }

    // 每分钟清一次过了保留期的绑定行（设备 + 模拟会话）。
    //
    // 这件事此前挂在选号路径上、每条转发请求跑一遍，两条 DELETE 各是一次按 last_seen_at 的
    // 全表扫加一次写事务，全程压着那把全局 `conn` 锁——会话绑定表默认保留 24 小时，多客户端
    // 时攒到几万行，实测单是扫一遍就要几毫秒，而它挡在每条请求的选号前面。
    //
    // 间隔取 1 分钟：保留期在后台可以按分钟配，清理的粒度要跟得上，否则配了 5 分钟的绑定
    // 要多挂好几分钟才真正删掉。两条 DELETE 都走 last_seen_at 索引，没有到期行时只是一次
    // 索引探查，一分钟一次压不到锁。选号侧已按保留期自己过滤，所以这个间隔只影响行什么时候
    // 真正删掉，不影响任何判定——跑得晚一点选出来的号完全一样。
    // 首个 tick 立即触发，兼作启动清理；同样走 spawn_blocking 不占异步线程。
    {
        let store = state.store.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(60));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let store = store.clone();
                match tokio::task::spawn_blocking(move || store.prune_expired_bindings()).await {
                    Ok(Ok((devices, sessions))) if devices + sessions > 0 => {
                        tracing::info!(devices, sessions, "pruned bindings past their retention")
                    }
                    Ok(Err(e)) => tracing::warn!(error = %e, "failed to prune expired bindings"),
                    _ => {}
                }
            }
        });
    }

    // 过期的控制台会话每小时清一次。认会话时过期的那条会顺手删掉，这里清的是再也没人
    // 拿来用过的（关了浏览器就没再回来）。首个 tick 立即触发，兼作启动清理。
    {
        let store = state.store.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                let store = store.clone();
                let result = tokio::task::spawn_blocking(move || {
                    store.prune_sessions()?;
                    // 启动时没清完的明文残留（被别的连接的读快照挡住了），在这里重试。
                    store.retry_pending_scrub()
                })
                .await;
                if let Ok(Err(e)) = result {
                    tracing::warn!(error = %e, "hourly console maintenance failed");
                }
            }
        });
    }

    // 学到的规则每小时按库重建一遍进程内记忆表：7 天保鲜期此前只在**读库**时生效
    // （`learned_rejections_with_time` 顺手删过期行），而请求路径判的是进程内 HashMap，进程
    // 不重启规则就永不过期——一条 7 天前学的拒答提示词能一直本地 403 下去。重建 = 读库
    // （过期行随手删掉）→ 清表 → 回填，同一把写锁内完成；读库到清表之间刚学到、还没落库的
    // 那一两条会从内存里掉一次，下个整点从库里回来，代价是那种组合多撞一次上游，可以接受。
    {
        let state = state.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(3600));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // 首个 tick 立即到，启动时已经回填过一遍，跳过。
            tick.tick().await;
            loop {
                tick.tick().await;
                let store = state.store.clone();
                let rows =
                    match tokio::task::spawn_blocking(move || store.learned_rejections()).await {
                        Ok(Ok(rows)) => rows,
                        Ok(Err(e)) => {
                            tracing::warn!(error = %e, "failed to reload learned rules for expiry");
                            continue;
                        }
                        Err(_) => continue,
                    };
                let before = proxy::learned_memory_len(
                    &state.shape_rejections,
                    &state.deprecated_fields,
                    &state.empty_replies,
                );
                let seeded = proxy::resync_learned_memories(
                    &state.shape_rejections,
                    &state.deprecated_fields,
                    &state.empty_replies,
                    rows,
                );
                // 启动时删过一遍，这里一般是空的；库被旧版本进程并行写过才会再有。
                drop_stale_learned_rules(&state.store, &seeded.stale);
                let after = seeded.shape + seeded.deprecated + seeded.empty_reply + seeded.refusal;
                if after != before {
                    tracing::info!(
                        before,
                        after,
                        "learned upstream rules resynced from the store; expired ones dropped from memory"
                    );
                }
            }
        });
    }

    // 官方最新发布版：先以库里上次学到（或网页上手动填）的值为准，之后每学到新值就写回。
    // 保活循环每 30min 学一次（下面），模拟会话的握手也会学。这样官方发新版后 luban 不用改
    // 代码，重启也不退回写死的 `CC_LATEST_KNOWN_RELEASE`。
    {
        sync_latest_release_from_store(&state.store);
        let store = state.store.clone();
        oauth::LATEST_RELEASE.install_persister(move |v| {
            store.set_setting(store::LATEST_CC_RELEASE, &oauth::release_string(v))
        });
    }

    // 会话保活（对齐 cap/2.1.145 抓包的真实客户端行为）：
    //   每 30min — event_logging + Datadog 遥测（idle 版本检查事件）  ← `keepalive_telemetry` 开关
    //   每张凭证在本进程里首次被保活 — bootstrap + penguin_mode + eval（启动握手），新加的号下个 tick 补
    //   每 1h   — policy_limits + settings
    //   每 6h   — eval（Statsig 特性标志刷新）                          ← 同一开关
    //   每 30min — downloads.claude.ai/claude-code-releases/latest（版本检查，无鉴权，
    //              每 tick 只拉一次、借第一张可用凭证的出口）：学到的最新版是来访 UA 自报
    //              版本的上限，见 `proxy::known_latest_release`
    // 指标不再由保活发假值：真实用量的指标由 `crate::telemetry` 按会话累计后发。
    // 空闲事件的身份优先挂到该凭证最近的真实会话上（同一 session_id / device_id / 版本），
    // 没有近期会话才用按账号派生的那套。
    {
        let store = state.store.clone();
        let clients = state.clients.clone();
        let telemetry = state.telemetry.clone();
        tokio::spawn(async move {
            let started = std::time::Instant::now();
            tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(
                crate::config::KEEPALIVE_INTERVAL_SECS,
            ));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            let mut tick_count: u64 = 0;
            // 本进程里已经跑过「首次保活」（bootstrap + penguin_mode + eval + policy/settings
            // 全套）的凭证。真实客户端每次进程启动都打一遍 bootstrap，luban 这边对应的是
            // 「这张凭证第一次被本进程保活」——按凭证记，而不是按进程的首个 tick：进程跑着时
            // 新加的号也得补上这一串，否则它在发出第一条 /v1/messages 之前只有 event_logging，
            // 从没 bootstrap 过。
            let mut seen: std::collections::HashSet<i64> = std::collections::HashSet::new();
            loop {
                tick.tick().await;
                tick_count += 1;
                let is_hourly = tick_count.is_multiple_of(crate::config::KEEPALIVE_HOURLY_TICKS);
                let is_eval_tick = tick_count.is_multiple_of(crate::config::KEEPALIVE_EVAL_TICKS);

                // 开关每 tick 重读：网页上拨了下一轮就生效。
                let send_telemetry = store.forward_flags().keepalive_telemetry;
                let creds = match store.list() {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(error = %e, "keepalive: failed to list credentials");
                        continue;
                    }
                };
                // 删掉的凭证不必再记着；同 id 不会复用（自增），忘了也无妨。已封禁的、订阅
                // 未生效暂停中的也忘掉：它们在下面被跳过（后者只刷 token），之后若被重新启用，
                // 就该像新号一样重新跑一遍 bootstrap，而不是因为停用前那一轮的标记还在而只发
                // event_logging。
                seen.retain(|id| {
                    creds
                        .iter()
                        .any(|c| c.id == *id && !c.is_banned() && !c.is_subscription_paused())
                });
                // 这一 tick 拉过 `releases/latest` 没有。它无鉴权、与账号无关，一轮拉一次就够；
                // 但也**不用直连**——借第一张能建出客户端的凭证的出口发，跟其他出站一个待遇。
                let mut fetched_latest = false;
                for cred in creds {
                    if cred.is_banned() {
                        continue;
                    }
                    let http = match clients.for_credential(&cred) {
                        Ok(c) => c,
                        Err(e) => {
                            let reason = format!("[proxy] {e:#}");
                            tracing::warn!(cred_id = cred.id, cred = %cred.label, error = %reason, "keepalive: proxy unusable, disabling the credential");
                            // 与转发路径的同一分支（`proxy::handle`）口径一致：来源 `proxy`、
                            // 完整错误进 error_message。保活没有来访请求，request_id 留空。
                            let _ = store.record_ban(
                                cred.id,
                                &store::BanContext {
                                    reason,
                                    source: "proxy",
                                    error_message: Some(format!("{e:#}")),
                                    ..Default::default()
                                },
                            );
                            continue;
                        }
                    };

                    if !fetched_latest {
                        fetched_latest = true;
                        let r = oauth::fetch_latest_release(&http).await;
                        if !r.is_ok() {
                            tracing::debug!(
                                cred_id = cred.id,
                                ?r,
                                "keepalive: releases/latest fetch failed"
                            );
                        }
                    }

                    // 保活前先确保 token 新鲜——懒刷新在无代理流量时不会触发，
                    // 长时间空闲会导致 refresh_token 过期（invalid_grant）。
                    let access_token = match store::ensure_fresh_token(&store, &clients, &cred)
                        .await
                    {
                        Ok(store::TokenAttempt::Ready(t)) => t,
                        Ok(store::TokenAttempt::Revoked(reason)) => {
                            tracing::warn!(cred_id = cred.id, cred = %cred.label, %reason, "keepalive: refresh_token revoked, disabling");
                            let _ = store.record_ban(cred.id, &store::refresh_ban(&reason));
                            continue;
                        }
                        Err(e) => {
                            tracing::warn!(cred_id = cred.id, cred = %cred.label, error = %e, "keepalive: token refresh failed (transient), skipping tick");
                            continue;
                        }
                    };

                    // 订阅未生效暂停中的号：**只刷 token**（上面刚刷过），遥测、握手、策略拉取
                    // 一概不发。它可能一停几周等续费，refresh_token 得靠这里保住，否则续费后第一次
                    // 连通性测试刷新失败、号被当成 token 作废封掉；但那些端点它每一发都是 403，
                    // 照发只会每轮多几条拒绝、每轮重跑一遍启动握手——真实客户端不会这样。
                    if cred.is_subscription_paused() {
                        continue;
                    }

                    // token 已就绪才算这张凭证的首次保活：刷新失败被 continue 掉的那轮不算，
                    // 下一轮再补 bootstrap。
                    let is_first = seen.insert(cred.id);
                    let is_eval = is_first || is_eval_tick;

                    // 组织 id 来自这个号最近一次 `/v1/messages` 响应头；还没转发过请求时缺省。
                    // 身份优先挂到该凭证最近的真实会话上（见 `KeepaliveCtx::new`）。
                    let session = telemetry.latest_session(
                        cred.id,
                        std::time::Duration::from_secs(crate::config::TELEMETRY_SESSION_IDLE_SECS),
                    );
                    let on_real_session = session.is_some();
                    let ctx = oauth::KeepaliveCtx::new(
                        &cred,
                        started.elapsed().as_secs_f64(),
                        telemetry.org_uuid(cred.id),
                        session,
                    );

                    // --- 每 tick：空闲遥测（可关） ---
                    let (ev_ok, dd_ok) = if send_telemetry {
                        let ev = oauth::keepalive_event_logging(&http, &access_token, &ctx).await;
                        if let oauth::KeepaliveResult::AuthRejected(rej) = &ev {
                            // 这一轮的首次握手没跑完：撤掉标记，下轮（没停成的号）或重新启用后
                            // （停成的号）重来一遍。库写失败时号仍是启用的，尤其不能当成「处理
                            // 完了」。唯一不撤的是订阅未生效：重来一遍只会再吃同样的 403——停成
                            // 了的号下一轮只刷 token（且被 `seen.retain` 忘掉），没停成的（人工
                            // 停用的号，保活照发）也不该每轮重发一遍启动握手。
                            if handle_keepalive_rejection(&store, &cred, rej)
                                != KeepaliveRejection::SubscriptionInactive
                            {
                                seen.remove(&cred.id);
                            }
                            continue;
                        }
                        (ev, oauth::keepalive_datadog_logs(&http, &ctx).await)
                    } else {
                        (oauth::KeepaliveResult::Ok, true)
                    };

                    // 这个号刚因为一个新会话跑过完整的启动握手（见
                    // [`crate::proxy::spawn_session_handshake`]）就别再发一遍：那一串已经
                    // 把 bootstrap / penguin / policy_limits / settings 全打过了，保活再来
                    // 一次，上游看到的是同一个账号几秒内把同一批端点打了两遍。
                    let just_handshook = oauth::handshake_recent(
                        cred.id,
                        std::time::Duration::from_secs(crate::config::KEEPALIVE_INTERVAL_SECS),
                    );

                    // --- 该凭证首次保活：启动握手 ---
                    let (boot_ok, peng_ok) = if is_first && !just_handshook {
                        let bo = oauth::keepalive_bootstrap(
                            &http,
                            &access_token,
                            &ctx,
                            &ctx.model_normalized(),
                        )
                        .await;
                        if let oauth::KeepaliveResult::AuthRejected(rej) = &bo {
                            // 同上：订阅未生效不撤标记。
                            if handle_keepalive_rejection(&store, &cred, rej)
                                != KeepaliveRejection::SubscriptionInactive
                            {
                                seen.remove(&cred.id);
                            }
                            continue;
                        }
                        let pg = oauth::keepalive_penguin_mode(&http, &access_token).await;
                        (bo, pg)
                    } else {
                        (oauth::KeepaliveResult::Ok, oauth::KeepaliveResult::Ok)
                    };

                    // --- 每 6h：eval（画像也算遥测，跟同一个开关） ---
                    // 同 policy/settings：刚因为新会话握过手的号跳过——那一串里已经发过
                    // 一次 eval，30 秒后的首个 tick 再发一次就是同一分钟内两条。
                    let eval_ok = if is_eval && send_telemetry && !just_handshook {
                        oauth::keepalive_eval(&http, &access_token, &ctx).await
                    } else {
                        oauth::KeepaliveResult::Ok
                    };

                    // --- 每 1h ---
                    // 官方这两条是同时发的（`cap/2.1.260-2` 的 00001/00002 相差 1ms），
                    // 一前一后 await 出来的间隔是 luban 自己造的。
                    let (pl_ok, st_ok) = if (is_hourly || is_first) && !just_handshook {
                        tokio::join!(
                            oauth::keepalive_policy_limits(&http, &access_token, &ctx),
                            oauth::keepalive_settings(&http, &access_token, &ctx),
                        )
                    } else {
                        (oauth::KeepaliveResult::Ok, oauth::KeepaliveResult::Ok)
                    };

                    let all_ok = ev_ok.is_ok()
                        && dd_ok
                        && boot_ok.is_ok()
                        && peng_ok.is_ok()
                        && eval_ok.is_ok()
                        && pl_ok.is_ok()
                        && st_ok.is_ok();
                    if all_ok {
                        tracing::debug!(
                            cred_id = cred.id, cred = %cred.label, tick = tick_count,
                            telemetry = send_telemetry, on_real_session,
                            session = %ctx.session_id.chars().take(8).collect::<String>(),
                            "keepalive: ok"
                        );
                    } else {
                        tracing::warn!(
                            cred_id = cred.id, cred = %cred.label, tick = tick_count,
                            telemetry = send_telemetry, on_real_session,
                            event_logging = ev_ok.is_ok(), datadog = dd_ok,
                            bootstrap = boot_ok.is_ok(), penguin = peng_ok.is_ok(),
                            eval = eval_ok.is_ok(), policy_limits = pl_ok.is_ok(), settings = st_ok.is_ok(),
                            "keepalive: partial failure"
                        );
                    }
                }
            }
        });
    }

    // 逐请求遥测的发送循环：每 5s 看一眼哪张凭证攒的事件/日志/指标到期了，用它自己的出站
    // 客户端与 token 发出去。见 [`crate::telemetry::run_flusher`]。
    tokio::spawn(crate::telemetry::run_flusher(
        state.telemetry.clone(),
        state.store.clone(),
        state.clients.clone(),
    ));
}
