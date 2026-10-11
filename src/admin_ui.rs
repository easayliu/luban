//! 使用 rust-embed 将 `admin-ui/dist` 前端构建产物内嵌进二进制并提供静态服务。
//!
//! 参考 kiro.rs 的做法：SPA fallback + 按路径设置缓存策略。

use std::sync::LazyLock;

use axum::{
    body::Body,
    http::{HeaderValue, Response, StatusCode, Uri, header},
    response::{IntoResponse, Redirect},
};
use base64::Engine;
use rust_embed::Embed;
use sha2::{Digest, Sha256};
use tower_http::compression::{
    CompressionLayer, DefaultPredicate, Predicate, predicate::NotForContentType,
};

/// 内嵌前端构建产物（编译期从 `admin-ui/dist` 读取）。
#[derive(Embed)]
#[folder = "admin-ui/dist"]
struct Asset;

/// 静态资源的响应压缩层——只挂在前端这几条路由上，**不能**套到 `/v1/*`：
/// 那边是 SSE 流式转发，中间压一层会把逐块下发攒成整包，客户端看到的就是"卡到最后一起出"。
///
/// 前端产物由 rust-embed 原样嵌进二进制、没有预压缩，主 bundle 近 1 MB、懒加载的设置页
/// 也有 120 多 KB；远程访问时切到设置页会先白屏等这段下载。gzip/br 压完只剩三成左右，
/// 而 `assets/` 又带 immutable 缓存，同一浏览器只会付一次这笔 CPU。
pub fn compression() -> CompressionLayer<impl Predicate> {
    // woff2 自带 brotli，再压一遍只出 CPU 不出字节；默认谓词已经排除 image/* 与 SSE。
    CompressionLayer::new()
        .gzip(true)
        .br(true)
        .compress_when(DefaultPredicate::new().and(NotForContentType::new("font/")))
}

/// 管理面（`/api/*` 与前端）统一加的安全响应头，挂成 `map_response` 中间件；`/v1/*` 不挂。
///
/// - `X-Frame-Options: DENY`：不许被别的站点嵌进 iframe 诱导点击（删号、显示 Key……）；
/// - `nosniff`：不让浏览器把 JSON / 资源猜成别的类型执行；
/// - `no-referrer`：地址栏的 hash 路由里有账号 id，别随外链带出去。
///
/// CSP 只对页面本身有意义，由 [`serve_index`] 单独加。
pub async fn security_headers(mut resp: Response<Body>) -> Response<Body> {
    let h = resp.headers_mut();
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("no-referrer"));
    resp
}

/// index.html 的 CSP：脚本只认同源文件，外加 index.html 里那段内联脚本（还原语言）的哈希——
/// 哈希按嵌进来的 index.html 现算，改了那段脚本不用跟着手改。登录 token 存在 localStorage，
/// 这道是哪天出现注入点时的兜底。样式放开 `unsafe-inline`：组件库大量用 `style` 属性。
static INDEX_CSP: LazyLock<HeaderValue> = LazyLock::new(|| {
    let html = Asset::get("index.html").map(|c| c.data.into_owned()).unwrap_or_default();
    let hashes: String = inline_scripts(&String::from_utf8_lossy(&html))
        .map(|js| {
            let digest = Sha256::digest(js.as_bytes());
            format!(" 'sha256-{}'", base64::engine::general_purpose::STANDARD.encode(digest))
        })
        .collect();
    let csp = format!(
        "default-src 'self'; script-src 'self'{hashes}; style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: blob:; font-src 'self' data:; connect-src 'self'; \
         object-src 'none'; base-uri 'none'; form-action 'self'; frame-ancestors 'none'"
    );
    HeaderValue::from_str(&csp).expect("CSP 只含可见 ASCII")
});

/// 页面里不带 `src` 的 `<script>` 的内容（原样，CSP 哈希按它逐字节算）。
fn inline_scripts(html: &str) -> impl Iterator<Item = &str> {
    html.split("<script").skip(1).filter_map(|rest| {
        let (attrs, after) = rest.split_once('>')?;
        if attrs.contains("src=") {
            return None;
        }
        after.split_once("</script>").map(|(js, _)| js)
    })
}

/// 将误发到首页的 POST 文档导航转换为 GET，避免浏览器刷新时要求重新提交表单。
///
/// 固定跳回 `/`，不复用请求体或查询参数；真正的 API POST 会先被主路由匹配，不会走这里。
pub async fn redirect_root_post() -> Redirect {
    Redirect::to("/")
}

