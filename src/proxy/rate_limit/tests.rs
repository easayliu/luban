use crate::proxy::{HeaderValue, header, store};

/// 429 作用域判定的回归用例，**头的取值逐字节取自两次真实的 fable-5 429**
/// （基础 5h/7d 都有余量，满掉的只有 `7d_oi`——fable 专用的超额池）。
///
/// 这条用例存在的理由：第二版判定把「任一窗口被拒/打满」一律判账号级，于是 fable
/// 吃满超额池就把整个账号冷却 24 小时——实测 7d_oi 仍 rejected 期间同一账号的
/// sonnet/opus 照常 200，账号级冷却纯属误伤。见 [`crate::proxy::rate_limit_scope`] 的演化史。
/// 线上实测（2026-09-02）：Pro 号打 fable 的 429 **一个额度窗口头都不带**，只有
/// `overage-disabled-reason=org_level_disabled`、几条 `credits-*` 和一个月后的
/// `unified-reset`。这不是限流而是「套餐不含」；此前被判成 transient，冷却 30 秒且不换号，
/// 客户端拿到 429 反复重试。
#[test]
fn plan_denial_429_is_unsupported_not_transient() {
    let hdr = |pairs: &[(&str, &str)]| {
        let mut h = crate::proxy::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                crate::proxy::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        crate::proxy::RateLimitInfo::from_headers(&h)
    };
    let fable = "claude-fable-5";
    let info = hdr(&[
        ("anthropic-ratelimit-unified-overage-disabled-reason", "org_level_disabled"),
        ("anthropic-ratelimit-unified-credits-can-purchase", "true"),
        ("anthropic-ratelimit-unified-credits-has-payment-method", "false"),
        ("anthropic-ratelimit-unified-credits-exhausted-included", "false"),
        ("anthropic-ratelimit-unified-reset", "1790812800"),
        ("anthropic-organization-id", "org"),
    ]);
    let scope = crate::proxy::rate_limit_scope(&info, Some(fable));
    assert_eq!(scope, crate::proxy::LimitScope::Unsupported(fable.into()));
    assert!(scope.worth_swapping() && !scope.account_level());
    assert_eq!(scope.model(), Some(fable));
    assert_eq!(info.cooldown_for(&scope), std::time::Duration::ZERO, "不打冷却");
    assert!(info.plan_denial_reason().contains("org_level_disabled"));
    assert_eq!(info.unified_reset, Some(1_790_812_800), "记录到点失效的时刻取 unified-reset");
    assert!(!info.no_limit_headers(), "它带了 reset，不是裸 429");

    // 同一个 overage-disabled-reason 若伴随满掉的超额池窗口，仍是「超额池满」：账号确实
    // 有 fable 额度，只是用完了，那一档走冷却、到点回来。
    let pool_full = hdr(&[
        ("anthropic-ratelimit-unified-overage-disabled-reason", "org_level_disabled"),
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
        ("anthropic-ratelimit-unified-7d_oi-utilization", "1.02"),
    ]);
    assert_eq!(crate::proxy::rate_limit_scope(&pool_full, Some(fable)).label(), "model");
    // 请求体里读不出模型名时照旧账号级兜底——没有模型可挂。
    assert!(crate::proxy::rate_limit_scope(&info, None).account_level());

    // 线上第二例：同一形态落在 sonnet-4-6 上。sonnet 是所有付费套餐都含的，这既不是
    // 「套餐不含」也不是撞上限（没有任何窗口说满了）：不落库、不停号，只给该号该模型 30 秒
    // 短冷却并换号重发。
    let sonnet = "claude-sonnet-4-6";
    let odd = hdr(&[
        ("anthropic-ratelimit-unified-overage-disabled-reason", "org_level_disabled"),
        (
            "anthropic-ratelimit-unified-reset",
            &(crate::credentials::now_secs() + 29 * 86400).to_string(),
        ),
    ]);
    let scope = crate::proxy::rate_limit_scope_for(&odd, Some(sonnet), false);
    assert_eq!(scope, crate::proxy::LimitScope::OverageDisabled(sonnet.into()));
    assert!(!scope.account_level() && scope.worth_swapping());
    assert_eq!(scope.model(), Some(sonnet), "模型级短冷却，不碰账号");
    assert_eq!(
        odd.cooldown_for(&scope).as_secs() as i64,
        crate::proxy::DEFAULT_MODEL_COOLDOWN_SECS,
        "没有 retry-after 就是 30 秒兜底，绝不睡到一个月后的 reset"
    );
    // 同一形态、fable、但账号是 Max：不可能是「套餐不含」，同样只做短冷却 + 换号。
    assert_eq!(
        crate::proxy::rate_limit_scope_for(&odd, Some("claude-fable-5-1[1m]"), true),
        crate::proxy::LimitScope::OverageDisabled("claude-fable-5-1[1m]".into())
    );
    // fable + 非 Max 才记「套餐不含」。
    assert_eq!(
        crate::proxy::rate_limit_scope_for(&odd, Some("claude-fable-5-1[1m]"), false).label(),
        "unsupported"
    );
    // 这一档即便上游顺手给了个巨大的 retry-after，也夹在瞬时上限内——它不代表撞上限。
    let with_retry = hdr(&[
        ("anthropic-ratelimit-unified-overage-disabled-reason", "org_level_disabled"),
        ("retry-after", "304802"),
    ]);
    let scope = crate::proxy::rate_limit_scope_for(&with_retry, Some(sonnet), false);
    assert_eq!(
        with_retry.cooldown_for(&scope).as_secs() as i64,
        crate::proxy::MAX_TRANSIENT_COOLDOWN_SECS
    );
}

