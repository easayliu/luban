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
    /// [`crate::proxy::original_games::TEXTS`] 里每一种写法在不在体里（按原始字节找），见
    /// [`Self::has_original_games_text`]。
    original_games: [bool; crate::proxy::original_games::TEXTS.len()],
    /// 首条用户消息的各个文本块（字符串内容算一块），认领派遣（`claim_delegation`）用。
    pub(super) first_user_texts: Vec<String>,
    /// 体的 JS 字符串长度（UTF-16 码元数；不是合法 UTF-8 时退回字节数），`requestBodyChars` 用。
    pub(super) chars: usize,
}

impl BodyFacts {
    fn of(body: &[u8]) -> Self {
        let v = serde_json::from_slice::<Value>(body).ok();
        let mut shape = v.as_ref().and_then(|v| shape_of(v, body));
        let tool_inputs = shape
            .iter_mut()
            .flat_map(|s| &mut s.tool_uses)
            .map(|tu| tu.input.take().to_string().into_boxed_str())
            .collect();
        Self {
            shape,
            tool_inputs,
            model: v.as_ref().and_then(model_of),
            original_games: crate::proxy::original_games::TEXTS.map(|t| {
                let t = t.as_bytes();
                body.windows(t.len()).any(|w| w == t)
            }),
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

    /// `text`（[`crate::proxy::original_games::text_for`] 给出的某一版正文）在不在体里。
    pub(super) fn has_original_games_text(&self, text: &str) -> bool {
        let i = crate::proxy::original_games::TEXTS.iter().position(|t| *t == text);
        debug_assert!(i.is_some(), "original_games::TEXTS 漏了 text_for 给出的一种写法");
        i.is_some_and(|i| self.original_games[i])
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
