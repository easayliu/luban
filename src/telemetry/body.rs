//! 逐请求遥测要从出站请求体里取的几样（[`BodyFacts`]），以及算出来之前替它们拿着原始字节的
//! [`CallBody`]。
//!
//! 遥测在流结束后才处理一条调用（还可能在队列里排一阵、被扣住等重发），而它要的只是体里的少数几样。
//! 此前 [`ApiCall`] 直接拎着整份出站体，几 MB 的体就陪着整条流、再陪着排队一起活着；现在转发路径在
//! 回程开流时就交给阻塞线程池解析一次（[`CallBody::digest_in_background`]），取完即放掉原始字节。
//! 原先消费者那边要解析两三遍（形态、`model`、首条用户消息各一遍），也并成了一遍。

use super::*;

/// 出站请求体里遥测用得到的全部，一次解析取齐。
#[derive(Debug)]
pub(super) struct BodyFacts {
    /// 见 [`parse_shape`]；体不是合法 JSON 或没有 `model` 时为 `None`。各 `tool_uses[].input` 在这里是
    /// `Null`，原样另存成紧凑 JSON 文本（[`Self::tool_inputs`]），用时由 [`Self::shape`] 拼回。
    shape: Option<RequestShape>,
    /// `shape.tool_uses` 各项的 `input`，序列化成紧凑 JSON 文本：这份结果要从回程开流一直留到流结束、
    /// 再排队等处理，展开成 [`Value`] 的工具入参（Write 的正文、大数组）比原文大一个数量级，
    /// 文本则与原文同量级。
    tool_inputs: Vec<Box<str>>,
    /// 顶层 `model`（扣住的标题建会话前要它，那时还没处理形态）。
    pub(super) model: Option<String>,
    /// [`crate::proxy::original_games::TEXTS`] 里每一种写法在不在 `system` 里，见
    /// [`Self::has_original_games_text`]。
    original_games: [bool; crate::proxy::original_games::TEXTS.len()],
    /// 2.1.321 的 `# Finishing work` / `# Memory, notes and feedback` 两节在不在 `system` 里，见
    /// [`Self::has_section`]；`None` 是看不出（第四块没组装）。
    finishing_notes: [Option<bool>; 2],
    /// `system` 里有第四块（会话相关那块，认同一块里的 `# Session-specific guidance` 与 `# Environment` 两节）：没有就看不出那几段在不在，见 [`Self::prompt_sent`]。
    prompt_sent: bool,
    /// 首条用户消息的各个文本块（字符串内容算一块），认领派遣（`claim_delegation`）用。
    pub(super) first_user_texts: Vec<String>,
    /// 体的 JS 字符串长度（UTF-16 码元数；不是合法 UTF-8 时退回字节数），`requestBodyChars` 用。
    pub(super) chars: usize,
}

impl BodyFacts {
    fn of(body: &[u8]) -> Self {
        let v = serde_json::from_slice::<Value>(body).ok();
        let mut shape = v.as_ref().and_then(|v| shape_of(v, body));
        // 提示词里的段落只在 `system` 里找：用户消息、工具结果里出现同样的文字（贴进来的日志、读到的文件）不是客户端
        // 组装的提示词，不能算数。
        let system = v.as_ref().map(system_texts).unwrap_or_default();
        // 原创游戏那段、Finishing / Memory 两节都在第四块（会话相关那块）里。第四块没组装（线程续轮的增量体只剩 billing
        // 那一块，或只有身份与基座）时在不在看不出，不能当「缺了」。认第四块看同一块里官方必有的两节（顶格）：
        // `# Session-specific guidance` 与 `# Environment`（五份模板都有），只带其中一个标题的自定义 system 不算。模拟路径
        // 另由 [`ApiCall::sim_no_rest`] 直接告知这次组没组装。
        let heading = |block: &str, h: &str| {
            block.starts_with(&format!("{h}\n")) || block.contains(&format!("\n{h}\n"))
        };
        let prompt_sent = system.iter().any(|block| {
            heading(block, "# Session-specific guidance") && heading(block, "# Environment")
        });
        let tool_inputs = shape
            .iter_mut()
            .flat_map(|s| &mut s.tool_uses)
            .map(|tu| tu.input.take().to_string().into_boxed_str())
            .collect();
        Self {
            shape,
            tool_inputs,
            model: v.as_ref().and_then(model_of),
            original_games: crate::proxy::original_games::TEXTS
                .map(|t| system.iter().any(|block| block.contains(t))),
            finishing_notes: crate::proxy::finishing_notes::SECTIONS.map(|s| {
                // 标题行连同官方正文第一行（[`crate::proxy::finishing_notes::Section::signature`]）顶格、在一块的开头或前面是
                // 空行：正文里顺带提到这个标题的、别处只借了同名标题的都不算。
                let sig = s.signature();
                prompt_sent.then(|| {
                    system.iter().any(|block| {
                        block.starts_with(sig) || block.contains(&format!("\n\n{sig}"))
                    })
                })
            }),
            prompt_sent,
            first_user_texts: v.as_ref().map(first_user_texts).unwrap_or_default(),
            chars: std::str::from_utf8(body).map(js_len).unwrap_or(body.len()),
        }
    }

