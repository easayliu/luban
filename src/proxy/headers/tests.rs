use crate::proxy::test_support::{PLAIN_BODY, all_on, names, sim_for};
use crate::proxy::{
    HeaderValue, build_forward_headers, config, header, merge_beta, store, uuid_v4,
};

/// 三个模型族的 `anthropic-beta`，逐字取自 `cap/raw` 的原始报文头
/// （claude-cli/2.1.220，每对都是同机、经 luban 与直连相隔几十秒）：
/// `(模型, 客户端自己发的, 官方订阅客户端发的)`。
///
/// haiku 那对是关键反例：它的客户端把 `claude-code-20250219` 排在**队尾**、`oauth` 在
/// 队首，与 opus/sonnet 正好相反，任何单一顺序表都同时对不上两边。
const BETA_PAIRS: &[(&str, &str, &str)] = &[
    (
        "opus-5 (00002/00006)",
        "claude-code-20250219,context-1m-2025-08-07,interleaved-thinking-2025-05-14,\
             redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
             mid-conversation-system-2026-04-07,effort-2025-11-24,fallback-credit-2026-06-01",
        "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,\
             interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
             advanced-tool-use-2025-11-20,effort-2025-11-24,fallback-credit-2026-06-01,\
             extended-cache-ttl-2025-04-11",
    ),
    (
        "sonnet-5 (00012/00009)",
        "claude-code-20250219,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
             effort-2025-11-24",
        "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
             redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
             mid-conversation-system-2026-04-07,advanced-tool-use-2025-11-20,\
             effort-2025-11-24,extended-cache-ttl-2025-04-11",
    ),
    (
        "haiku-4.5 (00026/00031)",
        "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,claude-code-20250219",
        "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,claude-code-20250219,\
             advanced-tool-use-2025-11-20,extended-cache-ttl-2025-04-11",
    ),
];

/// 2.1.258：API-key 端的**原始请求头**（`cap/2.1.258-api` 00025 haiku / 00017 sonnet /
/// 00006 opus / 00013 fable，CC 经 luban 的入站原文）喂进去，必须逐字节还原订阅端直连
/// 抓包的串（`cap/2.1.258` 00031 / 00026 / 00012 / 00013）。
///
/// API-key 端缺的是 oauth / advanced-tool-use / server-side-fallback / extended-cache-ttl /
/// cache-diagnosis；`fallback-credit` 与 `afk-mode` 四族都自带；fable 缺
/// thinking-display-updates 却多一个 redact-thinking。API-key 端 fable 样本带 `context-1m`
/// （[1m] 会话）而订阅端样本不带，期望值按 opus 的位置把它放在 `oauth` 之后。
#[test]
fn merged_beta_matches_2_1_258_official_order() {
    const CASES: &[(&str, &str, &str)] = &[
        (
            "claude-haiku-4-5-20251001",
            "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,claude-code-20250219,advisor-tool-2026-03-01,\
                 fallback-credit-2026-06-01,afk-mode-2026-01-31",
            "oauth-2025-04-20,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,claude-code-20250219,advisor-tool-2026-03-01,\
                 advanced-tool-use-2025-11-20,server-side-fallback-2026-07-01,\
                 fallback-credit-2026-06-01,afk-mode-2026-01-31,extended-cache-ttl-2025-04-11,\
                 cache-diagnosis-2026-04-07",
        ),
        (
            "claude-sonnet-5",
            "claude-code-20250219,interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
                 advisor-tool-2026-03-01,effort-2025-11-24,fallback-credit-2026-06-01,\
                 afk-mode-2026-01-31",
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
                 redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
                 context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
                 mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,\
                 advanced-tool-use-2025-11-20,effort-2025-11-24,\
                 server-side-fallback-2026-07-01,fallback-credit-2026-06-01,\
                 afk-mode-2026-01-31,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07",
        ),
        (
            "claude-opus-5",
            "claude-code-20250219,context-1m-2025-08-07,interleaved-thinking-2025-05-14,\
                 redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
                 context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
                 mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,effort-2025-11-24,\
                 fallback-credit-2026-06-01,afk-mode-2026-01-31",
            "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,\
                 interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
                 advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,effort-2025-11-24,\
                 server-side-fallback-2026-07-01,fallback-credit-2026-06-01,\
                 afk-mode-2026-01-31,extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07",
        ),
        (
            "claude-fable-5-1",
            "claude-code-20250219,context-1m-2025-08-07,interleaved-thinking-2025-05-14,\
                 redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
                 context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
                 mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,effort-2025-11-24,\
                 fallback-credit-2026-06-01,afk-mode-2026-01-31",
            "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,\
                 interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
                 context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
                 mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,\
                 advanced-tool-use-2025-11-20,effort-2025-11-24,\
                 server-side-fallback-2026-07-01,fallback-credit-2026-06-01,\
                 thinking-display-updates-2026-08-18,afk-mode-2026-01-31,\
                 extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07",
        ),
    ];
    // 来访自报 2.1.258，故参照的是 [`config::CC_PROFILES_2_1_258`] 那一版的官方形态。
    let v258 = Some((2u64, 1, 258));
    for (model, api_key_client, official) in CASES {
        assert_eq!(
            &merge_beta(Some(api_key_client), Some(model), v258),
            official,
            "{model} 的 beta 串没对齐"
        );
        // 订阅端客户端经 luban：本来就是官方串，一个字都不该动（幂等）。
        assert_eq!(&merge_beta(Some(official), Some(model), v258), official, "{model} 不幂等");
    }
    // 不知道模型时，族相关的两条不做：fable 的 API-key 串只补通用几项。
    let fable_api = CASES[3].1;
    let no_model = merge_beta(Some(fable_api), None, v258);
    assert!(no_model.contains("redact-thinking-2026-02-12"), "不知道族就不删: {no_model}");
    assert!(!no_model.contains("thinking-display-updates"), "不知道族就不补: {no_model}");
}

/// 2.1.270 的**完整订阅端串**经 [`merge_beta_for`] 必须一个字不动。
///
/// `cap/2.1.270/00017` / `00025`（sonnet-5 直连，同一会话首轮与续轮，beta 逐字相同）：
/// 这一版 sonnet 主线程**不发** `server-side-fallback` 与 `fallback-credit`，队尾多了
/// `message-threads`。v0.3.114 之前 [`config::cc_profile_at`] 把所有 ≥2.1.260 的来访都套
/// 2.1.260 那张表，那张表的 sonnet 行（外推的）两项都有，于是一条完整的官方请求会被塞回
/// `server-side-fallback-2026-06-01,fallback-credit-2026-06-01`——一个官方 2.1.270 不产生的串。
///
/// 同一批抓包里的标题生成 haiku（`00024`）与额度探测（`00005`）一并钉住：前者走非主线程
/// 豁免、后者没有 `claude-code`，本来就只补 `oauth`，这里防回归。
///
/// 其余三族**没有 2.1.270 样本、不外推**：仍按 2.1.260 表处理。opus 的完整 2.1.260 串在
/// 自报 2.1.270 时同样不该动——那张表的 opus 行本来就不发 `server-side-fallback`。
#[test]
fn merged_beta_is_idempotent_on_2_1_270_sonnet() {
    const SONNET_00017: &str = "claude-code-20250219,oauth-2025-04-20,\
             interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
             mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,\
             advanced-tool-use-2025-11-20,effort-2025-11-24,\
             thinking-display-updates-2026-08-18,afk-mode-2026-01-31,\
             extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,message-threads-2026-08-12";
    const TITLE_00024: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,\
             redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
             advisor-tool-2026-03-01,structured-outputs-2025-12-15,cache-diagnosis-2026-04-07,\
             message-threads-2026-08-12";
    const QUOTA_00005: &str = "oauth-2025-04-20,interleaved-thinking-2025-05-14,\
             redact-thinking-2026-02-12,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05";
    let v270 = Some((2u64, 1, 270));
    let haiku = Some("claude-haiku-4-5-20251001");
    assert_eq!(merge_beta(Some(SONNET_00017), Some("claude-sonnet-5"), v270), SONNET_00017);
    assert_eq!(merge_beta(Some(TITLE_00024), haiku, v270), TITLE_00024);
    assert_eq!(merge_beta(Some(QUOTA_00005), haiku, v270), QUOTA_00005);
    // 2.1.270 与 2.1.277 之间没抓包的小版本按最近一份已证形态（2.1.270 表）处理，不退回
    // 2.1.260 表；2.1.277 起另有自己的表（`merged_beta_is_idempotent_on_2_1_277_main_threads`）。
    assert_eq!(
        merge_beta(Some(SONNET_00017), Some("claude-sonnet-5"), Some((2, 1, 276))),
        SONNET_00017
    );
    // 2.1.270 sonnet 的 profile 行就是这条串去掉 `oauth` 与 `afk-mode`，模拟路径的拼法
    // （[`simulated_beta`]）要能把 `oauth` 放回原位。
    let seed = config::cc_profile_at(config::CcProfileKind::MainSonnet, v270);
    assert_eq!(seed.version, "2.1.270");
    assert_eq!(
        crate::proxy::simulated_beta(seed.beta, None),
        SONNET_00017.replace(",afk-mode-2026-01-31", "")
    );
    // 其余三族不外推：自报 2.1.270 的 opus 仍拿 2.1.260 的行，完整的 2.1.260 opus 串不动。
    let opus_260 = config::cc_profile_at(config::CcProfileKind::MainOpus, Some((2, 1, 260)));
    assert_eq!(opus_260.version, "2.1.260");
    assert_eq!(config::cc_profile_at(config::CcProfileKind::MainOpus, v270).beta, opus_260.beta);
    let opus_official = crate::proxy::simulated_beta(opus_260.beta, None);
    assert_eq!(merge_beta(Some(&opus_official), Some("claude-opus-5"), v270), opus_official);
}

