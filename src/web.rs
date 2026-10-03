//! 主站静态资源与前端路由入口；生产服务和浏览器测试共用同一实现。

use axum::Router;
use axum::extract::Request;
use axum::http::{HeaderValue, StatusCode, header};
use axum::middleware::{self, Next};
use axum::response::Response;
use std::path::Path;
use tower_http::compression::CompressionLayer;
use tower_http::services::{ServeDir, ServeFile};

pub fn router(directory: impl AsRef<Path>) -> Router {
    let directory = directory.as_ref().to_path_buf();
    // 前端路由不是错误页，必须保留入口文件的 200/304 状态。
    // not_found_service 会把无正文的 304 改成 404，导致 iOS 将恢复的页面识别为空文件。
    // 构建产物已预生成 .gz/.br 变体（vite.config.ts），命中 Accept-Encoding 时零压缩开销直发；
    // 未命中变体的文件（图标等小件）仍由 CompressionLayer 现场压缩兜底，已带编码的响应不会二次压缩。
    // Vite 产物全部落在 assets/ 且带内容哈希：单独路由并让缺失资源真 404，
    // 避免被 SPA fallback 用 index.html 顶替后又被 immutable 头错误长缓存。
    // route_service 不剥离已匹配前缀，ServeDir 直接以完整路径解析，共用同一根目录。
    let assets = ServeDir::new(&directory)
        .precompressed_gzip()
        .precompressed_br();
    let index = ServeFile::new(directory.join("index.html"))
        .precompressed_gzip()
        .precompressed_br();
    let files = ServeDir::new(&directory)
        .precompressed_gzip()
        .precompressed_br()
        .fallback(index);
    Router::new()
        .route_service("/assets/{*rest}", assets)
        .fallback_service(files)
        .layer(middleware::from_fn(static_cache_headers))
        .layer(CompressionLayer::new())
}

/// 带内容哈希的构建产物（/assets/*）可长缓存不可变存储；入口 HTML 与未哈希的
/// public 资源必须每次再验证，避免升级后旧页面继续引用已被替换的哈希资源。
async fn static_cache_headers(request: Request, next: Next) -> Response {
    let immutable = request.uri().path().starts_with("/assets/");
    let response = next.run(request).await;
    let status = response.status();
    if status != StatusCode::OK && status != StatusCode::NOT_MODIFIED {
        return response;
    }
    let mut response = response;
    let headers = response.headers_mut();
    if !headers.contains_key(header::CACHE_CONTROL) {
        let value = if immutable {
            HeaderValue::from_static("public, max-age=31536000, immutable")
        } else {
            // no-cache 允许存储但每次再验证，304 命中时无传输成本。
            HeaderValue::from_static("no-cache")
        };
        headers.insert(header::CACHE_CONTROL, value);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::{Method, Request, StatusCode, header};
    use axum::response::Response;
    use std::io::Write;
    use tower::ServiceExt;

    const HTML: &str = "<!doctype html><html lang=\"zh-CN\"><head><title>网盘测试</title></head><body><div id=\"app\">前端入口</div></body></html>";
    const CSS: &str = "body { color: #23312d; }";
    const APP_JS: &str = "export function boot(){ console.log('app asset') }\n";
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

    fn gzip_bytes(data: &[u8]) -> Vec<u8> {
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::best());
        encoder.write_all(data).unwrap();
        encoder.finish().unwrap()
    }

    fn brotli_bytes(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        {
            let mut writer = brotli::CompressorWriter::new(&mut out, 4096, 6, 22);
            writer.write_all(data).unwrap();
        }
        out
    }

    fn fixture() -> (Router, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::write(directory.path().join("index.html"), HTML).unwrap();
        std::fs::create_dir(directory.path().join("assets")).unwrap();
        std::fs::write(directory.path().join("assets/test.css"), CSS).unwrap();
        // 模拟带内容哈希的产物与预生成的压缩变体
        std::fs::write(directory.path().join("assets/app-abc123.js"), APP_JS).unwrap();
        std::fs::write(
            directory.path().join("assets/app-abc123.js.gz"),
            gzip_bytes(APP_JS.as_bytes()),
        )
        .unwrap();
        std::fs::write(
            directory.path().join("assets/app-abc123.js.br"),
            brotli_bytes(APP_JS.as_bytes()),
        )
        .unwrap();
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
    async fn hashed_assets_cache_immutable_and_serve_precompressed_variants() {
        let (app, directory) = fixture();
        let js_path = "/assets/app-abc123.js";
        let gzip_body = std::fs::read(directory.path().join("assets/app-abc123.js.gz")).unwrap();
        let br_body = std::fs::read(directory.path().join("assets/app-abc123.js.br")).unwrap();

        // brotli 优先：直接发送预生成变体，零运行时压缩开销
        let response = request(
            &app,
            Method::GET,
            js_path,
            &[("accept-encoding", "br, gzip")],
        )
        .await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()[header::CONTENT_ENCODING], "br");
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/javascript");
        assert_eq!(
            response.headers()[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
        assert_eq!(
            to_bytes(response.into_body(), 65536).await.unwrap(),
            br_body
        );

        // 仅支持 gzip 的客户端命中 .gz 变体
        let response = request(&app, Method::GET, js_path, &[("accept-encoding", "gzip")]).await;
        assert_eq!(response.headers()[header::CONTENT_ENCODING], "gzip");
        assert_eq!(
            to_bytes(response.into_body(), 65536).await.unwrap(),
            gzip_body
        );

        // 不声明 Accept-Encoding：返回原始内容且不带编码头
        let response = request(&app, Method::GET, js_path, &[]).await;
        assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
        assert_eq!(to_bytes(response.into_body(), 65536).await.unwrap(), APP_JS);

        // 304 再验证仍携带长缓存指令，客户端可继续信任已缓存变体
        let fresh = request(&app, Method::GET, js_path, &[]).await;
        let etag = fresh.headers()[header::ETAG].clone();
        let cached = request(
            &app,
            Method::GET,
            js_path,
            &[("if-none-match", etag.to_str().unwrap())],
        )
        .await;
        assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
        assert_eq!(
            cached.headers()[header::CACHE_CONTROL],
            "public, max-age=31536000, immutable"
        );
    }

    #[tokio::test]
    async fn index_and_spa_routes_revalidate_on_every_load() {
        let (app, _directory) = fixture();
        for path in ["/", "/index.html", "/files/", "/login"] {
            let response = request(&app, Method::GET, path, &[]).await;
            assert_eq!(response.status(), StatusCode::OK, "前端路由 {path}");
            assert_eq!(
                response.headers()[header::CACHE_CONTROL],
                "no-cache",
                "{path} 是未哈希的入口资源，必须每次再验证"
            );
        }
        // 404 不携带缓存指令，避免把缺失资源长缓存
        let missing = request(&app, Method::GET, "/assets/missing-404.js", &[]).await;
        assert_eq!(missing.status(), StatusCode::NOT_FOUND);
        assert!(!missing.headers().contains_key(header::CACHE_CONTROL));
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
