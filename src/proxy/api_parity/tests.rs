use serde_json::json;

use super::*;

fn betas(s: &[&str]) -> Vec<String> {
    s.iter().map(|b| b.to_string()).collect()
}

/// 直连只认 `summarized` / `omitted`；`updates` 与别的值、非字符串都回那句，路径带 thinking 的类型。
#[test]
fn display_outside_the_two_values_is_rejected_without_the_beta() {
    let msg = |v: serde_json::Value| client_display_rejection(Some(&v), &[]);
    assert_eq!(
        msg(json!({"thinking": {"type": "adaptive", "display": "updates"}})).as_deref(),
        Some("thinking.adaptive.display: Input should be 'summarized', 'omitted'")
    );
    assert_eq!(
        msg(json!({"thinking": {"type": "enabled", "budget_tokens": 1024, "display": "full"}}))
            .as_deref(),
        Some("thinking.enabled.display: Input should be 'summarized', 'omitted'")
    );
    assert!(msg(json!({"thinking": {"type": "adaptive", "display": 1}})).is_some());
    for ok in [json!("summarized"), json!("omitted"), serde_json::Value::Null] {
        assert_eq!(msg(json!({"thinking": {"type": "adaptive", "display": ok}})), None);
    }
    assert_eq!(msg(json!({"thinking": {"type": "adaptive"}})), None, "没写 display 不管");
    assert_eq!(
        msg(json!({"thinking": {"type": "between_tools", "display": "updates"}})),
        None,
        "between_tools 两边回同一句，交给上游"
    );
}

/// 来访自己带了 `thinking-display-updates`（日期不同也算）：出站同样带，两边一致，交给上游。
#[test]
fn display_with_the_client_beta_is_left_to_upstream() {
    let v = json!({"thinking": {"type": "adaptive", "display": "full"}});
    assert_eq!(client_display_rejection(Some(&v), &betas(&[BETA_DISPLAY_UPDATES])), None);
    assert_eq!(
        client_display_rejection(Some(&v), &betas(&["thinking-display-updates-2027-01-01"])),
        None
    );
}

/// 收 `enabled` 的模型、预算不小于 `max_tokens`、没带 interleaved → 拒；相等也拒。
#[test]
fn budget_not_below_max_tokens_is_rejected_on_enabled_models() {
    let b = |max: u64, budget: u64| json!({"max_tokens": max, "thinking": {"type": "enabled", "budget_tokens": budget}});
    for model in ["claude-haiku-4-5", "claude-opus-4-6", "claude-sonnet-4-5-20250929"] {
        assert!(client_budget_rejection(Some(&b(1024, 2000)), Some(model), &[]), "{model}");
        assert!(client_budget_rejection(Some(&b(1024, 1024)), Some(model), &[]), "{model} 相等");
        assert!(!client_budget_rejection(Some(&b(2048, 1024)), Some(model), &[]), "{model} 小于");
    }
}

/// 让位：带 interleaved、新一代模型、预算不足 1024、采样冲突、display 非法、缺 max_tokens。
#[test]
fn budget_check_yields_to_earlier_upstream_checks() {
    let m = Some("claude-haiku-4-5");
    let base = json!({"max_tokens": 1024, "thinking": {"type": "enabled", "budget_tokens": 2000}});
    assert!(!client_budget_rejection(Some(&base), m, &betas(&[BETA_INTERLEAVED_THINKING])));
    assert!(!client_budget_rejection(Some(&base), Some("claude-opus-5-5"), &[]));
    let with = |patch: serde_json::Value| {
        let mut v = base.clone();
        for (k, val) in patch.as_object().unwrap() {
            v[k] = val.clone();
        }
        v
    };
    assert!(!client_budget_rejection(
        Some(&with(
            json!({"max_tokens": 400, "thinking": {"type": "enabled", "budget_tokens": 500}})
        )),
        m,
        &[]
    ));
    assert!(!client_budget_rejection(Some(&with(json!({"temperature": 0.5}))), m, &[]));
    assert!(!client_budget_rejection(Some(&with(json!({"top_k": 5}))), m, &[]));
    assert!(!client_budget_rejection(
        Some(&with(
            json!({"thinking": {"type": "enabled", "budget_tokens": 2000, "display": "full"}})
        )),
        m,
        &[]
    ));
    let mut no_max = base.clone();
    no_max.as_object_mut().unwrap().remove("max_tokens");
    assert!(!client_budget_rejection(Some(&no_max), m, &[]));
    assert!(client_budget_rejection(Some(&with(json!({"temperature": 1}))), m, &[]), "1 不冲突");
}

const BILLING: &str =
    "x-anthropic-billing-header: cc_version=2.1.301.abc; cc_entrypoint=cli; cch=00000;";

fn sys(texts: &[&str]) -> serde_json::Value {
    json!({"system": texts.iter().map(|t| json!({"type": "text", "text": t})).collect::<Vec<_>>()})
}

