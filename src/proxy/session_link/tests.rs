use crate::proxy::test_support::{all_on, base_block, detect_with, parsed, test_cred};
use crate::proxy::{Bytes, HeaderValue, config, header};

/// `cc_prompt_index` 的计数（[`super::CcSessionLink::prompt_index`]）：会话第一条是 1，
/// 工具续轮沿用，新输入加一；换一张凭证就是另一条链，从 1 起。
#[test]
fn prompt_index_counts_new_prompts_per_credential_session() {
    use super::{CcSessionKey, CcSessionLink};
    let sid = crate::proxy::uuid_v4();
    let key = CcSessionKey { cred_id: 41, session_id: &sid };
    assert_eq!(CcSessionLink::load(key, true, true).prompt_index, 1, "会话第一条");
    assert_eq!(CcSessionLink::load(key, false, true).prompt_index, 1, "工具续轮沿用");
    assert_eq!(CcSessionLink::load(key, true, true).prompt_index, 2, "新输入加一");
    let other = CcSessionKey { cred_id: 42, session_id: &sid };
    assert_eq!(CcSessionLink::load(other, true, true).prompt_index, 1, "另一张凭证另起");
    assert_eq!(CcSessionLink::default().prompt_index, 0, "不在链上的不写");
}

/// 来访头上的 `x-claude-code-prompt-id` 就是这一轮的 prompt id：补出来的 `cc_prompt_id` 沿用它，
/// 同一轮后面没带头的续轮也沿用；下一次新输入没带头就另起一个。
#[test]
fn client_prompt_id_header_is_adopted() {
    use super::{CcSessionKey, CcSessionLink};
    let sid = crate::proxy::uuid_v4();
    let key = CcSessionKey { cred_id: 43, session_id: &sid };
    let pid = "6f5a49b5-5795-46a3-815b-05b2b4f52d2e";
    let first = CcSessionLink::load_turn(key, true, false, true, Some(pid));
    assert_eq!(first.prompt_id.as_deref(), Some(pid));
    let cont = CcSessionLink::load_turn(key, false, false, true, None);
    assert_eq!(cont.prompt_id.as_deref(), Some(pid), "续轮沿用这一轮的");
    let next = CcSessionLink::load_turn(key, true, false, true, None);
    assert!(next.prompt_id.is_some_and(|p| p != pid), "新输入换一个");
}

/// 前缀差异日志靠 [`super::text_diff`] 找出两轮 system 块里变的那一段：只剩中段、
/// 切点在字符边界上；[`super::cache_prefix_stable`] 变了回 `false`、没变回 `true`。
#[test]
fn prefix_diff_isolates_the_changed_segment() {
    use super::{CachePrefix, cache_prefix_stable, text_diff};
    // 现网那种尾块每轮追加：差异段就是追加的那 51 字节。
    let tail = "…memory\n\n# Notes";
    let grown = format!("{tail}\n\n<total_tokens>14999746 tokens left</total_tokens>");
    let (at, removed, added) = text_diff(tail, &grown).unwrap();
    assert_eq!(at, tail.len());
    assert_eq!(removed, "");
    assert_eq!(added, "\n\n<total_tokens>14999746 tokens left</total_tokens>");
    assert_eq!(added.len(), 51, "49 字节提醒加 \\n\\n 正是流水里每轮多出的 51");
    // 中段替换、多字节字符：切点不能落在字符中间。
    let (at, removed, added) = text_diff("前缀中文后缀", "前缀英文后缀").unwrap();
    assert_eq!(at, "前缀".len());
    assert_eq!((removed, added), ("中", "英"));
    // 数字变了（同长度替换）。
    let (at, removed, added) =
        text_diff("<total_tokens>15000000 tokens left>", "<total_tokens>14999746 tokens left>")
            .unwrap();
    assert_eq!(at, "<total_tokens>1".len());
    assert_eq!((removed, added), ("5000000", "4999746"));
    assert!(text_diff("same", "same").is_none());
    assert_eq!(text_diff("", "x").unwrap(), (0, "", "x"));
    assert_eq!(super::truncate_chars("abc", 5), "abc");
    assert_eq!(super::truncate_chars("abcdefgh", 3), "abc…(共 8 字符)");
    assert_eq!(super::tail_chars("abc", 5), "abc");
    assert_eq!(super::tail_chars("abcdefgh", 3), "(共 8 字符)…fgh");

    use crate::proxy::CcRequestKind::{Main, Subagent};
    let sid = crate::proxy::uuid_v4();
    let key = crate::proxy::CcSessionKey { cred_id: 7, session_id: &sid };
    let p = |tail: &str| CachePrefix { tools_fp: 1, system: vec!["base".into(), tail.into()] };
    assert!(!cache_prefix_stable(key, Main, p(tail)), "第一轮没有上一轮可比");
    assert!(cache_prefix_stable(key, Main, p(tail)), "同一份前缀");
    assert!(!cache_prefix_stable(key, Main, p(&grown)), "尾块变了");
    assert!(cache_prefix_stable(key, Main, p(&grown)), "稳住了");
    assert!(
        !cache_prefix_stable(key, Main, CachePrefix { tools_fp: 2, ..p(&grown) }),
        "tools 变了是另一条谱系，第一轮"
    );
    assert!(
        !cache_prefix_stable(
            key,
            Main,
            CachePrefix { tools_fp: 2, system: vec!["base".into(), "x".into(), "y".into()] }
        ),
        "块数变了是另一条谱系，第一轮"
    );

    // 现网那种交替：主线程 3 块 + 一套 tools，辅助请求 2 块 + 另一套 tools，共用会话 id。
    // 两条谱系各记各的，主线程第二轮照样判稳定，不被中间那条辅助请求打掉。
    let sid = crate::proxy::uuid_v4();
    let key = crate::proxy::CcSessionKey { cred_id: 7, session_id: &sid };
    let main = || CachePrefix { tools_fp: 11, system: vec!["a".into(), "b".into(), "c".into()] };
    let helper = || CachePrefix { tools_fp: 22, system: vec!["h".into(), "i".into()] };
    assert!(!cache_prefix_stable(key, Main, main()), "主线程第一轮");
    assert!(!cache_prefix_stable(key, Subagent, helper()), "辅助第一轮");
    assert!(cache_prefix_stable(key, Main, main()), "主线程第二轮：中间夹的辅助请求不算变");
    assert!(cache_prefix_stable(key, Subagent, helper()), "辅助第二轮同理");
    // 同形态不同类别也分开：类别在键里。
    assert!(!cache_prefix_stable(key, Subagent, main()), "同一份前缀换个类别是新谱系");
}