/// 2.1.270 sonnet 的**API-key 端没有抓包**，这里只钉落位规则，不宣称差分：把 2.1.258 那套
/// 「API-key 端缺 oauth / advanced-tool-use / extended-cache-ttl / cache-diagnosis」套到
/// `00017` 上（`server-side-fallback` 这一版本来就没有），补回去要落在官方位置——尤其
/// `cache-diagnosis` 不再是队尾，得插在 `message-threads` 之前。真样本到了若差分不同，
/// 改的是这条测试的输入，不是落位规则。
#[test]
fn merged_beta_places_cache_diagnosis_before_message_threads() {
    const OFFICIAL: &str = "claude-code-20250219,oauth-2025-04-20,\
             interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
             mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,\
             advanced-tool-use-2025-11-20,effort-2025-11-24,\
             thinking-display-updates-2026-08-18,afk-mode-2026-01-31,\
             extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,message-threads-2026-08-12";
    let stripped: Vec<&str> = OFFICIAL
        .split(',')
        .filter(|p| {
            ![
                config::OAUTH_BETA_HEADER,
                config::CC_BETA_ADVANCED_TOOL_USE,
                config::CC_BETA_EXTENDED_CACHE_TTL,
                config::CC_BETA_CACHE_DIAGNOSIS,
            ]
            .contains(p)
        })
        .collect();
    assert_eq!(
        merge_beta(Some(&stripped.join(",")), Some("claude-sonnet-5"), Some((2, 1, 270))),
        OFFICIAL
    );
}

/// 2.1.270 sonnet 官方串**只缺** `thinking-display-updates` 时，补回去要落在 `effort` 之后、
/// `afk-mode` 之前（`cap/2.1.270/00017`）。这一版没有 `fallback-credit`，此前只认它做锚点，
/// 于是这一项会被追加到队尾、排在 `message-threads` 后面。
///
/// 「只缺这一项」现实里对应 API-key 端的 2.1.270 sonnet（2.1.258 时 API-key 端的 fable 就是
/// 缺它），没有抓包，故只钉落位，不宣称差分。
#[test]
fn merged_beta_places_thinking_display_after_effort_without_fallback_credit() {
    const OFFICIAL: &str = "claude-code-20250219,oauth-2025-04-20,\
             interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
             context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
             mid-conversation-system-2026-04-07,advisor-tool-2026-03-01,\
             advanced-tool-use-2025-11-20,effort-2025-11-24,\
             thinking-display-updates-2026-08-18,afk-mode-2026-01-31,\
             extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,message-threads-2026-08-12";
    let without = OFFICIAL.replace(",thinking-display-updates-2026-08-18", "");
    assert_eq!(merge_beta(Some(&without), Some("claude-sonnet-5"), Some((2, 1, 270))), OFFICIAL);
    // 连 `afk-mode` 也没带（它是动态项）：仍紧跟 `effort`。
    let without_afk = without.replace(",afk-mode-2026-01-31", "");
    assert_eq!(
        merge_beta(Some(&without_afk), Some("claude-sonnet-5"), Some((2, 1, 270))),
        OFFICIAL.replace(",afk-mode-2026-01-31", "")
    );
}

/// 补齐 + 落位后应与官方客户端的 beta 串**逐字节一致**，三个模型族都要过。
#[test]
fn merged_beta_matches_official_order() {
    for (model, client, official) in BETA_PAIRS {
        let v = HeaderValue::from_str(client).unwrap();
        assert_eq!(
            &merge_beta(Some(v.to_str().unwrap()), None, Some((2, 1, 220))),
            official,
            "{model} 的 beta 串没对齐"
        );
    }
}

/// 客户端自有的那串**一字不动**：这是三对抓包里唯一稳定的不变量，重排它就等于自造判据。
#[test]
fn merged_beta_preserves_client_order() {
    for (model, client, _) in BETA_PAIRS {
        let v = HeaderValue::from_str(client).unwrap();
        let out = merge_beta(Some(v.to_str().unwrap()), None, Some((2, 1, 220)));
        let kept: Vec<&str> =
            out.split(',').filter(|b| client.split(',').any(|c| c.trim() == *b)).collect();
        let sent: Vec<&str> = client.split(',').map(str::trim).collect();
        assert_eq!(kept, sent, "{model} 的客户端自有串被重排了: {out}");
    }
}

/// 未知 beta 不被丢弃，也不被挪位——它在客户端串里什么位置就还在什么位置。
#[test]
fn merged_beta_keeps_unknown_betas_in_place() {
    let (_, client, official) = BETA_PAIRS[1];
    let v = HeaderValue::from_str(&format!("{client},some-future-beta-2027-01-01")).unwrap();
    let out = merge_beta(Some(v.to_str().unwrap()), None, Some((2, 1, 220)));
    // 客户端把它放在自有串末尾，官方串里它就该在 effort 之后、extended-cache-ttl 之前。
    assert_eq!(
        out,
        official.replace(
            ",extended-cache-ttl-2025-04-11",
            ",some-future-beta-2027-01-01,extended-cache-ttl-2025-04-11"
        )
    );
}

/// 无来访 beta 是退化情形（真实客户端必带），仍要给出确定输出：四个注入项按落位规则排。
#[test]
fn merged_beta_from_empty_is_deterministic() {
    assert_eq!(
        merge_beta(None, None, None),
        "oauth-2025-04-20,advanced-tool-use-2025-11-20,prompt-caching-scope-2026-01-05,\
             extended-cache-ttl-2025-04-11"
    );
}

