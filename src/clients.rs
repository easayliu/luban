//! 出站 HTTP 客户端与「逐账号代理」的客户端池。
//!
//! 一个凭证配了代理，它的**全部**出站流量就都得走那个代理——转发、token 刷新、profile
//! 拉取、连通性测试，一个都不能漏。漏一条的后果不是「慢一点」，而是那条请求带着真实出口
//! IP 打到上游，逐账号隔离当场失效，且从日志上完全看不出来。故取客户端的入口只有
//! [`ClientPool::for_credential`] 一个。

use anyhow::{Context, Result};

use crate::config;

/// 构造发往上游的 HTTP 客户端，刻意贴近官方客户端的传输形态：
/// - `http1_only`：官方客户端（Bun 自带的 HTTP 客户端）走 HTTP/1.1（抓包里有
///   `Connection`/`Host`，h2 不会有这两个头）。默认会经 ALPN 协商 h2，留下 h2 的
///   SETTINGS/伪头指纹；h2 还强制头名小写，逐头大小写也就无从谈起。
/// - `user_agent`：给 luban 自身发起的账号级请求（token 刷新、profile）兜底；转发 `/v1/*`
///   时来访客户端自己的 UA 会覆盖它。
/// - `default_headers` 里的 `accept-encoding`：**必须显式钉住**。开了解压 feature 后，
///   tower-http 的解压中间件会给「没带这个头」的请求补一个它自己的取值
///   `zstd,gzip,deflate,br`（顺序与写法都不是官方客户端会产生的）。
///
/// `proxy` 为 `Some` 时挂上代理，其余形态与直连那份**逐字节相同**——代理只改走法，不改
/// 请求本身，否则「配了代理的号」就多出一处与别的号不同的指纹。
pub fn upstream_client(proxy: Option<&str>) -> Result<wreq::Client> {
    use axum::http::{HeaderMap, HeaderValue, header::ACCEPT_ENCODING};

    let mut defaults = HeaderMap::new();
    defaults.insert(ACCEPT_ENCODING, HeaderValue::from_static(config::CC_ACCEPT_ENCODING));

    let builder = wreq::Client::builder()
        .http1_only()
        .user_agent(config::CC_USER_AGENT)
        .default_headers(defaults);
    let builder = match proxy {
        // `Proxy::all` 覆盖 http 与 https 两种目标；上游只有 https，但写 all 才不会因为
        // 哪天多一个 http 目标就悄悄绕开代理。
        Some(url) => {
            // **建之前必须再校验一次**：`Proxy::all` 成功不等于代理会生效（见
            // [`check_proxy_url`]），而库里完全可能存着这样一条——0.2.81 之前的入库校验拦不住，
            // 手工改库也能塞进来。校验不过就返回 Err，让 [`ClientPool::for_credential`] 把这个号
            // 整体判为不可用；绝不能建出一个「配置里有代理、实际直连」的客户端。
            check_proxy_url(url)?;
            builder
                .proxy(wreq::Proxy::all(url).with_context(|| format!("invalid proxy URL: {url}"))?)
        }
        // 不配代理时**不调用 `.no_proxy()`**：保留 wreq 默认的环境变量代理探测
        // （HTTPS_PROXY/ALL_PROXY 等），那是全局兜底，与逐账号代理各管一层。
        None => builder,
    };
    builder.build().context("failed to build the upstream HTTP client")
}

/// 代理 URL 支持的协议。
///
/// **socks4/socks4a 是刻意不收的**（0.2.82 起）。SOCKS4 协议里根本没有认证字段——只有一个
/// userid，而 wreq 的 V4 分支连 userid 都不发（`Socks4Stream::connect_with_socket`），于是 URL
/// 里写的 `user:pass@` 被**静默丢掉**；买来的代理十有八九要认证，结果就是一条看不出原因的连接
/// 失败。另一头 socks4a（把域名交给代理端解析的那种）是个扩展，靠把目标 IP 填成 `0.0.0.1` 这个
/// 哨兵值来示意「后面跟的是域名」，只会说纯 SOCKS4 的代理收到它直接挂。两条路都是坑，而
/// SOCKS4 本身早已绝迹，socks5h 能覆盖它的全部用途，所以干脆在入口就拒掉。
const PROXY_SCHEMES: &[&str] = &["http://", "https://", "socks5://", "socks5h://"];

