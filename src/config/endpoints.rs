//! OAuth 端点与 scope、上游 API 地址、token 提前刷新量。

/// Claude Code 公开 OAuth Client ID。
pub const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// 授权页地址（用户在浏览器打开、登录并同意授权）。
pub const AUTHORIZE_URL: &str = "https://claude.ai/oauth/authorize";

/// Token 交换 / 刷新端点。
pub const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";

/// 账号 profile 端点（用 access_token 获取邮箱/姓名/订阅等级）。
pub const PROFILE_URL: &str = "https://api.anthropic.com/api/oauth/profile";

/// 手动粘贴模式使用的 redirect_uri，token 端点会据此校验。
pub const REDIRECT_URI: &str = "https://platform.claude.com/oauth/code/callback";

/// 登录时申请的 OAuth scope 的默认值，与 Claude Code 保持一致（含顺序——scope 集合也是
/// 指纹的一部分）。`org:create_api_key` luban 自身用不到，但官方客户端就带着它；
/// `user:file_upload` 缺了会让经代理走 Files API 的上传被上游按 scope 拒掉。
///
/// 这只是**默认值**：实际申请哪几项由 settings 里的 [`crate::store::OAUTH_SCOPES`] 决定，
/// 没配就用这一串。要改的理由见 [`SCOPES_MINIMAL`]。
///
/// **`scope` 是必填的**：授权 URL 整个不带这个参数（想着由上游给默认范围）会被直接回
/// `Missing scope parameter`——2026-08-24 实测，所以没有「不传」这一档。少要权限只能是
/// **少几项**（[`SCOPES_MINIMAL`] 是现成的一档），要试别的组合就往设置里那个输入框填——
/// 那个框不做校验，写什么发什么，认不认由上游的同意页说，见 [`normalize_scopes`]。
pub const SCOPES: &str = "org:create_api_key user:profile user:inference \
                          user:sessions:claude_code user:mcp_servers user:file_upload";

/// 精简 scope：一次真实授权里观察到的最小集，只留 luban 自己用得上的三项。
///
/// - `user:inference` —— 转发 `/v1/*` 靠它，缺了这个号就只能登进来看额度；
/// - `user:profile` —— 交换后拉邮箱/等级/account_uuid，缺了只是标签与等级留白（登录不失败）；
/// - `user:file_upload` —— 经代理走 Files API 的上传。
///
/// 相比 [`SCOPES`] 少了 `org:create_api_key`（建 API key，luban 从不调）、
/// `user:sessions:claude_code`、`user:mcp_servers`（官方客户端自己的功能面）。
/// 少要权限的代价是**授权请求与官方客户端不再逐字一致**——scope 集合也是指纹的一部分，
/// 所以这不是默认值，是给「宁可少授权、不在意这点差异」的人留的一档。
///
/// 还有一条要知道：这一档只管**登录那一刻**。刷新 token 发的是固定的 [`REFRESH_SCOPES`]
/// （官方行为），后端允许刷新时扩展 scope，所以第一次刷新后这个号的 scope 就回到那五项了。
pub const SCOPES_MINIMAL: &str = "user:file_upload user:inference user:profile";

/// 刷新 token 时随请求发送的 `scope`。
///
/// **是固定常量，不是登录时申请的那组**：官方客户端（`services/oauth/client.ts` 的
/// `refreshOAuthToken`）对 claude.ai 订阅号刻意不传已存的 scopes，让缺省值
/// `CLAUDE_AI_OAUTH_SCOPES` 生效——后端允许刷新时**扩展** scope，这样老 token 不必重新登录
/// 就能拿到后来加进来的 `user:file_upload`。里面没有 `org:create_api_key`（那一项只在
/// 授权 URL 上出现）。
///
/// 副作用要知道：登录时选了 [`SCOPES_MINIMAL`] 的号，第一次刷新后 scope 会被扩回这五项。
/// 那正是官方客户端的行为，刻意不做「按 settings 发」——那会造出一条官方从不产生的请求体。
pub const REFRESH_SCOPES: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// 把填进来的 scope 串规整成「单空格分隔、按输入顺序去重」的形态。
///
/// 顺序按输入保留而不排序：scope 集合是指纹的一部分，照抄一份抓包的顺序就该原样发出去。
///
/// **只规整，不校验**：写什么都收，原样发给上游。这里曾拦过「必须含 `user:inference`」和
/// 一套字符集，结果把这个输入框唯一的用途——试上游到底认哪些 scope——给拦掉了：连
/// `user:inference-1` 这种明摆着是拿来探边界的值都存不进去。合不合法由上游的同意页判，
/// 它的报错（如 `Missing scope parameter`）比我们猜的白名单准。
pub fn normalize_scopes(raw: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for item in raw.split_whitespace() {
        if !out.contains(&item) {
            out.push(item);
        }
    }
    out.join(" ")
}

/// 用 OAuth access token 调用 Anthropic API 时必须携带的 beta 头。
pub const OAUTH_BETA_HEADER: &str = "oauth-2025-04-20";

/// 官方上游 API base（代理转发目标）。
pub const UPSTREAM_BASE_URL: &str = "https://api.anthropic.com";

/// 距离过期不足该秒数时视为需要刷新。
pub const REFRESH_LEEWAY_SECS: u64 = 300;