/// 正文预算：总量超限只丢正文不丢指纹（稳定性照判）；条目数到顶按最久未见淘汰；同一
/// 谱系更新时先扣旧条目的字节；容器开销算进字节；单条块数 / 字节上限在锁外判
/// （[`super::cache_prefix_stable`]），这里用 `text: None` 模拟已被判掉。
#[test]
fn prefix_table_keeps_fingerprints_when_text_is_over_budget() {
    use super::{CachePrefix, PrefixKey, PrefixLimits, PrefixTable};
    let now = std::time::Instant::now();
    let at = |secs: u64| now + std::time::Duration::from_secs(secs);
    let sz = std::mem::size_of::<String>();
    // 总预算：两条各「10 字符 + 一块头」正好装下，第三条装不下。
    let limits = PrefixLimits {
        entry_blocks: 64,
        entry_bytes: 256 * 1024,
        total_bytes: 2 * (10 + sz) + 5,
        entries: 3,
    };
    let mut t = PrefixTable::default();
    let p = |text: &str| CachePrefix { tools_fp: 1, system: vec![text.to_string()] };
    let k = |i: i64| PrefixKey {
        cred_id: i,
        session_id: format!("s{i}"),
        kind: crate::proxy::CcRequestKind::Main,
        tools_fp: 1,
        blocks: 1,
    };
    let rec = |t: &mut PrefixTable, i: i64, text: &str, keep: bool, when| {
        let pf = p(text);
        t.record(
            k(i),
            pf.system_fp(),
            keep.then(|| pf.system.clone()),
            pf.stored_bytes(),
            when,
            limits,
        )
    };

    // 容器开销算进字节：一块 10 字符占 10 + size_of::<String>()。
    assert_eq!(p("x".repeat(10).as_str()).stored_bytes(), 10 + sz);
    // 20 万个单字符块：字符 20 万，入表字节远大于此。
    let many = CachePrefix { tools_fp: 1, system: vec!["x".to_string(); 200_000] };
    assert_eq!(many.stored_bytes(), 200_000 + 200_000 * sz);

    // 单条在锁外被判掉（text: None）：只留指纹，稳定性照判。
    assert!(rec(&mut t, 1, "x", false, at(0)).is_none());
    assert!(t.map[&k(1)].system.is_none(), "单条超限不留正文");
    assert_eq!(t.text_bytes, 0);
    let prev = rec(&mut t, 1, "x", false, at(0)).unwrap();
    assert_eq!(prev.system_fp, p("x").system_fp());
    assert_ne!(prev.system_fp, p("y").system_fp());

    // 两条各 10 字符都留；第三条总量超限只留指纹。
    rec(&mut t, 2, "a".repeat(10).as_str(), true, at(1));
    rec(&mut t, 3, "b".repeat(10).as_str(), true, at(2));
    assert_eq!(t.text_bytes, 2 * (10 + sz));
    assert_eq!(t.map.len(), 3);
    // 条目数已到 3，新谱系进来淘汰最久未见的那一个（k(1)，seen 最早）。
    rec(&mut t, 4, "c".repeat(8).as_str(), true, at(3));
    assert_eq!(t.map.len(), 3, "条目数封顶");
    assert!(!t.map.contains_key(&k(1)), "淘汰最久未见的");
    assert!(t.map[&k(4)].system.is_none(), "总量超限只留指纹");
    assert_eq!(t.text_bytes, 2 * (10 + sz));
    // 同一谱系换成短正文：先扣旧的，再加新的。
    rec(&mut t, 2, "abc", true, at(4));
    assert_eq!(t.text_bytes, (10 + sz) + (3 + sz));
    assert_eq!(t.map[&k(2)].system.as_deref(), Some(&["abc".to_string()][..]));
}

/// 会话链条：`cc_prompt_id` 新输入换一轮、工具续轮沿用；`cc_prev_req` 与
/// `diagnostics.previous_message_id` 指向上一条请求/回复。
///
/// 这三个字段官方是**跨请求**的（`cap/2.1.260-2/00013` → `00059` → `00061`），一条一条
/// 独立造造不出来：造出来的会是「每条请求都是一次全新会话的第一条」。
/// 测试用的会话键：都挂在同一张凭证上，跨凭证隔离另见
/// [`session_link_is_isolated_per_credential`]。
fn key(session_id: &str) -> crate::proxy::CcSessionKey<'_> {
    crate::proxy::CcSessionKey { cred_id: 1, session_id }
}

/// **同一个会话 id 落到不同凭证上，两条链必须各走各的。**
///
/// 429/401 换号时，转发循环会带着**同一个**客户端会话 id 重跑一遍
/// `Simulation::detect` / `client_session_link`。只按会话 id 分桶的话，A 账号的
/// `cc_prev_req`（那是 A 的上游 request-id，B 从没见过）会被发到 B 上，同一次用户输入
/// 还会被再轮换一次 prompt id，而 B 的 `first_seen` 是 false ——B 这张凭证的启动握手
/// 就永远不会触发。遥测那边本来就是按 `(cred_id, session_id)` 分的，这里跟它对齐。
#[test]
fn session_link_is_isolated_per_credential() {
    let sid = format!("test-{}", crate::proxy::uuid_v4());
    let a = crate::proxy::CcSessionKey { cred_id: 1, session_id: &sid };
    let b = crate::proxy::CcSessionKey { cred_id: 2, session_id: &sid };

    let first_a = crate::proxy::CcSessionLink::load(a, true, true);
    assert!(first_a.first_seen, "A 的第一条");
    crate::proxy::CcSessionLink::record(
        a,
        &crate::proxy::ChainLane::Main,
        Some("req_a"),
        Some("msg_a"),
    );

    // 换到 B：同一个会话 id，但对 B 来说这是**新会话**——要触发 B 自己的启动握手，
    // 且拿不到 A 的那两个 id。
    let first_b = crate::proxy::CcSessionLink::load(b, false, true);
    assert!(first_b.first_seen, "换号后对 B 是新会话，B 的启动握手要触发");
    assert!(first_b.prev_req.is_none(), "A 的 request-id 不能发给 B");
    assert!(first_b.prev_message_id.is_none(), "A 的 message.id 同样不能");
    assert_ne!(first_b.prompt_id, first_a.prompt_id, "两条链各自的一轮");

    // B 记自己的，不影响 A。
    crate::proxy::CcSessionLink::record(
        b,
        &crate::proxy::ChainLane::Main,
        Some("req_b"),
        Some("msg_b"),
    );
    let again_a = crate::proxy::CcSessionLink::load(a, false, true);
    assert_eq!(again_a.prev_req.as_deref(), Some("req_a"), "A 还是 A 那条");
    assert_eq!(again_a.prompt_id, first_a.prompt_id, "换号没把 A 这一轮转掉");
}

#[test]
fn session_link_carries_prompt_and_previous_ids() {
    let sid = format!("test-{}", crate::proxy::uuid_v4());
    let new_prompt = || crate::proxy::CcSessionLink::load(key(&sid), true, true);
    let tool_turn = || crate::proxy::CcSessionLink::load(key(&sid), false, true);

    // 首条：有 prompt_id，没有上一条可指。
    let first = new_prompt();
    let pid = first.prompt_id.clone().expect("主线程每条都带 cc_prompt_id");
    assert!(first.prev_req.is_none(), "会话第一条没有 cc_prev_req");
    assert!(first.prev_message_id.is_none(), "首轮 previous_message_id 为 null");

    // 回程记下这一条的两个 id。
    crate::proxy::CcSessionLink::record(
        key(&sid),
        &crate::proxy::ChainLane::Main,
        Some("req_001"),
        Some("msg_001"),
    );

    // 工具续轮：prompt_id 沿用，两个 prev 指向上一条。
    let second = tool_turn();
    assert_eq!(second.prompt_id.as_deref(), Some(pid.as_str()), "工具续轮沿用同一轮 id");
    assert_eq!(second.prev_req.as_deref(), Some("req_001"));
    assert_eq!(second.prev_message_id.as_deref(), Some("msg_001"));

    // 新一轮用户输入：换 prompt_id，prev 仍指最近一条。
    crate::proxy::CcSessionLink::record(
        key(&sid),
        &crate::proxy::ChainLane::Main,
        Some("req_002"),
        Some("msg_002"),
    );
    let third = new_prompt();
    assert_ne!(third.prompt_id.as_deref(), Some(pid.as_str()), "新输入要换一轮 id");
    assert_eq!(third.prev_req.as_deref(), Some("req_002"));
    assert_eq!(third.prev_message_id.as_deref(), Some("msg_002"));

    // 「猜下一句」那类请求不写 cc_prompt_id，但仍在同一条链上
    // （`cap/2.1.260-2/00063`：有 cc_prev_req、没有 cc_prompt_id）。
    let suggestion = crate::proxy::CcSessionLink::load(key(&sid), false, false);
    assert!(suggestion.prompt_id.is_none(), "不写 cc_prompt_id");
    assert_eq!(suggestion.prev_req.as_deref(), Some("req_002"), "但照样接在链上");

    // 另一个会话是另一条链，不该串味。
    let other = crate::proxy::CcSessionLink::load(
        key(&format!("test-{}", crate::proxy::uuid_v4())),
        true,
        true,
    );
    assert!(other.prev_req.is_none(), "不同会话各走各的链");
}

