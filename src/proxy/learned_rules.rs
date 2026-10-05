use axum::body::{Body, Bytes};
use axum::http::{StatusCode, header};
use axum::response::Response;

use crate::store;

use super::ban::parse_upstream_error;
use super::body::{FALLBACKS_FIELD, first_turn_index};
use super::probe_detect::message_to_sse;
use super::rate_limit::MAX_TRANSIENT_COOLDOWN_SECS;
use super::request_max_tokens;
use super::simulation::field_is_empty;
use super::thinking::preserve_thinking_encoding;
use super::upstream::{Aggregated, SseAggregator};

/// 请求里那些「上游一旦不认，就会在报错里逐字点名」的取值。
///
/// 三条实测样本（都是 `invalid_request_error`，都换哪个号发都一样）：
/// ```text
/// This model does not support effort level 'xhigh'. Supported levels: high, low, max, medium.
/// role 'system' is not supported on this model
/// 'claude-fable-5' does not support tool types: computer_20250124. Did you mean one of
/// advisor_20260301, bash_20250124, browser_toolset_20260801, …
/// ```
/// 判据一律是「字段名 + 这次的取值被点名」的**共现**：报错里既出现该字段的名字
/// （[`ShapeProbe::keyword`]），又确实点了这次请求里的那个取值（[`ShapeProbe::cite`]）——
/// 两半都满足才认定「是这个取值把请求打死的」，见 [`remember_shape_rejection`]。单看字段名
/// 会误伤（`max_tokens` 那类报错也提字段名），单看取值串则可能撞上正文里的巧合。
///
/// 「被点名」怎么算按样本分两种：前两条点名的形态是 `'取值'`（[`cited_as_quoted`]）；第三条
/// 不带引号，且后半句还列着一串**合法**类型，裸子串判会把它们一并学成「不支持」，故另有
/// [`cited_in_tool_type_list`] 只认冒号后那一段。
pub(super) struct ShapeProbe {
    /// 记忆表里的字段标签，同时用于日志。
    pub(super) field: &'static str,
    /// 报错文案里必须出现的字段名（小写比对）。
    keyword: &'static str,
    /// 从请求体里取出该字段的全部取值（去重后）。
    values: fn(&serde_json::Value) -> Vec<String>,
    /// 这句报错有没有**点这个取值的名**：`(报错原文, 请求里的取值)`。
    cite: fn(&str, &str) -> bool,
}

/// 「条件句」的引子。命中其一即**不学**这条 400——见 [`remember_shape_rejection`]。
///
/// 判据的前提是「上游点名了这个取值 = 这个取值本身不被接受」，而条件句推翻了这个前提：
/// ```text
/// output_config.effort 'max' is not supported when thinking is disabled on this model.
/// ```
/// 这句里 `max` 并非一律不行，只是**在 thinking 关掉时**不行——学成「一律拒」，下次客户端
/// 开着 thinking 正常发 `max` 就会被本地误拒，而上游本来会接受。
///
/// **宁可漏学**：真正无条件的报错里恰好出现这些词，代价不过是每次都白发一趟上游；反过来把
/// 条件句学成无条件，代价是本地长期拒掉一批合法请求，且现象是「换个客户端就好了」，极难查。
const CONDITIONAL_MARKS: &[&str] = &[" when ", " unless ", " without ", " while ", " if "];

/// 「位置约束」的说法。同 [`CONDITIONAL_MARKS`]，命中即**不学**。
///
/// 2026-10-02 线上（`claude-opus-5-5`）：
/// ```text
/// messages.1: role 'system' must precede an 'assistant' message or end the array; the
/// directive-only form (content: [] with output_config) is accepted at any position
/// ```
/// 字段名 `role` 与取值 `'system'` 都在，又不含条件词，于是被学成「这个模型不收 system」，
/// 之后同模型带中途 system 的请求不论位置对错全在本地拒掉。它说的是**摆在哪儿**不行，不是
/// 这个取值不行；该拦的那部分由入口的 [`super::body::misplaced_system_role`] 按原话精确判。
const POSITIONAL_MARKS: &[&str] = &[" must precede ", " must follow ", " at any position"];

/// 这句 400 是不是「有前提」的（条件句或位置约束），是则不能学成「这个取值一律拒」。
/// 学的时候（[`remember_shape_rejection`]）与从库里回填时（[`seed_tables`]）共用。
fn is_qualified_rejection(message: &str) -> bool {
    let hay = message.to_lowercase();
    CONDITIONAL_MARKS.iter().chain(POSITIONAL_MARKS).any(|m| hay.contains(m))
}

/// 目前挂着的探针。新增一项只要写清「字段名怎么念、取值从哪儿取、怎么算被点名」，学习与
/// 拦截两侧都不必改——它们只跟这张表打交道。
pub(super) const SHAPE_PROBES: &[ShapeProbe] = &[
    ShapeProbe { field: "effort", keyword: "effort", values: effort_values, cite: cited_as_quoted },
    ShapeProbe { field: "role", keyword: "role", values: role_values, cite: cited_as_quoted },
    ShapeProbe {
        field: "tool_type",
        keyword: "tool types",
        values: tool_type_values,
        cite: cited_in_tool_type_list,
    },
];

/// `output_config.effort`（`"high"`/`"xhigh"` 等），没有则为空。
fn effort_values(body: &serde_json::Value) -> Vec<String> {
    match body.get("output_config").and_then(|c| c.get("effort")).and_then(|v| v.as_str()) {
        Some(s) => vec![s.to_string()],
        None => Vec::new(),
    }
}

/// `tools[].type` 里出现过的取值，去重。没写 `type` 的（普通自定义工具）本就没有取值可点。
///
/// **`custom` 不参与**：它是自定义工具的缺省类型，几乎每条 CC 请求都带着一堆；官方不会
/// 点它的名，留着只是白比对，却平添了「一次误学把该模型的正常请求全拦下」的误伤面。同
/// [`role_values`] 里排掉 `user`/`assistant` 的理由。
fn tool_type_values(body: &serde_json::Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(tools) = body.get("tools").and_then(|t| t.as_array()) else { return out };
    for ty in tools.iter().filter_map(|t| t.get("type")?.as_str()) {
        if ty != "custom" && !out.iter().any(|v| v == ty) {
            out.push(ty.to_string());
        }
    }
    out
}

/// 默认的点名判据：上游**逐字引用**了这次请求里的那个取值（`'xhigh'`、`'system'`）。
fn cited_as_quoted(message: &str, value: &str) -> bool {
    message.contains(&format!("'{value}'"))
}

/// 冒号后那一段里点到的名（`does not support tool types:` 与句号之间），逐项精确比。
///
/// **不能裸子串判**：这句 400 的后半截是「你是不是想用」的建议清单，列的全是该模型**认**的
/// 类型，而这次请求多半正带着其中几个（`bash_20250124`、`text_editor_20250728` 都在列）。
/// 按裸子串学，等于把请求里每个工具类型都学成「这个模型不收」，下一条普通 CC 请求就被本地
/// 拒死——比多发一趟上游严重得多，故只认被点名那一段。
fn cited_in_tool_type_list(message: &str, value: &str) -> bool {
    let hay = message.to_lowercase();
    let Some(at) = hay.find(TOOL_TYPE_MARK) else { return false };
    let rest = &hay[at + TOOL_TYPE_MARK.len()..];
    // 点名段止于句号（`computer_20250124.`）；类型名里不含句号，切了不会伤到取值本身。
    // 上游万一不写句号，再按建议清单的引子截一刀兜底。
    let named = rest.split('.').next().unwrap_or(rest);
    let named = named.split(TOOL_TYPE_SUGGEST).next().unwrap_or(named);
    named.split(',').any(|t| t.trim().eq_ignore_ascii_case(value))
}

/// [`cited_in_tool_type_list`] 的两个切点（小写比对）：点名段的起点，与建议清单的引子。
const TOOL_TYPE_MARK: &str = "does not support tool types:";
const TOOL_TYPE_SUGGEST: &str = "did you mean";

