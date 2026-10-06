//! Claude Code OAuth 常量与配置。
//!
//! 这些是 Claude Code 官方客户端使用的公开 OAuth 参数，luban 复用它们
//! 以完成「用 Claude 订阅账号登录」的授权流程。

mod betas;
mod client;
mod endpoints;
mod headers;
mod keepalive;
mod profile;
mod telemetry;
mod v2_1_258;
mod v2_1_260;
mod v2_1_270;
mod v2_1_277;
mod v2_1_280;
mod v2_1_285;
mod v2_1_291;

pub use betas::*;
pub use client::*;
pub use endpoints::*;
pub use headers::*;
pub use keepalive::*;
pub use profile::*;
pub use telemetry::*;
pub use v2_1_258::*;
pub use v2_1_260::*;
pub use v2_1_270::*;
pub use v2_1_277::*;
pub use v2_1_280::*;
pub use v2_1_285::*;
pub use v2_1_291::*;

#[cfg(test)]
mod tests {
    use super::*;

    /// [`CC_TOOL_NAMES`] 不得有重名（重名说明两段之间抄串了），且几个已证实的老版本旧名
    /// 必须在——它们不在时老版本 CC 的 `Glob` / `Grep` / `Task` 会被混淆成 `mcp__luban__*`。
    #[test]
    fn cc_tool_names_are_unique_and_cover_legacy_official_names() {
        let mut seen = std::collections::HashSet::new();
        for n in CC_TOOL_NAMES {
            assert!(seen.insert(*n), "白名单重名: {n}");
        }
        for legacy in [
            "Glob",
            "Grep",
            "Task",
            "TodoWrite",
            "KillShell",
            "BashOutput",
            "EndConversation",
            "ArtifactComments",
        ] {
            assert!(CC_TOOL_NAMES.contains(&legacy), "缺老版本官方名 {legacy}");
        }
        // 二进制里的内部 / feature-gate 工具没证实官方发过，不该混进来。
        for unverified in ["REPL", "JavaScript", "TeamCreate", "SuggestConnectors"] {
            assert!(!CC_TOOL_NAMES.contains(&unverified), "{unverified} 未证实，不该在白名单");
        }
    }

    /// 补发的额度探测跟着会话版本：SDK 版本头也得是那一版的（2.1.285 的会话里是 0.127.0，不是
    /// 当前模拟版本的 0.128.0）。
    #[test]
    fn stainless_version_follows_the_client_version() {
        assert_eq!(cc_stainless_version("2.1.291"), Some("0.128.0"));
        assert_eq!(cc_stainless_version("2.1.300"), Some("0.128.0"));
        assert_eq!(cc_stainless_version("2.1.285"), Some("0.127.0"));
        assert_eq!(
            cc_stainless_version("2.1.290"),
            Some("0.128.0"),
            "2.1.288 起的可执行文件都是它"
        );
        assert_eq!(cc_stainless_version("2.1.288"), Some("0.128.0"));
        assert_eq!(cc_stainless_version("2.1.287"), Some("0.127.0"));
        assert_eq!(cc_stainless_version("2.1.280"), Some("0.112.1"));
        assert_eq!(cc_stainless_version("2.1.251"), Some("0.112.1"));
        assert_eq!(cc_stainless_version("2.1.250"), None, "没样本的不猜");
        assert_eq!(cc_stainless_version("x"), None);
        // 模拟路径那张表里的值就是当前版本的。
        let table =
            CC_SIM_HEADERS.iter().find(|(k, _)| *k == "x-stainless-package-version").unwrap().1;
        assert_eq!(Some(table), cc_stainless_version(CC_VERSION_BASE));
    }

    /// [`CC_LATEST_KNOWN_RELEASE`] 是可信版本的下限：低于模拟版本会把 luban 自己发出去的
    /// 版本判成「不存在」，低于 2.1.270 表的版本则那张表永远选不中。
    #[test]
    fn latest_known_release_is_not_behind_any_profile_table() {
        let v = |s: &str| crate::proxy::parse_version(s).unwrap();
        let latest = v(CC_LATEST_KNOWN_RELEASE);
        assert!(latest >= v(CC_VERSION_BASE), "低于模拟版本 {CC_VERSION_BASE}");
        for p in cc_profile_tables().iter().flat_map(|t| t.iter()) {
            assert!(latest >= v(p.version), "{:?} 的 {} 表选不中", p.kind, p.version);
        }
    }

