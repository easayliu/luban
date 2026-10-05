//! 服务端拒答回退（`fallbacks` 字段）的取舍、补齐与被拒后的记忆。

use super::*;

/// 这条请求该补哪份 `fallbacks`（计费路径、主线程 profile，且该族的开关开着时）：
///
/// - fable 族（`fable_refusal_fallback`，默认关）：官方 2.1.260 那份
///   `[{"model":"claude-opus-5"}]`（profile 里逐字取自抓包），补上反而更像官方；
/// - opus-5 族（`opus_refusal_fallback`，**默认关**）：luban 自定的
///   [`config::OPUS_REFUSAL_FALLBACKS`]（4.8 → 4.6）。官方 opus 客户端不发这个字段，补了就是
///   官方从不产生的请求形态，是风控层面的自证风险，故只作为独立实验开关保留；
/// - 其余模型（sonnet / haiku / 4.x）：不补。**不是**它们没有分类器——官方文档明说 Sonnet 5
///   与 Opus 4.7/4.8 同样带实时网络安全分类器、同样以 200 + `stop_reason: "refusal"` 拒答——
///   而是官方客户端在这些模型上不发 `fallbacks` 字段，luban 不凭空造官方从不产生的请求形态
///   （opus-5 那条自定链就是为此才默认关的）。它们的拒答照样被嗅探器记流水、按提示词学
///   （[`UsageSniffer::classifier_refusal`] 不看模型族），只是不替它们换模型重跑。
///
/// 上游曾以 400 拒过这个模型的 fallback 目标（[`remember_fallback_rejection`]）的，也不补。
pub(in crate::proxy) fn refusal_fallbacks_for(
    model: Option<&str>,
    flags: store::ForwardFlags,
    billable: bool,
    cc_kind: CcRequestKind,
    learned: &DeprecatedFieldMemory,
) -> Option<&'static str> {
    if !billable || cc_kind != CcRequestKind::Main {
        return None;
    }
    let model = model?;
    let m = model.to_ascii_lowercase();
    let plan = if m.contains("fable") {
        if !flags.fable_refusal_fallback {
            return None;
        }
        cc_profile_for(model).fallbacks?
    } else if m.starts_with("claude-opus-5") {
        if !flags.opus_refusal_fallback {
            return None;
        }
        config::OPUS_REFUSAL_FALLBACKS
    } else {
        return None;
    };
    if learned.read().contains_key(&(model.to_string(), FALLBACKS_FIELD.to_string())) {
        return None;
    }
    Some(plan)
}

/// 客户端自己带了**数组形态**的 `fallbacks`（[`ensure_fallbacks`] 对它一个字不动）。有则
/// luban 不算补过：`refusal_fallbacks` 留 `None`，上游 400 拒它时不学、不剥掉重试。字符串
/// `"default"` 不算——那一份会被 luban 换成数组（[`ensure_fallbacks`] / [`normalize_fallbacks`]），
/// 出站的是 luban 的字面量。
pub(in crate::proxy) fn client_supplied_fallbacks(body: Option<&serde_json::Value>) -> bool {
    body.and_then(|v| v.get("fallbacks")).is_some_and(|f| !f.is_string())
}

/// 这条请求出站时会不会带一份**上游会照着换模型重跑**的 `fallbacks`——客户端自己写了合法的
/// 数组（[`valid_fallback_array`]，[`rewrite_body`] 一字不动送出），或 luban 按族开关要补
/// （[`refusal_fallbacks_for`]）。带的请求上游拒答后会自己换模型重跑，本地的「已拒答提示词」
/// 规则（[`known_refused_prompt`]）不该拦它。
///
/// **字符串形态一律不算**，与 [`client_supplied_fallbacks`] 同口径。2.1.258 那种 `"default"`
/// （`cap/2.1.258/00013`）在 luban 有计划时会被 [`ensure_fallbacks`] 换成计划（上面那条已经
/// 算进去了）；没计划时它原样出站，而头那侧按「客户端没带」处理、不补 `server-side-fallback`
/// beta，是「体里有字段、头上没声明」的形态，上游不会为它换模型重跑——放行只是白送一次拒答，
/// 该本地回放上游那次的 200。此前这里把没计划的 `"default"` 也当成「带了」，门禁与实际出站
/// 体不一致。空串或别的字面量上游一定 400，同样不算。
/// `cc_kind` 在这里按体与 beta 头现算：调用点在 `handle_inner` 早于主流程算 `cc_kind` 的
/// 位置，而 [`CcRequestKind::of`] 是纯函数。
pub(in crate::proxy) fn outbound_carries_fallbacks(
    body: Option<&serde_json::Value>,
    model: Option<&str>,
    flags: store::ForwardFlags,
    inbound_beta: &[String],
    learned: &DeprecatedFieldMemory,
) -> bool {
    let Some(v) = body else { return false };
    let client = v.get("fallbacks");
    // 客户端带了非字符串：luban 一个字不动（[`client_supplied_fallbacks`]），出站就是它那份——
    // 算不算「带了」看它是不是一份上游会认的数组；`[]`、`null`、`{}`、`[null]`、`[{}]` 上游
    // 一定 400，本地规则不能为它让路。
    if let Some(f) = client
        && !f.is_string()
    {
        return valid_fallback_array(f);
    }
    // 字段缺失或是字符串：luban 有计划就写计划（[`ensure_fallbacks`] 会把任何字符串换掉），
    // 没计划就是没带——字符串不算，见函数文档。
    let cc_kind = CcRequestKind::of(v, inbound_beta);
    refusal_fallbacks_for(model, flags, true, cc_kind, learned).is_some()
}