/// 上游只回了 request-id、没解析出 message.id（非流式解析失败、断流……）时，
/// 已有的 `previous_message_id` 不能被清空——官方那条链是不会中断的。
#[test]
fn session_link_record_keeps_what_it_was_not_told() {
    let sid = format!("test-{}", crate::proxy::uuid_v4());
    let _ = crate::proxy::CcSessionLink::load(key(&sid), true, true);
    crate::proxy::CcSessionLink::record(
        key(&sid),
        &crate::proxy::ChainLane::Main,
        Some("req_001"),
        Some("msg_001"),
    );
    crate::proxy::CcSessionLink::record(
        key(&sid),
        &crate::proxy::ChainLane::Main,
        Some("req_002"),
        None,
    );
    let link = crate::proxy::CcSessionLink::load(key(&sid), false, true);
    assert_eq!(link.prev_req.as_deref(), Some("req_002"), "request-id 更新了");
    assert_eq!(link.prev_message_id.as_deref(), Some("msg_001"), "message.id 保留上一次的");
}

/// 子代理各走一条链、不推进主线程（`cap/auto-2.1.306-20261021-full` `00075` → 子代理 `00077` →
/// `00080`；主线程 `00078` 仍指 `00075`），见 [`super::ChainLane`]。
#[test]
fn session_link_agents_have_their_own_chain() {
    use super::{CcSessionLink, ChainLane};
    let sid = format!("test-{}", crate::proxy::uuid_v4());
    let main = || CcSessionLink::load(key(&sid), false, true);
    let _ = CcSessionLink::load(key(&sid), true, true);
    CcSessionLink::record(key(&sid), &ChainLane::Main, Some("req_main"), Some("msg_main"));

    let first = main().on_agent_chain(key(&sid), "a1", None);
    assert!(first.prev_req.is_none(), "子代理首条不写 cc_prev_req");
    assert!(first.prev_message_id.is_none(), "首条 previous_message_id 为 null");
    assert_eq!(first.lane, Some(ChainLane::Agent("a1".into())));
    let lane = first.lane.clone().unwrap();
    CcSessionLink::record(key(&sid), &lane, Some("req_a1"), Some("msg_a1"));

    let next = main().on_agent_chain(key(&sid), "a1", None);
    assert_eq!(next.prev_req.as_deref(), Some("req_a1"), "接它自己的上一条");
    assert!(main().on_agent_chain(key(&sid), "a2", None).prev_req.is_none(), "另一个子代理另起");
    assert_eq!(main().prev_req.as_deref(), Some("req_main"), "主线程没被子代理推进");
    assert_eq!(main().prev_message_id.as_deref(), Some("msg_main"));
}

/// 失败后原样重发：客户端报的还是会话当前这一轮，不换 prompt id、`cc_prompt_index` 不加
/// （`cap/auto-2.1.306-20261021-retry` R4）；换了 id 才是新输入。
#[test]
fn session_link_retry_with_same_prompt_id_keeps_the_turn() {
    use super::CcSessionLink;
    let sid = format!("test-{}", crate::proxy::uuid_v4());
    let turn = |pid| CcSessionLink::load_turn(key(&sid), true, false, true, Some(pid));
    assert_eq!(turn("p1").prompt_index, 1);
    let retry = turn("p1");
    assert_eq!((retry.prompt_id.as_deref(), retry.prompt_index), (Some("p1"), 1), "重发不换轮");
    let next = turn("p2");
    assert_eq!((next.prompt_id.as_deref(), next.prompt_index), (Some("p2"), 2), "新输入换轮");
}

/// 没有 prompt 头（模拟路径）：主线程失败后指纹相同的那条是重发，不换轮；这一轮成功一条之后、
/// 或指纹不同（用户重新输入，官方把报错前那句与新的一句并进同一条 user 消息），照常换轮。
#[test]
fn session_link_retry_without_prompt_header_keeps_the_turn() {
    use super::CcSessionLink;
    let sid = format!("test-{}", crate::proxy::uuid_v4());
    let turn = |fp| CcSessionLink::load_retryable(key(&sid), true, false, true, None, Some(fp));
    let fail = |l: &CcSessionLink| CcSessionLink::mark_failed(key(&sid), l.turn.as_ref().unwrap());
    let first = turn(1);
    fail(&first);
    let retry = turn(1);
    assert_eq!((retry.prompt_id.clone(), retry.prompt_index), (first.prompt_id.clone(), 1), "重发");

    CcSessionLink::record_main(key(&sid), retry.turn.as_ref().unwrap(), Some("req_r"), None);
    let again = turn(1);
    assert_eq!(again.prompt_index, 2, "成功之后同样的输入是新的一轮");
    fail(&again);
    assert_eq!(turn(2).prompt_index, 3, "指纹不同即是新输入");
}

/// 失败标记绑着它那一轮：A 失败 → B 开了新一轮 → A 再来，是新的一轮，不沿用 B 的；A 的失败迟到
/// （会话已在 B）也不回写。
#[test]
fn session_link_failure_mark_is_bound_to_its_turn() {
    use super::CcSessionLink;
    let sid = format!("test-{}", crate::proxy::uuid_v4());
    let turn = |fp| CcSessionLink::load_retryable(key(&sid), true, false, true, None, Some(fp));
    let a = turn(1);
    CcSessionLink::mark_failed(key(&sid), a.turn.as_ref().unwrap());
    let b = turn(2);
    assert_eq!(b.prompt_index, 2);
    let a_again = turn(1);
    assert_eq!(a_again.prompt_index, 3, "旧轮的失败标记在开 B 时已作废");
    assert_ne!(a_again.prompt_id, b.prompt_id);

    // A 的失败迟到：会话已在 a_again 那一轮，按 A 那一轮记的失败不该生效。
    CcSessionLink::mark_failed(key(&sid), a.turn.as_ref().unwrap());
    assert_eq!(turn(1).prompt_index, 4, "迟到的旧失败没回写当前这一轮");
}