    /// [`cc_eager_tools_at`] 只认版本精确命中的行：2.1.270 的 opus 不继承 2.1.260 的 On，
    /// 样本之间的版本（2.1.259 / 2.1.261）与读不出版本都是 Unknown——与 [`cc_profile_at`]
    /// 给 beta 用的兜底分开。
    #[test]
    fn cc_eager_tools_at_requires_an_exact_version_match() {
        use CcEagerTools::{Off, On, Unknown};
        use CcProfileKind::*;
        assert_eq!(cc_eager_tools_at(MainOpus, Some((2, 1, 258))), On);
        assert_eq!(cc_eager_tools_at(MainOpus, Some((2, 1, 260))), On);
        assert_eq!(cc_eager_tools_at(MainFable, Some((2, 1, 260))), Off);
        assert_eq!(cc_eager_tools_at(MainSonnet, Some((2, 1, 270))), On);
        assert_eq!(cc_eager_tools_at(SdkSubagentHaiku, Some((2, 1, 260))), Off);
        assert_eq!(cc_eager_tools_at(MainOpus, Some((2, 1, 270))), Unknown, "opus 2.1.270 没样本");
        assert_eq!(cc_eager_tools_at(MainOpus, Some((2, 1, 259))), Unknown);
        assert_eq!(cc_eager_tools_at(MainOpus, Some((2, 1, 261))), Unknown);
        assert_eq!(
            cc_eager_tools_at(MainSonnet, Some((2, 1, 260))),
            Unknown,
            "外推行记的就是 Unknown"
        );
        assert_eq!(cc_eager_tools_at(MainOpus, None), Unknown);
        // 2.1.277 四族主线程与子代理全 On，fable 也是（2.1.260 时 Off）。
        for kind in [MainOpus, MainFable, MainSonnet, MainHaiku, SdkSubagentHaiku] {
            assert_eq!(cc_eager_tools_at(kind, Some((2, 1, 277))), On, "{kind:?}");
        }
        // 2.1.280 四族主线程全 On；子代理没有样本。
        for kind in [MainOpus, MainFable, MainSonnet, MainHaiku] {
            assert_eq!(cc_eager_tools_at(kind, Some((2, 1, 280))), On, "{kind:?}");
        }
        assert_eq!(cc_eager_tools_at(SdkSubagentHaiku, Some((2, 1, 280))), Unknown);
        // 2.1.285 四族主线程全 On（11 个模型的 14 个内建工具全带），子代理（`00120`）也是。
        for kind in [MainOpus, MainFable, MainSonnet, MainHaiku, SdkSubagentHaiku] {
            assert_eq!(cc_eager_tools_at(kind, Some((2, 1, 285))), On, "{kind:?}");
        }
        assert_eq!(cc_eager_tools_at(MainFable, Some((2, 1, 276))), Unknown);
        // 对照：beta 参照会落回最近一版，这里不会。
        assert_eq!(cc_profile_at(MainOpus, Some((2, 1, 270))).eager_tools, On);
    }