#[test]
fn rate_limit_scope_reads_every_window_not_just_5h_7d() {
    let hdr = |pairs: &[(&str, &str)]| {
        let mut h = crate::proxy::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                crate::proxy::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        crate::proxy::RateLimitInfo::from_headers(&h)
    };
    let fable = Some("claude-fable-5");
    let real = hdr(&[
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-representative-claim", "seven_day_overage_included"),
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-5h-utilization", "0.08"),
        ("anthropic-ratelimit-unified-7d-status", "allowed_warning"),
        ("anthropic-ratelimit-unified-7d-utilization", "0.76"),
        ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
        ("anthropic-ratelimit-unified-7d_oi-utilization", "1.01"),
        ("retry-after", "228721"),
    ]);
    let scope = crate::proxy::rate_limit_scope(&real, fable);
    assert_eq!(scope.model(), fable, "只有超额池（7d_oi）满 → 模型级，账号其余模型照常");
    // retry-after 优先且原样吃下（63 小时直指超额池的重置时刻）：睡满它、到点自己回池，
    // 中途放出去只会白撞 429——上限只挡明显异常的头，见 [`MAX_RATE_LIMIT_COOLDOWN_SECS`]。
    assert_eq!(real.cooldown(false).as_secs(), 228721);

    // 第二次抓包（2026-07-30，#54）：多了 overage-status 与 org_level_disabled，
    // 判定应当相同。overage 窗口被拒同样不算账号级。
    let real2 = hdr(&[
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-representative-claim", "seven_day_overage_included"),
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-5h-utilization", "0.2"),
        ("anthropic-ratelimit-unified-7d-status", "allowed"),
        ("anthropic-ratelimit-unified-7d-utilization", "0.7"),
        ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
        ("anthropic-ratelimit-unified-7d_oi-utilization", "1.02"),
        ("anthropic-ratelimit-unified-overage-status", "rejected"),
        ("retry-after", "304802"),
    ]);
    assert_eq!(crate::proxy::rate_limit_scope(&real2, fable).model(), fable);

    // 基础窗口自己满掉才是账号级：5h 被拒 → 所有模型一起让位。
    let exhausted = hdr(&[
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-utilization", "1.0"),
        ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
    ]);
    assert!(crate::proxy::rate_limit_scope(&exhausted, fable).account_level());
    // 没有任何逐窗口明细时，unified-status=rejected 兜底判账号级——宁可保守。
    let unified_only = hdr(&[("anthropic-ratelimit-unified-status", "rejected")]);
    assert!(crate::proxy::rate_limit_scope(&unified_only, fable).account_level());

    // 所有窗口都还有余量却被拒 → 这才是模型容量限制，只冷却该模型且不吃 reset。
    let far = crate::credentials::now_secs() as i64 + 4 * 3600;
    let capacity = hdr(&[
        ("anthropic-ratelimit-unified-status", "allowed"),
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-5h-utilization", "0.32"),
        ("anthropic-ratelimit-unified-5h-reset", &far.to_string()),
        ("anthropic-ratelimit-unified-reset", &far.to_string()),
    ]);
    let scope = crate::proxy::rate_limit_scope(&capacity, fable);
    assert_eq!(scope.model(), fable, "窗口都没满只该冷却这一个模型");
    assert!(!scope.worth_swapping(), "窗口都没满 → 不是这个号的问题，换号无益");
    assert_eq!(capacity.cooldown(false).as_secs(), 30, "模型级不该拿 reset 当冷却");
    assert!(capacity.cooldown(true).as_secs() > 3000, "账号级才按 reset 冷却");

    // retry-after 两档都优先；读不出模型名保守退回账号级；什么头都没有用默认值。
    let with_retry = hdr(&[("retry-after", "7")]);
    assert_eq!(with_retry.cooldown(false).as_secs(), 7);
    assert_eq!(with_retry.cooldown(true).as_secs(), 7);
    let bare = hdr(&[]);
    assert!(crate::proxy::rate_limit_scope(&bare, None).account_level());
    assert_eq!(bare.cooldown(true).as_secs(), 60);

    // 7d 窗口耗尽要睡满 7 天（冷却是硬门禁，中途放出去只会白撞）；离谱的头才被上限挡下。
    let seven_d = hdr(&[("retry-after", &(7 * 24 * 3600).to_string())]);
    assert_eq!(seven_d.cooldown(true).as_secs(), 7 * 24 * 3600);
    let absurd = hdr(&[("retry-after", "999999999")]);
    assert_eq!(absurd.cooldown(true).as_secs(), crate::proxy::MAX_RATE_LIMIT_COOLDOWN_SECS as u64);
}