/// 来访客户端的头（API-key 模式的 CC，取自抓包 041）。构造成 `HeaderMap` 时保持插入序，
/// 与 axum 从线上解析出来的顺序一致。
fn incoming_headers() -> crate::proxy::HeaderMap {
    let mut h = crate::proxy::HeaderMap::new();
    for (k, v) in [
        ("accept", "application/json"),
        ("accept-encoding", "gzip, deflate, br, zstd"),
        ("authorization", "Bearer luban-CLIENT-KEY"),
        ("connection", "keep-alive"),
        ("content-type", "application/json"),
        ("proxy-connection", "Keep-Alive"),
        ("x-claude-code-session-id", "6eb83bfc-fdf8-4c43-ba4a-6ff95c60a0de"),
        ("x-stainless-arch", "arm64"),
        ("x-stainless-os", "MacOS"),
        ("anthropic-beta", "claude-code-20250219,effort-2025-11-24"),
        ("anthropic-dangerous-direct-browser-access", "true"),
        ("anthropic-version", "2023-06-01"),
        ("x-app", "cli"),
    ] {
        h.insert(crate::proxy::HeaderName::from_static(k), HeaderValue::from_static(v));
    }
    h
}

/// 需要 luban 改写取值的头必须**留在来访客户端给它们的位置上**。
///
/// 剥离后再 `insert` 会把它们追加到队尾，得到官方客户端不会产生的头序——和 `merge_beta`
/// 处理的是同一类问题（那个管值内顺序，这个管头之间的顺序）。
#[test]
fn forward_headers_keep_client_order() {
    let out = build_forward_headers(&incoming_headers(), "sk-ant-oat01-REAL", all_on(), None, None);

    assert_eq!(
        names(&out),
        vec![
            "accept",
            "accept-encoding",
            "authorization", // 原位，值被换成 OAuth token
            "connection",    // keep-alive 保留转发
            "content-type",
            // proxy-connection 被剥离
            "x-claude-code-session-id",
            "x-stainless-arch",
            "x-stainless-os",
            "anthropic-beta", // 原位，值被合并重排
            "anthropic-dangerous-direct-browser-access",
            "anthropic-version", // 原位
            "x-app",
            "x-client-request-id", // 客户端没带，无原位可循，追加末尾
        ],
        "转发头序被打乱"
    );

    // 值确实被覆盖了，不是原样透传。
    assert_eq!(out["authorization"], "Bearer sk-ant-oat01-REAL");
    assert!(
        out["anthropic-beta"].to_str().unwrap().contains(config::OAUTH_BETA_HEADER),
        "anthropic-beta 未合并 oauth"
    );
}

/// 形态开关全关 = 只注入鉴权，其余头逐项原样：beta 不重排、不塞 oauth，
/// 也不补任何客户端没带的头。实测上游只强制 `Authorization`，故这条路径必须真能走通。
#[test]
fn all_flags_off_only_injects_auth() {
    let flags = store::ForwardFlags {
        spoof_identity: false,
        spoof_device_id: false,
        normalize_device_fp: false,
        billing_cch: false,
        cch_real_recompute: false,
        cch_sim_compute: false,
        fill_client_headers: false,
        merge_beta: false,
        system_shape: false,
        orig_header_case: false,
        thinking_signature_retry: false,
        thinking_modified_retry: false,
        redacted_thinking_retry: false,
        simulate_cc: false,
        simulate_full_system: false,
        fill_absent_tools: false,
        sim_trim_tools: false,
        sim_message_threads: false,
        fill_metadata: false,
        rate_limit_retry: false,
        cache_scope_global: false,
        cache_ttl_1h: false,
        eager_tool_streaming: false,
        nonstream_as_sse: false,
        strip_extra_fields: false,
        tool_name_mimic: false,
        inject_thinking: false,
        flatten_tool_schemas: true,
        strip_empty_text: true,
        hoist_system_role: false,
        reject_openai_shape: false,
        reject_session_conflict: false,
        reject_probes: false,
        reject_probes_strict: false,
        reject_refusals: false,
        reject_empty_replies: false,
        reject_learned_shapes: false,
        api_telemetry: false,
        keepalive_telemetry: false,
        fable_refusal_fallback: false,
        opus_refusal_fallback: false,
    };
    let out = build_forward_headers(&incoming_headers(), "sk-ant-oat01-REAL", flags, None, None);

    // 头序与来访一致，且末尾不再追加 x-client-request-id。
    assert_eq!(
        names(&out),
        vec![
            "accept",
            "accept-encoding",
            "authorization",
            "connection",
            "content-type",
            "x-claude-code-session-id",
            "x-stainless-arch",
            "x-stainless-os",
            "anthropic-beta",
            "anthropic-dangerous-direct-browser-access",
            "anthropic-version",
            "x-app",
        ],
        "全关时不应补头"
    );
    // 唯一必需的改动仍然生效。
    assert_eq!(out["authorization"], "Bearer sk-ant-oat01-REAL");
    // beta 原样转发：既不重排也不塞 oauth。
    assert_eq!(out["anthropic-beta"], "claude-code-20250219,effort-2025-11-24");
    assert!(
        !out["anthropic-beta"].to_str().unwrap().contains(config::OAUTH_BETA_HEADER),
        "merge_beta 关闭后不应塞 oauth beta"
    );
}

/// 客户端什么都没带时，`fill_client_headers` 决定补不补——关掉就真的一个都不补，
/// 只留鉴权（`accept-encoding` 由上游 client 的 default_headers 兜底，不在这一层）。
#[test]
fn fill_off_adds_nothing_for_bare_client() {
    let mut bare = crate::proxy::HeaderMap::new();
    bare.insert(
        crate::proxy::HeaderName::from_static("content-type"),
        HeaderValue::from_static("application/json"),
    );

    let on = build_forward_headers(&bare, "tok", all_on(), None, None);
    assert_eq!(
        names(&on),
        vec![
            "content-type",
            "anthropic-version",
            "anthropic-beta",
            "accept-encoding",
            "x-client-request-id",
            "authorization"
        ],
        "开启时应补齐这四个头"
    );

    let flags = store::ForwardFlags { fill_client_headers: false, ..all_on() };
    let off = build_forward_headers(&bare, "tok", flags, None, None);
    assert_eq!(
        names(&off),
        vec!["content-type", "anthropic-beta", "authorization"],
        "关闭后只该有客户端原有的头 + beta + 鉴权"
    );
    assert!(!off.contains_key("x-client-request-id"));
    assert!(!off.contains_key(header::ACCEPT_ENCODING));
    assert!(!off.contains_key("anthropic-version"));
}

/// 覆盖失败时必须把 `authorization` 摘掉，不能把来访者的接入 key 漏给上游。
/// （这条只有在「照常转发再覆盖」的写法下才存在，剥离式写法天然没有这个洞。）
#[test]
fn never_leaks_client_key_upstream() {
    // token 里塞进换行——`HeaderValue::from_str` 会拒绝，走到移除分支。
    let out = build_forward_headers(&incoming_headers(), "bad\ntoken", all_on(), None, None);
    assert!(!out.contains_key("authorization"), "构造失败时应移除该头: {out:?}");
    // 任何路径下都不得把接入 key 转发出去。
    for (_, v) in out.iter() {
        assert!(
            !v.to_str().unwrap_or("").contains("luban-CLIENT-KEY"),
            "接入 key 泄漏到上游: {out:?}"
        );
    }
    assert!(!out.contains_key("x-api-key"), "x-api-key 不应转发");
}

/// 起个裸 TCP「上游」，用 [`crate::clients::upstream_client`] 那份**真配置**打一发，
/// 返回请求的原始字节（不做大小写归一化——这里正是要看拼写的）。
async fn capture_wire(orig_case: bool) -> String {
    use std::io::{BufRead, BufReader, Write};

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut r = BufReader::new(&stream);
        let mut raw = String::new();
        loop {
            let mut line = String::new();
            if r.read_line(&mut line).unwrap() == 0 {
                break;
            }
            let end = line == "\r\n";
            raw.push_str(&line);
            if end {
                break;
            }
        }
        (&stream).write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").unwrap();
        raw
    });

    let req = crate::clients::upstream_client(None)
        .unwrap()
        .post(format!("http://{addr}/v1/messages?beta=true"))
        .headers(build_forward_headers(
            &incoming_headers(),
            "sk-ant-oat01-REAL",
            all_on(),
            None,
            None,
        ))
        .body(r#"{"model":"claude-sonnet-5"}"#);
    let req = if orig_case { req.orig_headers(crate::proxy::orig_header_case()) } else { req };
    let _ = req.send().await;

    server.join().unwrap()
}