    /// [`cc_profile_at`] 的六档：<2.1.260 与读不出版本取 2.1.258 表；2.1.260 ~ 2.1.269 取
    /// 2.1.260 表；2.1.270 ~ 2.1.276 先查 2.1.270 表，**只有 sonnet 有行**；2.1.277 ~ 2.1.279
    /// 查 2.1.277 表，helper 与安全分类没行；2.1.280 ~ 2.1.284 查 2.1.280 表，子代理与标题再落回
    /// 2.1.277 表；≥2.1.285 查 2.1.285 表，子代理依次落回 2.1.280、2.1.277 表。查不到的 kind
    /// 最后一律落回 2.1.260 表（没有样本不外推）。
    #[test]
    fn cc_profile_at_picks_the_newest_observed_table_per_kind() {
        use CcProfileKind::*;
        for kind in [
            MainOpus,
            MainFable,
            MainSonnet,
            MainHaiku,
            SdkSubagentHaiku,
            SessionTitleHaiku,
            QuotaProbe,
        ] {
            assert_eq!(cc_profile_at(kind, Some((2, 1, 277))).version, "2.1.277", "{kind:?}");
            assert_eq!(cc_profile_at(kind, Some((2, 1, 279))).version, "2.1.277", "{kind:?}");
        }
        // 2.1.280 表只有主线程四族与额度探测。
        for kind in [MainOpus, MainFable, MainSonnet, MainHaiku, QuotaProbe] {
            assert_eq!(cc_profile_at(kind, Some((2, 1, 280))).version, "2.1.280", "{kind:?}");
            assert_eq!(cc_profile_at(kind, Some((2, 1, 284))).version, "2.1.280", "{kind:?}");
        }
        for kind in [SdkSubagentHaiku, SessionTitleHaiku] {
            assert_eq!(cc_profile_at(kind, Some((2, 1, 280))).version, "2.1.277", "{kind:?}");
        }
        // 2.1.285 表多一行标题生成；2.1.285 ~ 2.1.290 查它。
        for kind in [MainOpus, MainFable, MainSonnet, MainHaiku, SessionTitleHaiku, QuotaProbe] {
            assert_eq!(cc_profile_at(kind, Some((2, 1, 285))).version, "2.1.285", "{kind:?}");
            assert_eq!(cc_profile_at(kind, Some((2, 1, 290))).version, "2.1.285", "{kind:?}");
        }
        // 2.1.291 表：主线程四族、标题、helper、额度探测；模拟路径用它。
        for kind in [
            MainOpus,
            MainFable,
            MainSonnet,
            MainHaiku,
            SessionTitleHaiku,
            HelperSubagentHaiku,
            QuotaProbe,
        ] {
            assert_eq!(cc_profile_at(kind, Some((2, 1, 291))).version, "2.1.291", "{kind:?}");
            assert_eq!(cc_profile_at(kind, Some((2, 1, 300))).version, "2.1.291", "{kind:?}");
            assert_eq!(cc_profile(kind).version, "2.1.291", "模拟路径用 2.1.291 表: {kind:?}");
        }
        // 第二批抓包补了子代理与 helper；子代理 2.1.291 没样本，落回 2.1.285。
        for kind in [SdkSubagentHaiku, HelperSubagentHaiku] {
            assert_eq!(cc_profile_at(kind, Some((2, 1, 285))).version, "2.1.285", "{kind:?}");
        }
        assert_eq!(cc_profile_at(SdkSubagentHaiku, Some((2, 1, 291))).version, "2.1.285");
        assert_eq!(cc_profile(SdkSubagentHaiku).version, "2.1.285");
        assert_eq!(cc_profile_at(HelperSubagentHaiku, Some((2, 1, 280))).version, "2.1.260");
        for kind in [SecurityClassifierSonnet] {
            for v in [(2, 1, 277), (2, 1, 280), (2, 1, 285), (2, 1, 291)] {
                assert_eq!(cc_profile_at(kind, Some(v)).version, "2.1.260", "{kind:?} {v:?}");
            }
            assert_eq!(cc_profile(kind).version, "2.1.260", "{kind:?}");
        }
        assert!(cc_profile_exact(MainOpus, "2.1.280").is_some());
        assert!(cc_profile_exact(MainOpus, "2.1.285").is_some());
        assert!(cc_profile_exact(MainOpus, "2.1.291").is_some());
        assert!(cc_profile_exact(SdkSubagentHaiku, "2.1.291").is_none(), "2.1.291 没有子代理样本");
        assert!(cc_profile_exact(SdkSubagentHaiku, "2.1.280").is_none(), "2.1.280 没有子代理样本");
        assert!(cc_profile_exact(SdkSubagentHaiku, "2.1.285").is_some(), "cap/2.1.285/00120");
        assert!(cc_profile_exact(MainOpus, "2.1.270").is_none(), "那一版只有 sonnet");
        assert!(cc_profile_exact(MainOpus, "2.1.276").is_none());
        assert_eq!(cc_profile_at(MainSonnet, Some((2, 1, 270))).version, "2.1.270");
        assert_eq!(cc_profile_at(MainSonnet, Some((2, 1, 276))).version, "2.1.270");
        assert_eq!(cc_profile_at(MainSonnet, Some((2, 1, 269))).version, "2.1.260");
        assert_eq!(cc_profile_at(MainSonnet, Some((2, 1, 260))).version, "2.1.260");
        assert_eq!(cc_profile_at(MainSonnet, Some((2, 1, 259))).version, "2.1.258");
        assert_eq!(cc_profile_at(MainSonnet, None).version, "2.1.258");
        for kind in [MainOpus, MainFable, MainHaiku, SessionTitleHaiku, QuotaProbe] {
            assert_eq!(cc_profile_at(kind, Some((2, 1, 270))).version, "2.1.260", "{kind:?}");
        }
        // 2.1.258 表只有主线程四族，其余 kind 在老版本下也落回 2.1.260 表。
        assert_eq!(cc_profile_at(QuotaProbe, Some((2, 1, 258))).version, "2.1.260");
        // 2.1.270 表里的每一行都得是 2.1.270 的，且 kind 不重复。
        let mut kinds: Vec<_> = CC_PROFILES_2_1_270.iter().map(|p| p.kind).collect();
        assert!(CC_PROFILES_2_1_270.iter().all(|p| p.version == "2.1.270"));
        kinds.dedup();
        assert_eq!(kinds.len(), CC_PROFILES_2_1_270.len());
    }

