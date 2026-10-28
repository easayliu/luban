//! 读写文件那一组：`file_operation`、`file_changed`、`edit_string_lengths`、`file_activity`、文件历史的
//! 跟踪 / 备份 / 快照，权限弹框上的 `unary_event`，以及进 OTel 指标的改动行数与编辑决定。账本与规则见
//! [`crate::telemetry::files`]（`super::super::files`）。

use super::*;
use crate::telemetry::compact::qc;
use crate::telemetry::files::{
    LastOp, MAIN_READER, bash_mentioned_exts, content_hash, language_name, path_hash,
};

/// 计入 `code_edit_tool.decision` 的工具（官方 `drt`）。
pub(super) fn is_edit_tool(name: &str) -> bool {
    matches!(name, "Edit" | "Write" | "NotebookEdit")
}

/// 权限弹框那条 `unary_event` 的 `completion_type` / `language_name`（官方 `Oo`）：Edit 是
/// `str_replace_single`、Write 是 `write_file_single`，语言按文件扩展名；NotebookEdit 按单元格类型；别的
/// 工具（Bash、AskUserQuestion、ExitPlanMode……）是 `tool_use_single` / `none`
/// （`cap/auto-2.1.305-20261021-full` 九组弹框）。
fn unary_kind(name: &str, input: &Value) -> (&'static str, String) {
    let path = input.get("file_path").and_then(|p| p.as_str());
    match (name, path) {
        ("Edit", Some(p)) => ("str_replace_single", language_name(p)),
        ("Write", Some(p)) => ("write_file_single", language_name(p)),
        ("NotebookEdit", _) => {
            let md = input.get("cell_type").and_then(|c| c.as_str()) == Some("markdown");
            ("tool_use_single", if md { "markdown" } else { "python" }.into())
        }
        _ => ("tool_use_single", "none".into()),
    }
}

