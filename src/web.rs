//! 主站静态资源与前端路由入口；生产服务和浏览器测试共用同一实现。

use axum::Router;
use std::path::Path;
use tower_http::compression::CompressionLayer;
use tower_http::services::{ServeDir, ServeFile};

pub fn router(directory: impl AsRef<Path>) -> Router {
    let directory = directory.as_ref();
    // 前端路由不是错误页，必须保留入口文件的 200/304 状态。
    // not_found_service 会把无正文的 304 改成 404，导致 iOS 将恢复的页面识别为空文件。
    let files = ServeDir::new(directory).fallback(ServeFile::new(directory.join("index.html")));
    Router::new()
        .fallback_service(files)
        .layer(CompressionLayer::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, StatusCode, header};
    use axum::response::Response;
    use tower::ServiceExt;

    const HTML: &str = "<!doctype html><html lang=\"zh-CN\"><head><title>网盘测试</title></head><body><div id=\"app\">前端入口</div></body></html>";
    const CSS: &str = "body { color: #23312d; }";
    const ROUTES: &[&str] = &[
        "/",
        "/index.html",
        "/files",
        "/files/",
        "/files/%E6%B5%8B%E8%AF%95/a.txt?view=list",
        "/login",
        "/apps",
        "/apps/books",
        "/transfers",
        "/me",
        "/s/test-share",
    ];

    fn fixture() -> (Router, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("index.html"), HTML).unwrap();
        std::fs::create_dir(directory.path().join("assets")).unwrap();
        std::fs::write(directory.path().join("assets/test.css"), CSS).unwrap();
        let app = router(directory.path()).layer(axum::middleware::from_fn_with_state(
            false,
            crate::security::response_headers,
        ));
        (app, directory)
    }

    async fn request(
        app: &Router,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
    ) -> Response {
        let mut request = Request::builder().method(method).uri(path);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        app.clone()
            .oneshot(request.body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn frontend_routes_return_successful_html() {
        let (app, _directory) = fixture();
        for path in ROUTES {
            let response = request(&app, Method::GET, path, &[]).await;
            assert_eq!(response.status(), StatusCode::OK, "前端路由 {path}");
            assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
            assert_eq!(
                response.headers()[header::X_CONTENT_TYPE_OPTIONS],
                "nosniff"
            );
            assert!(!response.headers().contains_key(header::CONTENT_DISPOSITION));
            assert_eq!(to_bytes(response.into_body(), 4096).await.unwrap(), HTML);
        }
    }

    #[tokio::test]
    async fn cache_revalidation_preserves_not_modified_instead_of_empty_not_found() {
        let (app, _directory) = fixture();
        let first = request(&app, Method::GET, "/", &[]).await;
        let etag = first.headers()[header::ETAG].clone();
        let modified = first.headers()[header::LAST_MODIFIED].clone();
        for path in ROUTES {
            for (name, value) in [
                ("if-none-match", etag.to_str().unwrap()),
                ("if-modified-since", modified.to_str().unwrap()),
            ] {
                for method in [Method::GET, Method::HEAD] {
                    let response = request(&app, method, path, &[(name, value)]).await;
                    assert_eq!(
                        response.status(),
                        StatusCode::NOT_MODIFIED,
                        "{path} 的 {name} 缓存校验不能变成无正文、无类型的 404"
                    );
                    assert_eq!(response.headers()[header::ETAG], etag);
                    assert_eq!(response.headers()[header::LAST_MODIFIED], modified);
                    assert!(
                        to_bytes(response.into_body(), 4096)
                            .await
                            .unwrap()
                            .is_empty()
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn stale_cache_validators_return_complete_html() {
        let (app, _directory) = fixture();
        for path in ROUTES {
            for header in [
                ("if-none-match", "\"old-version\""),
                ("if-modified-since", "Thu, 01 Jan 1970 00:00:00 GMT"),
            ] {
                let response = request(&app, Method::GET, path, &[header]).await;
                assert_eq!(
                    response.status(),
                    StatusCode::OK,
                    "旧缓存需要重新获取 {path}"
                );
                assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
                assert_eq!(to_bytes(response.into_body(), 4096).await.unwrap(), HTML);
            }
        }
    }

    #[tokio::test]
    async fn head_keeps_html_metadata_without_a_body() {
        let (app, _directory) = fixture();
        for path in ROUTES {
            let response = request(&app, Method::HEAD, path, &[]).await;
            assert_eq!(response.status(), StatusCode::OK, "前端路由 {path} 的 HEAD");
            assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
            assert_eq!(
                response.headers()[header::CONTENT_LENGTH].to_str().unwrap(),
                HTML.len().to_string()
            );
            assert!(
                to_bytes(response.into_body(), 4096)
                    .await
                    .unwrap()
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn static_assets_keep_their_content_type_and_cache_validation() {
        let (app, _directory) = fixture();
        let response = request(&app, Method::GET, "/assets/test.css", &[]).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/css");
        let etag = response.headers()[header::ETAG].clone();
        assert_eq!(to_bytes(response.into_body(), 4096).await.unwrap(), CSS);
        let cached = request(
            &app,
            Method::GET,
            "/assets/test.css",
            &[("if-none-match", etag.to_str().unwrap())],
        )
        .await;
        assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
        assert!(to_bytes(cached.into_body(), 4096).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn compressed_navigation_also_preserves_cache_status() {
        let (app, _directory) = fixture();
        let response = request(&app, Method::GET, "/files/", &[("accept-encoding", "gzip")]).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
        assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
        let etag = response.headers()[header::ETAG].clone();
        assert!(
            !to_bytes(response.into_body(), 4096)
                .await
                .unwrap()
                .is_empty()
        );
        let cached = request(
            &app,
            Method::GET,
            "/files/",
            &[
                ("accept-encoding", "gzip"),
                ("if-none-match", etag.to_str().unwrap()),
            ],
        )
        .await;
        assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
        assert!(to_bytes(cached.into_body(), 4096).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn missing_frontend_and_unsupported_methods_are_not_reported_as_success() {
        let empty = tempfile::tempdir().unwrap();
        let missing = request(&router(empty.path()), Method::GET, "/files/", &[]).await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        let (app, _directory) = fixture();
        let post = request(&app, Method::POST, "/files/", &[]).await;
        assert_eq!(post.status(), StatusCode::METHOD_NOT_ALLOWED);
    }
}