    /// 模型名 → 代际：`cap/2.1.285` 那 11 个模型逐个钉住，再加几种写法（带日期、带 `[1m]`、
    /// 族名在前的老写法、认不出的）。
    #[test]
    fn model_tier_follows_the_2_1_285_captures() {
        use CcModelTier::*;
        for (model, tier) in [
            ("claude-opus-5-5", Latest),
            ("claude-fable-5-1", Latest),
            ("claude-sonnet-5-5", Latest),
            ("claude-haiku-4-5-20251001", Latest),
            ("claude-sonnet-5", Gen5),
            ("claude-opus-5", Gen5),
            ("claude-fable-5", Gen5),
            ("claude-opus-4-8", Gen5),
            ("claude-opus-4-7", Legacy),
            ("claude-opus-4-6", Legacy),
            ("claude-sonnet-4-6", Legacy),
            // 抓包之外的写法。
            ("claude-opus-4-6[1m]", Legacy),
            ("claude-sonnet-4-5-20250929", Legacy),
            ("claude-3-7-sonnet-20250219", Legacy),
            ("claude-opus-4-1-20250805", Legacy),
            ("claude-opus-6", Latest),
            ("claude-sonnet-5-5[1m]", Latest),
            ("claude-opus", Latest),
            ("gpt-4o", Latest),
        ] {
            assert_eq!(cc_model_tier(model), tier, "{model}");
        }
        // haiku 与非主线程 profile 不去项。
        let haiku = cc_profile(CcProfileKind::MainHaiku);
        assert_eq!(cc_model_beta(haiku, "claude-haiku-3-5"), haiku.beta);
        let probe = cc_profile(CcProfileKind::QuotaProbe);
        assert_eq!(cc_model_beta(probe, "claude-opus-4-6"), probe.beta);
    }

    /// 规整只做两件事：压空白、按输入顺序去重。**不排序**——scope 集合是指纹的一部分，
    /// 用户照抄一份抓包的顺序就该原样发出去。
    #[test]
    fn normalize_keeps_the_order_it_was_given() {
        assert_eq!(
            normalize_scopes("  user:profile\n\tuser:inference  "),
            "user:profile user:inference"
        );
        assert_eq!(
            normalize_scopes("user:inference user:profile user:inference"),
            "user:inference user:profile"
        );
        assert_eq!(normalize_scopes("   "), "");
        // 默认那两串本身已经是规整形态（写常量时手抖多个空格也能被这条测出来）。
        assert_eq!(normalize_scopes(SCOPES), SCOPES);
        assert_eq!(normalize_scopes(SCOPES_MINIMAL), SCOPES_MINIMAL);
    }

    /// 不校验：探边界用的怪值、写错的分隔符、没见过的 scope 一律照收——拦下来就没法用这个
    /// 输入框去问上游「你认不认这个」了。只有空白会被压掉（空串 = 用默认值）。
    #[test]
    fn anything_goes_in_it_comes_out_normalized() {
        assert_eq!(normalize_scopes("user:inference-1"), "user:inference-1");
        assert_eq!(
            normalize_scopes("user:inference, user:profile"),
            "user:inference, user:profile"
        );
        assert_eq!(normalize_scopes("\"user:inference\""), "\"user:inference\"");
        assert_eq!(
            normalize_scopes("user:some_scope_invented_next_month"),
            "user:some_scope_invented_next_month"
        );
    }
}