/// `messages[].role` 里出现过的取值，去重。
///
/// **`user`/`assistant` 不参与**：官方永远不会点名这两个，留着只是白比对，还平添了
/// 「报错正文里恰好出现 `'user'` 就把整个模型的普通请求全拦下」的误伤面。
///
/// **开头那段 `system` 也不参与**（[`first_turn_index`] 之前的）：上游对它回的是另一句
/// （`messages.0: use the top-level 'system' parameter …`，2026-09-30 实测，新老模型都是），
/// 从不回 `role 'system' is not supported on this model`——后者说的只是对话中途那条。
/// 拿它去拦只有开头 system 的请求，拦下的就是一条上游会给出另一个原因的请求。
fn role_values(body: &serde_json::Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let Some(msgs) = body.get("messages").and_then(|m| m.as_array()) else { return out };
    let first_turn = first_turn_index(msgs);
    for (i, msg) in msgs.iter().enumerate() {
        let Some(role) = msg.get("role").and_then(|r| r.as_str()) else { continue };
        if role == "system" && i < first_turn {
            continue;
        }
        if !matches!(role, "user" | "assistant") && !out.iter().any(|v| v == role) {
            out.push(role.to_string());
        }
    }
    out
}

/// 上游拒过的「模型 + 字段 + 取值」组合 → 上游那句原话（原样留着回放）。
type ShapeRejections = std::collections::HashMap<(String, &'static str, String), String>;

/// [`ShapeRejections`] 的共享句柄，挂在 [`crate::web::AppState`] 上。
///
/// 进程内以这张表为准；学到新条目时**写穿**到 `learned_rejections` 表，启动时由
/// [`seed_learned_memories`] 读回来，重启不必每种组合再撞一次 400。
///
/// 持久化的风险是「本地永久拒掉一个其实已经支持的取值」——这是从一条报错里学来的推断，
/// 上游放开了本地没有信号能知道。两道闸：落库的条目只活 7 天
/// （[`store::LEARNED_REJECTION_TTL_SECS`]），到期丢掉重学；控制台可整表清空
/// （`DELETE /api/learned-rejections`）。
pub type ShapeMemory = std::sync::Arc<parking_lot::RwLock<ShapeRejections>>;

/// `learned_rejections.kind` 的四个取值，见 [`store::LearnedRejection`]。
pub const LEARNED_KIND_SHAPE: &str = "shape";
pub const LEARNED_KIND_DEPRECATED: &str = "deprecated";
pub const LEARNED_KIND_EMPTY_REPLY: &str = "empty_reply";
pub const LEARNED_KIND_REFUSAL: &str = "refusal";
pub const LEARNED_KIND_APP_REFUSAL: &str = "app_refusal";

/// 从进程内记忆表里删掉一条规则（控制台单条删除，与库里的删除配对）。返回是否确有其条。
pub fn forget_learned_memory(
    shape: &ShapeMemory,
    deprecated: &DeprecatedFieldMemory,
    empty: &EmptyReplyMemory,
    r: &store::LearnedRejection,
) -> bool {
    match r.kind.as_str() {
        LEARNED_KIND_SHAPE => {
            let Some(probe) = SHAPE_PROBES.iter().find(|p| p.field == r.field) else {
                return false;
            };
            shape.write().remove(&(r.model.clone(), probe.field, r.value.clone())).is_some()
        }
        LEARNED_KIND_DEPRECATED => {
            deprecated.write().remove(&(r.model.clone(), r.field.clone())).is_some()
        }
        LEARNED_KIND_EMPTY_REPLY => {
            let Some(max_tokens) = empty_reply_row_key(&r.field, &r.value) else { return false };
            empty.write().classes.remove(&(r.model.clone(), max_tokens)).is_some()
        }
        LEARNED_KIND_REFUSAL => {
            if r.field != REFUSAL_FIELD {
                return false;
            }
            empty.write().prompts.remove(&(r.model.clone(), r.value.clone())).is_some()
        }
        LEARNED_KIND_APP_REFUSAL => {
            if r.field != APP_REFUSAL_FIELD {
                return false;
            }
            empty.write().apps.remove(&(r.model.clone(), r.value.clone())).is_some()
        }
        _ => false,
    }
}

/// 学到的规则的全部种类名（`learned_rejections.kind` 列的取值域），控制台按种类清空时先拿它
/// 校验，再动库和内存。
pub const LEARNED_KINDS: [&str; 5] = [
    LEARNED_KIND_SHAPE,
    LEARNED_KIND_DEPRECATED,
    LEARNED_KIND_EMPTY_REPLY,
    LEARNED_KIND_REFUSAL,
    LEARNED_KIND_APP_REFUSAL,
];

/// 清掉某一种类的进程内规则（控制台「清空这一类」）。种类名对不上返回 `false`，什么都不动。
pub fn clear_learned_memory_kind(
    shape: &ShapeMemory,
    deprecated: &DeprecatedFieldMemory,
    empty: &EmptyReplyMemory,
    kind: &str,
) -> bool {
    match kind {
        LEARNED_KIND_SHAPE => shape.write().clear(),
        LEARNED_KIND_DEPRECATED => deprecated.write().clear(),
        LEARNED_KIND_EMPTY_REPLY => empty.write().classes.clear(),
        LEARNED_KIND_REFUSAL => empty.write().prompts.clear(),
        LEARNED_KIND_APP_REFUSAL => empty.write().apps.clear(),
        _ => return false,
    }
    true
}

/// [`seed_learned_memories`] 的回填结果：各类回填条数，以及**该从库里删掉**的过期规则。
#[derive(Debug, Default, PartialEq)]
pub struct SeededMemories {
    pub shape: usize,
    pub deprecated: usize,
    pub empty_reply: usize,
    pub refusal: usize,
    pub app_refusal: usize,
    /// 按新逻辑不该存在的旧行：不回填，交给调用方从库里删掉，免得下次启动再撞一遍。三种：
    /// - v0.3.89 那版把上游拒答（`stop_reason: "refusal"`）也学成了「请求类」——一条触发
    ///   拒答的内容把同形态的所有正常请求一起拦掉。判据是 `message`（当时截的上游回复）里带
    ///   `"stop_reason":"refusal"`（去空白比）；
    /// - 0.3.98 之前学的拒答规则没存上游那次的响应体（[`store::LearnedRejection::reply`]），
    ///   命中回的是 luban 自己造的 403；现在要原样回放上游的响应，没有体的删掉重学；
    /// - 0.3.188 之前把「位置约束」的 400 也学成了形态规则（[`POSITIONAL_MARKS`]）。
    pub stale: Vec<store::LearnedRejection>,
}

/// 启动时把落库的规则读回进程内记忆表，返回各类条数与该删的过期行（[`SeededMemories`]）。
///
/// 形态那类的 `field` 必须能对回 [`SHAPE_PROBES`] 里的某个探针（键里是 `&'static str`）；
/// 对不上的（旧版本学的、后来删掉的探针）直接跳过，不会因为一行脏数据拒绝启动。零输出那类
/// 的 `value` 必须是个整数（`max_tokens`）、且不是拒答学成的（见 [`SeededMemories::stale`]），
/// 拒答那类的 `field` 必须是 [`REFUSAL_FIELD`]，同理。
pub fn seed_learned_memories(
    shape: &ShapeMemory,
    deprecated: &DeprecatedFieldMemory,
    empty: &EmptyReplyMemory,
    rows: Vec<store::LearnedRejection>,
) -> SeededMemories {
    let mut shape_table = shape.write();
    let mut dep_table = deprecated.write();
    let mut empty_table = empty.write();
    seed_tables(&mut shape_table, &mut dep_table, &mut empty_table, rows)
}

/// 按库里现存的规则**重建**三张进程内记忆表：清空后回填，三把写锁一直拿着，请求路径看不到
/// 「表空了但还没回填」的中间态。
///
/// 7 天保鲜期落在库的 `learned_at` 列上，`Store::learned_rejections` 读库时顺手删过期行；
/// 但请求路径判的是这几张 HashMap，只回填不清表，进程活着规则就永不过期。`web` 里每小时
/// 调一次。读库到拿锁之间刚学到、还没落库的那一两条会从内存里掉一次，下个整点从库里回来。
pub fn resync_learned_memories(
    shape: &ShapeMemory,
    deprecated: &DeprecatedFieldMemory,
    empty: &EmptyReplyMemory,
    rows: Vec<store::LearnedRejection>,
) -> SeededMemories {
    let mut shape_table = shape.write();
    let mut dep_table = deprecated.write();
    let mut empty_table = empty.write();
    shape_table.clear();
    dep_table.clear();
    // 计数器是进程内的统计，不在库里，重建时留着——否则每小时归零，风暴应用每小时白挨几条。
    let counters = std::mem::take(&mut empty_table.app_counters);
    *empty_table = Default::default();
    empty_table.app_counters = counters;
    seed_tables(&mut shape_table, &mut dep_table, &mut empty_table, rows)
}

/// [`seed_learned_memories`] / [`resync_learned_memories`] 共用的回填本体：调用方已拿着三把
/// 写锁。
fn seed_tables(
    shape_table: &mut ShapeRejections,
    dep_table: &mut DeprecatedFieldRejections,
    empty_table: &mut EmptyReplyRejections,
    rows: Vec<store::LearnedRejection>,
) -> SeededMemories {
    let mut out = SeededMemories::default();
    for r in rows {
        match r.kind.as_str() {
            LEARNED_KIND_SHAPE => {
                let Some(probe) = SHAPE_PROBES.iter().find(|p| p.field == r.field) else {
                    continue;
                };
                // 旧版本按老判据学进库里的「有前提」的那种（0.3.188 之前的位置约束）：不回填、
                // 从库里删掉，否则升级后它还要在本地误拒到 7 天保鲜期满。
                if is_qualified_rejection(&r.message) {
                    out.stale.push(r);
                    continue;
                }
                if shape_table.len() >= SHAPE_MEMORY_CAP {
                    continue;
                }
                shape_table.entry((r.model, probe.field, r.value)).or_insert(r.message);
                out.shape += 1;
            }
            LEARNED_KIND_DEPRECATED => {
                if !(DEPRECATABLE_FIELDS.contains(&r.field.as_str()) || r.field == FALLBACKS_FIELD)
                    || dep_table.len() >= SHAPE_MEMORY_CAP
                {
                    continue;
                }
                dep_table.entry((r.model, r.field)).or_insert(r.message);
                out.deprecated += 1;
            }
            LEARNED_KIND_EMPTY_REPLY => {
                // 去掉空白再比：上游的 JSON 是紧凑的，但截到的原文若经过任何一层
                // pretty-print（或 SSE 里 `"stop_reason": "refusal"` 带空格），紧凑写法就对不上，
                // 学错的那条会一直连坐整类请求。
                let compact: String = r.message.chars().filter(|c| !c.is_whitespace()).collect();
                if compact.contains(r#""stop_reason":"refusal""#) {
                    out.stale.push(r);
                    continue;
                }
                let Some(max_tokens) = empty_reply_row_key(&r.field, &r.value) else { continue };
                if empty_table.classes.len() >= SHAPE_MEMORY_CAP {
                    continue;
                }
                empty_table.classes.entry((r.model, max_tokens)).or_insert(r.message);
                out.empty_reply += 1;
            }
            LEARNED_KIND_REFUSAL => {
                // 拒答格不设上限（见 [`EmptyReplyRejections`]），库里有多少回填多少。
                if r.field != REFUSAL_FIELD {
                    continue;
                }
                // 没带上游响应体的是 0.3.98 之前学的：那版命中回的是 luban 自己造的 403，
                // 现在要原样回放上游那次的响应，没有体就回放不出来——当过期删掉、下次重学。
                let Some(reply) = r.reply else {
                    out.stale.push(store::LearnedRejection { reply: None, ..r });
                    continue;
                };
                empty_table
                    .prompts
                    .entry((r.model, r.value))
                    .or_insert(RefusedPrompt { verdict: r.message, reply });
                out.refusal += 1;
            }
            LEARNED_KIND_APP_REFUSAL => {
                if r.field != APP_REFUSAL_FIELD {
                    continue;
                }
                let Some(reply) = r.reply else {
                    out.stale.push(store::LearnedRejection { reply: None, ..r });
                    continue;
                };
                empty_table
                    .apps
                    .entry((r.model, r.value))
                    .or_insert(RefusedPrompt { verdict: r.message, reply });
                out.app_refusal += 1;
            }
            _ => {}
        }
    }
    out
}

/// 三张进程内记忆表目前一共几条（重建前后对比、日志用）。
pub fn learned_memory_len(
    shape: &ShapeMemory,
    deprecated: &DeprecatedFieldMemory,
    empty: &EmptyReplyMemory,
) -> usize {
    // 加锁顺序与 [`resync_learned_memories`] 一致（shape → deprecated → empty），两者都同时
    // 持多把锁，顺序不同就是理论上的死锁。
    let s = shape.read();
    let d = deprecated.read();
    let e = empty.read();
    s.len() + d.len() + e.classes.len() + e.prompts.len() + e.apps.len()
}

// ── 上游回过零输出的请求类 ────────────────────────────────────────────
//
// 与上面两张表同一套「从上游学」的范式，但学的不是 400，是 **200**：上游收了输入的钱
// （`usage.input_tokens` 几百）、`output_tokens = 0`、一个字没回。封号复盘（luban-ban-9）里
// 这样的记录有 205 条，13 小时里每 37 秒一条，全是同一个下游中转对 `claude-fable-5` 发的
// 无 tools 单句小请求（`max_tokens` 1/4/16/64/512），而同形态换成 fable-5-1、或带上 tools、
// 或 `max_tokens` 放到几千，上游都正常回复——即这是「模型 + 请求类」层面的稳定行为，
// 不是偶发。每一条在上游侧都是「一台设备只问一句话、什么都没得到」的记录，白白留下探活
// 一样的痕迹；本地拒掉它们既省钱也少留痕。
//
// 判据放在**响应侧**而不是写死形态：哪些模型对哪种请求回空，luban 事先不知道，也不该猜。
// 第一条照常放行，回来零输出就把这一类记下来（并把上游原话截下来供人看），之后同类本地拒。
// 「类」取得很窄——模型 + 无 tools + 恰好一条用户消息 + 同一个 `max_tokens`——带 tools 的、
// 多轮的、换个 `max_tokens` 的一律不受影响；宁可多放一条，不误伤真业务。
//
// **拒答是另一回事**：`stop_reason: "refusal"` + `stop_details.category`（如 `cyber`）是内容
// 分类器拒了**那一条提示词**，换个形态照样拒、换条内容就不拒——和请求类毫无关系。它按
// 「模型 + 提示词哈希」记（[`prompt_digest`]），只拦逐字相同的重发，正常业务的其他请求一条
// 不拦。**只学分类器的判决**（[`UsageSniffer::classifier_refusal`]）：category 为空的是模型
// 自己拒的、带采样，重发可能就答；带 `recommended_model` 的是 fallback 没跑成，直接重试
// 可能就成——这两种只记流水不学。同一张表的两个格子，同一套落库/回填/删除。

/// 零输出记忆表里 `field` 列的固定值：类键的另一半就是这个字段的取值。
const EMPTY_REPLY_FIELD: &str = "max_tokens";
/// 拒答记忆表里 `field` 列的固定值：`value` 是提示词哈希（[`prompt_digest`]）。
const REFUSAL_FIELD: &str = "prompt_sha";
/// 按应用学的拒答里 `field` 列的固定值：`value` 是来访 system 的哈希（[`app_system_digest`]）。
const APP_REFUSAL_FIELD: &str = "system_sha";

/// 两格记忆：上游回过零输出的「模型 + `max_tokens`」，与上游拒答过的「模型 + 提示词哈希」。
/// 零输出格的值是本地拒时回给客户端的那段文案：当时截下的上游回复开头（信息就在开头——
/// 一段没有 `content` 的 Message）。拒答格的值是 [`RefusedPrompt`]：判决文案「[类别]
/// stop_details=<原样 JSON>」（判决在流末尾，开头截不到）给日志与控制台看，外加上游那次的
/// **完整响应体**——命中时不是 luban 自己造一条 403，而是把上游那次的 200 + 体原样回放给
/// 客户端，见 [`replay_refusal`]。
///
/// 拒答格**不分凭证**：分类器判决对同一条提示词是确定性的，换个号重发结果一样（这个池子里
/// 的号都是订阅端账号，不涉及官方按 organization 审批的网络安全验证计划），一张号上学到的
/// 对全池生效。
///
/// 拒答格**不设容量上限**：每条规则只拦逐字相同的那一条提示词，而 agent 循环里每一轮的提示词
/// 都不同（对话在变长），一个下游被分类器盯上时几小时就能灌进几百条各不相同的拒答——实测
/// 2 小时 512 条 `reasoning_extraction`，此前套用 [`SHAPE_MEMORY_CAP`] 撞满后新的就学不进了。
/// 每条几十字节、7 天到期（库里按 `learned_at` 删，进程内每小时按库重建），放着无妨；列表
/// 被淹没的问题由控制台按「模型 + 类别」折叠、按种类清空来解。
#[derive(Default)]
pub struct EmptyReplyRejections {
    pub(super) classes: std::collections::HashMap<(String, i64), String>,
    pub(super) prompts: std::collections::HashMap<(String, String), RefusedPrompt>,
    /// 按**应用**学的拒答：「模型 + 来访 system 哈希」→ 判决与回放体，见 [`known_app_refusal`]。
    /// 只对**识别不了会话**的来访（没有会话 id 也没有 device_id，Go-http-client 这类中转）学与判。
    pub(super) apps: std::collections::HashMap<(String, String), RefusedPrompt>,
    /// 按应用学的**计数器**：「模型 + system 哈希」→ 到过上游的请求数与其中被分类器拒答的条数，
    /// 学不学看比例（[`record_app_request`]）。只在进程内，不落库；每小时重建记忆表时保留
    /// （[`resync_learned_memories`]），表满时整体清掉从头计。
    pub(super) app_counters: std::collections::HashMap<(String, String), AppCounter>,
}

/// 一个识别不了会话的应用（模型 + system）在上游的战绩：到过上游几条、被分类器拒了几条。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(super) struct AppCounter {
    pub(super) total: u32,
    pub(super) refused: u32,
}