    /// 请求形态（每次给一份新的，处理时要改它）：把存成文本的工具入参解析回去。`Value` 转文本再解析回来
    /// 逐值不变（保序的对象键序照旧，数字按原样的整数 / 最短往返浮点写出）。
    pub(super) fn shape(&self) -> Option<RequestShape> {
        let mut shape = self.shape.clone()?;
        for (tu, input) in shape.tool_uses.iter_mut().zip(&self.tool_inputs) {
            tu.input = serde_json::from_str(input).unwrap_or(Value::Null);
        }
        Some(shape)
    }

    /// 留着的形态里没有展开的工具入参，入参文本加起来多少字节（测试用）。
    #[cfg(test)]
    pub(super) fn stored_tool_inputs(&self) -> Option<usize> {
        let shape = self.shape.as_ref()?;
        shape
            .tool_uses
            .iter()
            .all(|tu| tu.input.is_null())
            .then(|| self.tool_inputs.iter().map(|s| s.len()).sum())
    }

    /// `section` 那一节在不在 `system` 里（标题行连同官方正文第一行顶格）；`system` 只有 billing 那一块时看不出，`None`。
    pub(super) fn has_section(
        &self,
        section: crate::proxy::finishing_notes::Section,
    ) -> Option<bool> {
        self.finishing_notes[section as usize]
    }

    /// `system` 里组装了第四块：否则那里头的段落在不在都看不出。
    pub(super) fn prompt_sent(&self) -> bool {
        self.prompt_sent
    }

    /// `text`（[`crate::proxy::original_games::text_for`] 给出的某一版正文）在不在 `system` 里。
    pub(super) fn has_original_games_text(&self, text: &str) -> bool {
        let i = crate::proxy::original_games::TEXTS.iter().position(|t| *t == text);
        debug_assert!(i.is_some(), "original_games::TEXTS 漏了 text_for 给出的一种写法");
        i.is_some_and(|i| self.original_games[i])
    }
}

/// 顶层 `system` 的各段正文：字符串算一段，数组取各块的 `text`。
fn system_texts(v: &Value) -> Vec<&str> {
    match v.get("system") {
        Some(Value::String(s)) => vec![s.as_str()],
        Some(Value::Array(blocks)) => {
            blocks.iter().filter_map(|b| b.get("text").and_then(|t| t.as_str())).collect()
        }
        _ => Vec::new(),
    }
}

/// 顶层 `model`：是字符串才算。
fn model_of(v: &Value) -> Option<String> {
    v.get("model")?.as_str().map(str::to_string)
}

/// 请求体里首条用户消息的各个文本块（字符串内容算一块）。
fn first_user_texts(v: &Value) -> Vec<String> {
    let Some(m) = v["messages"].as_array().and_then(|ms| ms.iter().find(|m| m["role"] == "user"))
    else {
        return Vec::new();
    };
    match &m["content"] {
        Value::String(s) => vec![s.clone()],
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(Value::as_str).map(str::to_string))
            .collect(),
        _ => Vec::new(),
    }
}

/// 一条调用实际发往上游的请求体：先拿着原始字节，[`Self::facts`] 头一回算出 [`BodyFacts`] 后只留结果。
///
/// 克隆共享同一份状态（重试换体时整个换掉，见 `ReqLog::note_retry`）。
#[derive(Clone)]
pub struct CallBody {
    state: Arc<parking_lot::Mutex<BodyState>>,
    /// 测试里还要拿原始字节去比对（`parse_shape(&call.body)`），留一份。
    #[cfg(test)]
    raw: Bytes,
}

enum BodyState {
    Raw(Bytes),
    Ready(Arc<BodyFacts>),
}

impl CallBody {
    pub fn new(body: Bytes) -> Self {
        Self {
            #[cfg(test)]
            raw: body.clone(),
            state: Arc::new(parking_lot::Mutex::new(BodyState::Raw(body))),
        }
    }

    /// 在阻塞线程池里先把 [`BodyFacts`] 算好、随即放掉原始字节。转发路径在回程开流时调：之后整份出站
    /// 体就不必陪着流活到收尾。已经算过、或不在 tokio 运行时里时什么都不做（留给消费者用时现算）。
    pub fn digest_in_background(&self) {
        if !matches!(*self.state.lock(), BodyState::Raw(_)) {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let me = self.clone();
        rt.spawn_blocking(move || {
            me.facts();
        });
    }

    /// 取 [`BodyFacts`]：还没算的当场算（解析在锁外），算完把原始字节换成结果。后台那次与消费者
    /// 偶尔会同时算，两份结果相同，先写进去的那份留下。
    pub(super) fn facts(&self) -> Arc<BodyFacts> {
        let raw = match &*self.state.lock() {
            BodyState::Ready(f) => return f.clone(),
            BodyState::Raw(b) => b.clone(),
        };
        let facts = Arc::new(BodyFacts::of(&raw));
        drop(raw);
        let mut st = self.state.lock();
        match &*st {
            BodyState::Ready(f) => f.clone(),
            BodyState::Raw(_) => {
                *st = BodyState::Ready(facts.clone());
                facts
            }
        }
    }

    /// 原始字节已经放掉了（测试用）。
    #[cfg(test)]
    pub(super) fn digested(&self) -> bool {
        matches!(*self.state.lock(), BodyState::Ready(_))
    }
}

impl From<Bytes> for CallBody {
    fn from(body: Bytes) -> Self {
        Self::new(body)
    }
}

/// 测试里把它当原始字节用（`parse_shape(&call.body)` 之类）。
#[cfg(test)]
impl std::ops::Deref for CallBody {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.raw
    }
}