/// 入库时归一化的协议：本机解析 → 交给代理端解析。
///
/// **为什么默认改掉**：luban 的出站目标永远是 `api.anthropic.com` 这一个公网域名，不存在
/// 内网域名那类非本机解析不可的场景。而本机解析有两个实打实的坏处——把目标域名泄露给本地
/// DNS（花钱买的是出口隔离，本地解析器上却留了一串查询记录），以及解析出的 IP 是按**你**的
/// 位置就近的（上游走 anycast），然后再通过一个异地代理去连它，既绕远又与「真实用户从那个
/// 出口访问」的形态对不上。此外大量住宅/移动代理压根不接受 IP 形式的连接请求，只回一个
/// `unexpected EOF`——错误文本上完全看不出是这个原因，实测被它绕过好一阵。
///
/// **归一化发生在入库那一刻，不是发请求时**：存 `socks5://` 却按 `socks5h://` 跑，库里的值
/// 与真实行为就对不上，下次出问题看着配置推不出行为。改成入库即改写，网页上保存完立刻能
/// 看到它变成了 `socks5h://`，配置与行为始终一致，也不必额外解释。
const PROXY_SCHEME_UPGRADES: &[(&str, &str)] = &[("socks5://", "socks5h://")];

/// 校验一条代理 URL 能不能用，能则返回规范化后的串（去空白 + 协议归一化，见
/// [`PROXY_SCHEME_UPGRADES`]）。判据本身在 [`check_proxy_url`]——**「能构造出 `wreq::Proxy`」
/// 是个远远不够的判据**，那里解释了为什么。
///
/// **在入库那一刻校验，而不是发请求时**：存进去一条建不出客户端的代理，故障要等到下一次
/// 真有请求选中这个号才暴露，那时现场只剩一条「所有请求都失败」。
pub fn validate_proxy(raw: &str) -> Result<String> {
    let url = raw.trim();
    anyhow::ensure!(!url.is_empty(), "the proxy URL must not be empty");
    let url = match PROXY_SCHEME_UPGRADES.iter().find(|(from, _)| url.starts_with(from)) {
        Some((from, to)) => format!("{to}{}", &url[from.len()..]),
        None => url.to_string(),
    };
    // 校验归一化之后那串——存什么就验什么，免得验的和跑的是两条 URL。
    let uri = check_proxy_url(&url)?;
    // wreq 把 path/query 整段丢掉（`builder.path_and_query("/")`）。运行时那道校验故意不管这个
    // （免得让一条本来在跑的老配置突然报废），但**入库时要拦**：存着一段不会生效的尾巴，下次
    // 照着配置推行为一定推错。
    anyhow::ensure!(
        matches!(uri.path(), "" | "/") && uri.query().is_none(),
        "the proxy URL must not have a path or query: {url}"
    );
    // 真去构造一次兜底：上面的判据是照 wreq 的实现逐条复现的，这一步保证「至少建得出来」。
    wreq::Proxy::all(&url).with_context(|| format!("invalid proxy URL: {url}"))?;
    Ok(url)
}

