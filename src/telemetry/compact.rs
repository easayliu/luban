//! `/compact` 收尾那条 `tengu_reactive_compact_succeeded` 要的上下文统计（官方 `bSe` / `wSe`）与几个
//! token 估算（官方 `Cg` / `qc`）。
//!
//! 官方是在客户端内部的消息表上算的，代理只有压缩那条分叉请求的正文：客户端注入的附件在正文里是
//! `role: "system"` 的字符串消息，内部则是带一块 text 的 user 消息；tool_result 内部一律是块数组。
//! 这里按内部形态折算（`cap/auto-2.1.305-20261021-full` / `auto-2.1.303-20261019-full` 的 A0：人类消息
//! 17945 / 17736 对官方 17749 / 17591，总数差 1% 以内），分到工具名的那几组逐个对得上个位数。

use std::collections::{HashMap, HashSet};

use serde_json::{Value, json};

use super::shape::{js_len, read_dedup_of};

/// `qc`：JS 字符数 / 4 四舍五入。
pub(super) fn qc(s: &str) -> u64 {
    (js_len(s) as f64 / 4.0).round() as u64
}

/// 官方 `Cg` 对一块内容的估算（`u4n`）：text 按正文、tool_use 按「名字 + 入参 JSON」、tool_result 按
/// 内容递归、图片 / 文档固定 2000、thinking 按正文，其余按整块 JSON。
pub(super) fn block_tokens(b: &Value) -> u64 {
    match b {
        Value::String(s) => qc(s),
        Value::Object(_) => match b.get("type").and_then(|t| t.as_str()) {
            Some("text") => b.get("text").and_then(|t| t.as_str()).map_or(0, qc),
            Some("image" | "document") => 2000,
            Some("tool_result") => match b.get("content") {
                Some(Value::String(s)) => qc(s),
                Some(Value::Array(inner)) => inner.iter().map(block_tokens).sum(),
                _ => 0,
            },
            Some("tool_use") => {
                let name = b.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let input = b.get("input").cloned().unwrap_or_else(|| json!({}));
                qc(&format!("{name}{input}"))
            }
            Some("thinking") => b.get("thinking").and_then(|t| t.as_str()).map_or(0, qc),
            Some("redacted_thinking") => b.get("data").and_then(|t| t.as_str()).map_or(0, qc),
            _ => qc(&b.to_string()),
        },
        _ => 0,
    }
}

/// 官方 `Cg`：一串消息的估算。
pub(super) fn messages_tokens<'a>(msgs: impl IntoIterator<Item = &'a Value>) -> u64 {
    msgs.into_iter()
        .map(|m| match m.get("content") {
            Some(Value::String(s)) => qc(s),
            Some(Value::Array(blocks)) => blocks.iter().map(block_tokens).sum(),
            _ => 0,
        })
        .sum()
}

/// 压缩那条请求正文的上下文统计。
#[derive(Clone, Debug, Default, PartialEq)]
pub(super) struct CompactStats {
    pub(super) total: u64,
    pub(super) human: u64,
    pub(super) assistant: u64,
    pub(super) local: u64,
    pub(super) other: u64,
    /// 按工具名的调用块 / 结果块 token，首次出现的先后。
    pub(super) tool_requests: Vec<(String, u64)>,
    pub(super) tool_results: Vec<(String, u64)>,
    /// 同一个文件被 Read 多次：多出来那几次的 token（官方按平均值乘次数减一）与文件数。
    pub(super) duplicate_read_tokens: u64,
    pub(super) duplicate_read_files: usize,
    /// 最后一条 assistant 之后那几条按 `Cg` 估的 token：`preCompactTokens` = 上一条主线程的用量加它。
    pub(super) trailing: u64,
    /// 保留下来那组（最后一条 assistant 起）按 `Cg` 估的 token。
    pub(super) kept: u64,
    /// 保留下来那组的消息数（`preservedMessageCount`，含尾随的 system 角色消息）。
    pub(super) preserved: usize,
    /// 保留那组里的 `<total_tokens>` 提醒条数：压完之后它们还在上下文里，下一次压缩照样数到。
    pub(super) kept_total_tokens: u32,
    /// 保留下来那组里的 thinking 块数。
    pub(super) kept_thinking: usize,
    /// 保留下来那组里 Read 过（结果不是去重那句话）的文件：压缩后恢复时跳过。
    pub(super) kept_reads: Vec<String>,
    /// ToolSearch 加载过几次（`attachment_deferred_tools_record_count` 的估值）。
    pub(super) tool_search_loads: usize,
    /// ToolSearch 结果里加载进来的延迟工具（`tool_reference` 的名字，去重、按先后）：2.1.311 压缩后重宣告延迟工具池时
    /// 报它们的个数（`feature_sad{compact_names_loaded_tools}`，`cap/auto-2.1.311-20261027-full/00214` 三个）。
    pub(super) loaded_deferred: Vec<String>,
}