/// 「一个号被限流，所有号的卡片上都显示这个模型在冷却」那条线上问题的回归测试。
///
/// 成因是两件事叠在一起：**谁的额度都没满**的那种 429（模型容量限制、请求速率限制）
/// 曾与「超额池满」同判模型级，于是换号重试会拿同一条请求去下一个号上撞同一堵墙，把同一个
/// 模型的冷却一路盖满整池；而冷却是选号硬门禁，盖满之后新请求一条都进不来。且那种 429 上
/// 游偶尔会带一个按额度窗口算的大 `retry-after`，照单全收就是几十小时。
///
/// 故这一档单列成 [`LimitScope::Transient`]：不换号（只冷却撞上的那个号）、冷却夹在
/// [`MAX_TRANSIENT_COOLDOWN_SECS`] 以内。额度池满那一档的行为**不变**——额度是跟着账号
/// 走的，换号确实可能还有余量。
#[test]
fn a_429_that_is_not_this_credentials_fault_does_not_walk_the_pool() {
    let hdr = |pairs: &[(&str, &str)]| {
        let mut h = crate::proxy::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                crate::proxy::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        crate::proxy::RateLimitInfo::from_headers(&h)
    };
    let fable = Some("claude-fable-5");

    // 1) 超额池满（线上实测那份头）：这是这个号的额度，换号仍有意义，冷却照 retry-after
    //    睡满——两项都保持原样。
    let oi_full = hdr(&[
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-5h-utilization", "0.09"),
        ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
        ("anthropic-ratelimit-unified-7d_oi-utilization", "1.01"),
        ("retry-after", "228473"),
    ]);
    let scope = crate::proxy::rate_limit_scope(&oi_full, fable);
    assert_eq!(scope.model(), fable);
    assert!(scope.worth_swapping(), "额度池是跟着账号走的，换号可能还有余量");
    assert_eq!(oi_full.cooldown_for(&scope).as_secs(), 228473, "额度那档睡满 retry-after");

    // 2) 请求速率限制：窗口全都 allowed，却带了一个按额度窗口算出来的大 retry-after。
    //    不换号，且冷却夹到一分钟——照单全收会因为一阵拥堵把这个号锁掉两天多。
    let throttled = hdr(&[
        ("anthropic-ratelimit-unified-status", "allowed"),
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-5h-utilization", "0.11"),
        ("anthropic-ratelimit-unified-7d-status", "allowed"),
        ("anthropic-ratelimit-unified-7d-utilization", "0.40"),
        ("retry-after", "228473"),
    ]);
    let scope = crate::proxy::rate_limit_scope(&throttled, fable);
    assert!(!scope.worth_swapping(), "谁的额度都没满 → 换号只会在下一个号上撞同一发 429");
    assert_eq!(scope.model(), fable, "冷却仍落在这个号的这个模型上");
    assert_eq!(
        throttled.cooldown_for(&scope).as_secs(),
        crate::proxy::MAX_TRANSIENT_COOLDOWN_SECS as u64,
        "瞬时限流的冷却要被夹住"
    );

    // 3) 一个限流头都不带的 429（上游只给了 retry-after）：同样不换号，冷却照它给的秒数。
    let bare = hdr(&[("retry-after", "7")]);
    let scope = crate::proxy::rate_limit_scope(&bare, fable);
    assert!(!scope.worth_swapping());
    assert_eq!(bare.cooldown_for(&scope).as_secs(), 7);
    // 连 retry-after 都没有时退回模型级默认值，不是账号级那个 60 秒。
    let nothing = hdr(&[]);
    let scope = crate::proxy::rate_limit_scope(&nothing, fable);
    assert_eq!(
        nothing.cooldown_for(&scope).as_secs(),
        crate::proxy::DEFAULT_MODEL_COOLDOWN_SECS as u64
    );

    // 4) 账号级（基础窗口耗尽）照旧：换号有意义，且睡满窗口 reset。
    let exhausted = hdr(&[
        ("anthropic-ratelimit-unified-5h-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-utilization", "1.0"),
        ("retry-after", "3600"),
    ]);
    let scope = crate::proxy::rate_limit_scope(&exhausted, fable);
    assert!(scope.account_level() && scope.worth_swapping());
    assert_eq!(exhausted.cooldown_for(&scope).as_secs(), 3600);
}

/// 「限流头一条都没带」的判据不能靠 [`RateLimitInfo::raw`] 是否为空——线上那发裸 429
/// 的 `raw` 里躺着 `anthropic-organization-id` 与 `anthropic-workspace-id`（收头的过滤
/// 条件包含整个 `anthropic-` 前缀），非空却没有半点限流信息。这一列决定 429 要不要额外
/// 把响应体打出来，判错就是「该打的不打／不该打的每条都打」。
#[test]
fn no_limit_headers_ignores_the_non_ratelimit_anthropic_headers() {
    let hdr = |pairs: &[(&str, &str)]| {
        let mut h = crate::proxy::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                crate::proxy::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        crate::proxy::RateLimitInfo::from_headers(&h)
    };

    // 线上实测那发裸 429 的全部头：raw 非空，限流信息为零。
    let bare = hdr(&[
        ("anthropic-organization-id", "ca437ff6-03e7-44ac-849d-ba809e024327"),
        ("anthropic-workspace-id", "wrkspc_01FgbHGSko1X9SYxLsdgnV11"),
    ]);
    assert!(!bare.raw.is_empty(), "org/workspace id 确实会被收进 raw");
    assert!(bare.no_limit_headers(), "但它们不是限流头");
    assert!(hdr(&[]).no_limit_headers(), "什么头都没有当然算");

    // 任意一条限流信息在场就不算：逐项都要挡住，漏掉哪一项就会在正常额度 429 上多打日志。
    for one in [
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-reset", "1755480000"),
        ("retry-after", "30"),
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-5h-utilization", "0.2"),
        ("anthropic-ratelimit-unified-5h-reset", "1755480000"),
        // 没有专用列的窗口同样要认出来，理由同 [`rate_limit_scope`] 里的第 2 条教训。
        ("anthropic-ratelimit-unified-7d_oi-utilization", "1.02"),
    ] {
        assert!(!hdr(&[one]).no_limit_headers(), "{} 是限流头", one.0);
    }
}

