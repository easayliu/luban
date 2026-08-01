//! 使用 rust-embed 将 `admin-ui/dist` 前端构建产物内嵌进二进制并提供静态服务。
//!
//! 参考 kiro.rs 的做法：SPA fallback + 按路径设置缓存策略。

use axum::{
    body::Body,
    http::{Response, StatusCode, Uri, header},
    response::{IntoResponse, Redirect},
};
use rust_embed::Embed;

/// 内嵌前端构建产物（编译期从 `admin-ui/dist` 读取）。
#[derive(Embed)]
#[folder = "admin-ui/dist"]
struct Asset;

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
            .body(Body::from(content.data.into_owned()))
            .expect("build response"),
        None => Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Body::from("前端尚未构建。请在 admin-ui 目录执行 `pnpm build`。"))
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