/// 按名字累加，保留首次出现的先后。`index` 是名字在 `list` 里的下标：名字与 id 都是来访给的，逐个线性查找在
/// 刻意造出几十万个不同名字的体上是平方级，见 [`compact_stats_of`]。
fn bump(list: &mut Vec<(String, u64)>, index: &mut HashMap<String, usize>, name: &str, n: u64) {
    match index.get(name) {
        Some(&i) => list[i].1 += n,
        None => {
            index.insert(name.to_string(), list.len());
            list.push((name.to_string(), n));
        }
    }
}

/// `/compact` 的提示那块（末条用户消息的最后一块）。
fn is_compact_prompt(b: &Value) -> bool {
    b.get("type").and_then(|t| t.as_str()) == Some("text")
        && b.get("text")
            .and_then(|t| t.as_str())
            .is_some_and(|t| t.trim_start().starts_with("CRITICAL: Respond with TEXT ONLY"))
}

/// 正文里的消息折回客户端内部的形态：`role: "system"` 的字符串消息是一块 text 的 user 消息，tool_result
/// 的字符串内容是一块 text。返回的是 (是否 user, 块数组)。
fn internal_blocks(m: &Value) -> (bool, Vec<Value>) {
    let role = m.get("role").and_then(|r| r.as_str()).unwrap_or("user");
    let blocks = match m.get("content") {
        Some(Value::String(s)) => vec![json!({ "type": "text", "text": s })],
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter(|b| !is_compact_prompt(b))
            .map(|b| match (b.get("type").and_then(|t| t.as_str()), b.get("content")) {
                (Some("tool_result"), Some(Value::String(s))) => {
                    let mut b = b.clone();
                    b["content"] = json!([{ "type": "text", "text": s }]);
                    b
                }
                _ => b.clone(),
            })
            .collect(),
        _ => Vec::new(),
    };
    (role != "assistant", blocks)
}

/// system / user 消息里的文本（字符串内容，或各文本块连起来）。
fn message_text(m: &Value) -> String {
    match m.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => {
            blocks.iter().filter_map(|b| b.get("text").and_then(|t| t.as_str())).collect()
        }
        _ => String::new(),
    }
}

/// 统计里的工具名（官方 `B6o`：`skill__` 开头的归 `skill_tool`，其余过 `Mn`——`mcp__` 开头的归 `mcp_tool`，
/// 内置工具改由 MCP 提供时还原成内置名，另有两个旧名映射到 Bash / WebFetch，代理这边见不到这两种）。
/// 键名拼进 `tool_request_<名>_tokens`，自建 MCP 服务的工具原名不能直接出现。
fn stat_tool_name(name: &str) -> &str {
    if name.starts_with("skill__") {
        "skill_tool"
    } else if name.starts_with("mcp__") {
        "mcp_tool"
    } else {
        name
    }
}