/// 按应用学的门槛：拒答至少这么多条……
const APP_REFUSAL_MIN_REFUSALS: u32 = 3;
/// ……且占该应用到过上游的请求数的比例不低于这个百分数。封号复盘里的风暴应用拒答率 35% 到
/// 63%，同一批中转站上真人的 agent 会话（固定 18 字节 system、几十到两百多轮）1% 到 2%——
/// 单看一条拒答分不开两者，比例分得很开。
const APP_REFUSAL_MIN_RATIO_PCT: u32 = 30;
/// 计数器最多记多少个应用；满了整体清掉重计（只是进程内的统计，丢了代价是多送几条）。
pub(super) const APP_COUNTER_MAX_KEYS: usize = 4096;

/// 拒答格里的一条：上游对这条提示词的判决文案，与当时那次响应的原样体。
///
/// 没有 `reply` 的拒答**不进表**（0.3.98 之前学的行回填时按过期删掉、重学）：这一格的用途
/// 就是原样回放，回放不出来就不该拦。
#[derive(Debug, Clone, PartialEq)]
pub(super) struct RefusedPrompt {
    /// 「[类别] stop_details=<原样 JSON>」，见 [`ReqLog::note_unanswered_reply`]。
    pub(super) verdict: String,
    /// 上游那次的响应体与形态，见 [`store::LearnedReply`]。
    pub(super) reply: store::LearnedReply,
}