/// 迟到的旧响应：更早发出的成功不覆盖主链；这一轮已有一条成功后，并行的另一次重发迟到失败也不再
/// 把它标回待重试。
#[test]
fn session_link_late_responses_do_not_rewind_the_turn() {
    use super::CcSessionLink;
    let sid = format!("test-{}", crate::proxy::uuid_v4());
    let turn = |fp| CcSessionLink::load_retryable(key(&sid), true, false, true, None, Some(fp));
    let mark = |l: &CcSessionLink| l.turn.clone().unwrap();
    let a = turn(1);
    let b = turn(2);
    CcSessionLink::record_main(key(&sid), &mark(&b), Some("req_b"), Some("msg_b"));
    CcSessionLink::record_main(key(&sid), &mark(&a), Some("req_a"), Some("msg_a"));
    let next = CcSessionLink::load(key(&sid), false, true);
    assert_eq!(next.prev_req.as_deref(), Some("req_b"), "A 迟到的成功没覆盖 B");
    assert_eq!(next.prev_message_id.as_deref(), Some("msg_b"));

    // 同一轮两次并行重发：R0 失败，R1、R2 都是它的重发；R1 成功后 R2 迟到失败，这一轮仍算完成，
    // 同样的输入是新的一轮。
    let r0 = turn(3);
    CcSessionLink::mark_failed(key(&sid), &mark(&r0));
    let r1 = turn(3);
    let r2 = turn(3);
    assert_eq!((r1.prompt_index, r2.prompt_index), (r0.prompt_index, r0.prompt_index), "都是重发");
    CcSessionLink::record_main(key(&sid), &mark(&r1), Some("req_r1"), None);
    CcSessionLink::mark_failed(key(&sid), &mark(&r2));
    assert_eq!(turn(3).prompt_index, r0.prompt_index + 1, "已成功的那一轮不会被迟到的失败重新打开");
}

/// 指纹从最后一条 user 消息算起：末尾挂着同一条 system 提醒时，换了问题也认得出。
#[test]
fn turn_fp_sees_past_a_trailing_system_message() {
    let body = |q: &str| {
        serde_json::json!({"messages": [
            {"role": "user", "content": q},
            {"role": "system", "content": "reminder"},
        ]})
    };
    let fp = |q| super::turn_fp(&body(q)).unwrap();
    assert_ne!(fp("A"), fp("B"));
    assert_eq!(fp("A"), fp("A"));
}

/// 后台子代理属于派生它的那一轮：主线程换了轮它不跟着变，迟到的子代理也不把主线程拨回去。
#[test]
fn session_link_agent_prompt_is_isolated_from_main() {
    use super::CcSessionLink;
    let sid = format!("test-{}", crate::proxy::uuid_v4());
    let main = |new, pid| CcSessionLink::load_turn(key(&sid), new, false, true, pid);
    let _ = main(true, Some("p1"));
    let agent = main(false, None).on_agent_chain(key(&sid), "a1", None);
    assert_eq!(agent.prompt_id.as_deref(), Some("p1"));

    let _ = main(true, Some("p2"));
    let late = main(false, None).on_agent_chain(key(&sid), "a1", None);
    assert_eq!(late.prompt_id.as_deref(), Some("p1"), "没带头的续轮仍是派生它的那一轮");
    let late = main(false, None).on_agent_chain(key(&sid), "a1", Some("p1"));
    assert_eq!(late.prompt_id.as_deref(), Some("p1"));
    let now = main(false, None);
    assert_eq!((now.prompt_id.as_deref(), now.prompt_index), (Some("p2"), 2), "主线程没被倒拨");

    // 主线程进了本地命令那一轮（不写 cc_prompt_id），旧子代理仍写它自己那一轮。
    let local = CcSessionLink::load_turn(key(&sid), true, true, true, Some("p3"));
    assert!(local.prompt_id.is_none());
    let old = main(false, None).on_agent_chain(key(&sid), "a1", None);
    assert_eq!(old.prompt_id.as_deref(), Some("p1"), "本地命令那一轮不吞掉子代理的 prompt");
}

/// **真实 CC（API-key 模式）** 的主线程请求也要补上会话关联字段。
///
/// API-key 端那条 billing header 是光秃秃的
/// `cc_version=…; cc_entrypoint=cli;`——没有 `cch`、没有 `cc_prompt_id`、没有
/// `cc_prev_req`，整条也没有 `diagnostics`（`cap/2.1.258-api` 六份逐一核过）。luban 拿
/// OAuth token 转出去之后，上游看到的是「一条 OAuth 主线程请求，一个会话关联字段都
/// 没有」，而订阅端官方每条都有。
#[test]
fn api_key_cc_gets_the_oauth_session_chain() {
    const SID: &str = "d0c1fb05-9b19-4576-9465-e2b8a206dabf";
    let body = Bytes::from(format!(
        concat!(
            r#"{{"model":"claude-opus-5","max_tokens":64000,"#,
            r#""messages":[{{"role":"user","content":"hi"}}],"#,
            r#""system":[{{"type":"text","text":"x-anthropic-billing-header: "#,
            r#"cc_version=2.1.260.222; cc_entrypoint=cli;"}},"#,
            r#"{{"type":"text","text":"{}"}},{}],"#,
            r#""tools":[{{"name":"Bash"}}],"#,
            r#""metadata":{{"user_id":"{{\"device_id\":\"{}\",\"session_id\":\"{}\"}}"}}}}"#
        ),
        config::CC_SYSTEM_IDENTITY,
        base_block(),
        // 真 CC 的 device_id 恒为 64 位 hex；不合法的会被 detect 当成非官方客户端送去模拟。
        "832cb7e697190bc475b926c7994ef183a0f8a58e29818f182e11f924e1ea2870",
        SID
    ));
    let parsed_body = parsed(&body);
    let mut headers = crate::proxy::HeaderMap::new();
    headers.insert("x-claude-code-session-id", HeaderValue::from_static(SID));
    // 主线程的 beta 串（有 effort，不是那几套非主线程 profile）。
    headers.insert(
        "anthropic-beta",
        HeaderValue::from_static("claude-code-20250219,effort-2025-11-24"),
    );
    // UA 自报 CC + 工具列表含官方名 → 真实 CC 客户端，不走模拟。
    headers.insert(header::USER_AGENT, HeaderValue::from_static(config::CC_USER_AGENT));

    assert!(detect_with(&body, &headers, all_on()).is_none(), "真实 CC 客户端不模拟");

    let (sid, link) = crate::proxy::client_session_link(
        parsed_body.as_ref(),
        &headers,
        None,
        all_on(),
        true,
        &test_cred(),
    )
    .expect("主线程 CC 请求该补会话链");
    assert_eq!(sid, SID, "键是客户端自己的会话 id");

    let out = crate::proxy::rewrite_body_out(
        &body,
        &test_cred(),
        "fp",
        all_on(),
        None,
        None,
        None,
        false,
        None,
        true,
        true,
        None,
        Some(&link),
        crate::proxy::CcRequestKind::Main,
        None,
    )
    .0;
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let billing = v["system"][0]["text"].as_str().unwrap();
    assert!(billing.contains("; cch="), "cch 照旧补: {billing}");
    assert!(billing.contains("cc_prompt_id="), "缺的这项要补上: {billing}");
    assert_eq!(
        v["diagnostics"],
        serde_json::json!({"previous_message_id": serde_json::Value::Null}),
        "首轮 diagnostics 字段在、值为 null: {v}"
    );
    // 段序：cch → cc_prev_req → cc_prompt_id（官方序）。
    assert!(billing.find("cch=").unwrap() < billing.find("cc_prompt_id=").unwrap());

    // 客户端自己写了 cc_prompt_id 的不动——它比我们更清楚自己的链。
    let own = Bytes::from(
        String::from_utf8(body.to_vec())
            .unwrap()
            .replace("cc_entrypoint=cli;", "cc_entrypoint=cli; cc_prompt_id=mine;"),
    );
    let out = crate::proxy::rewrite_body_out(
        &own,
        &test_cred(),
        "fp",
        all_on(),
        None,
        None,
        None,
        false,
        None,
        true,
        true,
        None,
        Some(&link),
        crate::proxy::CcRequestKind::Main,
        None,
    )
    .0;
    let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
    let billing = v["system"][0]["text"].as_str().unwrap();
    assert!(billing.contains("cc_prompt_id=mine;"), "保留客户端自己那个: {billing}");
    assert_eq!(billing.matches("cc_prompt_id=").count(), 1, "别补第二个: {billing}");

    // 来访自报 2.1.285：主线程在 `cc_prompt_id` 之后再补 `cc_turn_origin` 与会话里的第几轮
    // （`cap/2.1.285/00113`）；子代理只有 `cc_prompt_id`（`00120`）。版本读不出（上面几次）
    // 两样都不补。
    let rewrite = |kind| {
        let out = crate::proxy::rewrite_body_out(
            &body,
            &test_cred(),
            "fp",
            all_on(),
            None,
            None,
            None,
            false,
            None,
            true,
            true,
            Some(crate::proxy::CcClient { version: "2.1.285", entrypoint: "cli" }),
            Some(&link),
            kind,
            None,
        )
        .0;
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        v["system"][0]["text"].as_str().unwrap().to_string()
    };
    let main = rewrite(crate::proxy::CcRequestKind::Main);
    let n = link.prompt_index;
    assert!(n >= 1);
    assert!(
        main.ends_with(&format!("; cc_turn_origin=human; cc_prompt_index={n}; cc_turn_index={n};")),
        "{main}"
    );
    let sub = rewrite(crate::proxy::CcRequestKind::Subagent);
    assert!(sub.contains("cc_prompt_id=") && !sub.contains("cc_turn_origin"), "{sub}");
}