/// 取出线上的头名，保留原始拼写与顺序。
fn wire_names(raw: &str) -> Vec<&str> {
    raw.lines()
        .skip(1) // 请求行
        .filter(|l| !l.is_empty())
        .map(|l| l.split(':').next().unwrap())
        .collect()
}

/// 线上字节的看门狗：头名的**拼写与顺序**都要与官方客户端一致
/// （基准是 `cap/raw/00006` 的原始报文头，claude-cli/2.1.220 直连）。
///
/// 这条同时钉住三件事，任一退化都会失败：
/// 1. 分裂大小写——标准头首字母大写、`anthropic-*`/`x-app`/`x-client-request-id` 全小写、
///    `X-Stainless-OS` 的 `OS` 全大写（机械 title-case 会写成 `X-Stainless-Os`）。
/// 2. `User-Agent` 落在 `Content-Type` 与 `X-Claude-Code-Session-Id` 之间，而
///    `Connection`/`Host`/`Accept-Encoding`/`Content-Length` 是队尾四个——这是官方线序，
///    不是字母序（曾照 `cap/040.json` 的字母序排过，那是抓包工具重排的产物）。
/// 3. 显式的 `Connection: keep-alive` 确实发出（客户端库默认认为 HTTP/1.1 隐含、不发）。
#[tokio::test]
async fn wire_bytes_match_official_header_form() {
    let raw = capture_wire(true).await;
    assert_eq!(
        wire_names(&raw),
        &[
            "Accept",
            "Authorization",
            "Content-Type",
            "User-Agent", // 来访没带，由 upstream_client 兜底，靠 CC_HEADER_ORDER 归位
            "X-Claude-Code-Session-Id",
            "X-Stainless-Arch",
            "X-Stainless-OS",
            "anthropic-beta",
            "anthropic-dangerous-direct-browser-access",
            "anthropic-version",
            "x-app",
            "x-client-request-id",
            // 队尾四个：客户端库自己追加的那些，官方也在这个位置。
            "Connection",
            "Host",
            "Accept-Encoding",
            "Content-Length",
        ],
        "线上头名的拼写或顺序与官方形态不符:\n{raw}"
    );
    assert!(raw.contains("Connection: keep-alive"), "显式的 Connection 头被吞掉了:\n{raw}");
}

/// `orig_header_case` 关掉后的形态。**注意它并不等于换 wreq 之前那份**：
///
/// - 头名退回全小写（这点与 reqwest 时代一致）；
/// - 但 `default_headers` 里的 `user-agent`/`accept-encoding` 被**前置到队首**，
///   来访客户端的头序因此被打散（reqwest 是把它们并进原位/队尾的）；
/// - `host`/`content-length` 仍在队尾。
///
/// 也就是说这条 off 路径比开着**更不像**官方客户端，它的用途只是出问题时能二分
/// （「是 OrigHeaderMap 引入的问题，还是别处」），不是一个可用的形态选择。
#[tokio::test]
async fn wire_bytes_fall_back_to_lowercase_when_off() {
    let raw = capture_wire(false).await;
    let wire = wire_names(&raw);

    let tail = wire.len() - 2;
    assert_eq!(
        &wire[..tail],
        &[
            // default_headers 被前置，不在来访客户端给它们的位置上
            "user-agent",
            "accept-encoding",
            // 以下按来访头序
            "accept",
            "authorization",
            "connection",
            "content-type",
            "x-claude-code-session-id",
            "x-stainless-arch",
            "x-stainless-os",
            "anthropic-beta",
            "anthropic-dangerous-direct-browser-access",
            "anthropic-version",
            "x-app",
            "x-client-request-id",
        ],
        "关掉开关后的形态与实测不符:\n{raw}"
    );
    // 位置不可控，只断言这两个确实在末尾。
    let mut appended = wire[tail..].to_vec();
    appended.sort_unstable();
    assert_eq!(appended, ["content-length", "host"], "\n{raw}");
}

/// 2.1.285 起模拟的 messages 请求带 `anthropic-dispatch-id: v2d`，额度探测不带
/// （`cap/2.1.285/00030` / `00017`）；线上位置在 `anthropic-dangerous-direct-browser-access`
/// 与 `anthropic-version` 之间。SDK 版本头 2.1.291 起是 0.128.0。
#[test]
fn simulated_messages_carry_the_dispatch_id() {
    let sim = sim_for(PLAIN_BODY);
    let out =
        build_forward_headers(&crate::proxy::HeaderMap::new(), "tok", all_on(), Some(&sim), None);
    assert_eq!(out["anthropic-dispatch-id"], config::CC_DISPATCH_ID);
    assert_eq!(out["x-stainless-package-version"], "0.128.0");
    // 新输入那条不带额度宽限头（[`config::CC_USAGE_LIMIT_HEADER`]）。
    assert!(out.get("anthropic-usage-limit").is_none());
    // 与 billing header 里的 `cc_prompt_id` 同值。
    let pid = sim.link.prompt_id.as_deref().expect("主线程有 prompt id");
    assert_eq!(out["x-claude-code-prompt-id"], pid);
    let probe = crate::proxy::probe_simulation(
        &crate::proxy::test_support::test_cred(),
        "claude-haiku-4-5-20251001",
    );
    assert_eq!(probe.profile.kind, config::CcProfileKind::QuotaProbe);
    let out =
        build_forward_headers(&crate::proxy::HeaderMap::new(), "tok", all_on(), Some(&probe), None);
    assert!(out.get("anthropic-dispatch-id").is_none(), "额度探测官方不带");
    assert!(out.get("x-claude-code-prompt-id").is_none(), "没有 cc_prompt_id 就不带");
    let order = config::CC_HEADER_ORDER;
    let at = |h: &str| order.iter().position(|x| *x == h).unwrap();
    assert_eq!(at("anthropic-dispatch-id"), at("anthropic-dangerous-direct-browser-access") + 1);
    // 2.1.291 的额度宽限头夹在 dispatch-id 与 anthropic-version 之间（`cap/auto-2.1.291-20261006-full/00036`）。
    assert_eq!(at("anthropic-usage-limit"), at("anthropic-dispatch-id") + 1);
    assert_eq!(at("anthropic-version"), at("anthropic-usage-limit") + 1);
    assert_eq!(at("x-claude-code-prompt-id"), at("x-claude-code-prev-tool-durations") + 1);
    assert_eq!(at("x-claude-code-request-class"), at("x-claude-code-prompt-id") + 1);
}