/// [`EmptyReplyRejections`] 的共享句柄。与 [`ShapeMemory`] 同一套持久化：写穿到
/// `learned_rejections`（`kind = "empty_reply"` / `"refusal"`）、启动回填、7 天保鲜。
pub type EmptyReplyMemory = std::sync::Arc<parking_lot::RwLock<EmptyReplyRejections>>;

/// 提示词哈希：`system`、`messages`、`tools`、`tool_choice` 四个字段紧凑序列化后的 sha256
/// 前 16 位 hex。只看来访体（学与判看同一侧）；没有 `messages` 的不算。
///
/// `tools` 也进哈希：分类器看的是整条请求，同一段文字配不同的工具集（能不能执行命令、
/// 能不能读写文件）风险不一样，判决未必一样——键取窄一点，宁可多送一条，不用一条判决拦
/// 另一种上下文。缺失的字段按「没有」哈希，与显式 `null` / `[]` 不同，这是有意的：形态
/// 不同就是不同的请求。
pub(super) fn prompt_digest(body: &serde_json::Value) -> Option<String> {
    use sha2::{Digest, Sha256};
    let messages = body.get("messages")?;
    let mut h = Sha256::new();
    for field in ["system", "tools", "tool_choice"] {
        if let Some(v) = body.get(field) {
            h.update(v.to_string().as_bytes());
        }
        h.update(b"\0");
    }
    h.update(messages.to_string().as_bytes());
    Some(h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect())
}

/// 来访 `system` 的哈希：紧凑序列化后 sha256 前 16 位 hex。没有 `system`、或是空串 / 空数组
/// 的返回 `None`——没有 system 的请求识别不出「哪个应用」，不学也不判。
///
/// 这是**识别不了会话的来访**的「会话」替身：不带会话 id 也不带 device_id 的中转流量，luban
/// 派生的会话 id 按「账号 + 指纹」恒定、还把同账号上的 Go、python、node 全折成一个，拿它当键
/// 既太粗又和账号绑死；而对这类来访，system 就是「哪个应用在说话」——封号复盘（`ban/`）里那场
/// reasoning_extraction 风暴 568 条请求只有 4 种 system，正文每条不同、提示词哈希永远对不上。
pub(super) fn app_system_digest(body: &serde_json::Value) -> Option<String> {
    use sha2::{Digest, Sha256};
    let system = body.get("system")?;
    let empty = match system {
        serde_json::Value::String(s) => s.trim().is_empty(),
        serde_json::Value::Array(a) => a.is_empty(),
        serde_json::Value::Null => true,
        _ => false,
    };
    if empty {
        return None;
    }
    let mut h = Sha256::new();
    h.update(system.to_string().as_bytes());
    Some(h.finalize().iter().take(8).map(|b| format!("{b:02x}")).collect())
}

/// 这条请求是不是来自**已知被上游拒答过的应用**（同一模型、`system` 逐字相同，见
/// [`app_system_digest`]）；是则给出当时记下的判决与上游那次的原样响应体。只对识别不了会话的
/// 来访调用，见 [`EmptyReplyRejections::apps`]。
pub(super) fn known_app_refusal(
    mem: &EmptyReplyMemory,
    model: Option<&str>,
    body: Option<&serde_json::Value>,
) -> Option<RefusedPrompt> {
    let model = model?;
    let table = mem.read();
    if table.apps.is_empty() {
        return None;
    }
    let digest = app_system_digest(body?)?;
    table.apps.get(&(model.to_string(), digest)).cloned()
}

/// 一条识别不了会话的来访到过上游、拿到了 200：给它的应用（模型 + system 哈希）记一笔；
/// 若这条是分类器拒答（`refusal` 带判决文案与回放体），拒答数也加一，并判要不要学：拒答至少
/// [`APP_REFUSAL_MIN_REFUSALS`] 条、且占该应用请求数不低于 [`APP_REFUSAL_MIN_RATIO_PCT`]%，
/// 学成 `app_refusal` 规则（[`remember_app_refusal`]），返回新学到的那条供落库。
///
/// 为什么按比例而不是一条就学：同一批中转站上既有拒答率过半的风暴应用，也有固定 system、
/// 偶尔撞一次分类器的真人会话（复盘里 1% 到 2%）；一条就学会把后者整个应用连坐 7 天。
/// 已学过的应用不再计。
pub(super) fn record_app_request(
    mem: &EmptyReplyMemory,
    model: &str,
    digest: &str,
    refusal: Option<(&str, &store::LearnedReply)>,
) -> Option<store::LearnedRejection> {
    let key = (model.to_string(), digest.to_string());
    {
        let mut table = mem.write();
        if table.apps.contains_key(&key) {
            return None;
        }
        if !table.app_counters.contains_key(&key)
            && table.app_counters.len() >= APP_COUNTER_MAX_KEYS
        {
            table.app_counters.clear();
        }
        let counter = table.app_counters.entry(key).or_default();
        counter.total += 1;
        refusal?;
        counter.refused += 1;
        let enough = counter.refused >= APP_REFUSAL_MIN_REFUSALS
            && counter.refused * 100 >= counter.total * APP_REFUSAL_MIN_RATIO_PCT;
        if !enough {
            tracing::info!(
                model = %model, system_sha = %digest,
                refused = counter.refused, total = counter.total,
                min_refusals = APP_REFUSAL_MIN_REFUSALS, min_ratio_pct = APP_REFUSAL_MIN_RATIO_PCT,
                "session-less app refused again; below the app-level learning threshold, not learned yet"
            );
            return None;
        }
    }
    let (message, reply) = refusal?;
    remember_app_refusal(mem, model, digest, message, reply.clone())
}