/// 出站开头多一块 billing header、客户端块原样顺延：出站 1、2 对应来访 0、1。
#[test]
fn index_map_for_plain_prepend() {
    let map = billing_system_index_map(Some(&sys(&["a", "b"])), &sys(&[BILLING, "a", "b"]), true);
    assert_eq!(map, Some(vec![0, 1]));
    // 真实 CC 缺 header、前插了一块：同样顺延一位。
    let map = billing_system_index_map(Some(&sys(&["a", "b"])), &sys(&[BILLING, "a", "b"]), false);
    assert_eq!(map, Some(vec![0, 1]));
}

/// 首块混着抄来的 billing header 与业务指令：那行去掉、块保留，出站 1 对应来访 0。
/// 只剩 billing header 的块不出站：出站 1 对应来访 1。
#[test]
fn index_map_follows_dropped_billing_lines() {
    let mixed = format!("{BILLING}\nbe brief");
    let map = billing_system_index_map(Some(&sys(&[&mixed])), &sys(&[BILLING, "be brief"]), true);
    assert_eq!(map, Some(vec![0]));
    let map = billing_system_index_map(Some(&sys(&[BILLING, "b"])), &sys(&[BILLING, "b"]), true);
    assert_eq!(map, Some(vec![1]));
    // 空白块同样不出站：出站 1、2 对应来访 1、3。
    let map = billing_system_index_map(
        Some(&sys(&["", "a", " ", "b"])),
        &sys(&[BILLING, "a", "b"]),
        true,
    );
    assert_eq!(map, Some(vec![1, 3]));
}

/// 对不上就不给表：字符串 system、整段挪走、正文不一致、出站首块不是 billing header。
#[test]
fn index_map_refuses_when_blocks_do_not_line_up() {
    let out = sys(&[BILLING, "a", "b"]);
    for simulated in [true, false] {
        let map = |inb: &serde_json::Value, out: &serde_json::Value| {
            billing_system_index_map(Some(inb), out, simulated)
        };
        assert_eq!(map(&json!({"system": "a"}), &out), None);
        assert_eq!(map(&sys(&["a", "b"]), &sys(&[BILLING])), None);
        assert_eq!(map(&sys(&["a", "x"]), &out), None);
        assert_eq!(map(&sys(&["a", "b"]), &sys(&["z", "a", "b"])), None);
    }
}

/// 真实 CC 来访已带 billing header：luban 没前插、没删块，一律不改下标——哪怕首块去掉首行后恰好
/// 等于第二块（按模拟那套规则就会误判成「前插一块、删一块」）。
#[test]
fn index_map_leaves_real_cc_with_its_own_header_alone() {
    let both = format!("{BILLING}\n{BILLING}\n");
    let one = format!("{BILLING}\n");
    let inb = sys(&[&both, &one]);
    assert_eq!(billing_system_index_map(Some(&inb), &inb, false), None);
    // 真实 CC 不删块：来访的空白块出站照在，按模拟那套跳过它就对不上。
    let map = billing_system_index_map(Some(&sys(&["", "a"])), &sys(&[BILLING, "", "a"]), false);
    assert_eq!(map, Some(vec![0, 1]));
}

/// `system.N` 按表还原，`system.0`、别的名字、无下标的不动；体的其余部分原样。
#[test]
fn unshift_rewrites_only_system_indices_in_the_message() {
    let err = |m: &str| {
        Bytes::from(
            json!({"type": "error", "error": {"type": "invalid_request_error", "message": m}, "request_id": "req_1"})
                .to_string(),
        )
    };
    let fixed = unshift_system_index(
        &err("system.2.cache_control.ephemeral.ttl: Input should be '5m' or '1h'"),
        &[0, 1],
    )
    .unwrap();
    let v: serde_json::Value = serde_json::from_slice(&fixed).unwrap();
    assert_eq!(
        v["error"]["message"],
        "system.1.cache_control.ephemeral.ttl: Input should be '5m' or '1h'"
    );
    assert_eq!(v["request_id"], "req_1");
    let map: Vec<usize> = (0..10).collect();
    assert_eq!(
        remap_system_paths("system.10.text and system.3.type", &map).as_deref(),
        Some("system.9.text and system.2.type")
    );
    assert_eq!(remap_system_paths("system.0.text: Field required", &map), None);
    assert_eq!(remap_system_paths("system: text content blocks must be non-empty", &map), None);
    assert_eq!(remap_system_paths("xsystem.2 and my_system.3", &map), None);
    assert_eq!(remap_system_paths("system.1.text", &[1]), None, "映射后下标没变就不算改");
    assert_eq!(remap_system_paths("system.5.text", &[0]), None, "超出表的不动");
    assert!(unshift_system_index(&Bytes::from_static(b"not json"), &map).is_none());
}
