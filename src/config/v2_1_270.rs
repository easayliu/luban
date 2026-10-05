//! 2.1.270 的 profile 表，及这一版还缺的抓包。

use super::*;

/// 2.1.270 的 profile，**只有抓到样本的那一行**：主线程 sonnet-5（`cap/2.1.270/00017`、
/// `00025`，同一会话的首轮与续轮，beta 串逐字相同）。
///
/// 相对 [`CC_PROFILES_2_1_260`] 里那行外推的 2.1.260 sonnet：
/// - **整项不发** `server-side-fallback` 与 `fallback-credit`——2.1.258 起四族都带的两项，到
///   这一版的 sonnet 主线程一个都没有了；
/// - 队尾新增 [`CC_BETA_MESSAGE_THREADS`]，配 body 顶层的 `thread`（[`CC_BODY_ORDER_MAIN_2_1_270`]）；
/// - 后缀是 `100`（两条都是），不再是 2.1.258 四族通用的 `1e2`。
///
/// **其余 kind 一行都不编。** opus / fable / haiku 的 2.1.270 主线程没有样本，「跟着 sonnet 一起
/// 不发那两项」是外推——2.1.260 时 opus 就已单独不发 `server-side-fallback` 而 fable 留着换了
/// 日期，同一版本里各族并不同步，外推不成立。查不到的 kind 由 [`cc_profile_at`] 落回 2.1.260
/// 那张表，与此前的行为一致。
///
/// 同一批抓包里另有标题生成 haiku（`00024`，后缀 `0e3`，beta 队尾同样多了 `message-threads`）
/// 与额度探测（`00005`，与 2.1.260 逐字相同）。标题生成那行没编：它走
/// [`crate::proxy::merge_beta_for`] 的非主线程豁免、只补 `oauth`，本来就原样透传；模拟路径用的仍是
/// 2.1.260 表（[`CC_USER_AGENT`] 自报 2.1.260），编了也没人用。
///
/// **这张表目前只喂 [`crate::proxy::merge_beta_for`]**（经 [`cc_profile_at`]）。模拟路径走
/// [`cc_profile`]，不看版本。
pub const CC_PROFILES_2_1_270: &[CcProfile] = &[CcProfile {
    kind: CcProfileKind::MainSonnet,
    version: "2.1.270",
    // `cap/2.1.270/00017`（sonnet-5 直连），去掉 `oauth` 与 `afk-mode`。
    beta: "claude-code-20250219,interleaved-thinking-2025-05-14,\
           thinking-token-count-2026-05-13,context-management-2025-06-27,\
           prompt-caching-scope-2026-01-05,mid-conversation-system-2026-04-07,\
           advisor-tool-2026-03-01,advanced-tool-use-2025-11-20,effort-2025-11-24,\
           thinking-display-updates-2026-08-18,extended-cache-ttl-2025-04-11,\
           cache-diagnosis-2026-04-07,message-threads-2026-08-12",
    subagent: false,
    system: CcSystemShape::Identity,
    thinking: CcThinking::AdaptiveUpdates,
    fallbacks: None,
    body_key_order: CC_BODY_ORDER_MAIN_2_1_270,
    // `cap/2.1.270/00017` / `00025` 全带。
    eager_tools: CcEagerTools::On,
    request_class: "main",
    effort: None,
}];

/// **2.1.270 还缺的抓包**（`cap/2.1.270` 只有 sonnet 主线程、标题生成 haiku、额度探测三种）。
///
/// 1. opus / fable / haiku 的主线程——[`CC_PROFILES_2_1_270`] 因此只有 sonnet 一行，其余落回
///    2.1.260 表。「是不是也一起不发 `server-side-fallback` / `fallback-credit`」抓到之前不猜。
/// 2. API-key 端任何一族——「API-key → OAuth」的差分在 2.1.270 上还是不是 2.1.258 那五项
///    （oauth / advanced-tool-use / server-side-fallback / extended-cache-ttl / cache-diagnosis）
///    无从验证，尤其不知道 API-key 端发不发 `message-threads`；
///    [`crate::proxy::merge_beta_for`] 因此不补它。
/// 3. 子代理 / helper / 安全分类——非主线程豁免让它们照旧只补 `oauth`，但各自的官方串有没有
///    变（比如也长出 `message-threads`）没有证据。
pub mod cc_2_1_270_missing_samples {}