/// 把一个识别不了会话的应用（模型 + system 哈希）记成「上游一律拒答」，之后同应用的每条请求
/// 回放 `reply`。门槛在 [`record_app_request`] 里判，这里只管写。返回**这次新学到**的那条
/// （已有的不重复），调用方拿去落库。
pub(super) fn remember_app_refusal(
    mem: &EmptyReplyMemory,
    model: &str,
    digest: &str,
    excerpt: &str,
    reply: store::LearnedReply,
) -> Option<store::LearnedRejection> {
    let mut table = mem.write();
    let key = (model.to_string(), digest.to_string());
    if table.apps.contains_key(&key) {
        return None;
    }
    table.apps.insert(key, RefusedPrompt { verdict: excerpt.to_string(), reply: reply.clone() });
    tracing::warn!(
        model = %model,
        system_sha = %digest,
        reply_sse = reply.sse,
        "learned an app-level refusal: this session-less app is refused often enough that every further request with the same model and system gets upstream's refusal replayed locally"
    );
    Some(store::LearnedRejection {
        kind: LEARNED_KIND_APP_REFUSAL.into(),
        model: model.to_string(),
        field: APP_REFUSAL_FIELD.into(),
        value: digest.to_string(),
        message: excerpt.to_string(),
        reply: Some(reply),
    })
}

/// 这条提示词是不是**已知**被上游拒答过的（同一模型、`system` + `messages` + `tools` +
/// `tool_choice` 逐字相同）；是则给出当时记下的判决文案（`[类别] stop_details=…`）与上游那次
/// 的原样响应体（[`RefusedPrompt`]）。
pub(super) fn known_refused_prompt(
    mem: &EmptyReplyMemory,
    model: Option<&str>,
    body: Option<&serde_json::Value>,
) -> Option<RefusedPrompt> {
    let model = model?;
    let table = mem.read();
    if table.prompts.is_empty() {
        return None;
    }
    let digest = prompt_digest(body?)?;
    table.prompts.get(&(model.to_string(), digest)).cloned()
}

/// 上游拒答了这条提示词 → 连同上游那次的原样响应体记进 [`EmptyReplyMemory`]。返回**这次
/// 新学到**的那条（已有的不重复），调用方拿去落库。不设上限，见 [`EmptyReplyRejections`]。
pub(super) fn remember_refused_prompt(
    mem: &EmptyReplyMemory,
    model: &str,
    digest: &str,
    excerpt: &str,
    reply: store::LearnedReply,
) -> Option<store::LearnedRejection> {
    let mut table = mem.write();
    let key = (model.to_string(), digest.to_string());
    if table.prompts.contains_key(&key) {
        return None;
    }
    table.prompts.insert(key, RefusedPrompt { verdict: excerpt.to_string(), reply: reply.clone() });
    tracing::info!(
        model = %model,
        prompt_sha = %digest,
        reply_sse = reply.sse,
        reply_bytes = reply.body.len(),
        "learned a refused prompt; an identical resend will get upstream's refusal replayed locally from now on"
    );
    Some(store::LearnedRejection {
        kind: LEARNED_KIND_REFUSAL.into(),
        model: model.to_string(),
        field: REFUSAL_FIELD.into(),
        value: digest.to_string(),
        message: excerpt.to_string(),
        reply: Some(reply),
    })
}

/// [`UsageSniffer::reply`] 最多留多少字节，超过就不学这条拒答。输出前被拒的体只有几百字节到
/// 几 KB（流式是 `message_start` + 若干 `ping` + `message_delta` + `message_stop`）；64 KiB
/// 是给 `stop_details.explanation` 与偶发的长 usage 留的余量，正常回复早在几 KB 内就见到
/// 第一个内容块、停止攒了。
pub(super) const REFUSAL_REPLY_BYTES: usize = 64 * 1024;

/// SSE 响应的 `content-type`，与上游一致。
pub(super) const SSE_CONTENT_TYPE: &str = "text/event-stream; charset=utf-8";

