//! 测试夹具：跨多个子模块共用的构造器与常量。
//!
//! 只在 `cfg(test)` 下编译；每一项都是从原 `proxy::tests` 里原样搬过来的。

use axum::body::Bytes;
use axum::http::{HeaderValue, header};

use crate::store;

/// 实测的形态类 400 原文（逐字），见 [`crate::proxy::ShapeProbe`]。
pub(super) const ROLE_400: &str = "role 'system' is not supported on this model";

/// 把原始 body 解析一次，模拟 [`super::handle`] 里那一步——生产路径全程只解析一次，
/// 测试也走同一个形态，免得两边对「非法 JSON 怎么办」的理解漂开。
pub(super) fn parsed(b: &Bytes) -> Option<serde_json::Value> {
    serde_json::from_slice(b).ok()
}

/// 形态开关全开（= 默认，也是加入开关机制之前的既有行为）。
pub(super) fn all_on() -> store::ForwardFlags {
    store::ForwardFlags::default()
}

/// `rewrite_body` 的测试简写：固定不做流式化。绝大多数用例验的是 system/metadata 那几项
/// 改写，流式化另有专门用例（[`forces_stream_true_and_keeps_key_order`]）。
///
/// `session_out` 跟着 `bare_session` 走——转发路径上「来访没带 metadata」那条正是这个
/// 关系（见 [`super::outbound_session_id`]）。只验出站归一的用例直接调
/// [`rewrite_body_with_session`]。
pub(super) fn rewrite_body(
    body: &Bytes,
    cred: &crate::credentials::Credential,
    device_fp: &str,
    flags: store::ForwardFlags,
    sim: Option<&super::Simulation>,
    bare_session: Option<&str>,
) -> Bytes {
    rewrite_body_with_session(body, cred, device_fp, flags, sim, bare_session, bare_session)
}

/// [`rewrite_body`] 的完整版：`bare_session`（要不要补一份 metadata）与 `session_out`
/// （出站两处要落的那个会话 id）分开给。
pub(super) fn rewrite_body_with_session(
    body: &Bytes,
    cred: &crate::credentials::Credential,
    device_fp: &str,
    flags: store::ForwardFlags,
    sim: Option<&super::Simulation>,
    bare_session: Option<&str>,
    session_out: Option<&str>,
) -> Bytes {
    // 测试默认按「出站头里带了 thinking-display-updates」跑，fable 的 display 才补得上；
    // 头上没有那项 beta 的反例见 `skips_thinking_display_without_the_beta`。
    super::rewrite_body(
        body,
        cred,
        device_fp,
        flags,
        sim,
        bare_session,
        session_out,
        false,
        None,
        true,
        true,
        None,
        None,
        super::CcRequestKind::Main,
        None,
    )
}

/// 抓包 040 里的真实 account_uuid。
pub(super) const ACCOUNT_UUID: &str = "27aa7c53-0d20-42d2-806a-60c710529405";

/// 按顺序取出 `HeaderMap` 里的头名。
pub(super) fn names(h: &super::HeaderMap) -> Vec<String> {
    h.iter().map(|(k, _)| k.as_str().to_string()).collect()
}

pub(super) fn gzip(data: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    e.write_all(data).unwrap();
    e.finish().unwrap()
}

pub(super) fn test_cred() -> crate::credentials::Credential {
    crate::credentials::Credential {
        id: 1,
        label: "t".into(),
        tier: None,
        org_type: None,
        rate_limit_tier: None,
        access_token: "a".into(),
        refresh_token: "r".into(),
        expires_at: u64::MAX,
        priority: 0,
        disabled: false,
        device_limit: 0,
        session_limit: 0,
        rpm_limit: 0,
        quota_pause_pct: None,
        quota_pause_pct_7d: None,
        ban_reason: None,
        account_uuid: Some(ACCOUNT_UUID.into()),
        org_uuid: None,
        subscription_created_at: None,
        resume_at: None,
        proxy: None,
        created_at: 0,
        updated_at: 0,
    }
}