/// 压缩请求的正文：压缩提示那块本身不算，其余全算（提示后面追的那条 `<total_tokens>` 官方也算在保留的
/// 那组里：A0 的 `preservedMessageCount` 是 assistant + 工具结果 + 它 = 3）。
pub(super) fn compact_stats_of(msgs: &[Value]) -> CompactStats {
    let mut st = CompactStats::default();
    // 下面几张表都按来访给的 id / 名字 / 路径查，一律走哈希（同一个 id 重复出现时认先出现的那个，同原来的线性查找）。
    let mut names: HashMap<String, String> = HashMap::new();
    let mut read_paths: HashMap<String, String> = HashMap::new();
    // 路径 → (次数, token)；只拿来求和，不看先后。
    let mut reads: HashMap<String, (u64, u64)> = HashMap::new();
    let (mut request_index, mut result_index) = (HashMap::new(), HashMap::new());
    let mut loaded_seen: HashSet<String> = HashSet::new();
    for m in msgs {
        let (user, blocks) = internal_blocks(m);
        for b in &blocks {
            let t = qc(&b.to_string());
            st.total += t;
            match b.get("type").and_then(|t| t.as_str()) {
                Some("text") => {
                    let text = b.get("text").and_then(|x| x.as_str()).unwrap_or("");
                    if user
                        && (text.contains("local-command-stdout")
                            || text.contains("local-command-stderr"))
                    {
                        st.local += t;
                    } else if user {
                        st.human += t;
                    } else {
                        st.assistant += t;
                    }
                }
                Some("tool_use") => {
                    let raw = b.get("name").and_then(|n| n.as_str()).unwrap_or("unknown");
                    let name = stat_tool_name(raw);
                    let id = b.get("id").and_then(|i| i.as_str()).unwrap_or("");
                    bump(&mut st.tool_requests, &mut request_index, name, t);
                    names.entry(id.to_string()).or_insert_with(|| name.to_string());
                    if raw == "ToolSearch" {
                        st.tool_search_loads += 1;
                    }
                    if name == "Read"
                        && let Some(p) =
                            b.get("input").and_then(|i| i.get("file_path")).and_then(|p| p.as_str())
                    {
                        read_paths.entry(id.to_string()).or_insert_with(|| p.to_string());
                    }
                }
                Some("tool_result") => {
                    let id = b.get("tool_use_id").and_then(|i| i.as_str()).unwrap_or("");
                    let name = names.get(id).map_or("unknown", String::as_str);
                    bump(&mut st.tool_results, &mut result_index, name, t);
                    let refs = b.get("content").and_then(|c| c.as_array()).into_iter().flatten();
                    for r in refs {
                        if r.get("type").and_then(|t| t.as_str()) == Some("tool_reference")
                            && let Some(n) = r.get("tool_name").and_then(|n| n.as_str())
                            && loaded_seen.insert(n.to_string())
                        {
                            st.loaded_deferred.push(n.to_string());
                        }
                    }
                    if let Some(p) = read_paths.get(id) {
                        let (c, tok) = reads.entry(p.clone()).or_default();
                        *c += 1;
                        *tok += t;
                    }
                }
                _ => st.other += t,
            }
        }
    }
    for (count, tokens) in reads.values() {
        if *count > 1 {
            st.duplicate_read_files += 1;
            st.duplicate_read_tokens += tokens / count * (count - 1);
        }
    }
    // 保留的那组：最后一条 assistant 起到末尾（官方 `splitKind: round`）。
    let la = msgs.iter().rposition(|m| m.get("role").and_then(|r| r.as_str()) == Some("assistant"));
    if let Some(la) = la {
        let strip = |m: &Value| {
            let mut m = m.clone();
            if let Some(blocks) = m.get_mut("content").and_then(|c| c.as_array_mut()) {
                blocks.retain(|b| !is_compact_prompt(b));
            }
            m
        };
        let kept: Vec<Value> = msgs[la..].iter().map(strip).collect();
        st.kept = messages_tokens(&kept);
        st.trailing = messages_tokens(&kept[1..]);
        st.preserved = kept.len();
        st.kept_total_tokens = kept[1..]
            .iter()
            .filter(|m| {
                m.get("role").and_then(|r| r.as_str()) == Some("system")
                    && message_text(m).trim_start().starts_with("<total_tokens>")
            })
            .count() as u32;
        st.kept_thinking = kept[0].get("content").and_then(|c| c.as_array()).map_or(0, |b| {
            b.iter()
                .filter(|x| {
                    matches!(
                        x.get("type").and_then(|t| t.as_str()),
                        Some("thinking" | "redacted_thinking")
                    )
                })
                .count()
        });
        let mut dedup_ids: HashSet<&str> = HashSet::new();
        for m in &kept[1..] {
            if let Some(blocks) = m.get("content").and_then(|c| c.as_array()) {
                for b in blocks {
                    if b.get("type").and_then(|t| t.as_str()) == Some("tool_result")
                        && b.get("content")
                            .and_then(|c| c.as_str())
                            .is_some_and(|c| read_dedup_of(c).is_some())
                        && let Some(id) = b.get("tool_use_id").and_then(|i| i.as_str())
                    {
                        dedup_ids.insert(id);
                    }
                }
            }
        }
        if let Some(blocks) = kept[0].get("content").and_then(|c| c.as_array()) {
            for b in blocks {
                if b.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                    && b.get("name").and_then(|n| n.as_str()) == Some("Read")
                    && let Some(id) = b.get("id").and_then(|i| i.as_str())
                    && !dedup_ids.contains(id)
                    && let Some(p) =
                        b.get("input").and_then(|i| i.get("file_path")).and_then(|p| p.as_str())
                {
                    st.kept_reads.push(p.to_string());
                }
            }
        }
    }
    st
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimates_follow_the_official_rules() {
        assert_eq!(qc("abcdefg"), 2);
        assert_eq!(block_tokens(&json!({"type":"image","source":{}})), 2000);
        assert_eq!(
            block_tokens(&json!({"type":"tool_use","name":"Bash","input":{"command":"ls"}})),
            5
        );
        assert_eq!(
            block_tokens(
                &json!({"type":"tool_result","tool_use_id":"t","content":[{"type":"text","text":"12345678"}]})
            ),
            2
        );
    }

    /// 工具名照官方归类：`skill__` 开头是 `skill_tool`，`mcp__` 开头是 `mcp_tool`（同类聚合），结果块跟着调用块的
    /// 归类走，ToolSearch 照旧计数。
    #[test]
    fn tool_names_are_bucketed_like_the_official_stats() {
        let msgs = vec![
            json!({"role":"user","content":[{"type":"text","text":"go"}]}),
            json!({"role":"assistant","content":[
                {"type":"tool_use","id":"m1","name":"mcp__my_local_server__lookup_tokens","input":{}},
                {"type":"tool_use","id":"m2","name":"mcp__other__fetch","input":{}},
                {"type":"tool_use","id":"s1","name":"skill__deploy","input":{}},
                {"type":"tool_use","id":"b1","name":"Bash","input":{"command":"ls"}}
            ]}),
            json!({"role":"user","content":[
                {"type":"tool_result","tool_use_id":"m1","content":"a"},
                {"type":"tool_result","tool_use_id":"m2","content":"b"},
                {"type":"tool_result","tool_use_id":"s1","content":"c"},
                {"type":"tool_result","tool_use_id":"b1","content":"d"}
            ]}),
        ];
        let st = compact_stats_of(&msgs);
        let keys = |v: &[(String, u64)]| v.iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
        assert_eq!(keys(&st.tool_requests), ["mcp_tool", "skill_tool", "Bash"]);
        assert_eq!(keys(&st.tool_results), ["mcp_tool", "skill_tool", "Bash"]);
    }

    #[test]
    fn splits_the_last_round_and_classifies_blocks() {
        let msgs = vec![
            json!({"role":"user","content":[{"type":"text","text":"hello there"}]}),
            json!({"role":"system","content":"<total_tokens>1 tokens left</total_tokens>"}),
            json!({"role":"assistant","content":[{"type":"thinking","thinking":"","signature":"s"},{"type":"tool_use","id":"r1","name":"Read","input":{"file_path":"/p/a.md"}}]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"r1","content":"1\tx\n"},{"type":"text","text":"CRITICAL: Respond with TEXT ONLY. …"}]}),
            json!({"role":"system","content":"<total_tokens>0 tokens left</total_tokens>"}),
        ];
        let st = compact_stats_of(&msgs);
        assert_eq!(st.preserved, 3);
        assert_eq!(st.kept_thinking, 1);
        assert_eq!(st.kept_reads, ["/p/a.md"]);
        assert_eq!(st.tool_requests.len(), 1);
        assert_eq!(st.tool_requests[0].0, "Read");
        assert_eq!(st.tool_results.len(), 1);
        assert_eq!(st.trailing, qc("1\tx\n") + qc("<total_tokens>0 tokens left</total_tokens>"));
        assert!(st.human > 0 && st.assistant == 0 && st.other > 0);
        assert_eq!(st.total, st.human + st.other + st.tool_requests[0].1 + st.tool_results[0].1);
    }

    /// 加载过的延迟工具去重、按先后；几十万个各不相同的 id / 名字也是一遍扫完（表都走哈希）。
    #[test]
    fn loaded_tools_dedup_and_scale_linearly() {
        let refs = |names: &[&str]| {
            names.iter().map(|n| json!({"type":"tool_reference","tool_name":n})).collect::<Vec<_>>()
        };
        let msgs = vec![
            json!({"role":"assistant","content":[{"type":"tool_use","id":"t1","name":"ToolSearch","input":{}}]}),
            json!({"role":"user","content":[{"type":"tool_result","tool_use_id":"t1","content":refs(&["B","A","B"])}]}),
        ];
        assert_eq!(compact_stats_of(&msgs).loaded_deferred, ["B", "A"]);

        let n = 200_000;
        let uses: Vec<Value> = (0..n)
            .map(|i| json!({"type":"tool_use","id":format!("u{i}"),"name":format!("tool_{i}"),"input":{}}))
            .collect();
        let results: Vec<Value> = (0..n)
            .map(|i| {
                json!({"type":"tool_result","tool_use_id":format!("u{i}"),
                "content":[{"type":"tool_reference","tool_name":format!("d{i}")}]})
            })
            .collect();
        let msgs = vec![
            json!({"role":"assistant","content":uses}),
            json!({"role":"user","content":results}),
        ];
        let started = std::time::Instant::now();
        let st = compact_stats_of(&msgs);
        assert_eq!((st.loaded_deferred.len(), st.tool_results.len()), (n, n));
        assert!(started.elapsed() < std::time::Duration::from_secs(20), "{:?}", started.elapsed());
    }
}
