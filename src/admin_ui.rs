//! 使用 rust-embed 将 `admin-ui/dist` 前端构建产物内嵌进二进制并提供静态服务。
//!
//! 参考 kiro.rs 的做法：SPA fallback + 按路径设置缓存策略。

use axum::{
    body::Body,
    http::{Response, StatusCode, Uri, header},
    response::IntoResponse,
};
use rust_embed::Embed;

/// 内嵌前端构建产物（编译期从 `admin-ui/dist` 读取）。
#[derive(Embed)]
#[folder = "admin-ui/dist"]
struct Asset;

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
        let mime = mime_guess::from_path(path)
            .first_or_octet_stream()
            .to_string();
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
            .body(Body::from(
                "前端尚未构建。请在 admin-ui 目录执行 `pnpm build`。",
            ))
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
    path.rsplit('/')
        .next()
        .map(|f| f.contains('.'))
        .unwrap_or(false)
}