/// 补齐的 x-client-request-id 是标准 uuid v4 形态。
#[test]
fn generates_uuid_v4() {
    let u = uuid_v4();
    let parts: Vec<&str> = u.split('-').collect();
    assert_eq!(parts.iter().map(|p| p.len()).collect::<Vec<_>>(), vec![8, 4, 4, 4, 12]);
    assert!(u.chars().all(|c| c.is_ascii_hexdigit() || c == '-'), "非法字符: {u}");
    assert_eq!(parts[2].as_bytes()[0], b'4', "version 位应为 4: {u}");
    assert!(matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'), "variant 位不对: {u}");
    assert_ne!(u, uuid_v4(), "每请求应不同");
}

/// 头侧 `ensure_fallback_beta`：补 06-01 那条在 `effort` 之后；已有（不论日期）不动；
/// `build_forward_headers_for` 传 `fallback_beta` 时 opus 的官方串（本不发）也带上。
#[test]
fn fallback_beta_is_added_once_after_effort() {
    let out = crate::proxy::ensure_fallback_beta(
        "claude-code-20250219,effort-2025-11-24,fallback-credit-2026-06-01".into(),
    );
    assert_eq!(
        out,
        "claude-code-20250219,effort-2025-11-24,server-side-fallback-2026-06-01,fallback-credit-2026-06-01"
    );
    let had = "a,server-side-fallback-2026-07-01,b".to_string();
    assert_eq!(crate::proxy::ensure_fallback_beta(had.clone()), had, "同名不同日期算已有");
    assert_eq!(
        crate::proxy::ensure_fallback_beta(String::new()),
        "server-side-fallback-2026-06-01"
    );
    assert_eq!(
        crate::proxy::ensure_fallback_beta("x,y".into()),
        "x,y,server-side-fallback-2026-06-01"
    );

    let sim = sim_for(PLAIN_BODY);
    let with = crate::proxy::build_forward_headers_for(
        &crate::proxy::HeaderMap::new(),
        "tok",
        all_on(),
        Some(&sim),
        None,
        Some("claude-opus-5"),
        true,
        crate::proxy::BetaCtx::MAIN,
    );
    let beta = with.get("anthropic-beta").unwrap().to_str().unwrap().to_string();
    assert_eq!(beta.matches("server-side-fallback-").count(), 1, "{beta}");
    let idx = |b: &str| beta.split(',').position(|p| p.starts_with(b)).unwrap();
    assert!(idx("effort-") < idx("server-side-fallback-"), "{beta}");
    // 2.1.291 默认权限模式的 opus 串里 `effort` 下一格是 `thinking-binding-controls`
    // （`dangerous-tool-use` 只在 auto 模式下发）。
    assert!(idx("server-side-fallback-") < idx("thinking-binding-controls-"), "{beta}");
    let without = crate::proxy::build_forward_headers_for(
        &crate::proxy::HeaderMap::new(),
        "tok",
        all_on(),
        Some(&sim),
        None,
        Some("claude-opus-5"),
        false,
        crate::proxy::BetaCtx::MAIN,
    );
    assert!(
        !without.get("anthropic-beta").unwrap().to_str().unwrap().contains("server-side-fallback")
    );
}

/// 按 2.1.285 表产出的 `anthropic-beta` 必须**逐字节**等于官方那串——这是
/// [`config::CC_PROFILES_2_1_285`] 里几串 beta 与 [`config::cc_model_beta`] 按代际去项的唯一正确性
/// 依据。官方串逐字取自 `cap/2.1.285`（同一会话里 `/model` 切了 11 个模型，各一轮主线程），
/// 去掉动态的 `afk-mode`。模拟路径现在用的 2.1.293 表见 [`simulated_beta_matches_2_1_293`]。
///
#[test]
fn simulated_beta_matches_official() {
    let cases: &[(&str, &str, &str)] = &[
        (
            "00030",
            "claude-opus-5-5",
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
                 per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
                 advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
                 mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
                 dangerous-tool-use-2026-09-03,thinking-binding-controls-2026-08-01,\
                 thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
                 cache-diagnosis-2026-04-07,message-threads-2026-08-12",
        ),
        (
            "00039",
            "claude-fable-5-1",
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
                 per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
                 advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
                 mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
                 dangerous-tool-use-2026-09-03,thinking-binding-controls-2026-08-01,\
                 thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
                 cache-diagnosis-2026-04-07,message-threads-2026-08-12",
        ),
        (
            "00045",
            "claude-sonnet-5-5",
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
                 per-turn-control-2026-07-01,advisor-tool-2026-03-01,\
                 advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,\
                 effort-2025-11-24,dangerous-tool-use-2026-09-03,\
                 thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
                 extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,\
                 message-threads-2026-08-12",
        ),
        (
            "00051",
            "claude-haiku-4-5-20251001",
            "oauth-2025-04-20,interleaved-thinking-2025-05-14,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,claude-code-20250219,advisor-tool-2026-03-01,\
                 advanced-tool-use-2025-11-20,dangerous-tool-use-2026-09-03,\
                 thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
                 extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,\
                 message-threads-2026-08-12",
        ),
        (
            "00055",
            "claude-sonnet-5",
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
                 advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
                 mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
                 dangerous-tool-use-2026-09-03,thinking-binding-controls-2026-08-01,\
                 thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
                 cache-diagnosis-2026-04-07,message-threads-2026-08-12",
        ),
        (
            "00061",
            "claude-opus-5",
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
                 mid-conversation-tool-changes-2026-07-01,advisor-tool-2026-03-01,\
                 advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,\
                 effort-2025-11-24,dangerous-tool-use-2026-09-03,\
                 thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
                 extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,\
                 message-threads-2026-08-12",
        ),
        (
            "00067",
            "claude-fable-5",
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
                 mid-conversation-tool-changes-2026-07-01,advisor-tool-2026-03-01,\
                 advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,\
                 effort-2025-11-24,dangerous-tool-use-2026-09-03,\
                 thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
                 extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,\
                 message-threads-2026-08-12",
        ),
        (
            "00072",
            "claude-opus-4-8",
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
                 mid-conversation-tool-changes-2026-07-01,advisor-tool-2026-03-01,\
                 advanced-tool-use-2025-11-20,mid-conversation-system-clear-at-2026-08-21,\
                 effort-2025-11-24,dangerous-tool-use-2026-09-03,\
                 thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
                 extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,\
                 message-threads-2026-08-12",
        ),
        (
            "00077",
            "claude-opus-4-7",
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,\
                 advanced-tool-use-2025-11-20,effort-2025-11-24,dangerous-tool-use-2026-09-03,\
                 thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
                 extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,\
                 message-threads-2026-08-12",
        ),
        (
            "00083",
            "claude-opus-4-6",
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,\
                 advanced-tool-use-2025-11-20,effort-2025-11-24,dangerous-tool-use-2026-09-03,\
                 thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
                 extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,\
                 message-threads-2026-08-12",
        ),
        (
            "00088",
            "claude-sonnet-4-6",
            "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,\
                 advanced-tool-use-2025-11-20,effort-2025-11-24,dangerous-tool-use-2026-09-03,\
                 thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
                 extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,\
                 message-threads-2026-08-12",
        ),
    ];
    let at_285 = |model: &str| {
        config::cc_profile_at(crate::proxy::cc_profile_for(model).kind, Some((2, 1, 285)))
    };
    for (cap, model, official) in cases {
        let profile = at_285(model);
        assert_eq!(profile.version, "2.1.285");
        let beta = config::cc_model_beta(profile, model);
        assert_eq!(crate::proxy::simulated_beta(&beta, None), *official, "{model}（{cap}）");
    }
    // 认不出的模型退回 sonnet 族全集。
    assert_eq!(
        config::cc_model_beta(at_285("gpt-4o"), "gpt-4o"),
        cases[2].2.replacen("oauth-2025-04-20,", "", 1)
    );

    // 客户端自己要的 beta 不丢，去重后追加在官方串之后。
    let with_client = crate::proxy::simulated_beta(
        crate::proxy::cc_profile_for("claude-sonnet-5").beta,
        Some("output-128k-2025-02-19, effort-2025-11-24"),
    );
    assert!(with_client.contains("output-128k-2025-02-19"), "客户端的 beta 被丢了: {with_client}");
    assert_eq!(with_client.matches("effort-2025-11-24").count(), 1, "重复项: {with_client}");

    // `context-1m` 有官方位置：`oauth` 之后、`interleaved-thinking` 之前
    // （`cap/auto-2.1.285-20260930/00235`）；不带就不注入。
    let opus = crate::proxy::cc_profile_for("claude-opus-5-5");
    let one_m = crate::proxy::simulated_beta(
        opus.beta,
        Some("output-128k-2025-02-19,context-1m-2025-08-07"),
    );
    assert!(
        one_m.starts_with(
            "claude-code-20250219,oauth-2025-04-20,context-1m-2025-08-07,\
                 interleaved-thinking-2025-05-14,"
        ),
        "{one_m}"
    );
    assert!(one_m.ends_with(",output-128k-2025-02-19"), "其余客户端项仍追加在队尾: {one_m}");
    assert!(!crate::proxy::simulated_beta(opus.beta, None).contains("context-1m"));
}

/// 模拟路径（2.1.293 表）主线程的 `anthropic-beta` 逐字节等于 2.1.293 **默认权限模式**的官方
/// 串（`cap/auto-2.1.293-20261008-full`：`00419` opus-5-5、`00256` sonnet-5-5、`00383` haiku-4.5、
/// `00344` haiku-5-5；fable 默认模式没样本，与 opus 同串，见 [`config::CC_PROFILES`]）。auto 模式
/// 那串多 `dangerous-tool-use` 与 `afk-mode`，模拟请求不带 `safeguards`，两项都不该出现。
#[test]
fn simulated_beta_matches_2_1_293() {
    let opus = "claude-code-20250219,oauth-2025-04-20,interleaved-thinking-2025-05-14,\
         thinking-token-count-2026-05-13,context-management-2025-06-27,\
         prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
         per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
         inline-tools-2026-09-15,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
         mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
         thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
         extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,message-threads-2026-08-12";
    let haiku = "oauth-2025-04-20,interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
         context-management-2025-06-27,prompt-caching-scope-2026-01-05,claude-code-20250219,\
         advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
         thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
         extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,message-threads-2026-08-12";
    let haiku_5_5 = "oauth-2025-04-20,interleaved-thinking-2025-05-14,\
         thinking-token-count-2026-05-13,context-management-2025-06-27,\
         prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,claude-code-20250219,\
         per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
         inline-tools-2026-09-15,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
         mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
         thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
         extended-cache-ttl-2025-04-11,cache-diagnosis-2026-04-07,message-threads-2026-08-12";
    for (cap, model, official) in [
        ("00419", "claude-opus-5-5", opus),
        ("00256", "claude-sonnet-5-5", opus),
        ("00546 同串", "claude-fable-5-1", opus),
        ("00383", "claude-haiku-4-5-20251001", haiku),
        ("00383", "claude-haiku-4-5", haiku),
        ("00344", "claude-haiku-5-5", haiku_5_5),
        ("00305 同串", "haiku", haiku_5_5),
    ] {
        let profile = crate::proxy::cc_profile_for(model);
        assert_eq!(profile.version, "2.1.293");
        let beta = config::cc_model_beta(profile, model);
        let out = crate::proxy::simulated_beta(&beta, None);
        assert_eq!(out, official, "{model}（{cap}）");
        assert!(!out.contains("dangerous-tool-use") && !out.contains("afk-mode"), "{model}");
    }
}

/// 2.1.277 三个辅助 profile 的 beta 串逐字对上抓包（去掉 `oauth` 之后；四族主线程由
/// [`simulated_beta_matches_official`] 钉住）。
#[test]
fn profile_betas_match_the_2_1_277_captures() {
    use config::CcProfileKind::*;
    let cases: &[(config::CcProfileKind, &str, &str)] = &[
        (
            SdkSubagentHaiku,
            "cap/2.1.277/00049",
            "interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
                 context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
                 claude-code-20250219,advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,\
                 thinking-binding-controls-2026-08-01,thinking-display-updates-2026-08-18,\
                 cache-diagnosis-2026-04-07,message-threads-2026-08-12",
        ),
        (
            SessionTitleHaiku,
            "cap/2.1.277/00022",
            "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,\
                 structured-outputs-2025-12-15,cache-diagnosis-2026-04-07",
        ),
        (
            QuotaProbe,
            "cap/2.1.277/00005",
            "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05",
        ),
    ];
    for (kind, cap, official) in cases {
        let p = config::cc_profile_at(*kind, Some((2, 1, 277)));
        assert_eq!(p.version, "2.1.277", "{kind:?}");
        assert_eq!(p.beta, *official, "{kind:?}（{cap}）");
    }
    // 2.1.293 的额度探测（`cap/auto-2.1.293-20261008-full/00017`）与 2.1.277 逐字相同。
    let quota = config::cc_profile(QuotaProbe);
    assert_eq!((quota.version, quota.beta), ("2.1.293", cases[2].2));
    // 2.1.291 的标题生成（默认模式下 13 条）与 2.1.277 逐字相同。
    let title = config::cc_profile_at(SessionTitleHaiku, Some((2, 1, 291)));
    assert_eq!((title.version, title.beta), ("2.1.291", cases[1].2));
    // 2.1.293 换成 haiku-5-5：`prompt-caching-scope` 与 `advisor` 之间多了一串，`structured-outputs`
    // 挪到 `effort` 之后（`cap/auto-2.1.293-20261008-full/00030`）。
    let title = config::cc_profile(SessionTitleHaiku);
    assert_eq!(
        (title.version, title.beta),
        (
            "2.1.293",
            "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
             per-turn-control-2026-07-01,mid-conversation-tool-changes-2026-07-01,\
             inline-tools-2026-09-15,advisor-tool-2026-03-01,\
             mid-conversation-system-clear-at-2026-08-21,effort-2025-11-24,\
             structured-outputs-2025-12-15,cache-diagnosis-2026-04-07"
        )
    );
    // 2.1.285 的标题生成（`cap/2.1.285/00038`，auto 模式会话）比 2.1.277 多 `dangerous-tool-use`
    // 与队尾的 `message-threads`。
    assert_eq!(
        config::cc_profile_at(SessionTitleHaiku, Some((2, 1, 285))).beta,
        "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,\
             structured-outputs-2025-12-15,dangerous-tool-use-2026-09-03,\
             cache-diagnosis-2026-04-07,message-threads-2026-08-12"
    );
    // 每个主线程 profile 的 `oauth` 都由 simulated_beta 落到官方位置：以 claude-code
    // 开头的紧随其后，haiku 排在最前（cap/2.1.285/00051 头两项 `oauth,interleaved`）。
    assert!(
        crate::proxy::simulated_beta(config::cc_profile(MainHaiku).beta, None)
            .starts_with("oauth-2025-04-20,interleaved-thinking-2025-05-14,")
    );
    assert!(
        crate::proxy::simulated_beta(config::cc_profile(MainOpus).beta, None)
            .starts_with("claude-code-20250219,oauth-2025-04-20,interleaved-thinking-")
    );
}

/// 六个已观察的 2.1.260 profile 的 beta 串逐字对上抓包（去掉 `oauth` 与动态的 `afk-mode`
/// 之后）。这张表现在只给 2.1.260 ~ 2.1.276 来访的 [`merge_beta_for`] 做参照，与 2.1.277 表没
/// 编的两个 kind 兜底，验收照旧。
#[test]
fn profile_betas_match_the_2_1_260_captures() {
    use config::CcProfileKind::*;
    // 每项：profile、抓包出处、官方串（去掉 oauth 与 afk-mode）。
    let cases: &[(config::CcProfileKind, &str, &str)] = &[
        (
            SdkSubagentHaiku,
            "cap/2.1.260/00020",
            "interleaved-thinking-2025-05-14,thinking-token-count-2026-05-13,\
                 context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
                 claude-code-20250219,thinking-display-updates-2026-08-18,\
                 cache-diagnosis-2026-04-07",
        ),
        (
            HelperSubagentHaiku,
            "cap/2.1.260/00024",
            "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,server-side-fallback-2026-06-01,\
                 fallback-credit-2026-06-01,cache-diagnosis-2026-04-07",
        ),
        (
            SessionTitleHaiku,
            "cap/2.1.260-2/00058",
            "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05,advisor-tool-2026-03-01,\
                 structured-outputs-2025-12-15,fallback-credit-2026-06-01,\
                 cache-diagnosis-2026-04-07",
        ),
        (
            SecurityClassifierSonnet,
            "cap/2.1.260/00019",
            "claude-code-20250219,context-1m-2025-08-07,\
                 interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
                 context-management-2025-06-27,prompt-caching-scope-2026-01-05,\
                 mid-conversation-system-2026-04-07,auto-mode-classifier-2026-07-16,\
                 extended-cache-ttl-2025-04-11",
        ),
        (
            QuotaProbe,
            "cap/2.1.260-2/00004",
            "interleaved-thinking-2025-05-14,redact-thinking-2026-02-12,\
                 thinking-token-count-2026-05-13,context-management-2025-06-27,\
                 prompt-caching-scope-2026-01-05",
        ),
    ];
    for (kind, cap, official) in cases {
        let p = config::cc_profile_at(*kind, Some((2, 1, 260)));
        assert_eq!(p.version, "2.1.260", "{kind:?}");
        assert_eq!(p.beta, *official, "{kind:?}（{cap}）");
    }
}

/// [`merge_beta_for`] 对一条**完整的** 2.1.277 / 2.1.280 / 2.1.285 订阅端串必须幂等：参照串按来访版本
/// 取那一版的表，`server-side-fallback` 四族都不在参照里、不补；2.1.277 的 `fallback-credit`
/// 已在位，2.1.280 的参照里没有它、也不补。参照选错一版就会把上一版才有的项塞回来——
/// 2.1.280 的串拿 2.1.277 表去补，就会在 `effort` 后面多出一个 `fallback-credit`。
#[test]
fn merged_beta_is_idempotent_on_2_1_277_and_2_1_280_main_threads() {
    use config::CcProfileKind::*;
    for version in [(2, 1, 277), (2, 1, 280)] {
        for (kind, model) in [
            (MainOpus, "claude-opus-5"),
            (MainFable, "claude-fable-5-1"),
            (MainSonnet, "claude-sonnet-5"),
            (MainHaiku, "claude-haiku-4-5-20251001"),
        ] {
            let official =
                crate::proxy::simulated_beta(config::cc_profile_at(kind, Some(version)).beta, None);
            assert_eq!(
                merge_beta(Some(&official), Some(model), Some(version)),
                official,
                "{kind:?} {version:?}: 完整的官方串过 merge_beta 不该多一项"
            );
            assert!(!official.contains("server-side-fallback"), "{kind:?}");
        }
    }
    // 2.1.285：11 个模型各自的官方串（族全集按代际去项，[`config::cc_model_beta`]）过
    // merge_beta 同样一项不多——它只按族补缺，老模型少的那几项（`mid-conversation-system`
    // 一系、`per-turn-control`）不会被补回来。
    for model in [
        "claude-opus-5-5",
        "claude-fable-5-1",
        "claude-sonnet-5-5",
        "claude-haiku-4-5-20251001",
        "claude-sonnet-5",
        "claude-opus-5",
        "claude-fable-5",
        "claude-opus-4-8",
        "claude-opus-4-7",
        "claude-opus-4-6",
        "claude-sonnet-4-6",
    ] {
        let version = Some((2, 1, 285));
        let profile = config::cc_profile_at(super::cc_profile_kind_for(model), version);
        assert_eq!(profile.version, "2.1.285", "{model}");
        let official = crate::proxy::simulated_beta(&config::cc_model_beta(profile, model), None);
        assert_eq!(
            merge_beta(Some(&official), Some(model), version),
            official,
            "{model}: 完整的 2.1.285 官方串过 merge_beta 不该多一项"
        );
    }
}

/// 官方辅助请求经 [`merge_beta_for`] 之后**一项都不多**：它们各有一套更短的 beta 集合，
/// 主线程那几项（`advanced-tool-use`/`server-side-fallback`/`extended-cache-ttl`…）
/// 补进去就是一个官方从不产生的串。API-key 端不发 `oauth`，那一项要补。
#[test]
fn merge_beta_leaves_official_helper_requests_alone() {
    for (name, kind) in [
        ("标题生成", config::CcProfileKind::SessionTitleHaiku),
        ("安全分类", config::CcProfileKind::SecurityClassifierSonnet),
        ("无工具 helper", config::CcProfileKind::HelperSubagentHaiku),
        ("额度探测", config::CcProfileKind::QuotaProbe),
    ] {
        let official = crate::proxy::simulated_beta(config::cc_profile(kind).beta, None);
        // API-key 端那份就是官方串去掉 oauth；merge_beta 应当只把它补回来。
        let api_key_side: Vec<&str> =
            official.split(',').filter(|p| *p != config::OAUTH_BETA_HEADER).collect();
        assert_eq!(
            merge_beta(Some(&api_key_side.join(",")), None, Some((2, 1, 260))),
            official,
            "{name}: merge_beta 不该给辅助请求补主线程那几项"
        );
    }
}

/// **SDK 子代理不是主线程**：它有 `claude-code`，前几条判据都放它过去，于是会被补上
/// `advanced-tool-use` 与 `extended-cache-ttl`——官方那条（`cap/2.1.260/00020`）两样都没有。
///
/// 判据是「有 `thinking-display-updates`，却一个 `effort`/`advisor-tool`/`per-turn-control`
/// 都没有」。这一条不能误伤 2.1.220 那代的 haiku（[`BETA_PAIRS`] 第三对）——它压根没有
/// `thinking-display-updates`，照旧要补那两项。
#[test]
fn merge_beta_leaves_the_sdk_subagent_alone() {
    // 2.1.260 的子代理串。2.1.277 的子代理带上了 advisor-tool / advanced-tool-use，这条
    // 判据认不出它——见 config::cc_2_1_277_missing_samples 第 3 条，没有 API-key 端样本前不改。
    let official = crate::proxy::simulated_beta(
        config::cc_profile_at(config::CcProfileKind::SdkSubagentHaiku, Some((2, 1, 260))).beta,
        None,
    );
    let api_key_side: Vec<&str> =
        official.split(',').filter(|p| *p != config::OAUTH_BETA_HEADER).collect();
    let out = merge_beta(
        Some(&api_key_side.join(",")),
        Some("claude-haiku-4-5-20251001"),
        Some((2, 1, 260)),
    );
    assert_eq!(out, official, "SDK 子代理只该补 oauth");
    assert!(!out.contains("advanced-tool-use"), "官方那条没有这一项: {out}");
    assert!(!out.contains("extended-cache-ttl"), "也没有这一项: {out}");

    // 反例：2.1.220 的 haiku 没有 thinking-display-updates，照旧按主线程补齐。
    let (_, client_220, official_220) = BETA_PAIRS[2];
    assert_eq!(
        &merge_beta(Some(client_220), None, Some((2, 1, 220))),
        official_220,
        "老世代的 haiku 不该被当成 SDK 子代理"
    );
}

/// 模拟路径追加客户端 beta 时，**同名不同日期**算已经有了，**互斥项**直接丢。
///
/// 两种都是官方从不产生的组合：两条 `server-side-fallback`、或
/// `redact-thinking` 与 `thinking-display-updates` 并存。
#[test]
fn simulated_beta_rejects_conflicting_client_betas() {
    // 2.1.260 的官方 fable 串里是 06-01；客户端带 07-01，不该拼出两条。（2.1.277 四族都不发
    // 这一项了，故用 2.1.260 那行做同名不同日期的样本。）
    let fable_260 = config::cc_profile_at(config::CcProfileKind::MainFable, Some((2, 1, 260))).beta;
    let out = crate::proxy::simulated_beta(fable_260, Some("server-side-fallback-2026-07-01"));
    assert_eq!(out.matches("server-side-fallback-").count(), 1, "只该有一条: {out}");
    assert!(out.contains("server-side-fallback-2026-06-01"), "留官方那条日期: {out}");
    // 2.1.277 的 fable 串里没有它：客户端带来的那条照发（不是同名、不是互斥，上游判）。
    let fable = crate::proxy::cc_profile_for("claude-fable-5-1").beta;
    let out = crate::proxy::simulated_beta(fable, Some("server-side-fallback-2026-07-01"));
    assert_eq!(out.matches("server-side-fallback-").count(), 1, "{out}");

    // opus 官方串有 thinking-display-updates，客户端带 redact-thinking → 丢。
    let opus = crate::proxy::cc_profile_for("claude-opus-5").beta;
    let out = crate::proxy::simulated_beta(opus, Some(config::CC_BETA_REDACT_THINKING));
    assert!(!out.contains("redact-thinking"), "互斥项不能并存: {out}");
    assert!(out.contains(config::CC_BETA_THINKING_DISPLAY_UPDATES), "{out}");

    // 反向也拦：官方串有 redact-thinking（额度探测）时，客户端的 display-updates 丢掉。
    let probe = config::cc_profile(config::CcProfileKind::QuotaProbe).beta;
    let out = crate::proxy::simulated_beta(probe, Some(config::CC_BETA_THINKING_DISPLAY_UPDATES));
    assert!(!out.contains("thinking-display-updates"), "反向同样互斥: {out}");

    // 不冲突的照旧追加，一个都不能少。
    let out = crate::proxy::simulated_beta(opus, Some("output-128k-2025-02-19"));
    assert!(out.contains("output-128k-2025-02-19"), "{out}");
}

/// 2.1.260 的 fable 缺了 `server-side-fallback-2026-06-01` 时必须补得回来。
///
/// 回归：`seed_has` 原先是逐字比对常量 `…-2026-07-01`，而 fable 的官方串里是 06-01，
/// 于是「官方发这一项吗」永远答否，整个补齐分支成了死代码。
#[test]
fn merge_beta_backfills_the_fable_server_side_fallback() {
    // 2.1.260 fable 的 API-key 端形态：缺 oauth / advanced-tool-use /
    // server-side-fallback / extended-cache-ttl / cache-diagnosis。
    let api_key = "claude-code-20250219,interleaved-thinking-2025-05-14,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
             per-turn-control-2026-07-01,effort-2025-11-24,fallback-credit-2026-06-01,\
             thinking-display-updates-2026-08-18";
    let out = merge_beta(Some(api_key), Some("claude-fable-5-1"), Some((2, 1, 260)));
    assert!(
        out.contains(config::CC_BETA_SERVER_SIDE_FALLBACK_JUN),
        "fable 该补上 06-01 那条: {out}"
    );
    assert!(
        !out.contains(config::CC_BETA_SERVER_SIDE_FALLBACK),
        "不能补成 2.1.258 那个日期: {out}"
    );
    assert_eq!(out.matches("server-side-fallback-").count(), 1, "{out}");
    // 补完就是官方那串（去掉动态的 afk-mode）——这才是这条测试真正要钉的。
    assert_eq!(
        out,
        crate::proxy::simulated_beta(
            config::cc_profile_at(config::CcProfileKind::MainFable, Some((2, 1, 260))).beta,
            None
        )
    );
    // 位置：`effort` 之后、`fallback-credit` 之前（官方序）。
    let idx = |b: &str| out.split(',').position(|p| p.starts_with(b)).unwrap();
    assert!(idx("effort-") < idx("server-side-fallback-"));
    assert!(idx("server-side-fallback-") < idx("fallback-credit-"));

    // 2.1.260 的 opus 整项不发，别给它补。
    let out = merge_beta(Some(api_key), Some("claude-opus-5"), Some((2, 1, 260)));
    assert!(!out.contains("server-side-fallback-"), "opus 2.1.260 不发这一项: {out}");
}

/// 带日期的 beta 按**名字**判在不在：一个已经带 `server-side-fallback-2026-06-01` 的
/// 2.1.260 来访，不该再被插一条 `-2026-07-01`。
#[test]
fn merge_beta_does_not_duplicate_a_redated_beta() {
    // fable 2.1.260 的 API-key 端形态：有 per-turn-control 与 06-01 那条。
    let incoming = "claude-code-20250219,interleaved-thinking-2025-05-14,\
             thinking-token-count-2026-05-13,context-management-2025-06-27,\
             prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
             per-turn-control-2026-07-01,effort-2025-11-24,\
             server-side-fallback-2026-06-01,fallback-credit-2026-06-01";
    let out = merge_beta(Some(incoming), Some("claude-fable-5-1"), Some((2, 1, 260)));
    assert_eq!(out.matches("server-side-fallback-").count(), 1, "只该有一条: {out}");
    assert!(out.contains("server-side-fallback-2026-06-01"), "保留客户端那条日期: {out}");
    assert!(out.contains(config::OAUTH_BETA_HEADER), "oauth 要补上: {out}");
}

/// 模拟模式下来访那套头一个不留：UA/x-app/x-stainless-* 全是官方取值，
/// 客户端自带的非官方头（`x-my-tool`）不转发，`anthropic-beta` 取并集。
#[test]
fn simulated_headers_replace_client_headers() {
    let mut client = crate::proxy::HeaderMap::new();
    for (k, v) in [
        ("user-agent", "python-httpx/0.27.0"),
        ("accept", "text/event-stream"),
        ("x-my-tool", "cherry-studio"),
        ("anthropic-beta", "output-128k-2025-02-19"),
    ] {
        client.insert(crate::proxy::HeaderName::from_static(k), HeaderValue::from_static(v));
    }
    let sim = sim_for(PLAIN_BODY);
    let out = build_forward_headers(&client, "sk-ant-oat01-REAL", all_on(), Some(&sim), None);
    let get = |k: &str| out.get(k).and_then(|v| v.to_str().ok()).unwrap_or_default();

    assert_eq!(get("user-agent"), config::CC_USER_AGENT);
    assert_eq!(get("accept"), "application/json", "官方即便流式也发 application/json");
    assert_eq!(get("x-app"), "cli");
    assert_eq!(get("x-stainless-os"), "MacOS");
    assert_eq!(get("anthropic-version"), "2023-06-01");
    assert_eq!(get("accept-encoding"), config::CC_ACCEPT_ENCODING);
    assert_eq!(get("x-claude-code-session-id"), sim.session_id, "会话 id 与 metadata 同值");
    assert!(!get("x-client-request-id").is_empty(), "每请求一个 uuid");
    assert_eq!(get("authorization"), "Bearer sk-ant-oat01-REAL");
    assert!(out.get("x-my-tool").is_none(), "客户端的非官方头不该带到上游");
    assert!(get("anthropic-beta").contains("output-128k-2025-02-19"), "客户端 beta 不该丢");
    assert!(get("anthropic-beta").contains(config::OAUTH_BETA_HEADER));

    // 表里的头全在，且没有多出表外的头（除四个由 HTTP 客户端自己追加的）。
    for (name, _) in config::CC_SIM_HEADERS {
        assert!(out.contains_key(*name), "缺头 {name}");
    }
}

/// `extended-cache-ttl` 只认真断点上的 `cache_control.ttl`：历史工具入参里的 `ttl:"1h"`
/// （以 `cap/auto-2.1.285-20260930/00175` 那种体为底）、schema 里的同名字段都不算；
/// 消息块与 `tool_result` 内层块上的 1h 断点照认。
#[test]
fn only_real_cache_breakpoints_count_as_ttl_1h() {
    let ctx = |body: serde_json::Value| {
        let raw = body.to_string();
        super::BetaCtx::of(crate::proxy::CcRequestKind::Main, raw.as_bytes(), Some(&body), &[])
            .ttl_1h
    };
    let tool_input = serde_json::json!({
        "system": [{ "type": "text", "text": "x", "cache_control": { "type": "ephemeral" } }],
        "tools": [{ "name": "Cache", "input_schema": { "properties": { "ttl": { "const": "1h" } } } }],
        "messages": [
            { "role": "user", "content": "hi" },
            { "role": "assistant", "content": [
                { "type": "tool_use", "id": "t1", "name": "Cache",
                  "input": { "ttl": "1h", "cache_control": { "ttl": "1h" } } },
            ] },
        ],
    });
    assert!(!ctx(tool_input), "工具入参与 schema 里的 ttl 不是断点");
    let on_block = serde_json::json!({ "messages": [{ "role": "user", "content": [
            { "type": "text", "text": "hi", "cache_control": { "type": "ephemeral", "ttl": "1h" } },
        ] }] });
    assert!(ctx(on_block));
    let in_result = serde_json::json!({ "messages": [{ "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "t1", "content": [
                { "type": "text", "text": "ok", "cache_control": { "type": "ephemeral", "ttl": "1h" } },
            ] },
        ] }] });
    assert!(ctx(in_result));
}