impl EventBuilder<'_> {
    /// 弹框的 `unary_event`：弹出时 `response`（与 `show_permission_request` 同一刻）、点头 `accept`、
    /// 拒绝 `reject`。
    pub(super) fn push_unary(
        &mut self,
        dt: DateTime<Utc>,
        event: &str,
        (name, input): (&str, &Value),
        message_id: &str,
    ) {
        if !self.env.f.v285 {
            return;
        }
        let (completion, language) = unary_kind(name, input);
        self.push(
            dt,
            "tengu_unary_event",
            json!({
                "event": event,
                "completion_type": completion,
                "language_name": language,
                "message_id": message_id,
                "platform": "darwin"
            }),
        );
    }

    /// 弹框关掉、焦点回到输入框时，文件建议的索引刷新一次：点头或拒绝后 40 ~ 50ms 一条 `git ls-files`，耗时
    /// 13 ~ 22ms（`cap/auto-2.1.305-20261021-full` B1 / B3c / B4 的权限弹框、A0 的 AskUserQuestion 与
    /// ExitPlanMode）。个数同启动那条（代理不知道仓库里有几个文件）；不在 git 仓库里不报。
    pub(super) fn push_file_suggestions_refresh(&mut self, dt: DateTime<Utc>) {
        if !self.env.f.v285 || !self.subst.git {
            return;
        }
        let duration = 13 + (self.env.req_hash + dt.timestamp_subsec_millis()) % 10;
        self.push(
            ms(dt, 45),
            "tengu_file_suggestions_git_ls_files",
            json!({ "file_count": 15, "tracked_count": 15, "untracked_count": 0, "duration_ms": duration }),
        );
    }

    /// 编辑类工具的一次权限决定，进 `code_edit_tool.decision`：自动放行的来源是 `config`，弹框点头是
    /// `user_temporary`，被拒是 `user_reject`。
    pub(super) fn note_edit_decision(
        &mut self,
        (name, input): (&str, &Value),
        decision: &'static str,
        source: &'static str,
    ) {
        if !self.env.f.v285 || !is_edit_tool(name) {
            return;
        }
        // 语言按工具的路径参数的扩展名（官方 `urt` → `Jut(getPath(input))`）：NotebookEdit 是 `notebook_path`，
        // 普通 `.ipynb` 查不到就是 `unknown`。弹框那条 `unary_event` 才按单元格类型报 python / markdown。
        // 取不到路径官方整项不带。
        let language = input
            .get(if name == "NotebookEdit" { "notebook_path" } else { "file_path" })
            .and_then(|p| p.as_str())
            .map(language_name);
        self.decisions.push(EditDecision { decision, source, tool: name.to_string(), language });
    }

    /// 一个读写文件的工具跑完：Read 报带内容哈希的 `file_operation{read}`；Edit / Write 先走文件历史的跟踪
    /// （头一次改这个文件时备份原文件、记一笔），再报改动行数、`file_operation`、Edit 的字符串长度与
    /// `file_activity`，排在 `tool_<名字>` 与 `tool_use_success` 之前（`cap/auto-2.1.305-20261021-full` B1
    /// 的 Edit：备份与跟踪 .771，其余 .777，同刻接 `tool_edit`）。
    pub(super) fn emit_file_ops(&mut self, tu: &ToolUse, t_done: DateTime<Utc>, message_id: &str) {
        if !self.env.f.v285 || tu.is_error {
            return;
        }
        // NotebookEdit 的路径参数是 `notebook_path`，成功后只有一条 `file_activity{notebook_edit}`（官方 `s8s`：
        // 扩展名取 `notebook_path`、`isNewFile` 固定 false），不走下面 Edit / Write 的账本与改动统计。
        if tu.name == "NotebookEdit" {
            if let Some(path) = tu.input.get("notebook_path").and_then(|p| p.as_str()) {
                let mut activity = json!({ "messageID": message_id, "activity": "notebook_edit" });
                let ext = file_ext(path).to_lowercase();
                if !ext.is_empty() {
                    activity["fileExtension"] = json!(ext);
                }
                activity["toolName"] = json!("NotebookEdit");
                activity["isNewFile"] = json!(false);
                self.push(t_done, "tengu_file_activity", activity);
            }
            return;
        }
        let Some(path) = tu.input.get("file_path").and_then(|p| p.as_str()).map(str::to_string)
        else {
            return;
        };
        let ext = file_ext(&path).to_lowercase();
        match tu.name.as_str() {
            "Read" => {
                // 重读账本里已有的文件：先报一条 `file_read_reread{priorOp}`（上一次是读、改还是种进来的）。
                // 文件没变、又是同一段时客户端直接回去重那句话、不读文件：只再报 `file_read_dedup{ext}`
                // （种进来的多一项 `source: "seeded"`，ext 是小写扩展名，超过 10 个字符是 `other`，没有就不带），
                // 不报 `file_operation`，账本也不动（官方 `ePr`）。
                let reader = self.reader();
                let prior = match tu.read_dedup {
                    Some(dedup) => {
                        let prior = match dedup {
                            ReadDedup::Seeded => LastOp::Seeded,
                            ReadDedup::Plain => {
                                self.files.prior_op(&reader, &path).unwrap_or_default()
                            }
                        };
                        let mut d = Map::new();
                        if dedup == ReadDedup::Seeded {
                            d.insert("source".into(), json!("seeded"));
                        }
                        if !ext.is_empty() {
                            d.insert(
                                "ext".into(),
                                json!(if ext.len() > 10 { "other" } else { &ext }),
                            );
                        }
                        self.push(
                            ms(t_done, -3),
                            "tengu_file_read_reread",
                            json!({ "priorOp": prior.prior_op() }),
                        );
                        self.push(ms(t_done, -2), "tengu_file_read_dedup", Value::Object(d));
                        return;
                    }
                    None => {
                        let partial =
                            tu.input.get("offset").is_some() || tu.input.get("limit").is_some();
                        self.files.note_read(
                            &reader,
                            &path,
                            tu.read_content.clone(),
                            tu.stripped_bytes,
                            partial,
                            tu.image_b64 > 0,
                        )
                    }
                };
                if let Some(prior) = prior {
                    self.push(
                        ms(t_done, -3),
                        "tengu_file_read_reread",
                        json!({ "priorOp": prior.prior_op() }),
                    );
                }
                let mut op = json!({
                    "operation": "read",
                    "tool": "FileReadTool",
                    "filePathHash": path_hash(&path)
                });
                // 读图片时哈希的是那串 base64。PDF 按 `pages` 分页读的哈希的是固定串 `PDF pages <pages>`
                // （官方 `OE({…, content: `PDF pages ${w}`})`，`w` 是原样的入参），不是渲染出来的图。
                let pdf_pages = path
                    .rsplit_once('.')
                    .is_some_and(|(_, ext)| ext.eq_ignore_ascii_case("pdf"))
                    .then(|| tu.input.get("pages").and_then(|p| p.as_str()))
                    .flatten();
                let hash = match (pdf_pages, tu.read_content.as_deref()) {
                    (Some(pages), _) => content_hash(&format!("PDF pages {pages}")),
                    (None, Some(c)) => content_hash(c),
                    (None, None) => tu.image_hash.clone(),
                };
                if let Some(h) = hash {
                    op["contentHash"] = json!(h);
                }
                self.push(ms(t_done, -2), "tengu_file_operation", op);
            }
            "Edit" => {
                let old = tu.input.get("old_string").and_then(|s| s.as_str()).unwrap_or("");
                let new = tu.input.get("new_string").and_then(|s| s.as_str()).unwrap_or("");
                let replace_all =
                    tu.input.get("replace_all").and_then(|b| b.as_bool()).unwrap_or(false);
                self.track_file(&path, true, ms(t_done, -6));
                let reader = self.reader();
                let (added, removed) = self.files.apply_edit(&reader, &path, old, new, replace_all);
                self.loc.push((added, removed));
                self.push(
                    t_done,
                    "tengu_file_changed",
                    json!({ "lines_added": added, "lines_removed": removed }),
                );
                self.push(
                    t_done,
                    "tengu_file_operation",
                    json!({ "operation": "edit", "tool": "FileEditTool", "filePathHash": path_hash(&path) }),
                );
                self.push(
                    t_done,
                    "tengu_edit_string_lengths",
                    json!({ "oldStringBytes": old.len(), "newStringBytes": new.len(), "replaceAll": replace_all }),
                );
                let mut activity = json!({ "messageID": message_id, "activity": "edit" });
                if !ext.is_empty() {
                    activity["fileExtension"] = json!(ext);
                }
                activity["toolName"] = json!("Edit");
                activity["isNewFile"] = json!(false);
                self.push(t_done, "tengu_file_activity", activity);
            }
            "Write" => {
                let content = tu.input.get("content").and_then(|s| s.as_str()).unwrap_or("");
                let created = tu.result_head.starts_with("File created successfully");
                self.track_file(&path, !created, ms(t_done, -4));
                let reader = self.reader();
                let (added, removed) = self.files.apply_write(&reader, &path, content, created);
                self.loc.push((added, removed));
                // 写的是 CLAUDE.md 另报一条（`/init`，`cap/auto-2.1.305-20261021-full` B4）。
                if path.ends_with("/CLAUDE.md") {
                    self.push(t_done, "tengu_write_claudemd", json!({}));
                }
                self.push(
                    t_done,
                    "tengu_file_changed",
                    json!({ "lines_added": added, "lines_removed": removed }),
                );
                self.push(
                    t_done,
                    "tengu_file_operation",
                    json!({
                        "operation": "write",
                        "tool": "FileWriteTool",
                        "filePathHash": path_hash(&path),
                        "type": if created { "create" } else { "update" }
                    }),
                );
                let mut activity = json!({ "messageID": message_id, "activity": "write" });
                if !ext.is_empty() {
                    activity["fileExtension"] = json!(ext);
                }
                activity["toolName"] = json!("Write");
                self.push(t_done, "tengu_file_activity", activity);
            }
            _ => {}
        }
    }

    /// Bash 命令里提到的文件（官方 `s8s` 的 Bash 分支）：每种扩展名一条 `file_activity{bash_mention}`，
    /// 只在成功时报，排在 `bash_tool_command_executed` 之后、`tool_bash` 之前（`cap/auto-2.1.306-20261021-full/00664`）。
    pub(super) fn emit_bash_mentions(
        &mut self,
        command: &str,
        t_done: DateTime<Utc>,
        message_id: &str,
    ) {
        if !self.env.f.v285 {
            return;
        }
        for ext in bash_mentioned_exts(command) {
            self.push(
                t_done,
                "tengu_file_activity",
                json!({ "messageID": message_id, "activity": "bash_mention", "fileExtension": ext, "toolName": "Bash" }),
            );
        }
    }

    /// 这条调用属于哪条线的 `readFileState`：子代理是它的 `agent-id`，其余是主线程。
    fn reader(&self) -> String {
        self.env.call.agent.agent_id.clone().unwrap_or_else(|| MAIN_READER.to_string())
    }

    /// 文件历史的跟踪：头一次改这个文件时，原本存在就先备份（版本 1、改之前的大小），再记一笔。
    fn track_file(&mut self, path: &str, exists: bool, dt: DateTime<Utc>) {
        let Some(t) = self.files.track(path, exists) else { return };
        if let Some(size) = t.backup_size {
            self.push(
                dt,
                "tengu_file_history_backup_file_created",
                json!({ "version": t.version, "fileSize": size }),
            );
        }
        self.push(
            dt,
            "tengu_file_history_track_edit_success",
            json!({ "isNewFile": t.backup_size.is_none(), "version": t.version }),
        );
    }

    /// `/compact` 收尾：2.1.305 起先报摘要回包的形态（`compact_summary_reply`，`turn_end` 后 3ms），再把最近读写
    /// 过的文件（至多 5 个，越近越前；记忆文件、计划文件、图片与保留那组里刚读过的不算，见
    /// [`Files::restore_candidates`]）重新读回——每个一条读取上限覆盖，再各一条带内容哈希的
    /// `file_operation{read}` 与 `post_compact_file_restore_success`。2.1.305 是读与恢复成对交错，2.1.296 ~
    /// 2.1.303 是三种各自成串（`cap/auto-2.1.305-20261021-full` A0 .367 ~ .371；`auto-2.1.303-20261019-full` 同场景）。
    /// 恢复完再把延迟工具池与 MCP 说明池按保留下来的那几条消息重宣告一遍（A0 .373 / .378，303 .749 / .751）。
    /// 返回恢复了几个文件（`restoredAttachmentCount` 用）。
    pub(super) fn emit_compact_restore(&mut self, t2: DateTime<Utc>) -> usize {
        let env = self.env;
        if !env.f.v285 {
            return 0;
        }
        if env.f.v305 {
            let mut reply = json!({
                "path": "reactive",
                "summaryBlock": "closed",
                "unansweredRequestTail": "none",
                "outputTokens": env.call.output_tokens,
                "stopReason": env.call.stop_reason.as_deref().unwrap_or("end_turn")
            });
            // 2.1.311 起末尾多第几次要摘要（回包里没有 `<summary>` 时再要一次，`cap/auto-2.1.311-20261027-full/00224`
            // 一次就成，报 1）。
            if env.f.v311 {
                reply["attempt"] = json!(1);
            }
            self.push(ms(t2, 3), "tengu_compact_summary_reply", reply);
        }
        let kept_reads =
            env.shape.compact_stats.as_ref().map(|s| s.kept_reads.clone()).unwrap_or_default();
        let files = self.files.restore_candidates(MAIN_READER, 5, &kept_reads);
        let tl = ms(t2, 8);
        for _ in &files {
            self.push(
                tl,
                "tengu_file_read_limits_override",
                json!({ "hasMaxTokens": true, "hasMaxSizeBytes": false }),
            );
        }
        let op = |path: &str, content: &Option<String>| {
            let mut op = json!({ "operation": "read", "tool": "FileReadTool", "filePathHash": path_hash(path) });
            if let Some(h) = content.as_deref().and_then(content_hash) {
                op["contentHash"] = json!(h);
            }
            op
        };
        if env.f.v305 {
            for (i, (path, content)) in files.iter().enumerate() {
                let t = ms(t2, 10 + i as i64);
                self.push(t, "tengu_file_operation", op(path, content));
                self.push(t, "tengu_post_compact_file_restore_success", json!({}));
            }
        } else {
            for (path, content) in &files {
                self.push(ms(t2, 10), "tengu_file_operation", op(path, content));
            }
            for _ in &files {
                self.push(ms(t2, 11), "tengu_post_compact_file_restore_success", json!({}));
            }
        }
        // 两个池子的重宣告：meta 照首次输入模板里那两条，`callSite` / `querySource` 换成压缩的，
        // `messagesLength` 是保留下来的消息数（延迟工具池只在正文有延迟工具时有，见 `Subst::deferred`）。
        let preserved = env.shape.compact_stats.as_ref().map_or(0, |s| s.preserved);
        let (t_deferred, t_mcp) =
            if env.f.v305 { (ms(t2, 14), ms(t2, 19)) } else { (ms(t2, 12), ms(t2, 14)) };
        let tpl_meta = |name: &str| {
            env.tpl
                .prompt
                .iter()
                .chain(&env.tpl.first_prompt)
                .find(|e| e.name == name)
                .map(|e| substitute(&e.meta, &self.subst))
        };
        let deferred =
            self.subst.deferred.then(|| tpl_meta("tengu_deferred_tools_pool_change")).flatten();
        let mcp = tpl_meta("tengu_mcp_instructions_pool_change");
        if let Some(mut meta) = deferred {
            meta["messagesLength"] = json!(preserved);
            meta["attachmentCount"] = json!(0);
            meta["callSite"] = json!("reactive_compact");
            meta["querySource"] = json!("unknown");
            self.push(t_deferred, "tengu_deferred_tools_pool_change", meta);
            // 2.1.311：重宣告的延迟工具里有压缩前加载过的，官方想在宣告里点名它们，开关 `tengu_radiant_lemur`
            // 没开（服务端 false），报一条 `feature_sad{flag_off}` 带个数，紧跟在池子那条后面（同一毫秒）。
            let loaded = env.shape.compact_stats.as_ref().map_or(0, |s| s.loaded_deferred.len());
            if env.f.v311 && loaded > 0 {
                let mut sad = json!({
                    "count": loaded,
                    "feature_name": "compact_names_loaded_tools",
                    "error_code": "flag_off"
                });
                sad["tengu_quizzical_giraffe"] = json!("unset");
                self.push_dd(t_deferred, "tengu_feature_sad", sad);
            }
        }
        if let Some(mut meta) = mcp {
            meta["messagesLength"] = json!(preserved);
            meta["attachmentCount"] = json!(0);
            self.push(t_mcp, "tengu_mcp_instructions_pool_change", meta);
        }
        files.len()
    }

    /// `/compact` 成功那条 `tengu_reactive_compact_succeeded`（与 `compact_reactive` 同一毫秒、在逐出提示之前，
    /// `cap/auto-2.1.305-20261021-full` A0 .382；2.1.303 .753）。97 项照官方 `oCr` 的键序：
    /// - 分组与保留：组数照 `reactive_compact_attempt` 那条的估法，保留最后一组（`splitKind: round`），
    ///   `preservedUuidCount` / `preservedMessageCount` 是那组的消息数，`keptThinkingBlockCount` 是里面的
    ///   thinking 块数（都从压缩请求正文数，见 [`CompactStats`]）；`forkAssistantMessageCount` 1、
    ///   `summaryPromptArm: no_scratchpad`、`trigger: manual`（自动压缩与手动在请求上分不出，一律按手动）；
    /// - token：`preCompactTokens` = 上一条主线程的用量总和（官方 `Bf`：最后一条带用量的 assistant 消息）加
    ///   它之后那几条按 `Cg` 估的；`postCompactTokens` = 摘要正文 + 保留那组 + 恢复的文件 + 固定那几种附件
    ///   （估值照 [`fill_attachment_estimates`]）；`compaction*` 是这次分叉调用自己的用量；
    /// - `restoredAttachmentCount`：恢复的文件 + 计划文件引用（有计划文件时）+ 固定附件（延迟工具、子代理
    ///   清单、技能清单、MCP 说明、环境……305 起 6 条、之前 5 条：A0 各恢复 3 个文件、带计划文件，报 10 / 9）；
    ///   `durationMs` / `userWaitMs` 比 fork 统计多 30ms 上下；
    /// - 上下文统计（`total_tokens` 起那一长串）：按类型与工具名分的 token、附件计数、百分比，见
    ///   [`compact_stats_of`]；附件计数照各轮宣告过的累加（[`Session::attachment_counts`]），另加官方内部才有的
    ///   `session_context` / `date` / `credential_org` 各 1、`prompt_snapshot` 2（两份抓包都是）与
    ///   `deferred_tools_record`（代理按主线程 ToolSearch 加载过的次数估，官方还会数内部记录，A0 是 7 对 3）。
    pub(super) fn emit_compact_succeeded(&mut self, t3: DateTime<Utc>, restored_files: usize) {
        let env = self.env;
        let (call, shape) = (env.call, env.shape);
        let TurnFacts { message_tokens, ref attachment_counts, v305, .. } = *env.f;
        if !env.f.v285 {
            return;
        }
        let st = shape.compact_stats.clone().unwrap_or_default();
        let total_in = call.input_tokens + call.cache_read_tokens + call.cache_creation_tokens;
        let hit_rate =
            if total_in > 0 { call.cache_read_tokens as f64 / total_in as f64 } else { 0.0 };
        let groups = shape.assistant_messages + 2;
        let pre = message_tokens.max(0) as u64 + st.trailing;
        // 摘要在压缩后那条 user 消息里套着固定的开头与结尾（约 300 字）。
        let summary = qc(&"x".repeat(call.text_chars + 300));
        let restored: u64 = self
            .files
            .restore_candidates(MAIN_READER, 5, &st.kept_reads)
            .iter()
            .map(|(p, c)| {
                // 恢复的文件以「Called the Read tool with the following input: …」加 `cat -n` 正文挂进去。
                let body = c.as_deref().map_or(0, |c| js_len(c) + c.lines().count() * 7);
                qc(&"x".repeat(js_len(p) + body + 110))
            })
            .sum();
        // 压缩时放回的固定附件（官方 `pve`），恢复条数、`postCompactTokens` 的估算与压完之后的附件账本都按这一份：
        // `cap/auto-2.1.305-20261021-full/00173` 压缩后首条请求里依次是延迟工具 / 子代理 / 技能清单、MCP 说明、
        // 环境与模型。没有延迟工具（关了工具搜索）就没有延迟工具清单；技能清单 305 起才放回
        // （`cap/auto-2.1.303-20261019-full/00175` 没有这一段，所以 303 是 5 条、305 是 6 条）。
        let restored_kinds: Vec<&str> = [
            ("deferred_tools_delta", self.subst.deferred),
            ("agent_listing_delta", true),
            ("skill_listing", v305),
            ("mcp_instructions_delta", true),
            ("environment", true),
            ("model", true),
        ]
        .into_iter()
        .filter_map(|(k, on)| on.then_some(k))
        .collect();
        let mut fixed = json!({ "attachment_types": restored_kinds });
        fill_attachment_estimates(&mut fixed, "unknown", shape.sdk, &env.f.display_model);
        let fixed: u64 = fixed["attachment_token_estimates"]
            .as_array()
            .map(|a| a.iter().filter_map(|x| x.as_str()?.parse::<u64>().ok()).sum())
            .unwrap_or(0);
        let post = summary + st.kept + restored + fixed;
        let restored_count =
            restored_files + usize::from(self.files.has_plan_file()) + restored_kinds.len();
        // 压完之后消息表里还在的附件（下一次压缩照这个数）：保留那组带的 `<total_tokens>` 提醒，加上这次放回去
        // 的恢复文件、计划文件引用与上面那几种固定附件。
        let mut after: Vec<(String, u32)> = Vec::new();
        let mut keep = |k: &str, n: u32| {
            if n > 0 {
                after.push((k.to_string(), n));
            }
        };
        keep("total_tokens_reminder", st.kept_total_tokens);
        keep("file", restored_files as u32);
        keep("plan_file_reference", u32::from(self.files.has_plan_file()));
        for k in &restored_kinds {
            keep(k, 1);
        }
        self.post_compact_attachments = Some(after);
        let mut m = Map::new();
        let mut put = |k: &str, v: Value| {
            m.insert(k.to_string(), v);
        };
        if let Some(effort) = &env.effort {
            put("effort_level", json!(effort));
        }
        put("attempts", json!(1));
        put("groupsPreserved", json!(1));
        put("totalGroups", json!(groups));
        put("splitKind", json!("round"));
        put("headTruncations", json!(0));
        put("preservedUuidCount", json!(st.preserved));
        put("preservedMessageCount", json!(st.preserved));
        put("keptThinkingBlockCount", json!(st.kept_thinking));
        put("keptThinkingStripped", json!(false));
        put("keptThinkingStripDecidedBy", json!("flag"));
        put("forkAssistantMessageCount", json!(1));
        put("summaryPromptArm", json!("no_scratchpad"));
        put("trigger", json!("manual"));
        put("preCompactTokens", json!(pre));
        put("postCompactTokens", json!(post));
        put("restoredAttachmentCount", json!(restored_count));
        let duration = env.total + if v305 { 40 } else { 31 };
        put("durationMs", json!(duration));
        put("userWaitMs", json!(duration));
        put("precomputed", json!(false));
        put("manualPrecomputeReuse", json!("miss_not_ready"));
        put("compactionInputTokens", json!(call.input_tokens));
        put("compactionOutputTokens", json!(call.output_tokens));
        put("compactionCacheReadTokens", json!(call.cache_read_tokens));
        put("compactionCacheCreationTokens", json!(call.cache_creation_tokens));
        put("compactionTotalTokens", json!(total_in + call.output_tokens));
        put("cacheHitRate", js_number(hit_rate));
        put("cacheCold", json!(false));
        // 上下文统计（官方 `wSe`）。
        put("total_tokens", json!(st.total));
        put("human_message_tokens", json!(st.human));
        put("assistant_message_tokens", json!(st.assistant));
        put("local_command_output_tokens", json!(st.local));
        put("other_tokens", json!(st.other));
        let mut attachments: Vec<(String, u32)> = attachment_counts.clone();
        let mut add = |name: &str, n: u32| {
            if n > 0 && !attachments.iter().any(|(k, _)| k == name) {
                attachments.push((name.to_string(), n));
            }
        };
        if env.f.prompt_seq > 0 {
            add("session_context", 1);
            add("date", 1);
            add("credential_org", 1);
            add("prompt_snapshot", 2);
        }
        add("deferred_tools_record", st.tool_search_loads as u32);
        for (name, n) in &attachments {
            put(&format!("attachment_{name}_count"), json!(n));
        }
        for (name, n) in &st.tool_requests {
            put(&format!("tool_request_{name}_tokens"), json!(n));
        }
        for (name, n) in &st.tool_results {
            put(&format!("tool_result_{name}_tokens"), json!(n));
        }
        put("duplicate_read_tokens", json!(st.duplicate_read_tokens));
        put("duplicate_read_file_count", json!(st.duplicate_read_files));
        if st.total > 0 {
            let pct = |n: u64| json!((n as f64 / st.total as f64 * 100.0).round() as u64);
            put("human_message_percent", pct(st.human));
            put("assistant_message_percent", pct(st.assistant));
            put("local_command_output_percent", pct(st.local));
            put("duplicate_read_percent", pct(st.duplicate_read_tokens));
            put("tool_request_percent", pct(st.tool_requests.iter().map(|(_, n)| n).sum()));
            put("tool_result_percent", pct(st.tool_results.iter().map(|(_, n)| n).sum()));
            for (name, n) in &st.tool_requests {
                put(&format!("tool_request_{name}_percent"), pct(*n));
            }
            for (name, n) in &st.tool_results {
                put(&format!("tool_result_{name}_percent"), pct(*n));
            }
        }
        self.push(t3, "tengu_reactive_compact_succeeded", Value::Object(m));
    }

    /// 用户提交一次（新输入、`/compact`、`/exit`）：已跟踪的文件里上次备份后又改过的各升一版备份，排在
    /// 那条 `snapshot_success` 之前（`cap/auto-2.1.305-20261021-full` A0：.374 / .374、.487 ~ .488）。
    pub(super) fn emit_snapshot_backups(&mut self, dt: DateTime<Utc>) {
        if !self.env.f.v285 {
            return;
        }
        for (version, size) in self.files.snapshot() {
            self.push(
                dt,
                "tengu_file_history_backup_file_created",
                json!({ "version": version, "fileSize": size }),
            );
        }
        // 模板那条 `snapshot_success` 的张数照账本数的。
        self.subst.prompt_index = self.files.snapshots();
    }
}