/// 校验一条代理 URL 会不会被 wreq **真正当成代理**，成功时返回解析出的 URI。
///
/// **为什么 `wreq::Proxy::all` 成功还不够**：那一步只要求「能解析成 `Uri`、且 scheme 与
/// authority 都在」，它连 scheme 是不是代理协议都不看。真正决定代理生不生效的是 wreq 内部的
/// `parse_env_uri`（名字像是只管环境变量，其实显式 `.proxy()` 也走它——`proxy/matcher.rs` 里
/// `Builder::build` 对 all/http/https 三份都调它）。它认不出来时**返回 `None` 而不报错**，
/// `Client::builder().build()` 照样成功，于是拿到一个「配置里有代理、实际没有代理」的客户端：
/// 请求带着真实 IP 直连打上游，日志上完全看不出来。实测会这样的几条：
///
/// - `socks5://u:pa#ss@h:1080` —— 密码里的裸 `#` 被当成 fragment 切掉，authority 塌成 `u:pa`；
/// - `socks5://h:notaport`、`socks5://h:99999` —— 端口不是合法 u16；
/// - `ftp://h:21` —— scheme 压根不是代理协议，wreq 认不出来就当没配代理。
///
/// 这条路把 [`ClientPool::for_credential`] 那句「配了代理就绝不退回直连」整个绕过去了，所以
/// 判据在这里照 wreq 的实现逐条复现，且**入库时与每次建客户端时各拦一道**（见
/// [`upstream_client`]）。
fn check_proxy_url(url: &str) -> Result<axum::http::Uri> {
    use axum::http::{Uri, uri::Authority};

    anyhow::ensure!(
        PROXY_SCHEMES.iter().any(|s| url.starts_with(s)),
        "unsupported proxy scheme (expected one of: {})",
        PROXY_SCHEMES.join(", ")
    );
    let uri: Uri = url.parse().with_context(|| format!("invalid proxy URL: {url}"))?;
    let authority = uri.authority().with_context(|| format!("the proxy URL has no host: {url}"))?;
    // userinfo 由 wreq 单独取走（`authority.as_str().rsplit_once('@')`），要能自成一个合法
    // authority 的是 host:port 那半。
    let host_port = authority.as_str().rsplit_once('@').map_or(authority.as_str(), |(_, hp)| hp);
    let host_port: Authority = host_port.parse().with_context(|| {
        format!(
            "invalid host:port in the proxy URL: {url} \
             (special characters in user:pass@ must be percent-encoded, e.g. # as %23)"
        )
    })?;
    anyhow::ensure!(!host_port.host().is_empty(), "the proxy URL has no host: {url}");
    match host_port.port_u16() {
        // 没写端口时 wreq 给 socks 补 1080——`h:notaport` 会被拼成 `h:notaport:1080`，解析失败，
        // 代理就此静默失效。原地复现这一步，把它变成一条能看懂的错误。
        None => anyhow::ensure!(
            format!("{host_port}:1080").parse::<Authority>().is_ok(),
            "the proxy port must be a number in 1-65535: {url}"
        ),
        Some(0) => anyhow::bail!("the proxy port must not be 0: {url}"),
        Some(_) => {}
    }
    Ok(uri)
}

/// 「代理 URL → 客户端」缓存。
///
/// **必须缓存**：`wreq::Client` 自带连接池与 TLS 配置，每请求新建一个等于每请求重做 TLS
/// 握手、且连接永远复用不上——对走代理的号来说这是成倍的延迟。反过来 `Client` 内部是 Arc，
/// clone 极廉价，所以对外一律返回 clone。
pub struct ClientPool {
    /// 不配代理的号共用这一份（它仍可能受环境变量代理影响，见 [`upstream_client`]）。
    direct: wreq::Client,
    /// 代理 URL → 该代理的客户端。用 `RwLock` 而非 `Mutex`：命中缓存是绝对多数，
    /// 而那条路只需要读锁。
    by_proxy: parking_lot::RwLock<std::collections::HashMap<String, wreq::Client>>,
}

impl ClientPool {
    pub fn new() -> Result<Self> {
        Ok(Self { direct: upstream_client(None)?, by_proxy: Default::default() })
    }

    /// 不绑定任何凭证的出站客户端（测试、以及登录时未指定代理的兜底）。
    ///
    /// 登录换码时如果用户指定了代理，[`crate::web::exchange`] 会临时构建一个走代理的
    /// 客户端；没指定时退回这份直连。
    pub fn direct(&self) -> &wreq::Client {
        &self.direct
    }

    /// 取该凭证该用的客户端。没配代理时返回直连那份。
    ///
    /// **配了代理但建不出客户端时返回 Err，绝不退回直连**：退回去就是拿真实 IP 去打上游，
    /// 而调用方恰恰是为了不这么做才配的代理。宁可这个号整体不可用（错误会被上层记成刷新
    /// 失败/转发失败，人能看见），也不要静默泄露。
    pub fn for_credential(&self, cred: &crate::credentials::Credential) -> Result<wreq::Client> {
        let Some(url) = cred.proxy.as_deref().map(str::trim).filter(|s| !s.is_empty()) else {
            return Ok(self.direct.clone());
        };
        if let Some(client) = self.by_proxy.read().get(url) {
            return Ok(client.clone());
        }
        // 双检：两条请求同时首次用上同一个代理时，谁先拿到写锁谁建，另一条直接复用。
        let mut table = self.by_proxy.write();
        if let Some(client) = table.get(url) {
            return Ok(client.clone());
        }
        let client = upstream_client(Some(url))
            .with_context(|| format!("credential #{} has an unusable proxy", cred.id))?;
        table.insert(url.to_string(), client.clone());
        tracing::info!(cred_id = cred.id, cred = %cred.label, proxy = %url, "built a proxied upstream client");
        Ok(client)
    }