/// 裸 429 的判据必须取**注入之前**那份快照（`handle` 里的 `upstream_limit`），
/// 不能在注入之后重解 `up.headers()`。
///
/// 走 transient 那档时 `handle` 会把算出来的退避写回 `retry-after` 再交回客户端；
/// 曾经它在那之后又拿同一个 `up` 重解了一遍限流头，于是自己塞的那条被当成上游给的读回来，
/// [`RateLimitInfo::no_limit_headers`] 恒为 false，「裸 429 把响应体打出来」那个分支
/// 永远不触发——而它正是为这一档写的，且那一档的失败原因**只**写在响应体里。
///
/// 这条盯住的是「重解是有损的、快照不受影响」这个事实；`handle` 究竟用了哪一份，
/// 单元测试够不着（要真实上游），由 `UPSTREAM_BASE_URL` 指向本地假上游的那套端到端跑法
/// 覆盖：日志里必须出现 `carried no rate-limit headers at all` 那一行。
#[test]
fn the_bare_429_verdict_must_come_from_the_pre_injection_snapshot() {
    let mut h = crate::proxy::HeaderMap::new();
    h.insert(
        crate::proxy::HeaderName::from_static("anthropic-organization-id"),
        HeaderValue::from_static("ca437ff6-03e7-44ac-849d-ba809e024327"),
    );
    h.insert(
        crate::proxy::HeaderName::from_static("anthropic-workspace-id"),
        HeaderValue::from_static("wrkspc_01FgbHGSko1X9SYxLsdgnV11"),
    );

    // 收到这发 429 的那一刻解一份留着——这就是 `handle` 里的 `upstream_limit`。
    // 限流信息为零 → rate_limit_scope 判 Transient，于是走注入 `retry-after` 那条路。
    let snapshot = crate::proxy::RateLimitInfo::from_headers(&h);
    assert!(snapshot.no_limit_headers());
    assert_eq!(
        crate::proxy::rate_limit_scope(&snapshot, Some("claude-opus-5")),
        crate::proxy::LimitScope::Transient("claude-opus-5".into())
    );

    // handle 在 transient 档把退避写回响应头，交给客户端退避。
    h.insert(header::RETRY_AFTER, HeaderValue::from(30u64));

    // 此刻重解是**有损**的：读回来的是我们自己塞的那条，判据被污染。曾经的 bug 就在这。
    let reparsed = crate::proxy::RateLimitInfo::from_headers(&h);
    assert_eq!(reparsed.retry_after, Some(30), "读回来的是我们自己塞的那条");
    assert!(!reparsed.no_limit_headers(), "重解之后就认不出这是发裸 429 了");

    // 快照不受注入影响——正因如此 `handle` 必须复用它，而不是回头重解 `up.headers()`。
    assert!(snapshot.no_limit_headers(), "快照仍然认得出这是发裸 429");
    assert_eq!(snapshot.retry_after, None, "快照里不该有我们自己塞的那条");
}

/// 落库展示用的全窗口快照：三张分开收集的表（status / utilization / reset）要按窗口名
/// 合并回一份，且**窗口名不写死**——`7d_oi` 那类没有专用列的必须在里面，那正是这一列
/// 存在的理由（见 [`store::QuotaWindow`]）。
#[test]
fn snapshot_windows_merge_every_reported_window() {
    let hdr = |pairs: &[(&str, &str)]| {
        let mut h = crate::proxy::HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                crate::proxy::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        crate::proxy::RateLimitInfo::from_headers(&h)
    };
    // 逐字取自第二次真实的 fable-5 429（同 rate_limit_scope_reads_every_window_not_just_5h_7d）。
    let info = hdr(&[
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-representative-claim", "seven_day_overage_included"),
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-5h-utilization", "0.2"),
        ("anthropic-ratelimit-unified-5h-reset", "9000"),
        ("anthropic-ratelimit-unified-7d-status", "allowed"),
        ("anthropic-ratelimit-unified-7d-utilization", "0.7"),
        ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
        ("anthropic-ratelimit-unified-7d_oi-utilization", "1.02"),
        // 只报了 status、没有 utilization/reset 的窗口也不能漏。
        ("anthropic-ratelimit-unified-overage-status", "rejected"),
        ("retry-after", "304802"),
    ]);
    let windows = info.windows();
    let by = |n: &str| windows.iter().find(|w| w.name == n).unwrap_or_else(|| panic!("缺 {n}"));

    assert_eq!(windows.len(), 4, "5h / 7d / 7d_oi / overage 四个都要在：{windows:?}");
    assert_eq!(by("5h").utilization, Some(0.2));
    assert_eq!(by("5h").reset, Some(9_000));
    assert_eq!(by("5h").status.as_deref(), Some("allowed"));
    // 没有专用列的那个——这一列的全部意义所在。
    assert_eq!(by("7d_oi").utilization, Some(1.02));
    assert_eq!(by("7d_oi").status.as_deref(), Some("rejected"));
    assert_eq!(by("7d_oi").reset, None, "上游没给 reset 就该是空，不许编");
    // 三张表里只出现在 status 那张的窗口同样要被带出来。
    assert_eq!(by("overage").status.as_deref(), Some("rejected"));
    assert_eq!(by("overage").utilization, None);
    // 顺序即上游响应头里首次出现的顺序，前端照着渲染就是原序。
    assert_eq!(
        windows.iter().map(|w| w.name.as_str()).collect::<Vec<_>>(),
        ["5h", "7d", "7d_oi", "overage"]
    );

    // 不带任何限流头的响应给出空列表——落库那侧靠它判断「要不要覆盖快照」。
    assert!(hdr(&[]).windows().is_empty());
    // 不带窗口名的 `…-unified-status` / `…-unified-reset` 不得造出一个名字为空的假窗口。
    let unified_only = hdr(&[
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-reset", "9000"),
    ]);
    assert!(unified_only.windows().is_empty(), "{:?}", unified_only.windows());
}