/// API-key 模式客户端的三块 `system`，形状取自 `cap/raw/00002`（内容缩短）：
/// billing header 无断点、身份句带 5m 断点、合并块 = 基座 ‖ `\n\n` ‖ 锚点开头的其余部分。
/// 尾部那条 role=system 的消息也带一个 5m 断点，和真实客户端一样。
pub(super) const API_SHAPE_BODY: &str = concat!(
    r#"{"model":"claude-opus-5","messages":[{"role":"system","content":[{"type":"text","#,
    r#""text":"deferred tools","cache_control":{"type":"ephemeral"}}]}],"#,
    r#""system":[{"type":"text","text":"x-anthropic-billing-header: cc_entrypoint=cli;"},"#,
    r#"{"type":"text","text":"You are Claude Code, Anthropic's official CLI for Claude.","#,
    r#""cache_control":{"type":"ephemeral"}},"#,
    r#"{"type":"text","text":"\nBASE — 基座\n\nWrite code that reads like the surrounding "#,
    r#"code: match its comment density, naming, and idiom.\n\nREST","#,
    r#""cache_control":{"type":"ephemeral"}}],"#,
    r#""metadata":{"user_id":"{\"device_id\":\"dddd\",\"account_uuid\":\"\",\"session_id\":\"ssss\"}"}}"#
);

pub(super) fn err_json(message: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "type": "error",
        "error": {"type": "invalid_request_error", "message": message}}))
    .unwrap()
}

// ---------- 非 CC 请求的模拟（Simulation） ----------

/// 一条普通客户端会发的请求：没有 system、没有 metadata，头也不是 CC 那套。
pub(super) const PLAIN_BODY: &str = concat!(
    r#"{"model":"claude-opus-5","max_tokens":1024,"#,
    r#""messages":[{"role":"user","content":"hi"}],"stream":true}"#
);

/// 一块够长的「基座提示词」，给要装成真 CC 的测试体用：真 CC 只要写了身份句就一定带
/// 基座（[`super::has_cc_base_prompt`]），测试体里少了它会被当成去掉基座的第三方而走模拟。
pub(super) fn base_block() -> String {
    format!(r#"{{"type":"text","text":"{}"}}"#, "x".repeat(1200))
}

/// 测试里一律走这个判定，别直接调 [`super::Simulation::detect`]：`from_cc_client` 要按
/// 代理里那条式子从 UA 算——**只看 UA**，`metadata.user_id` 和 session 头不再构成跳过理由。
pub(super) fn detect_with(
    body: &Bytes,
    headers: &super::HeaderMap,
    flags: store::ForwardFlags,
) -> Option<super::Simulation> {
    let v = parsed(body);
    let from_cc_client = super::trusted_cc_version(&super::ua_of(headers)).is_some();
    // 会话键按代理里那条式子从体算（缓存前缀）：解析不了的体没有键，给个定值即可——那种
    // 体 `detect` 本来就返回 `None`。
    let key = v.as_ref().map(super::sim_session_key).unwrap_or_default();
    // 夹具不走选号、没有槽位，会话 id 按缓存前缀派生。
    super::Simulation::detect(
        v.as_ref(),
        headers,
        from_cc_client,
        flags,
        &test_cred(),
        "fp",
        super::SimSessionSeed::Prefix(&key),
    )
}

pub(super) fn detect_for(body: &Bytes, flags: store::ForwardFlags) -> Option<super::Simulation> {
    detect_with(body, &super::HeaderMap::new(), flags)
}

pub(super) fn sim_for(body: &str) -> super::Simulation {
    detect_for(&Bytes::from(body.to_string()), all_on()).expect("普通请求应判为需要模拟")
}

/// 平台头齐全的一份来访头，供设备指纹用例用。
pub(super) fn platform_headers(client_ua: Option<&'static str>) -> super::HeaderMap {
    let mut h = super::HeaderMap::new();
    h.insert("x-stainless-arch", HeaderValue::from_static("arm64"));
    h.insert("x-stainless-os", HeaderValue::from_static("MacOS"));
    if let Some(ua) = client_ua {
        h.insert(header::USER_AGENT, HeaderValue::from_static(ua));
    }
    h
}

/// 由 `k=v` 造一份限流头解析结果，给下面两个测试共用。
pub(super) fn rl_headers(pairs: &[(&str, &str)]) -> super::RateLimitInfo {
    let mut h = super::HeaderMap::new();
    for (k, v) in pairs {
        h.insert(
            super::HeaderName::from_bytes(k.as_bytes()).unwrap(),
            HeaderValue::from_str(v).unwrap(),
        );
    }
    super::RateLimitInfo::from_headers(&h)
}