/// 把上游那次的拒答按**这次来访要的形态**回放：200 + 上游的体。
///
/// 形态一致（学的时候是 SSE、这次也要流式；或都是非流式）原样发字节；不一致才转换：SSE →
/// 整段 JSON 走 [`SseAggregator`]（与来访非流式、上游 SSE 时的聚合路径同一套），整段 JSON →
/// SSE 走 [`message_to_sse`]。转不出来（学到的体残缺、解析失败）返回 `None`，调用方照常转发
/// 上游——宁可多送一条，不回一段拼不齐的响应。
pub(super) fn replay_refusal(reply: &store::LearnedReply, wants_stream: bool) -> Option<Response> {
    let (content_type, body): (&str, Vec<u8>) = match (reply.sse, wants_stream) {
        (true, true) => (SSE_CONTENT_TYPE, reply.body.clone().into_bytes()),
        (false, false) => ("application/json", reply.body.clone().into_bytes()),
        (true, false) => {
            let mut agg = SseAggregator::default();
            agg.feed(reply.body.as_bytes());
            match agg.finish() {
                Aggregated::Message(msg) => ("application/json", serde_json::to_vec(&msg).ok()?),
                Aggregated::UpstreamError(_) | Aggregated::Incomplete(_) => return None,
            }
        }
        (false, true) => {
            let msg: serde_json::Value = serde_json::from_str(&reply.body).ok()?;
            (SSE_CONTENT_TYPE, message_to_sse(&msg)?.into_bytes())
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .body(Body::from(body))
        .ok()
}

/// 落库行的 `(field, value)` 能否对回一个类键：`field` 必须是 `max_tokens`、`value` 是整数。
fn empty_reply_row_key(field: &str, value: &str) -> Option<i64> {
    (field == EMPTY_REPLY_FIELD).then(|| value.parse().ok()).flatten()
}

/// 这条请求属于哪个「零输出请求类」：`(模型, max_tokens)`。只有**无 tools（按值算）、恰好一条
/// 用户消息、带 `max_tokens`** 的请求才归类；其余（带 tools、多轮、没写 `max_tokens`）返回
/// `None`——它们既不学也不拒。模型取来访声明的那个（与 [`known_empty_reply`] 同一侧，学与
/// 判必须看同一个值）。
pub(super) fn empty_reply_class(
    model: Option<&str>,
    body: Option<&serde_json::Value>,
) -> Option<(String, i64)> {
    let (model, v) = (model?, body?);
    let single_user_message = v
        .get("messages")
        .and_then(|m| m.as_array())
        .is_some_and(|m| m.len() == 1 && m[0].get("role").and_then(|r| r.as_str()) == Some("user"));
    if !single_user_message || !field_is_empty(v.get("tools")) {
        return None;
    }
    Some((model.to_string(), request_max_tokens(Some(v))?))
}

/// 这条请求是不是**已知**会被上游回零输出的那一类；是则给出 `(max_tokens, 上游当时的回复)`。
///
/// 只有「同一个模型、同一种无 tools 单条消息、同一个 `max_tokens`」确实回过一次零输出才
/// `Some`。没学过的一律照常往上游发。
pub(super) fn known_empty_reply(
    mem: &EmptyReplyMemory,
    model: Option<&str>,
    body: Option<&serde_json::Value>,
) -> Option<(i64, String)> {
    let key = empty_reply_class(model, body)?;
    let table = mem.read();
    if table.classes.is_empty() {
        return None;
    }
    table.classes.get(&key).map(|excerpt| (key.1, excerpt.clone()))
}

/// 上游对这一类回了零输出 → 记进 [`EmptyReplyMemory`]。返回**这次新学到**的那条（已有的
/// 不重复、表满了不记），调用方拿去落库。
pub(super) fn remember_empty_reply(
    mem: &EmptyReplyMemory,
    model: &str,
    max_tokens: i64,
    excerpt: &str,
) -> Option<store::LearnedRejection> {
    let mut table = mem.write();
    let key = (model.to_string(), max_tokens);
    if table.classes.contains_key(&key) || table.classes.len() >= SHAPE_MEMORY_CAP {
        return None;
    }
    table.classes.insert(key, excerpt.to_string());
    tracing::info!(
        model = %model,
        max_tokens,
        "learned an empty-reply request class; tool-less single-message requests of this shape will be rejected locally from now on"
    );
    Some(store::LearnedRejection {
        kind: LEARNED_KIND_EMPTY_REPLY.into(),
        model: model.to_string(),
        field: EMPTY_REPLY_FIELD.into(),
        value: max_tokens.to_string(),
        message: excerpt.to_string(),
        reply: None,
    })
}

// ── 已废弃字段的自动剥离 ──────────────────────────────────────────────
//
// 与 ShapeProbe 共享「从上游 400 里学」的范式，但行为正好相反：
// - ShapeProbe 学到的是「模型 + 取值」组合，命中即**拒绝**（回放上游原话）。
// - 这里学到的是「模型 + 字段」组合，命中即**剥掉该字段后正常转发**。
//
// 典型案例：`temperature` / `top_p` / `top_k` 在部分新模型上被标为 deprecated——
// 客户端的意图（发一条消息）是合法的，只是多带了一个上游不再接受的参数。剥掉它、
// 请求照常成功，比拒掉再让客户端去改 SDK 参数好得多。

/// 可能被上游按模型废弃的**顶层**字段。来访请求里有这个字段、且上游那条 400 含
/// `` `字段名` `` + `deprecated` → 记下来，之后同模型自动剥掉。
///
/// 只放确实是**可选**的采样/生成参数——缺了它们请求也完全合法。`model`、`messages`
/// 之类缺了上游直接 400，剥掉只是换一种死法。
pub(super) const DEPRECATABLE_FIELDS: &[&str] = &["temperature", "top_p", "top_k"];

/// 上游拒过的「模型 + 已废弃字段」→ 上游那句原话（只做日志，不回放）。
type DeprecatedFieldRejections = std::collections::HashMap<(String, String), String>;

/// [`DeprecatedFieldRejections`] 的共享句柄。与 [`ShapeMemory`] 同一套持久化：写穿到
/// `learned_rejections`、启动回填、7 天保鲜。
pub type DeprecatedFieldMemory = std::sync::Arc<parking_lot::RwLock<DeprecatedFieldRejections>>;

/// 拒绝日志的抑制表：键（`device:<id>` / `session:<id>`）→ (上次真打了日志的时刻, 从那以后
/// 憋掉的条数)。
type RejectionCounters = std::collections::HashMap<String, (std::time::Instant, u64)>;

/// [`RejectionCounters`] 的共享句柄，挂在 [`crate::web::AppState`] 上。
pub type RejectionLog = std::sync::Arc<parking_lot::Mutex<RejectionCounters>>;

/// 同一个键两条拒绝日志之间至少隔多久。
///
/// 取 10 秒：撞上限的客户端往往每几十毫秒重试一次（实测有 67ms 一发的），一条不落地记就是
/// 每秒十几行 WARN，几分钟能把日志刷得没法看，真正要查的东西全被挤走了。10 秒足够把一次
/// 突发收成一行，又短到「这台机器还在撞」这件事不会从日志里消失——限流本身最长也就 60 秒。
pub(super) const REJECTION_LOG_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

/// 抑制表最多留多少个键。键是客户端自报的 id，乱编 id 的脚本能把表撑大，故与限流窗口同样
/// 需要清扫（[`take_rejection_log_slot`]）。
const REJECTION_LOG_MAX_KEYS: usize = 4096;

/// 这条拒绝要不要真打一行日志：要打则返回**上一行之后憋掉了多少条**（首次为 0），
/// 不打则 `None`。
///
/// 抑制掉的条数不会凭空消失，它会记在下一行日志的 `suppressed=` 上——否则「刷了多少」这个
/// 唯一有用的量就没了，而那正是判断「客户端在正常退避」还是「压根没读 retry-after」的依据。
///
/// **代价说清楚**：客户端不再发了之后，最后那截憋着的条数没有下一行可挂，就丢了；表被撑爆
/// 触发清扫时，被清掉的老键同理。两者都只影响计数的尾巴，不影响「撞没撞、撞了多久」——
/// 为它加一个定时冲刷的后台任务，不值当。
pub(super) fn take_rejection_log_slot(log: &RejectionLog, key: &str) -> Option<u64> {
    let now = std::time::Instant::now();
    let mut map = log.lock();
    if map.len() > REJECTION_LOG_MAX_KEYS {
        map.retain(|_, (at, _)| now.duration_since(*at) < REJECTION_LOG_WINDOW);
    }
    match map.get_mut(key) {
        // 窗口内：憋着，只把计数加一。
        Some((at, suppressed)) if now.duration_since(*at) < REJECTION_LOG_WINDOW => {
            *suppressed += 1;
            None
        }
        // 窗口过了：把憋着的条数交出去，重新开始计。
        Some((at, suppressed)) => {
            let n = std::mem::take(suppressed);
            *at = now;
            Some(n)
        }
        None => {
            map.insert(key.to_string(), (now, 0));
            Some(0)
        }
    }
}

/// 同一条「账号 + 模型」路线上连撞瞬时限流的记录：(连撞档位, **进入这一档的时刻**)。
///
/// 第二项是档位的锚点而不是「上次命中时刻」：升档只看这个锚点走了多久，同一档窗口内再撞
/// 多少发都不刷新它，见 [`next_transient_backoff_at`]。
type TransientStreaks = std::collections::HashMap<(i64, String), (u32, std::time::Instant)>;

/// [`TransientStreaks`] 的共享句柄，挂在 [`crate::web::AppState`] 上。
///
/// 只在进程内活着：连撞的是「此刻这一阵拥堵」，重启后从头数起本来就是对的。
pub type TransientBackoff = std::sync::Arc<parking_lot::Mutex<TransientStreaks>>;

/// 瞬时限流退避的**首次**等待秒数。之后逐次翻倍，封顶 [`MAX_TRANSIENT_COOLDOWN_SECS`]。
///
/// 起点取 2 秒而不是 1 秒：1 秒的退避对一个正在拥堵的上游几乎等于不退，第一发就该给客户端
/// 一个真的能让出口喘口气的间隔；而 2 秒对偶发的单次限流也不算长。
pub(super) const TRANSIENT_BACKOFF_BASE_SECS: u64 = 2;

/// 一个档位挂了多久没能往上走，就把连撞计数清零。
///
/// 取封顶值的两倍：走到封顶时我们让客户端等 60 秒，那么「等满了、回来了、再撞」属于同一串
/// 拥堵，不该清零；而两倍于此都没能升档（最长的一档也才 60 秒，故这中间至少有 60 秒没人撞
/// 过），说明上一阵已经过去，下次该从 2 秒重新数起——不然计数只增不减，几小时后偶发一次
/// 限流也会被判成「连撞第 9 次」，直接甩给客户端 60 秒。
pub(super) const TRANSIENT_BACKOFF_RESET: std::time::Duration =
    std::time::Duration::from_secs(2 * MAX_TRANSIENT_COOLDOWN_SECS as u64);

/// 退避表最多留多少格。键是 `(账号, 模型)`，模型名来自来访请求体，故与拒绝日志同样需要清扫。
const TRANSIENT_BACKOFF_MAX_KEYS: usize = 4096;

/// 同一条「账号 + 模型」路线上最多连撞到第几档瞬时 429，超过就不再当它是「一阵拥堵」。
///
/// 取 6：正好是退避涨到封顶的那一档（2→4→8→16→32→60）。**退避都涨到头了还在撞**，说明这
/// 不是一阵拥堵，而是这条路线此刻真的走不通——再无限吞下去，客户端就只是一直吃 429，而我们
/// 手里明明还有别的号没试过。到点即把这一格挪出调度池（见 [`park_rate_limited`]），
/// 让**后续**请求改走别的号；连撞计数同时清零，冷却过后重新从 2 秒数起。
///
/// 数的是**档位**不是发数，两者的区别就是这一档的成败：档位只随墙钟往上走（见
/// [`next_transient_backoff_at`]），故走到第 6 档意味着这条路线已经连续坏了
/// 2+4+8+16+32≈62 秒。曾经它数的是发数，于是一批并发一次性就把 6 格吃光——线上那份日志里
/// 6 条在飞的请求在 63 毫秒内撞完（`ttft_ms` 都在 230 上下），把这个号的这个模型直接硬冷却
/// 挪出了调度池，1.5 秒内一路点掉 5 个号，正是 [`park_rate_limited`] 那段注释里说要防的
/// 「转够一圈全池都在冷却」。
pub(super) const TRANSIENT_MAX_ATTEMPTS: u32 = 6;

/// 一条请求因「套餐不含这个模型」最多换几个号。一次失败就学到一条记录、之后的选号自动绕开，
/// 所以这个数只在**冷启动**（记录还没学到）时被碰到；设成池子里 Pro 号的常见数量级即可。
pub(super) const MODEL_DENIAL_MAX_SWAPS: usize = 4;

/// 这个号是不是 Max 档（个人 Max，或团队/企业号拿的 Max 额度档——两者 `tier` 都以 `Max` 开头，
/// 见 `crate::oauth::tier_from`）。等级未知按「不是」算：那时唯一的信息来源是上游的判决。
/// 只有 Max 含 fable/mythos，其余等级撞到那形态的 429 都记「套餐不含」，见
/// [`LimitScope::Unsupported`]。
pub(super) fn is_max_plan(cred: &crate::credentials::Credential) -> bool {
    cred.tier.as_deref().is_some_and(|t| t.starts_with("Max"))
}

/// 连撞到第 `attempts` 档时该让客户端等多久：`base * 2^(attempts-1)`，封顶
/// [`MAX_TRANSIENT_COOLDOWN_SECS`]。
pub(super) fn transient_backoff_for(attempts: u32) -> std::time::Duration {
    // 移位次数先夹住，免得在 u64 上左移过界。
    let shift = attempts.saturating_sub(1).min(u32::BITS - 1);
    let secs = TRANSIENT_BACKOFF_BASE_SECS
        .saturating_mul(1u64 << shift)
        .min(MAX_TRANSIENT_COOLDOWN_SECS as u64);
    std::time::Duration::from_secs(secs)
}

/// 这条「账号 + 模型」路线该让客户端等多久再来——**连撞一次翻一倍**，封顶
/// [`MAX_TRANSIENT_COOLDOWN_SECS`]，静默 [`TRANSIENT_BACKOFF_RESET`] 后清零。
///
/// **为什么必须是指数而不是一个固定值**：瞬时限流那档我们已经不换号、也不再把号挪出调度池
/// （见 [`park_rate_limited`]），交回客户端的就是一发 429。若每次都告诉它「30 秒后再来」，
/// 一个正在拥堵的出口面对的就是一群按固定节拍同时回来的客户端——退避的意义正在于**让重试
/// 的密度随失败次数下降**，固定值做不到这一点，秒级重试更是直接把拥堵喂大。指数退避让第一次
/// 偶发限流几乎无感（2 秒），而真的撞上一堵墙时迅速拉到分钟级。
///
/// 返回 `(该等多久, 这是连撞的第几档)`。第二项到达 [`TRANSIENT_MAX_ATTEMPTS`] 即为「吞够了」，
/// 此时计数就地清零——那一发之后这个号的这个模型会被挪出调度池，冷却过去再撞属于新的一串。
///
/// **升档只看墙钟，不看发数**：同一档的退避时长走完之前再撞多少发都还是这一档。理由见
/// [`TRANSIENT_MAX_ATTEMPTS`]——按发数数的话，一批并发就等于一串连撞，档位量到的是客户端
/// 的并发度而不是「等过一轮还在撞」。顺带这也让同一瞬间在飞的那批请求拿到同一个
/// `retry-after`，而不是各拿一个（线上那份日志里同一毫秒的两发一个 30 一个 30、隔 60 毫秒
/// 就变成 32 和 60，对客户端毫无意义）。
pub(super) fn next_transient_backoff(
    state: &TransientBackoff,
    cred_id: i64,
    model: &str,
) -> (std::time::Duration, u32) {
    next_transient_backoff_at(state, cred_id, model, std::time::Instant::now())
}

/// 同 [`next_transient_backoff`]，但由调用方给出「现在」——清零那条路要等两分钟才走得到，
/// 拿真实时钟测等于不测。
pub(super) fn next_transient_backoff_at(
    state: &TransientBackoff,
    cred_id: i64,
    model: &str,
    now: std::time::Instant,
) -> (std::time::Duration, u32) {
    let mut map = state.lock();
    if map.len() > TRANSIENT_BACKOFF_MAX_KEYS {
        map.retain(|_, (_, at)| now.duration_since(*at) < TRANSIENT_BACKOFF_RESET);
    }
    let slot = map.entry((cred_id, model.to_string())).or_insert((0, now));
    // 这一档挂了够久都没能往上走 → 上一阵拥堵已经过去，这是新的一串，从头数起。
    let held = now.duration_since(slot.1);
    if held >= TRANSIENT_BACKOFF_RESET {
        slot.0 = 0;
    }
    // 升档的唯一条件是「这一档的退避时长已经走完，客户端等过一轮回来还在撞」。窗口内的并发
    // 共用当前档位：既不递增，**也不刷新锚点**——刷新的话，一个压根不认 `retry-after`、
    // 200 毫秒就重来的客户端会把锚点一直往后推，档位永远卡在第 1 档，「吞够了」那条逃生口
    // 就此形同虚设。锚点不动，档位便按墙钟自己往上爬，与客户端的重试密度解耦。
    if slot.0 == 0 || held >= transient_backoff_for(slot.0) {
        slot.0 = slot.0.saturating_add(1);
        slot.1 = now;
    }
    let attempts = slot.0;
    // 吞够了：这一发之后该号的该模型要被挪出调度池，计数就地清零，冷却过后重新从头数起。
    // 不清的话冷却一到期，第一发就又被判成「连撞第 7 次」，这个号再没有机会证明自己好了。
    if attempts >= TRANSIENT_MAX_ATTEMPTS {
        slot.0 = 0;
    }
    (transient_backoff_for(attempts), attempts)
}

/// 记忆表的容量上限。每个「模型 + 字段 + 没见过的取值」占一格，而取值来自来访请求，
/// 也就是说这张表的增长是外部可控的——封顶后不再插入（既有条目照常生效）。
pub(super) const SHAPE_MEMORY_CAP: usize = 512;

/// 上游用一条 400 点名了请求里的某个取值 → 记进 [`ShapeMemory`]，之后同款组合由
/// [`known_shape_rejection`] 在本地拦下，不再往上游送。
///
/// 记忆按**请求里写的那个模型名**索引（别名与全名各算一格）：客户端每次发的是同一串，
/// 拿它当键既够用，又不会把某个模型学到的结论套到别的模型头上。
///
/// 返回**这次新学到**的条目（已知的不算），调用方拿去落库；空 Vec 即什么也没学到。
pub(super) fn remember_shape_rejection(
    mem: &ShapeMemory,
    model: Option<&str>,
    body: Option<&serde_json::Value>,
    err: &[u8],
) -> Vec<store::LearnedRejection> {
    let mut learned = Vec::new();
    // 认不出模型名、或请求体不是 JSON：这条 400 照常透传给客户端，只是学不到东西。
    let (Some(model), Some(body)) = (model, body) else { return learned };
    let (_, message) = parse_upstream_error(err);
    // 条件句、位置约束一律不学：这条 400 说的是「在某某前提下不行」，不是「这个取值不行」。
    if is_qualified_rejection(&message) {
        return learned;
    }
    let hay = message.to_lowercase();
    for probe in SHAPE_PROBES {
        if !hay.contains(probe.keyword) {
            continue;
        }
        for value in (probe.values)(body) {
            // 上游必须**点了这个取值的名**，才算认定是它的锅（形态见 [`ShapeProbe::cite`]）。
            if !(probe.cite)(&message, &value) {
                continue;
            }
            let mut table = mem.write();
            let key = (model.to_string(), probe.field, value.clone());
            if table.contains_key(&key) || table.len() >= SHAPE_MEMORY_CAP {
                continue;
            }
            table.insert(key, message.clone());
            learned.push(store::LearnedRejection {
                kind: LEARNED_KIND_SHAPE.into(),
                model: model.to_string(),
                field: probe.field.to_string(),
                value: value.clone(),
                message: message.clone(),
                reply: None,
            });
            tracing::info!(
                model = %model,
                field = %probe.field,
                value = %value,
                "learned a request-shape rejection; the same combination will be rejected locally from now on"
            );
        }
    }
    learned
}

/// 上游的 400 里出现 `` `字段名` `` + `deprecated` → 记进 [`DeprecatedFieldMemory`]，
/// 之后同模型转发前自动剥掉该字段。与 [`remember_shape_rejection`] 并行调用。
///
/// 典型上游原文：`` `temperature` is deprecated for this model. ``
/// 判据是「`deprecated` 出现 + 反引号包裹的字段名与请求里确实存在的顶层键匹配」，
/// 两项**共现**才认——单看 `deprecated` 会误伤，单看反引号里的串可能碰巧。
///
/// 返回**这次新学到**的条目，调用方拿去落库（同 [`remember_shape_rejection`]）。
pub(super) fn remember_deprecated_field(
    mem: &DeprecatedFieldMemory,
    model: Option<&str>,
    body: Option<&serde_json::Value>,
    err: &[u8],
) -> Vec<store::LearnedRejection> {
    let mut learned = Vec::new();
    let (Some(model), Some(body)) = (model, body) else { return learned };
    let (_, message) = parse_upstream_error(err);
    let hay = message.to_lowercase();
    if !hay.contains("deprecated") {
        return learned;
    }
    let Some(obj) = body.as_object() else { return learned };
    for &field in DEPRECATABLE_FIELDS {
        if !obj.contains_key(field) {
            continue;
        }
        if !message.contains(&format!("`{field}`")) {
            continue;
        }
        let mut table = mem.write();
        let key = (model.to_string(), field.to_string());
        if table.contains_key(&key) || table.len() >= SHAPE_MEMORY_CAP {
            continue;
        }
        table.insert(key, message.clone());
        learned.push(store::LearnedRejection {
            kind: LEARNED_KIND_DEPRECATED.into(),
            model: model.to_string(),
            field: field.to_string(),
            value: String::new(),
            message: message.clone(),
            reply: None,
        });
        tracing::info!(
            model = %model,
            field = %field,
            "learned a deprecated-field rejection; the field will be stripped for this model from now on"
        );
    }
    learned
}

/// 请求体里是否带了**这个模型已学到**的废弃字段（不看静态名单）。`sampling_policy=reject`
/// 时用：学到的组合也该本地拒，否则设置项名不副实——只拒名单里的、放过学到的。
pub(super) fn has_learned_deprecated_field(
    mem: &DeprecatedFieldMemory,
    model: Option<&str>,
    body: Option<&serde_json::Value>,
) -> bool {
    let (Some(model), Some(obj)) = (model, body.and_then(|v| v.as_object())) else { return false };
    let table = mem.read();
    if table.is_empty() {
        return false;
    }
    DEPRECATABLE_FIELDS
        .iter()
        .any(|&f| obj.contains_key(f) && table.contains_key(&(model.to_string(), f.to_string())))
}

/// 请求体里是否带了 [`DEPRECATABLE_FIELDS`] 中的任何一项——用已解析的 `body_json` 判，零开销。
pub(super) fn has_deprecated_sampling_field(body: Option<&serde_json::Value>) -> bool {
    let Some(obj) = body.and_then(|v| v.as_object()) else { return false };
    DEPRECATABLE_FIELDS.iter().any(|&f| obj.contains_key(f))
}

/// 模型是否不支持 sampling 参数（`temperature`/`top_p`/`top_k`）。
///
/// 4.7+ 及 Sonnet 5 / Fable 5 / Mythos 5 全系列已移除这些参数，传了会 400。
/// 注意 **4.6 仍然允许**——与 prefill 的 4.6+ 全系列不同。
pub(super) fn model_rejects_sampling(model: &str) -> bool {
    [
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-sonnet-5",
        "claude-opus-5",
        "claude-fable-5",
        "claude-mythos-5",
    ]
    .iter()
    .any(|p| model.starts_with(p))
}

/// 请求体里有没有该模型已经被标记为 deprecated 的字段；有则从 `body` 里剥掉后返回
/// 新的 `Bytes`，没有则原样返回（零拷贝）。
///
/// **先用已经解析好的 `body_json` 做只读检查**，命中了才重新解析 `body` 做改写——
/// 绝大多数请求根本不带 `temperature` 或者模型没有废弃它，走的是零开销的快速路径。
///
/// 除了运行时学到的 [`DeprecatedFieldMemory`]，还按官方文档预置了已知模型的 deprecated
/// 字段（[`model_rejects_sampling`]），避免冷启动第一条请求白撞一次 400。
///
/// `use_static_list`：是否启用静态预置名单。`sampling_policy=off` 时传 `false`，
/// 关掉主动剥离但保留运行时学习兜底。
pub(super) fn maybe_strip_deprecated(
    mem: &DeprecatedFieldMemory,
    model: Option<&str>,
    body_json: Option<&serde_json::Value>,
    body: Bytes,
    use_static_list: bool,
) -> Bytes {
    let Some(model) = model else { return body };
    let Some(bj) = body_json else { return body };
    let Some(obj) = bj.as_object() else { return body };
    let static_reject = use_static_list && model_rejects_sampling(model);
    let table = mem.read();
    let to_strip: Vec<&str> = DEPRECATABLE_FIELDS
        .iter()
        .filter(|&&f| {
            obj.contains_key(f)
                && (static_reject || table.contains_key(&(model.to_string(), f.to_string())))
        })
        .copied()
        .collect();
    drop(table);
    if to_strip.is_empty() {
        return body;
    }
    let mut v: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return body,
    };
    if let Some(obj) = v.as_object_mut() {
        for f in &to_strip {
            obj.remove(*f);
        }
    }
    tracing::debug!(model, fields = ?to_strip, "stripped deprecated fields from request");
    match serde_json::to_vec(&v) {
        Ok(bytes) => Bytes::from(preserve_thinking_encoding(&body, bytes)),
        Err(_) => body,
    }
}

/// 这条请求里有没有**已知**会被该模型拒掉的取值；有则给出上游当初那句原话。
///
/// 只有「同一个模型、同一个字段、同一个取值确实被上游拒过一次」才返回 `Some`。没学过的
/// 组合一律照常往上游发——这张表只用来挡住确定无疑的重复失败，绝不替上游做没有依据的判断。
///
/// `system_hoisted`：这条请求出站前 `role:"system"` 会被 [`hoist_system_role_messages`] 整条
/// 挪到顶层（billable 且 [`hoists_system_role`]）。这里查的是入站原件，而上游根本看不到
/// 这个 role，学到的 `role 'system'` 就管不着它——拿它去拦，拦下的是一条修补后本来能过的请求。
pub(super) fn known_shape_rejection(
    mem: &ShapeMemory,
    model: Option<&str>,
    body: Option<&serde_json::Value>,
    system_hoisted: bool,
) -> Option<(&'static str, String, String)> {
    let (model, body) = (model?, body?);
    let table = mem.read();
    if table.is_empty() {
        return None;
    }
    SHAPE_PROBES.iter().find_map(|probe| {
        (probe.values)(body).into_iter().find_map(|value| {
            if system_hoisted && probe.field == "role" && value == "system" {
                return None;
            }
            let message = table.get(&(model.to_string(), probe.field, value.clone()))?;
            Some((probe.field, value, message.clone()))
        })
    })
}

#[cfg(test)]
mod tests;