    /// 丢弃某条代理 URL 的缓存客户端。改/清代理后调用，免得旧连接池继续把请求送去老代理。
    pub fn forget(&self, url: &str) {
        if self.by_proxy.write().remove(url.trim()).is_some() {
            tracing::info!(proxy = %url.trim(), "dropped the cached client for a proxy");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cred_with(proxy: Option<&str>) -> crate::credentials::Credential {
        crate::credentials::Credential {
            id: 1,
            label: "t".into(),
            tier: None,
            org_type: None,
            rate_limit_tier: None,
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_at: 0,
            priority: 0,
            disabled: false,
            device_limit: 0,
            rpm_limit: 0,
            ban_reason: None,
            account_uuid: None,
            org_uuid: None,
            subscription_created_at: None,
            resume_at: None,
            proxy: proxy.map(str::to_string),
            created_at: 0,
            updated_at: 0,
        }
    }

    /// 四种协议都收，别的一律拒——尤其别把 `socks5` 这种漏了 `://` 的写法放进去。
    /// socks4/socks4a 也在拒的那一列，理由见 [`PROXY_SCHEMES`]。
    #[test]
    fn accepts_the_documented_schemes_only() {
        for ok in [
            "socks5://127.0.0.1:1080",
            "socks5h://user:pass@example.com:1080",
            "http://127.0.0.1:8080",
            "https://proxy.example.com:8443",
        ] {
            assert!(validate_proxy(ok).is_ok(), "该收: {ok}");
        }
        for bad in [
            "",
            "   ",
            "127.0.0.1:1080",
            "socks5:127.0.0.1:1080",
            "ftp://x:1",
            "socks6://x:1",
            // SOCKS4 带不了认证、socks4a 又只有部分代理认，一律不收。
            "socks4://10.0.0.1:1080",
            "socks4a://10.0.0.1:1080",
            "socks4://u:p@example.com:1080",
        ] {
            assert!(validate_proxy(bad).is_err(), "该拒: {bad}");
        }
    }

    /// 首尾空白吃掉——网页上粘贴代理串时最容易带进来的就是这个。
    #[test]
    fn trims_surrounding_whitespace() {
        assert_eq!(
            validate_proxy("  socks5h://127.0.0.1:1080 \n").unwrap(),
            "socks5h://127.0.0.1:1080"
        );
    }

    /// **本机解析的 socks5 入库时改写成交给代理端解析的 socks5h**，理由见
    /// [`PROXY_SCHEME_UPGRADES`]。改写在入库那一刻发生，故库里存的就是实际会跑的那条。
    #[test]
    fn upgrades_local_dns_schemes_on_save() {
        for (input, want) in [
            ("socks5://127.0.0.1:1080", "socks5h://127.0.0.1:1080"),
            // 账号密码、端口一并原样带过去，只换协议头。
            ("socks5://u:p@example.com:16901", "socks5h://u:p@example.com:16901"),
        ] {
            assert_eq!(validate_proxy(input).unwrap(), want, "该归一化: {input}");
        }
        // 已经是目标形态、以及 http(s) 那两种，一律原样不动。
        for same in
            ["socks5h://127.0.0.1:1080", "http://127.0.0.1:8080", "https://proxy.example.com:8443"]
        {
            assert_eq!(validate_proxy(same).unwrap(), same, "不该动: {same}");
        }
    }

    /// **能构造出 `wreq::Proxy` 不等于代理会生效**：下面这些串都能构造出来，但 wreq 内部认不
    /// 出来就静默不挂代理，请求带着真实 IP 直连打上游（判据与实测见 [`check_proxy_url`]）。
    /// 一条都不能放进库里。
    #[test]
    fn rejects_urls_that_wreq_would_silently_ignore() {
        for bad in [
            // 密码里的裸 `#`/`/`：`#` 之后被当 fragment 切掉，authority 塌成 `u:pa`。
            "socks5://u:pa#ss@example.com:1080",
            "socks5://u:pa/ss@example.com:1080",
            // 端口不是合法 u16（`notaport`/越界/0）。
            "socks5://example.com:notaport",
            "socks5://example.com:99999",
            "socks5://example.com:0",
            "http://u:pa#ss@example.com:8080",
            "http://example.com:notaport",
            // path/query 会被 wreq 整段丢掉，别让它进库。
            "socks5://example.com:1080/path",
            "http://example.com:8080?a=1",
        ] {
            assert!(validate_proxy(bad).is_err(), "该拒: {bad}");
        }
        // 正常形态照收：percent-encode 过的密码、IPv6、以及不写端口（wreq 给 socks 补 1080）。
        for ok in [
            "socks5h://u:pa%23ss@example.com:1080",
            "socks5h://[::1]:1080",
            "socks5h://example.com",
            "http://example.com:8080/",
        ] {
            assert!(validate_proxy(ok).is_ok(), "该收: {ok}");
        }
    }

    /// **护栏：凡是 [`validate_proxy`] 放行的串，wreq 都必须真把它当代理用。**
    ///
    /// [`check_proxy_url`] 的判据是照 wreq 当前实现（`proxy/matcher.rs` 的 `parse_env_uri`）
    /// 逐条复现的，wreq 一升级就可能漂。漂了的话故障形态是最坏的那种——静默直连、真实 IP 打
    /// 上游、日志上完全看不出来——所以这里拿一个本地监听口**当目标**钉住它：那个口一旦收到
    /// 连接，就说明代理没挂上、请求直连了。
    ///
    /// 全程只碰 loopback：代理指向本机的死端口，连接立刻被拒，不做 DNS 解析也不等超时。
    #[tokio::test]
    async fn every_accepted_proxy_url_actually_gets_used() {
        let target_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = format!("http://{}/", target_listener.local_addr().unwrap());

        // 先证明这套装置分得清「走代理」和「直连」：拿一个**真在监听**的本地口当代理，请求就
        // 必须落在代理口上。（我们不回应 SOCKS 握手，所以那条请求最终会失败——不关心。）
        let proxy_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy_port = proxy_listener.local_addr().unwrap().port();
        let url = validate_proxy(&format!("socks5://127.0.0.1:{proxy_port}")).unwrap();
        let client = upstream_client(Some(&url)).unwrap();
        let sent = target.clone();
        tokio::spawn(async move {
            let _ = client.get(&sent).send().await;
        });
        tokio::select! {
            _ = proxy_listener.accept() => {}
            _ = target_listener.accept() => panic!("请求没走代理，直连到了目标"),
            _ = tokio::time::sleep(std::time::Duration::from_secs(5)) => {
                panic!("代理口和目标口都没收到连接，这个测试装置已经失效")
            }
        }

        // 端口 1 上不会有东西在听：连代理这一步立刻被拒，请求到不了目标——除非代理没挂上。
        for raw in [
            "socks5://u:p@127.0.0.1:1",
            "socks5h://u:pa%23ss@127.0.0.1:1",
            "socks5h://[::1]:1",
            "http://u:p@127.0.0.1:1",
            "https://127.0.0.1:1",
        ] {
            let url = validate_proxy(raw).unwrap_or_else(|e| panic!("该收 {raw}: {e:#}"));
            let client = upstream_client(Some(&url)).expect("该建得出客户端");
            let _ = client.get(&target).send().await;

            let leaked = tokio::time::timeout(
                std::time::Duration::from_millis(10),
                target_listener.accept(),
            )
            .await
            .is_ok();
            assert!(!leaked, "代理被静默绕过、请求直连到了目标：{raw} → {url}");
        }
    }

    /// 老库里可能已经存着一条 wreq 会静默忽略的代理串（0.2.81 之前的入库校验拦不住，手工改库
    /// 也能塞进来）。那种串**必须**让这个号整体不可用：建出一个「看着配了代理、实际直连」的
    /// 客户端，等于拿真实 IP 打上游而且日志上看不出来——比这个号不可用坏得多。
    #[test]
    fn a_silently_ignored_proxy_errors_instead_of_going_direct() {
        let pool = ClientPool::new().unwrap();
        for stored in [
            "socks5h://example.com:notaport",
            "socks5h://u:pa#ss@example.com:1080",
            "ftp://example.com:21",
        ] {
            let Err(err) = pool.for_credential(&cred_with(Some(stored))) else {
                panic!("wreq 会静默忽略的代理串该报错，而不是直连出去: {stored}");
            };
            assert!(format!("{err:#}").contains("proxy"), "错误该点明是代理的问题: {err:#}");
        }
        assert_eq!(pool.by_proxy.read().len(), 0, "建失败不该留下缓存");
    }

    /// 没配代理 → 直连那一份；配了 → 另一份，且同一个 URL 第二次取命中缓存（同一个客户端）。
    #[test]
    fn caches_one_client_per_proxy_url() {
        let pool = ClientPool::new().unwrap();
        assert_eq!(pool.by_proxy.read().len(), 0);

        pool.for_credential(&cred_with(None)).expect("直连不该失败");
        assert_eq!(pool.by_proxy.read().len(), 0, "没配代理不该建缓存");

        let c = cred_with(Some("socks5://127.0.0.1:1080"));
        pool.for_credential(&c).expect("建代理客户端不该失败（不连接，只是配置）");
        pool.for_credential(&c).expect("第二次该命中缓存");
        assert_eq!(pool.by_proxy.read().len(), 1, "同一个 URL 只该建一份");

        pool.for_credential(&cred_with(Some("socks5://127.0.0.1:1081"))).unwrap();
        assert_eq!(pool.by_proxy.read().len(), 2, "不同 URL 各一份");

        pool.forget("socks5://127.0.0.1:1080");
        assert_eq!(pool.by_proxy.read().len(), 1, "forget 该把那一份清掉");
    }

    /// 空串/纯空白视同没配，走直连——避免网页上把输入框清空后留下一个 `""` 就让号不可用。
    #[test]
    fn blank_proxy_is_treated_as_unset() {
        let pool = ClientPool::new().unwrap();
        pool.for_credential(&cred_with(Some("   "))).expect("空白该当没配");
        assert_eq!(pool.by_proxy.read().len(), 0);
    }

    /// 配了代理但那条 URL 建不出客户端 → 返回 Err，**不退回直连**。
    /// 退回去就是拿真实 IP 打上游，而这个号配代理正是为了不这么做。
    #[test]
    fn a_broken_proxy_errors_instead_of_falling_back() {
        let pool = ClientPool::new().unwrap();
        let Err(err) = pool.for_credential(&cred_with(Some("socks5://"))) else {
            panic!("建不出客户端的代理该报错，而不是退回直连");
        };
        assert!(format!("{err:#}").contains("proxy"), "错误该点明是代理的问题: {err:#}");
        assert_eq!(pool.by_proxy.read().len(), 0, "建失败不该留下缓存");
    }
}

#[cfg(test)]
mod live {
    /// 拿一条**真实代理**打一发，打印出口 IP。默认不跑，需显式给 `LUBAN_TEST_PROXY`：
    ///
    /// ```sh
    /// LUBAN_TEST_PROXY=socks5h://user:pass@host:1080 \
    ///   cargo test -- --ignored --nocapture live::
    /// ```
    ///
    /// **排查「代理配了但不通」时先跑它**：它把代理这一层单独摘出来，能立刻分清是代理本身
    /// 的问题还是 luban 的问题。尤其注意 `socks5://` 与 `socks5h://` 的差别——前者在本机解析
    /// DNS、把 IP 交给代理，不少代理（住宅/移动出口居多）只接受域名形式，于是只回一个
    /// `unexpected EOF`，从错误文本上完全看不出是这个原因。换成 `socks5h://` 即可。
    #[tokio::test]
    #[ignore]
    async fn through_a_real_proxy() {
        let url = std::env::var("LUBAN_TEST_PROXY")
            .expect("set LUBAN_TEST_PROXY=socks5h://user:pass@host:port");
        let client = super::upstream_client(Some(&url)).expect("failed to build the client");
        match client.get("https://api.ipify.org").send().await {
            Ok(r) => println!("status={} exit_ip={:?}", r.status(), r.text().await),
            Err(e) => {
                println!("FAILED: {e}");
                let mut src = std::error::Error::source(&e);
                while let Some(s) = src {
                    println!("  caused by: {s}");
                    src = s.source();
                }
                panic!("could not reach the upstream through {url}");
            }
        }
    }
}