/// **fable 撞 429 绝不能停用整个账号。**
///
/// 这是一条真实事故的护栏：fable 走的是超额池（`7d_oi`），它满了的时候基础 5h/7d 还空着，
/// 同一账号的 sonnet/opus 照常 200。把这种 429 判成账号级，等于因为一个模型没容量就把整个
/// 号从调度池里摘掉——现在账号级还会**落库停用**，误伤代价比以前的进程内冷却大得多，
/// 所以这里直接钉住 [`crate::proxy::park_rate_limited`] 的落点，而不只是钉判定函数。
#[test]
fn model_level_429_never_disables_the_account() {
    let hdr = |kv: &[(&str, &str)]| {
        let mut h = crate::proxy::HeaderMap::new();
        for (k, v) in kv {
            h.insert(
                crate::proxy::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        crate::proxy::RateLimitInfo::from_headers(&h)
    };
    let store = store::CredentialStore::open_in_memory().unwrap();
    let cred = store.insert("a", None, "at", "rt", u64::MAX, None, None, 1).unwrap();
    let fable = Some("claude-fable-5");

    // 实测形态：只有超额池满，基础窗口都有余量。
    let oi_full = hdr(&[
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-representative-claim", "seven_day_overage_included"),
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-5h-utilization", "0.20"),
        ("anthropic-ratelimit-unified-7d-status", "allowed"),
        ("anthropic-ratelimit-unified-7d-utilization", "0.70"),
        ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
        ("anthropic-ratelimit-unified-7d_oi-utilization", "1.02"),
        ("retry-after", "304802"),
    ]);
    let scope = crate::proxy::rate_limit_scope(&oi_full, fable);
    assert_eq!(scope.model(), fable, "超额池满只该判模型级");
    crate::proxy::park_rate_limited(&store, &cred, &scope, oi_full.cooldown(false), false);

    let after = store.get(cred.id).unwrap().unwrap();
    assert!(!after.disabled, "fable 撞 429 不该停用整个账号");
    assert!(after.resume_at.is_none(), "更不该写恢复时刻——账号压根没被停");
    assert_eq!(after.ban_reason, None, "卡片上不该显示成这个号出了问题");
    // 但 fable 自己确实要让位，而 sonnet 照常可用。
    let pick =
        |m| store.select_for_device(store::Select { model: Some(m), ..Default::default() }).is_ok();
    assert!(!pick("claude-fable-5"), "fable 应被模型级冷却挡下");
    assert!(pick("claude-sonnet-5"), "同一个号的 sonnet 不该被牵连");

    // 对照组：基础窗口真耗尽才落库停用，并写下到点自动恢复的时刻。
    let base_gone = hdr(&[
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-utilization", "1.0"),
        ("retry-after", "3600"),
    ]);
    let scope = crate::proxy::rate_limit_scope(&base_gone, fable);
    assert!(scope.account_level(), "基础窗口耗尽才是账号级");
    crate::proxy::park_rate_limited(&store, &cred, &scope, base_gone.cooldown(true), false);

    let after = store.get(cred.id).unwrap().unwrap();
    assert!(after.disabled, "额度真耗尽才关调度开关");
    let resume_at = after.resume_at.expect("应写下自动恢复时刻");
    let wait = resume_at as i64 - crate::credentials::now_secs() as i64;
    assert!((3595..=3600).contains(&wait), "恢复时刻应取上游给的等待时间，实得 {wait}");
    assert!(after.ban_reason.unwrap().contains("1h"), "停用原因该写清楚还要等多久");
}

/// 瞬时限流吞到上限之后必须**真的**把这条路线挪出调度池，否则「最多吞几次」等于没有上限。
///
/// 两档行为差别只在最后那个参数上，故放在一个用例里对照：没吞够时只留展示标记、这个号照常
/// 参与选号；吞够了就走硬门禁，后续请求改走别的号。
#[test]
fn a_transient_rate_limit_only_leaves_the_pool_after_the_attempt_cap() {
    let store = store::CredentialStore::open_in_memory().unwrap();
    let cred = store.insert("a", None, "at", "rt", u64::MAX, None, None, 1).unwrap();
    let scope = crate::proxy::LimitScope::Transient("claude-opus-5".into());
    let wait = std::time::Duration::from_secs(30);
    let pick =
        |m| store.select_for_device(store::Select { model: Some(m), ..Default::default() }).is_ok();

    // 没 exhaust：短 gate——阻止同一个号被立刻再选中，避免反复 429。
    crate::proxy::park_rate_limited(&store, &cred, &scope, wait, false);
    assert!(!pick("claude-opus-5"), "短 gate 也应阻止选号");
    let models = store.rate_limited_models(cred.id);
    assert_eq!(models.len(), 1, "界面上要看得见");
    assert!(models[0].2, "瞬时限速现在也走 gate，gated 应为 true");

    // exhaust：退避已经涨到头还在撞，说明这条路线此刻真的走不通，让后续请求改走别的号。
    crate::proxy::park_rate_limited(&store, &cred, &scope, wait, true);
    assert!(!pick("claude-opus-5"), "到上限后这个模型必须被挡下");
    assert!(pick("claude-sonnet-5"), "但只挡这一个模型，别的模型不该被牵连");
    assert!(store.rate_limited_models(cred.id)[0].2, "此刻挂着门禁，gated 应为 true");

    let after = store.get(cred.id).unwrap().unwrap();
    assert!(!after.disabled, "这一档从头到尾都不该停用账号");
    assert_eq!(after.ban_reason, None, "更不该在卡片上显示成这个号出了问题");
}

/// 额度到阈值（默认 90%）就提前停调度，不必等真撞上一发 429；而超额池逼近上限时**不停**
/// ——它满了同一账号的别的模型照常 200，与 [`crate::proxy::rate_limit_scope`] 同一条口径。
#[test]
fn quota_threshold_parks_the_account_before_any_429() {
    let hdr = |kv: &[(&str, &str)]| {
        let mut h = crate::proxy::HeaderMap::new();
        for (k, v) in kv {
            h.insert(
                crate::proxy::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        crate::proxy::RateLimitInfo::from_headers(&h)
    };
    let now = crate::credentials::now_secs() as i64;
    let at = |secs: i64| (now + secs).to_string();
    let store = store::CredentialStore::open_in_memory().unwrap();
    let cred = store.insert("a", None, "at", "rt", u64::MAX, None, None, 1).unwrap();

    // 还没到阈值：一切照旧，200 就是 200。
    let plenty = hdr(&[
        ("anthropic-ratelimit-unified-status", "allowed"),
        ("anthropic-ratelimit-unified-5h-utilization", "0.60"),
        ("anthropic-ratelimit-unified-5h-reset", &at(2 * 3600)),
    ]);
    assert!(!crate::proxy::park_if_quota_nearly_exhausted(&store, &cred, &plenty));
    assert!(!store.get(cred.id).unwrap().unwrap().disabled, "60% 还远没到该停的时候");

    // 超额池 99%：那是「这条超额通道快走不通了」，不是账号额度耗尽，停号即误伤。
    let oi_hot = hdr(&[
        ("anthropic-ratelimit-unified-5h-utilization", "0.10"),
        ("anthropic-ratelimit-unified-7d_oi-utilization", "0.99"),
        ("anthropic-ratelimit-unified-7d_oi-reset", &at(50 * 3600)),
    ]);
    assert!(!crate::proxy::park_if_quota_nearly_exhausted(&store, &cred, &oi_hot));
    assert!(!store.get(cred.id).unwrap().unwrap().disabled, "超额池快满不该停整个号");

    // 5h 93%：还没被拒（status 仍是 allowed，上游也没回 429），照样提前退场。
    // 同一份头里 7d 也有 95%，但天级那档默认是关的——**只**按 5h 判、也只睡到 5h 的
    // 那个 reset（2 小时），不能被一个高位的 7d 拖成 50 小时：那 5 小时后这个号明明
    // 又能干活了。
    let hot = hdr(&[
        ("anthropic-ratelimit-unified-status", "allowed"),
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-5h-utilization", "0.93"),
        ("anthropic-ratelimit-unified-5h-reset", &at(2 * 3600)),
        ("anthropic-ratelimit-unified-7d-status", "allowed"),
        ("anthropic-ratelimit-unified-7d-utilization", "0.95"),
        ("anthropic-ratelimit-unified-7d-reset", &at(50 * 3600)),
    ]);
    assert!(crate::proxy::park_if_quota_nearly_exhausted(&store, &cred, &hot));
    let after = store.get(cred.id).unwrap().unwrap();
    assert!(after.disabled, "越过阈值就该把号挪出调度池");
    let wait = after.resume_at.expect("按阈值停的号必须能到点自恢复") as i64 - now;
    assert!((2 * 3600 - 5..=2 * 3600).contains(&wait), "应睡到 5h reset，实得 {wait}");
    let reason = after.ban_reason.expect("卡片上要说清为什么不干活");
    assert!(reason.contains("93.0%") && reason.contains("90%"), "原因文案：{reason}");

    // 幂等：同一批限流头被并发在途的请求各看一遍，不该反复写库。
    assert!(crate::proxy::park_if_quota_nearly_exhausted(&store, &cred, &hot));

    // 只有 7d 高位、5h 还空着：默认**不停**。这个号这 5 小时完全能干活，周用量偏高不是
    // 停它的理由——真把周额度用光了上游会自己回 429，账号级冷却那条路接手。
    let store = store::CredentialStore::open_in_memory().unwrap();
    let cred = store.insert("a", None, "at", "rt", u64::MAX, None, None, 1).unwrap();
    let weekly_hot = hdr(&[
        ("anthropic-ratelimit-unified-5h-utilization", "0.10"),
        ("anthropic-ratelimit-unified-5h-reset", &at(3600)),
        ("anthropic-ratelimit-unified-7d-utilization", "0.97"),
        ("anthropic-ratelimit-unified-7d-reset", &at(50 * 3600)),
    ]);
    assert!(!crate::proxy::park_if_quota_nearly_exhausted(&store, &cred, &weekly_hot));
    assert!(!store.get(cred.id).unwrap().unwrap().disabled, "7d 那档默认关，不该停号");

    // 单独把天级那档打开（95%）：同一份头就该停，且睡到 **7d** 的 reset——这一档的代价
    // 本来就是「停到下个周重置」，配它的人要的正是这个。
    store.set_setting(store::QUOTA_PAUSE_PCT_7D, "95").unwrap();
    assert!(crate::proxy::park_if_quota_nearly_exhausted(&store, &cred, &weekly_hot));
    let after = store.get(cred.id).unwrap().unwrap();
    let wait = after.resume_at.expect("同样要能到点自恢复") as i64 - now;
    assert!((50 * 3600 - 5..=50 * 3600).contains(&wait), "应睡到 7d reset，实得 {wait}");
    let reason = after.ban_reason.expect("原因要写清是哪个窗口、按哪个阈值");
    assert!(
        reason.contains("7d") && reason.contains("97.0%") && reason.contains("95%"),
        "{reason}"
    );

    // 两档互不干扰：5h 那档配成 0（关）时，7d 那档照样按自己的阈值停号。
    let only_7d = store::CredentialStore::open_in_memory().unwrap();
    let c = only_7d.insert("b", None, "at", "rt", u64::MAX, None, None, 1).unwrap();
    only_7d.set_setting(store::QUOTA_PAUSE_PCT, "0").unwrap();
    only_7d.set_setting(store::QUOTA_PAUSE_PCT_7D, "95").unwrap();
    assert!(crate::proxy::park_if_quota_nearly_exhausted(&only_7d, &c, &weekly_hot));
    assert!(only_7d.get(c.id).unwrap().unwrap().disabled, "5h 那档关着不影响 7d 那档");

    // 阈值配成 0 = 关掉本机制，退回「收到 429 才停」。
    let store = store::CredentialStore::open_in_memory().unwrap();
    let cred = store.insert("a", None, "at", "rt", u64::MAX, None, None, 1).unwrap();
    store.set_setting(store::QUOTA_PAUSE_PCT, "0").unwrap();
    assert!(!crate::proxy::park_if_quota_nearly_exhausted(&store, &cred, &hot));
    assert!(!store.get(cred.id).unwrap().unwrap().disabled);

    // 阈值可手调，两个方向都要成立。先调高：配 99 时上面那份 95% 的头不该再停号
    // （默认的 90 是会停的）。
    let warm = hdr(&[
        ("anthropic-ratelimit-unified-5h-utilization", "0.95"),
        ("anthropic-ratelimit-unified-5h-reset", &at(3600)),
    ]);
    store.set_setting(store::QUOTA_PAUSE_PCT, "99").unwrap();
    assert!(!crate::proxy::park_if_quota_nearly_exhausted(&store, &cred, &warm));
    assert!(!store.get(cred.id).unwrap().unwrap().disabled, "阈值调高后 95% 不该停");

    // 再调低：配 80 时同一份头就该停。
    store.set_setting(store::QUOTA_PAUSE_PCT, "80").unwrap();
    assert!(crate::proxy::park_if_quota_nearly_exhausted(&store, &cred, &warm));
    assert!(store.get(cred.id).unwrap().unwrap().disabled);
}

/// 逐账号阈值覆盖全局：账号自己配了的那档用账号的，`Some(0)` 是「这个号这一档不停」
/// 而不是「跟随」，`None` 才跟随；两档各自覆盖、互不串档。
#[test]
fn quota_threshold_is_overridable_per_credential() {
    let hdr = |kv: &[(&str, &str)]| {
        let mut h = crate::proxy::HeaderMap::new();
        for (k, v) in kv {
            h.insert(
                crate::proxy::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        crate::proxy::RateLimitInfo::from_headers(&h)
    };
    let now = crate::credentials::now_secs() as i64;
    let at = |secs: i64| (now + secs).to_string();
    let warm = hdr(&[
        ("anthropic-ratelimit-unified-5h-utilization", "0.95"),
        ("anthropic-ratelimit-unified-5h-reset", &at(3600)),
        ("anthropic-ratelimit-unified-7d-utilization", "0.97"),
        ("anthropic-ratelimit-unified-7d-reset", &at(50 * 3600)),
    ]);
    let store = store::CredentialStore::open_in_memory().unwrap();
    let fresh = |id: i64| store.get(id).unwrap().unwrap();

    // 全局 90：没配覆盖的号 95% 该停。
    let a = store.insert("a", None, "at", "rt-a", u64::MAX, None, None, 1).unwrap();
    assert!(crate::proxy::park_if_quota_nearly_exhausted(&store, &a, &warm));
    assert!(fresh(a.id).disabled, "跟随全局 90 的号 95% 该停");

    // 账号自己配 99：同一份头不停；配回 None 又跟随全局。
    let b = store.insert("b", None, "at", "rt-b", u64::MAX, None, None, 1).unwrap();
    assert!(store.set_quota_pause_pcts(b.id, Some(99), None).unwrap());
    let b = fresh(b.id);
    assert_eq!((b.quota_pause_pct, b.quota_pause_pct_7d), (Some(99), None));
    assert!(!crate::proxy::park_if_quota_nearly_exhausted(&store, &b, &warm));
    assert!(!fresh(b.id).disabled, "账号阈值 99 覆盖全局 90，95% 不该停");
    assert!(store.set_quota_pause_pcts(b.id, None, None).unwrap());
    let b = fresh(b.id);
    assert!(crate::proxy::park_if_quota_nearly_exhausted(&store, &b, &warm));
    assert!(fresh(b.id).disabled, "清掉覆盖就回到全局 90");

    // 账号配 0 = 这个号这一档不停，哪怕全局开着；7d 档没配、全局也关，整个不停。
    let c = store.insert("c", None, "at", "rt-c", u64::MAX, None, None, 1).unwrap();
    assert!(store.set_quota_pause_pcts(c.id, Some(0), None).unwrap());
    let c = fresh(c.id);
    assert!(!crate::proxy::park_if_quota_nearly_exhausted(&store, &c, &warm));
    assert!(!fresh(c.id).disabled, "账号 5h 档配 0 即不停，不是跟随全局");

    // 只给这个号开 7d 档（95）而全局 7d 关着：按 7d 停、睡到 7d 的 reset。
    let d = store.insert("d", None, "at", "rt-d", u64::MAX, None, None, 1).unwrap();
    assert!(store.set_quota_pause_pcts(d.id, Some(0), Some(95)).unwrap());
    let d = fresh(d.id);
    assert!(crate::proxy::park_if_quota_nearly_exhausted(&store, &d, &warm));
    let after = fresh(d.id);
    assert!(after.disabled);
    let wait = after.resume_at.expect("按阈值停的号要能到点自恢复") as i64 - now;
    assert!((50 * 3600 - 5..=50 * 3600).contains(&wait), "应睡到 7d reset，实得 {wait}");
    assert!(after.ban_reason.unwrap().contains("7d"));

    // 反过来：全局 5h 关着、账号自己开 80，95% 也停。
    store.set_setting(store::QUOTA_PAUSE_PCT, "0").unwrap();
    let e = store.insert("e", None, "at", "rt-e", u64::MAX, None, None, 1).unwrap();
    assert!(!crate::proxy::park_if_quota_nearly_exhausted(&store, &e, &warm));
    assert!(store.set_quota_pause_pcts(e.id, Some(80), None).unwrap());
    let e = fresh(e.id);
    assert!(crate::proxy::park_if_quota_nearly_exhausted(&store, &e, &warm));
    assert!(fresh(e.id).disabled, "全局关着不妨碍账号自己开");

    // 越界值夹到 0..=100；不存在的号返回 false。
    assert!(store.set_quota_pause_pcts(e.id, Some(250), Some(-3)).unwrap());
    let e = fresh(e.id);
    assert_eq!((e.quota_pause_pct, e.quota_pause_pct_7d), (Some(100), Some(0)));
    assert!(!store.set_quota_pause_pcts(9999, Some(50), None).unwrap());

    // 批量：整份覆盖所选的号（含把已有覆盖清回 None），没选的不动，返回改了几条。
    let n = store.set_quota_pause_pcts_many(&[a.id, e.id, 9999], None, Some(120)).unwrap();
    assert_eq!(n, 2);
    for id in [a.id, e.id] {
        let c = fresh(id);
        assert_eq!((c.quota_pause_pct, c.quota_pause_pct_7d), (None, Some(100)));
    }
    let d = fresh(d.id);
    assert_eq!((d.quota_pause_pct, d.quota_pause_pct_7d), (Some(0), Some(95)), "没选的不动");
    assert_eq!(store.set_quota_pause_pcts_many(&[], Some(1), None).unwrap(), 0);
}

/// 关掉「429 冷却/换号重试」总开关的人要的是完全不干预调度，那时阈值机制也必须闭嘴。
#[test]
fn quota_threshold_obeys_the_rate_limit_retry_switch() {
    let mut h = crate::proxy::HeaderMap::new();
    h.insert(
        crate::proxy::HeaderName::from_static("anthropic-ratelimit-unified-5h-utilization"),
        HeaderValue::from_static("1.0"),
    );
    let info = crate::proxy::RateLimitInfo::from_headers(&h);
    let store = store::CredentialStore::open_in_memory().unwrap();
    let cred = store.insert("a", None, "at", "rt", u64::MAX, None, None, 1).unwrap();
    store.set_setting(store::RATE_LIMIT_RETRY, "false").unwrap();
    assert!(!crate::proxy::park_if_quota_nearly_exhausted(&store, &cred, &info));
    assert!(!store.get(cred.id).unwrap().unwrap().disabled, "总开关关着就不该动调度");
}

/// 冷却睡到**上游返回的那个重置时刻**，不是写死的 5 小时/7 天：没有 `retry-after` 时，
/// 取被拒的那个基础窗口自己的 `*-reset`，而不是 `unified-reset`、也不是最早的那个。
#[test]
fn account_cooldown_sleeps_until_the_exhausted_window_reset() {
    let hdr = |kv: &[(&str, &str)]| {
        let mut h = crate::proxy::HeaderMap::new();
        for (k, v) in kv {
            h.insert(
                crate::proxy::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        crate::proxy::RateLimitInfo::from_headers(&h)
    };
    let now = crate::credentials::now_secs() as i64;
    let at = |secs: i64| (now + secs).to_string();

    // 5h 打满、7d 还有余量：该睡到 5h 自己的 reset（这里剩 2 小时，不是「5 小时」），
    // 而不是 unified-reset 说的 9 小时、也不是 7d 的 30 小时。
    //
    // `retry-after` 也故意给了，且比 5h reset 少一分钟——线上真实的对不上就长这样：
    // 它是相对秒数（上游向下取整、还要锚回本地时钟），reset 是绝对时刻，两者口径不同。
    // 账号级必须吃 reset，否则卡片会一边写「12:20 重置」一边写「12:19 恢复」。
    let five_h_gone = hdr(&[
        ("anthropic-ratelimit-unified-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-utilization", "1.0"),
        ("anthropic-ratelimit-unified-5h-reset", &at(2 * 3600)),
        ("anthropic-ratelimit-unified-7d-status", "allowed"),
        ("anthropic-ratelimit-unified-7d-utilization", "0.4"),
        ("anthropic-ratelimit-unified-7d-reset", &at(30 * 3600)),
        ("anthropic-ratelimit-unified-reset", &at(9 * 3600)),
        ("retry-after", &(2 * 3600 - 60).to_string()),
    ]);
    assert!(crate::proxy::rate_limit_scope(&five_h_gone, Some("claude-sonnet-5")).account_level());
    let secs = five_h_gone.cooldown(true).as_secs() as i64;
    assert!(
        (2 * 3600 - 5..=2 * 3600).contains(&secs),
        "应睡到 5h 窗口的 reset（而非 retry-after 的 {}），实得 {secs}",
        2 * 3600 - 60
    );
    // 模型级那档没有「哪个窗口满了」可言，仍旧只认 retry-after。
    assert_eq!(five_h_gone.cooldown(false).as_secs() as i64, 2 * 3600 - 60);

    // 两个基础窗口都满 → 取**最晚**的那个：5h 到点了 7d 照样拦着，早醒只是白撞一发。
    let both_gone = hdr(&[
        ("anthropic-ratelimit-unified-5h-status", "rejected"),
        ("anthropic-ratelimit-unified-5h-reset", &at(2 * 3600)),
        ("anthropic-ratelimit-unified-7d-status", "rejected"),
        ("anthropic-ratelimit-unified-7d-reset", &at(50 * 3600)),
    ]);
    let secs = both_gone.cooldown(true).as_secs() as i64;
    assert!((50 * 3600 - 5..=50 * 3600).contains(&secs), "应睡到较晚的 7d reset，实得 {secs}");

    // 满的只有超额池：那不是账号额度耗尽，它的 reset 不该被当成账号冷却
    // （判定本身也是模型级，这里只钉住 reset 口径不被超额窗口污染）。
    let oi_gone = hdr(&[
        ("anthropic-ratelimit-unified-5h-status", "allowed"),
        ("anthropic-ratelimit-unified-5h-reset", &at(3 * 3600)),
        ("anthropic-ratelimit-unified-7d_oi-status", "rejected"),
        ("anthropic-ratelimit-unified-7d_oi-reset", &at(60 * 3600)),
    ]);
    let secs = oi_gone.cooldown(true).as_secs() as i64;
    assert!(
        (3 * 3600 - 5..=3 * 3600).contains(&secs),
        "超额池的 reset 不该当账号冷却，实得 {secs}"
    );
}