/// 作为整个应用的 fallback：命中静态资源则返回，否则 SPA fallback 到 index.html。
/// （`/api/*` 由主路由先行匹配，不会走到这里。）
pub async fn fallback(uri: Uri) -> impl IntoResponse {
    let path = uri.path().trim_start_matches('/');

    if path.contains("..") {
        return Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .body(Body::from("Invalid path"))
            .expect("build response");
    }

    // 直接打 `/index.html` 也是控制台本身，得和 SPA 兜底一样带上 CSP，不能当普通资源回。
    if path == "index.html" {
        return serve_index();
    }

    if let Some(content) = Asset::get(path) {
        let mime = mime_guess::from_path(path).first_or_octet_stream().to_string();
        return Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, mime)
            .header(header::CACHE_CONTROL, cache_control(path))
            .body(Body::from(content.data.into_owned()))
            .expect("build response");
    }

    // 非资源路径（无扩展名）→ SPA fallback 到 index.html。
    if !is_asset_path(path) {
        return serve_index();
    }

    Response::builder()
        .status(StatusCode::NOT_FOUND)
        .body(Body::from("Not found"))
        .expect("build response")
}

fn serve_index() -> Response<Body> {
    match Asset::get("index.html") {
        Some(content) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "text/html; charset=utf-8")
            .header(header::CACHE_CONTROL, "no-cache")
            .header(header::CONTENT_SECURITY_POLICY, INDEX_CSP.clone())
            .body(Body::from(content.data.into_owned()))
            .expect("build response"),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("Frontend not built yet. Run `pnpm build` in the admin-ui directory."))
            .expect("build response"),
    }
}

fn cache_control(path: &str) -> &'static str {
    if path.ends_with(".html") {
        "no-cache"
    } else if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    }
}

fn is_asset_path(path: &str) -> bool {
    path.rsplit('/').next().map(|f| f.contains('.')).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{
        Router,
        body::to_bytes,
        http::{Method, Request, header},
        routing::get,
    };
    use tower::ServiceExt;

    fn app() -> Router {
        Router::new()
            .route("/", get(fallback).post(redirect_root_post))
            .fallback_service(get(fallback))
    }

    #[tokio::test]
    async fn spa_fallback_serves_unknown_get_route() {
        let response = app()
            .oneshot(
                Request::builder()
                    .uri("/unknown/route")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("serve request");

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get(header::CONTENT_TYPE),
            Some(&header::HeaderValue::from_static("text/html; charset=utf-8"))
        );
    }

    #[tokio::test]
    async fn root_post_uses_see_other_to_replace_post_history_with_get() {
        let response = app()
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/")
                    .body(Body::empty())
                    .expect("build request"),
            )
            .await
            .expect("serve request");

        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        assert_eq!(
            response.headers().get(header::LOCATION),
            Some(&header::HeaderValue::from_static("/"))
        );

        let redirected = app()
            .oneshot(Request::builder().uri("/").body(Body::empty()).expect("build request"))
            .await
            .expect("follow redirect");
        assert_eq!(redirected.status(), StatusCode::OK);
        assert_eq!(
            redirected.headers().get(header::CONTENT_TYPE),
            Some(&header::HeaderValue::from_static("text/html; charset=utf-8"))
        );
    }

    /// 内联脚本的哈希与浏览器的算法一致：`<script>` 与 `</script>` 之间逐字节取，带 src 的跳过。
    #[test]
    fn inline_script_hashes_cover_only_inline_scripts() {
        let html = "<head><script>\n  a()\n</script><script type=\"module\" src=\"/x.js\"></script></head>";
        assert_eq!(inline_scripts(html).collect::<Vec<_>>(), vec!["\n  a()\n"]);
        let csp = INDEX_CSP.to_str().unwrap();
        assert!(csp.contains("frame-ancestors 'none'"), "{csp}");
        assert!(csp.contains("script-src 'self'"), "{csp}");
    }

    #[tokio::test]
    async fn spa_fallback_rejects_other_post_routes() {
        for uri in ["/unknown/route", "/missing.js"] {
            let response = app()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(uri)
                        .body(Body::empty())
                        .expect("build request"),
                )
                .await
                .expect("serve request");

            assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
            assert_ne!(
                response.headers().get(header::CONTENT_TYPE),
                Some(&header::HeaderValue::from_static("text/html; charset=utf-8"))
            );
            assert!(
                to_bytes(response.into_body(), 1024).await.expect("read response body").is_empty()
            );
        }
    }
}