/// 三个关联字段**逐类给**，不能一刀切成主线程：官方六个 profile 在这三项上各不相同。
///
/// 最要紧的是「猜下一句」：它的末条正是一句 `[SUGGESTION MODE: …]` 的 user 消息，
/// 照 `last_is_new_prompt_body` 判就会换一轮 prompt id，把主线程那条链一起带偏；
/// 而官方那条（`cap/2.1.260-2/00063`）**根本不写** `cc_prompt_id`。
#[test]
fn session_chain_fields_follow_the_request_kind() {
    const SID: &str = "d0c1fb05-9b19-4576-9465-e2b8a206dabf";
    let body = |tools: &str, last: &str, billing_extra: &str| {
        Bytes::from(format!(
            concat!(
                r#"{{"model":"claude-opus-5","max_tokens":64000,"#,
                r#""messages":[{{"role":"user","content":"hi"}},"#,
                r#"{{"role":"assistant","content":"y"}},"#,
                r#"{{"role":"user","content":"{}"}}],"#,
                r#""system":[{{"type":"text","text":"x-anthropic-billing-header: "#,
                r#"cc_version=2.1.260.222; cc_entrypoint=cli;{}"}}],"#,
                r#""tools":{},"#,
                r#""metadata":{{"user_id":"{{\"session_id\":\"{}\"}}"}}}}"#
            ),
            last, billing_extra, tools, SID
        ))
    };
    let mut headers = crate::proxy::HeaderMap::new();
    headers.insert("x-claude-code-session-id", HeaderValue::from_static(SID));
    headers.insert(
        "anthropic-beta",
        HeaderValue::from_static("claude-code-20250219,effort-2025-11-24"),
    );
    let link = |b: &Bytes| {
        let parsed_body = parsed(b);
        crate::proxy::client_session_link(
            parsed_body.as_ref(),
            &headers,
            None,
            all_on(),
            true,
            &test_cred(),
        )
        .map(|(_, l)| l)
    };

    // 主线程：三项全给，新输入换一轮 id。
    let main = link(&body(r#"[{"name":"Bash"}]"#, "next question", "")).expect("主线程要补");
    let first_pid = main.prompt_id.clone().expect("主线程写 cc_prompt_id");
    assert!(main.diagnostics, "主线程写 diagnostics");

    // 「猜下一句」：**不写 cc_prompt_id、不写 diagnostics、也不换轮**。
    let sugg = link(&body(
        r#"[{"name":"Bash"}]"#,
        "[SUGGESTION MODE: Suggest what the user might type next.]",
        "",
    ))
    .expect("它仍在链上——官方那条带 cc_prev_req");
    assert!(sugg.prompt_id.is_none(), "官方 00063 没有 cc_prompt_id");
    assert!(!sugg.diagnostics, "官方 00063 也没有 diagnostics");

    // 换轮没被它带偏：主线程再来一条新输入，拿到的仍是「上一轮之后的下一轮」，
    // 而不是被猜下一句先转过一次。
    let again = link(&body(r#"[{"name":"Bash"}]"#, "another question", "")).unwrap();
    assert_ne!(again.prompt_id.as_ref(), Some(&first_pid), "新输入该换一轮");

    // SDK 子代理：两项都写，但**不换轮**（跟父会话共用同一个 prompt id）。
    let before = link(&body(r#"[{"name":"Bash"}]"#, "tool turn", "")).unwrap();
    let sub =
        link(&body(r#"[{"name":"Bash"}]"#, "brand new user message", " cc_is_subagent=true;"))
            .expect("子代理要补");
    assert!(sub.diagnostics, "官方 00020 带 diagnostics");
    assert_eq!(sub.prompt_id, before.prompt_id, "子代理沿用父会话那一轮，不换");

    // 无工具 helper：写 cc_prompt_id，**不写** diagnostics（官方 00024 没有）。
    let helper = link(&body("[]", "anything", " cc_is_subagent=true;")).expect("helper 要补");
    assert!(helper.prompt_id.is_some(), "官方 00024 带 cc_prompt_id");
    assert!(!helper.diagnostics, "官方 00024 没有 diagnostics");
}

/// 官方**本来就非流式**的那两类不能被 `nonstream_as_sse` 改成流式，额度探测也不能被
/// 补上 `system`。
///
/// 抓包：安全分类（`cap/2.1.260/00019`、`00030`）与额度探测（`2.1.260-2/00004`）整条都
/// 没有 `stream` 字段；额度探测连 `system` 都没有（四个顶层键 `model → max_tokens →
/// messages → metadata`）。那个开关的本意是「官方恒为流式，非流式一看就不是 CC」，
/// 对这两类恰好相反。
#[test]
fn official_nonstream_aux_requests_are_left_alone() {
    use crate::proxy::CcRequestKind as K;
    let beta = |s: &str| -> Vec<String> {
        s.split(',').filter(|p| !p.is_empty()).map(str::to_string).collect()
    };

    // 额度探测：没有 system、max_tokens:1。
    let probe: serde_json::Value =
        serde_json::from_slice(&crate::proxy::probe_body("claude-haiku-4-5-20251001")).unwrap();
    let kind = K::of(&probe, &beta("oauth-2025-04-20,interleaved-thinking-2025-05-14"));
    assert_eq!(kind, K::QuotaProbe, "{probe}");
    assert!(kind.keeps_nonstream(), "官方那条没有 stream");
    assert!(!kind.allows_system_prefix(), "官方那条没有 system，别补");
    assert!(!kind.on_session_chain(), "也没有 billing header，不进链");

    // 安全分类：非流式，但有 system。
    let classifier = serde_json::json!({
            "model": "claude-sonnet-5",
            "max_tokens": 64,
            "system": [{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.260.3de; cc_entrypoint=cli;"}],
            "messages": [{"role":"user","content":"check"}],
            "stop_sequences": ["</severity>"],
            "thinking": {"type":"disabled"}});
    let kind = K::of(&classifier, &beta("claude-code-20250219,auto-mode-classifier-2026-07-16"));
    assert_eq!(kind, K::Classifier);
    assert!(kind.keeps_nonstream(), "官方那条非流式");
    assert!(kind.allows_system_prefix(), "它有 system，缺 billing 时照补");
    assert!(!kind.on_session_chain(), "三项都不写");

    // 标题生成是**流式**的，不能跟着一起豁免。
    let title = serde_json::json!({
            "model": "claude-haiku-4-5-20251001",
            "max_tokens": 32000,
            "stream": true,
            "system": [{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.260.ced; cc_entrypoint=cli;"}],
            "messages": [{"role":"user","content":"name it"}],
            "tools": []});
    let kind = K::of(&title, &beta("structured-outputs-2025-12-15,advisor-tool-2026-03-01"));
    assert_eq!(kind, K::Title);
    assert!(!kind.keeps_nonstream(), "官方标题生成是 stream:true");

    // 主线程照旧要被改成流式。
    let main = serde_json::json!({
            "model": "claude-opus-5",
            "max_tokens": 64000,
            "system": [{"type":"text","text":"x-anthropic-billing-header: cc_version=2.1.260.222; cc_entrypoint=cli;"}],
            "messages": [{"role":"user","content":"hi"}],
            "tools": [{"name":"Bash"}]});
    let kind = K::of(&main, &beta("claude-code-20250219,effort-2025-11-24"));
    assert_eq!(kind, K::Main);
    assert!(!kind.keeps_nonstream());
    assert!(kind.allows_system_prefix());
}

/// 额度探测的判据必须**窄**：只看「没有 system + max_tokens:1」的话，任何客户端的
/// 一 token 探活都会被认成它，跟着被免掉流式化、免掉 `system` 前缀——而它需要那个前缀
/// 才能用上订阅额度。宁可漏认（退回 helper 走通用路径），也不能错认。
#[test]
fn quota_probe_detection_is_narrow() {
    use crate::proxy::CcRequestKind as K;
    let no_beta: Vec<String> = Vec::new();
    // 官方那条（`cap/2.1.258/00004` 与 `cap/2.1.260-2/00004` 逐字节相同）。
    let official: serde_json::Value =
        serde_json::from_slice(&crate::proxy::probe_body("claude-haiku-4-5-20251001")).unwrap();
    assert_eq!(K::of(&official, &no_beta), K::QuotaProbe, "{official}");

    // 每一处偏离都不该再算额度探测。
    let variants: &[(&str, serde_json::Value)] = &[
        (
            "模型不是 haiku",
            serde_json::json!({
                "model": "claude-opus-5", "max_tokens": 1,
                "messages": [{"role":"user","content":"quota"}]}),
        ),
        (
            "正文不是 quota",
            serde_json::json!({
                "model": "claude-haiku-4-5-20251001", "max_tokens": 1,
                "messages": [{"role":"user","content":"ping"}]}),
        ),
        (
            "不止一条消息",
            serde_json::json!({
                "model": "claude-haiku-4-5-20251001", "max_tokens": 1,
                "messages": [{"role":"user","content":"quota"},
                             {"role":"assistant","content":"ok"}]}),
        ),
        (
            "带工具",
            serde_json::json!({
                "model": "claude-haiku-4-5-20251001", "max_tokens": 1, "tools": [],
                "messages": [{"role":"user","content":"quota"}]}),
        ),
        (
            "带 system",
            serde_json::json!({
                "model": "claude-haiku-4-5-20251001", "max_tokens": 1, "system": "s",
                "messages": [{"role":"user","content":"quota"}]}),
        ),
        (
            "max_tokens 不是 1",
            serde_json::json!({
                "model": "claude-haiku-4-5-20251001", "max_tokens": 2,
                "messages": [{"role":"user","content":"quota"}]}),
        ),
    ];
    for (why, v) in variants {
        assert_ne!(K::of(v, &no_beta), K::QuotaProbe, "{why}: {v}");
    }

    // 单个 text 块的写法也算（官方发的是裸字符串，但这是等价形态）。
    let block = serde_json::json!({
            "model": "claude-haiku-4-5-20251001", "max_tokens": 1,
            "messages": [{"role":"user","content":[{"type":"text","text":"quota"}]}]});
    assert_eq!(K::of(&block, &no_beta), K::QuotaProbe);
}

/// 会话链的判据**独立于**「要不要补主线程 beta」——两者在六个 profile 上的分界线
/// 不在同一处，SDK 子代理正好落在两栏相反的那一格：它的 beta 不该被补，但它确实带着
/// `cc_prompt_id` 与 `diagnostics`（`cap/2.1.260/00020`）。混用一个判据它就被整个跳过。
///
/// 只有标题生成与安全分类不补——官方这两类的 billing header 里就没有 `cc_prompt_id`。
#[test]
fn api_key_cc_session_chain_skips_only_title_and_classifier() {
    const SID: &str = "d0c1fb05-9b19-4576-9465-e2b8a206dabf";
    let make = |tools: &str, beta: &'static str| {
        let body = Bytes::from(format!(
            concat!(
                r#"{{"model":"claude-haiku-4-5-20251001","max_tokens":32000,"#,
                r#""messages":[{{"role":"user","content":"hi"}}],"#,
                r#""system":[{{"type":"text","text":"x-anthropic-billing-header: "#,
                r#"cc_version=2.1.260.ced; cc_entrypoint=cli;"}}],"#,
                r#""tools":{},"#,
                r#""metadata":{{"user_id":"{{\"session_id\":\"{}\"}}"}}}}"#
            ),
            tools, SID
        ));
        let parsed_body = parsed(&body);
        let mut h = crate::proxy::HeaderMap::new();
        h.insert("x-claude-code-session-id", HeaderValue::from_static(SID));
        h.insert("anthropic-beta", HeaderValue::from_static(beta));
        crate::proxy::client_session_link(
            parsed_body.as_ref(),
            &h,
            None,
            all_on(),
            true,
            &test_cred(),
        )
        .is_some()
    };
    // 标题生成：有 structured-outputs → 不补。
    assert!(!make("[]", "structured-outputs-2025-12-15,advisor-tool-2026-03-01"));
    // 安全分类：有 auto-mode-classifier → 不补。
    assert!(!make("[]", "claude-code-20250219,auto-mode-classifier-2026-07-16"));
    // **SDK 子代理要补**：它的 beta 集合不该被 `merge_beta` 补主线程那几项，但它自己
    // 带着 `cc_prompt_id` 与 `diagnostics`。两个判据必须分开。
    assert!(make(
        r#"[{"name":"Bash"}]"#,
        "claude-code-20250219,thinking-display-updates-2026-08-18"
    ));
    // 主线程：补。
    assert!(make(r#"[{"name":"Bash"}]"#, "claude-code-20250219,effort-2025-11-24"));

    // 两个判据确实分家了：同一串 beta，一个说「别补主线程项」，一个说「要补会话链」。
    let sdk: Vec<String> = "claude-code-20250219,thinking-display-updates-2026-08-18"
        .split(',')
        .map(str::to_string)
        .collect();
    assert!(crate::proxy::is_official_non_main_beta(&sdk), "beta 不该被补主线程项");
}

/// **各类请求写哪几项会话链字段，与官方抓包逐条对**（`cap/2.1.285`、
/// `cap/auto-2.1.285-20260930` 全部带 billing header 的 `/v1/messages`）：`cc_prompt_id` 写不写、
/// `diagnostics` 写不写两边必须一致；`cc_prev_req` 官方写了的，这一类就得是要写的（会话第一条
/// 官方也不写，反过来不能要求）。分错一类——WebSearch 子调用当主线程、`/compact` 当主线程——
/// 就会在这里多出或少掉一项。抓包目录不在就跳过。
#[test]
fn link_fields_follow_official_captures() {
    use super::CcRequestKind;
    let mut seen = 0;
    let mut bad = Vec::new();
    for sub in ["2.1.285", "auto-2.1.285-20260930"] {
        let dir = format!("{}/cap/{sub}", env!("CARGO_MANIFEST_DIR"));
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        let mut files: Vec<_> = entries
            .filter_map(|e| Some(e.ok()?.path()))
            .filter(|p| p.to_str().is_some_and(|f| f.ends_with(".req.raw")))
            .collect();
        files.sort();
        for path in files {
            let raw = std::fs::read(&path).unwrap();
            let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
            let head = std::str::from_utf8(&raw[..sep]).unwrap();
            if !head.lines().next().unwrap().contains("/v1/messages") {
                continue;
            }
            let v: serde_json::Value = serde_json::from_slice(&raw[sep + 4..]).unwrap();
            let Some(billing) = v
                .get("system")
                .and_then(|s| s.as_array())
                .and_then(|s| s.first())
                .and_then(|b| b.get("text"))
                .and_then(|t| t.as_str())
                .filter(|t| t.starts_with("x-anthropic-billing-header:"))
            else {
                continue;
            };
            seen += 1;
            let beta: Vec<String> = head
                .lines()
                .find_map(|l| l.strip_prefix("anthropic-beta: "))
                .unwrap_or_default()
                .split(',')
                .map(str::to_string)
                .collect();
            let kind = CcRequestKind::of(&v, &beta);
            let name = format!("{sub}/{}", &path.file_name().unwrap().to_str().unwrap()[..5]);
            let has_prompt = billing.contains("cc_prompt_id=");
            let has_prev = billing.contains("cc_prev_req=");
            let has_diag = v.get("diagnostics").is_some();
            // 按抓包顺序过一遍会话链：本地命令起头的那一轮整轮不写，只看分类判不出来。
            let sid = head
                .lines()
                .find_map(|l| l.strip_prefix("X-Claude-Code-Session-Id: "))
                .unwrap_or_default()
                .to_string();
            let key = super::CcSessionKey { cred_id: -4285, session_id: &sid };
            let header_pid = head
                .lines()
                .find_map(|l| l.strip_prefix("x-claude-code-prompt-id: "))
                .map(str::trim);
            let link = super::CcSessionLink::load_turn(
                key,
                kind.rotates_prompt() && crate::telemetry::last_is_new_prompt_body(&v),
                super::starts_local_turn(&v),
                kind.wants_prompt_id(),
                header_pid,
            );
            if link.prompt_id.is_some() != has_prompt {
                bad.push(format!("{name} {kind:?}: cc_prompt_id 官方 {has_prompt}"));
            }
            // 来访头上报了 prompt id 的，补出来的 `cc_prompt_id` 与它逐字相同——官方两处本来就同值。
            let billed = billing
                .split(';')
                .find_map(|f| f.trim().strip_prefix("cc_prompt_id="))
                .map(str::trim);
            if header_pid.is_some() && billed.is_some() && link.prompt_id.as_deref() != billed {
                bad.push(format!("{name} {kind:?}: cc_prompt_id 与 x-claude-code-prompt-id 不同"));
            }
            if has_prev && !kind.wants_prev_req() {
                bad.push(format!("{name} {kind:?}: 官方写了 cc_prev_req"));
            }
            if kind.wants_diagnostics(Some((2, 1, 285))) != has_diag {
                bad.push(format!("{name} {kind:?}: diagnostics 官方 {has_diag}"));
            }
        }
    }
    if seen == 0 {
        eprintln!("skipped: captures not present");
        return;
    }
    assert!(bad.is_empty(), "{} 条：\n{}", bad.len(), bad.join("\n"));
}

/// 侧查询判定（[`super::CcRequestKind::is_side_query`]）与官方抓包逐条对：标题、起名、安全
/// 分类、预热、额度探测、WebFetch 页面处理（主线程与子代理发起的）、WebSearch 子调用全算；
/// 主线程、子代理（含摘要）、猜下一句、分叉一条都不算。抓包目录不在就跳过。
#[test]
fn side_queries_match_official_captures() {
    use super::CcRequestKind as K;
    let root = concat!(env!("CARGO_MANIFEST_DIR"), "/cap");
    let Ok(dirs) = std::fs::read_dir(root) else {
        eprintln!("skipped: captures not present");
        return;
    };
    let mut side = 0;
    let mut bad = Vec::new();
    for d in dirs.flatten() {
        for f in std::fs::read_dir(d.path()).into_iter().flatten().flatten() {
            let path = f.path();
            if !path.to_str().is_some_and(|p| p.ends_with(".req.raw")) {
                continue;
            }
            let raw = std::fs::read(&path).unwrap();
            let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
            let head = std::str::from_utf8(&raw[..sep]).unwrap();
            // count_tokens 按路径认，不走体判据。
            if !head.lines().next().unwrap().starts_with("POST /v1/messages?")
                && !head.lines().next().unwrap().starts_with("POST /v1/messages ")
            {
                continue;
            }
            let Ok(v) = serde_json::from_slice::<serde_json::Value>(&raw[sep + 4..]) else {
                continue;
            };
            let beta: Vec<String> = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.trim().eq_ignore_ascii_case("anthropic-beta").then(|| v.trim().to_string())
                })
                .unwrap_or_default()
                .split(',')
                .map(str::to_string)
                .collect();
            let kind = K::of(&v, &beta);
            let got = kind.is_side_query(&v);
            let want = matches!(
                kind,
                K::Title | K::Classifier | K::Prewarm | K::QuotaProbe | K::Helper | K::Auxiliary
            );
            side += usize::from(got);
            if got != want {
                bad.push(format!("{} {kind:?}: 侧查询 {got}", path.display()));
            }
        }
    }
    assert!(bad.is_empty(), "{} 条：\n{}", bad.len(), bad.join("\n"));
    assert!(side >= 40, "抓包里的侧查询少了：{side}");
}

/// 第三方的普通多轮对话不能被当成侧查询：开着结构化输出的多轮对话（[`super::CcRequestKind::of`]
/// 判成标题）、不带工具的多轮对话（判成辅助调用）、首轮只有一条消息的普通提问，都要照常占会话。
#[test]
fn ordinary_conversations_are_not_side_queries() {
    use super::CcRequestKind as K;
    let structured = vec![config::CC_BETA_STRUCTURED_OUTPUTS.to_string()];
    let turns = |n: usize| -> Vec<serde_json::Value> {
        (0..n)
            .map(|i| {
                let role = if i % 2 == 0 { "user" } else { "assistant" };
                serde_json::json!({"role": role, "content": format!("turn {i}")})
            })
            .collect()
    };
    for n in [1, 3, 5] {
        let v = serde_json::json!({
            "model": "claude-sonnet-5",
            "system": "You are a helpful assistant.",
            "messages": turns(n),
            "max_tokens": 1024
        });
        let kind = K::of(&v, &structured);
        assert_eq!(kind, K::Title, "前提：结构化输出 beta 被判成标题");
        assert!(!kind.is_side_query(&v), "结构化输出对话 {n} 条消息");
        let kind = K::of(&v, &[]);
        assert_eq!(kind, K::Auxiliary, "前提：无工具被判成辅助调用");
        assert!(!kind.is_side_query(&v), "无工具对话 {n} 条消息");
    }
    // 真的标题生成但带了历史（不是官方形态）：也不算。
    let v = serde_json::json!({
        "model": "claude-haiku-4-5",
        "system": "You are naming a coding session so the user can pick it out",
        "messages": turns(3),
        "max_tokens": 32000
    });
    assert!(!K::of(&v, &structured).is_side_query(&v));
    // 官方那条：一条消息 + 标题提示词。
    let v = serde_json::json!({
        "model": "claude-haiku-4-5",
        "system": "You are naming a coding session so the user can pick it out",
        "messages": turns(1),
        "max_tokens": 32000
    });
    assert!(K::of(&v, &structured).is_side_query(&v));
}

/// 谱系表的整表清过期隔一阵才做一次（[`super::SESSION_SWEEP_EVERY`]），但被查的那一条过没过期
/// 每次当场判：两次整表清理之间过期的上一轮照样当它不存在；清理时腾出的字节从总量里扣掉。
#[test]
fn prefix_table_treats_an_expired_entry_as_gone_between_sweeps() {
    use super::{CC_SESSION_IDLE, CachePrefix, PrefixKey, PrefixLimits, PrefixTable};
    let t0 = std::time::Instant::now();
    let secs = std::time::Duration::from_secs;
    let mut t = PrefixTable::default();
    let k = |i: i64| PrefixKey {
        cred_id: i,
        session_id: format!("s{i}"),
        kind: crate::proxy::CcRequestKind::Main,
        tools_fp: 1,
        blocks: 1,
    };
    let rec = |t: &mut PrefixTable, i: i64, when| {
        let pf = CachePrefix { tools_fp: 1, system: vec!["x".repeat(10)] };
        let (fp, bytes) = (pf.system_fp(), pf.stored_bytes());
        t.record(k(i), fp, Some(pf.system), bytes, when, PrefixLimits::DEFAULT)
    };
    let one = CachePrefix { tools_fp: 1, system: vec!["x".repeat(10)] }.stored_bytes();

    rec(&mut t, 1, t0);
    // 快到期时另一条谱系触发一次整表清理：k(1) 还没过期，留着。
    rec(&mut t, 2, t0 + CC_SESSION_IDLE - secs(5));
    assert!(t.map.contains_key(&k(1)));
    // 10 秒后（没到下一次整表清理）k(1) 已过期：上一轮当不存在。
    assert!(rec(&mut t, 1, t0 + CC_SESSION_IDLE + secs(5)).is_none(), "过期的上一轮不算");
    assert_eq!(t.text_bytes, 2 * one);
    // 再过很久，下一次整表清理把过期的 k(2) 清掉、字节扣回来；新来的 k(3) 照常入表。
    rec(&mut t, 3, t0 + 2 * CC_SESSION_IDLE + secs(10));
    assert!(!t.map.contains_key(&k(2)));
    assert!(!t.map.contains_key(&k(1)), "k(1) 也在这一次过期了");
    assert_eq!(t.text_bytes, one);
}

/// 会话表的整表清过期隔一阵才做一次（[`super::SESSION_SWEEP_EVERY`]），但
/// [`CcSessionLink::load_turn`] 查的那一条过期了当场就当新会话重来：`first_seen`、prompt 计数从 1
/// 起，不接上一次对话的链。
#[test]
fn an_expired_session_starts_over_between_sweeps() {
    use super::{CC_SESSION_IDLE, CC_SESSIONS, CC_SESSIONS_SWEPT, CcSessionLink};
    let now = std::time::Instant::now();
    // 单调时钟从开机算起；开机不到 3 小时造不出一个过期时刻，跳过。
    let Some(old) = now.checked_sub(CC_SESSION_IDLE + std::time::Duration::from_secs(1)) else {
        return;
    };
    let sid = "expired-between-sweeps";
    assert!(CcSessionLink::load(key(sid), true, true).first_seen);
    assert_eq!(CcSessionLink::load(key(sid), true, true).prompt_index, 2);
    // 这一条过期了，而整表清理刚做过、这次不会再做。
    CC_SESSIONS.lock().get_mut(&(1, sid.to_string())).unwrap().last_seen = old;
    *CC_SESSIONS_SWEPT.lock() = Some(std::time::Instant::now());
    let again = CcSessionLink::load(key(sid), true, true);
    assert!(again.first_seen, "过期的会话当新会话");
    assert_eq!(again.prompt_index, 1, "计数从头来");
}

/// 退回 `create` 时的断档原因（[`super::ThreadMiss`]）：会话里没有状态是 `no_state`；历史中间
/// 某条变了是 `history_changed`，`common` 指到变了的那条；首轮（历史里还没有回复）不算断档。
#[test]
fn thread_miss_names_where_the_history_diverged() {
    use super::{CcSessionKey, CcSessionLink, ReplyFp, ThreadMsg, thread_decision};
    let m = |fp: u64, assistant: bool| ThreadMsg {
        fp,
        assistant,
        tool_use_ids: Vec::new(),
        reply: ReplyFp::default(),
    };
    let sid = crate::proxy::uuid_v4();
    let key = CcSessionKey { cred_id: 44, session_id: &sid };
    let (_, _, _, miss) = thread_decision(key, 7, &[m(1, false)], true, 1);
    assert_eq!(miss, None, "首轮不算断档");
    let turn1 = [m(1, false), m(2, true), m(3, false)];
    let (_, _, _, miss) = thread_decision(key, 7, &turn1, false, 1);
    assert_eq!(miss.map(|x| x.why), Some("no_state"), "会话表里还没有这个会话");

    CcSessionLink::load(key, true, true);
    let (_, pending, _, _) = thread_decision(key, 7, &turn1, false, 1);
    CcSessionLink::record_thread(&pending, "msg_1", Vec::new(), ReplyFp::upstream(), 0);
    let turn2 = [m(1, false), m(2, true), m(30, false), m(4, true), m(5, false)];
    let (_, _, _, miss) = thread_decision(key, 7, &turn2, false, 2);
    let miss = miss.expect("接不上");
    assert_eq!(miss.why, "history_changed");
    assert_eq!((miss.same_root, miss.best_len, miss.common), (1, 3, 2), "第 2 条变了");
}