/// 一份上游会认的 `fallbacks` 数组：非空，每一项是带非空 `model` 字符串的对象。官方定义就
/// 这一种元素形态（可选 `max_tokens` 覆盖不在此判），别的写法上游 400。
pub(in crate::proxy) fn valid_fallback_array(f: &serde_json::Value) -> bool {
    f.as_array().is_some_and(|a| {
        !a.is_empty()
            && a.iter()
                .all(|e| e.get("model").and_then(|m| m.as_str()).is_some_and(|m| !m.is_empty()))
    })
}

/// 记忆表里「这个模型不收 `fallbacks`」那条的字段名。与已废弃字段同一张表、同一套落库
/// （`kind = "deprecated"`），但**不在** [`DEPRECATABLE_FIELDS`] 里：那张名单还管
/// `sampling_policy` 的静态拒绝与剥离，把 `fallbacks` 混进去会让 4.7+ 模型上客户端自带的
/// `fallbacks` 被当成采样参数剥掉或拒掉。
pub(in crate::proxy) const FALLBACKS_FIELD: &str = "fallbacks";

/// 上游那条 400 是不是冲着 `fallbacks` 来的（目标模型不在 `allowed_fallback_models`、
/// 与请求模型重复、条数超限……）。只按 message 判：这类错误归在 `invalid_request_error`
/// 名下，靠类型分不出来；而 `fallback` 这个词只在这一件事上出现。
pub(in crate::proxy) fn is_fallback_rejection(err: &[u8]) -> bool {
    let (_, message) = parse_upstream_error(err);
    message.to_lowercase().contains("fallback")
}

/// 上游以 400 拒了 luban 补的 `fallbacks` → 记进 [`DeprecatedFieldMemory`]（模型 +
/// `fallbacks`），之后 [`refusal_fallbacks_for`] 对该模型不再补。返回**这次新学到**的那条，
/// 调用方拿去落库（同 [`remember_deprecated_field`]）。
pub(in crate::proxy) fn remember_fallback_rejection(
    mem: &DeprecatedFieldMemory,
    model: &str,
    err: &[u8],
) -> Option<store::LearnedRejection> {
    let (_, message) = parse_upstream_error(err);
    let mut table = mem.write();
    let key = (model.to_string(), FALLBACKS_FIELD.to_string());
    if table.contains_key(&key) || table.len() >= SHAPE_MEMORY_CAP {
        return None;
    }
    table.insert(key, message.clone());
    tracing::warn!(
        model = %model,
        upstream_message = %message.chars().take(300).collect::<String>(),
        "upstream rejected the fallbacks luban added; this model will be sent without them from now on"
    );
    Some(store::LearnedRejection {
        kind: LEARNED_KIND_DEPRECATED.into(),
        model: model.to_string(),
        field: FALLBACKS_FIELD.into(),
        value: String::new(),
        message,
        reply: None,
    })
}

/// 补 `fallbacks`：客户端没写、或写的是 2.1.258 的字符串 `"default"`，都换成 `plan`
/// 那份数组；客户端自己已经发了数组形态（自己就是 2.1.260 一代）时原样不动。
///
/// 位置按 [`config::CC_BODY_ORDER_MAIN`]：`context_management` 之后、`output_config` 之前；
/// 找不到锚点时追加，随后 [`align_cc_top_level_order`] 归位。要补什么、补给谁由
/// [`refusal_fallbacks_for`] 决定，这里只管写。
pub(in crate::proxy) fn ensure_fallbacks(v: &mut serde_json::Value, plan: &str) -> bool {
    if v.get("fallbacks").is_some_and(|f| !f.is_string()) {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(plan) else { return false };
    if v.get("fallbacks").is_some() {
        let Some(obj) = v.as_object_mut() else { return false };
        obj.insert("fallbacks".into(), value);
    } else {
        insert_top_level(
            v,
            "fallbacks",
            value,
            &["context_management", "temperature", "thinking", "max_tokens", "metadata", "model"],
        );
    }
    true
}

/// `fallbacks` 的**形态**归一（该族的 refusal fallback 开关关着时走这条）：2.1.258 的官方 fable 发
/// 字符串 `"default"`，2.1.260 换成了数组 `[{"model":"claude-opus-5"}]`（`cap/2.1.260/00018`）。
///
/// 只在客户端自己已经要了 fallback 时改形态，不替它凭空开——开关关着即用户明确不要
/// luban 替他换模型跑。客户端已经发了数组形态时原样不动。
pub(in crate::proxy) fn normalize_fallbacks(
    v: &mut serde_json::Value,
    profile: &config::CcProfile,
) -> bool {
    let Some(official) = profile.fallbacks else { return false };
    // 只认字符串形态的旧写法；已经是数组（或别的我们不认识的形态）就不动。
    if !v.get("fallbacks").is_some_and(|f| f.is_string()) {
        return false;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(official) else { return false };
    let Some(obj) = v.as_object_mut() else { return false };
    // `insert` 对已有键原位改值（`preserve_order`），键序不动。
    obj.insert("fallbacks".into(), value);
    true
}