#[cfg(test)]
mod section_tests {
    use super::*;
    use crate::proxy::finishing_notes::Section;

    /// 提示词里的段落只认 `system` 里的：用户消息、工具结果里出现同样的标题或正文不算，`system` 正文里顺带提到标题
    /// （不在行首）也不算。
    #[test]
    fn prompt_sections_are_read_from_the_system_blocks_only() {
        let games = crate::proxy::original_games::TEXT;
        let fin = Section::FinishingWork.text();
        let facts = |v: Value| BodyFacts::of(v.to_string().as_bytes());
        let in_messages = facts(
            json!({"model": "claude-opus-5-5", "system": [{"type": "text", "text": "base\n\n# Session-specific guidance\n - g\n\n# Environment\n - x"}],
            "messages": [
                {"role": "user", "content": format!("log:\n\n{fin}\n\n# Memory, notes and feedback\nx\n\n{games}")},
                {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "t", "content": format!("\n\n{fin}")}]}
            ]}),
        );
        assert_eq!(in_messages.has_section(Section::FinishingWork), Some(false));
        assert_eq!(in_messages.has_section(Section::MemoryNotes), Some(false));
        assert!(!in_messages.has_original_games_text(games));

        let in_system = facts(json!({"model": "claude-fable-5-2", "system": [
            {"type": "text", "text": "billing"},
            {"type": "text", "text": format!("intro\n\n# Session-specific guidance\n - g\n\n# Environment\n - x\n\n{games}\n\n{fin}\n\nmentions # Memory, notes and feedback\ninline")}
        ], "messages": [{"role": "user", "content": "hi"}]}));
        assert_eq!(in_system.has_section(Section::FinishingWork), Some(true));
        assert_eq!(in_system.has_section(Section::MemoryNotes), Some(false), "不在行首的不算");
        assert!(in_system.has_original_games_text(games));

        // 字符串形态的 system、整块就是那一节。
        let mem = Section::MemoryNotes.text();
        let whole = facts(
            json!({"model": "m", "system": format!("# Session-specific guidance\n - g\n\n# Environment\n - x\n\n{mem}"), "messages": []}),
        );
        assert_eq!(whole.has_section(Section::MemoryNotes), Some(true));
        assert_eq!(whole.has_section(Section::FinishingWork), Some(false));
    }

    /// 只借了同名标题的段落（`heron_brook` 下发的正文、客户端自己的提示词）不是内置那一节；线程续轮 system 只剩 billing
    /// 那一块时看不出，不当成缺了。
    #[test]
    fn a_borrowed_heading_is_not_the_builtin_section_and_billing_only_is_unknown() {
        let facts = |v: Value| BodyFacts::of(v.to_string().as_bytes());
        let borrowed = facts(json!({"model": "claude-fable-5-2", "system": [
            {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.321.abc; cc_entrypoint=cli;"},
            {"type": "text", "text": "intro\n\n# Session-specific guidance\n - g\n\n# Environment\n - x\n\n# Finishing work\nHeron's own guidance about wrapping up."}
        ], "messages": []}));
        assert_eq!(borrowed.has_section(Section::FinishingWork), Some(false));

        let billing_only = facts(json!({"model": "claude-fable-5-2", "system": [
            {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.321.abc; cc_entrypoint=cli;"}
        ], "messages": []}));
        assert_eq!(billing_only.has_section(Section::FinishingWork), None);
        assert_eq!(billing_only.has_section(Section::MemoryNotes), None);
        assert!(!billing_only.prompt_sent(), "原创游戏那段同样看不出");
        assert!(borrowed.prompt_sent());

        // 只有身份与基座、第四块没组装：同样看不出。
        let no_fourth = facts(json!({"model": "claude-fable-5-2", "system": [
            {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.321.abc; cc_entrypoint=cli;"},
            {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."},
            {"type": "text", "text": "You are an agent working with the user toward their goals."}
        ], "messages": []}));
        assert!(!no_fourth.prompt_sent());

        // 只带 `# Environment` 一个标题的自定义 system 不是官方第四块。
        let custom = facts(json!({"model": "claude-fable-5-2", "system": [
            {"type": "text", "text": "x-anthropic-billing-header: cc_version=2.1.321.abc; cc_entrypoint=cli;"},
            {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."},
            {"type": "text", "text": "# Environment\nMy own environment notes."}
        ], "messages": []}));
        assert!(!custom.prompt_sent());
        assert_eq!(no_fourth.has_section(Section::FinishingWork), None);
    }
}
